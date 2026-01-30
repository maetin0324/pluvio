#!/bin/bash
#------- qsub option -----------
#PBS -A NBB
#PBS -l elapstim_req=0:10:00
#PBS -T openmpi
#PBS -v NQSV_MPI_VER=4.1.8/gcc11.4.0-cuda12.8.1
#------- Program execution -----------
set -euo pipefail

# Increase file descriptor limit
ulimit -n 65536

module purge
module load "openmpi/$NQSV_MPI_VER"

# Requires
# - SCRIPT_DIR
# - OUTPUT_DIR
# - MPI_EXAMPLE_PREFIX
# - DATA_SIZE (optional, default: 4MiB)
# - NUM_TRANSFERS (optional, default: 1024)
# - WINDOW_SIZE (optional, default: 64)
# - CLIENT_PPN (optional, default: 1)

source "$SCRIPT_DIR/common.sh"

JOB_START=$(timestamp)
NNODES=$(wc --lines "${PBS_NODEFILE}" | awk '{print $1}')
JOBID=$(echo "$PBS_JOBID" | cut -d : -f 2)
JOB_OUTPUT_DIR="${OUTPUT_DIR}/${JOB_START}-${JOBID}-${NNODES}"
REGISTRY_DIR="${JOB_OUTPUT_DIR}/registry"

# Data directory - use local SSD if available
if [ -d "/scr" ]; then
    DATA_DIR="/scr/${USER}/mpi_read_bench"
else
    DATA_DIR="${JOB_OUTPUT_DIR}/data"
fi

# Benchmark parameters (with defaults)
: ${DATA_SIZE:=$((4 << 20))}
: ${NUM_TRANSFERS:=1024}
: ${WINDOW_SIZE:=64}
: ${CLIENT_PPN:=1}

# Server always runs with ppn=1
SERVER_PPN=1
SERVER_NP=$((NNODES * SERVER_PPN))

# Client runs with configurable ppn
CLIENT_NP=$((NNODES * CLIENT_PPN))

IFS=" " read -r -a nqsii_mpiopts_array <<<"$NQSII_MPIOPTS"

echo "=========================================="
echo "MPI Remote READ Benchmark Configuration"
echo "=========================================="
echo "Job ID: ${JOBID}"
echo "Nodes: ${NNODES}"
echo "Output: ${JOB_OUTPUT_DIR}"
echo "Registry: ${REGISTRY_DIR}"
echo "Data Dir: ${DATA_DIR}"
echo "Binary: ${MPI_EXAMPLE_PREFIX}/mpi_example"
echo ""
echo "Process Configuration:"
echo "  Server: PPN=${SERVER_PPN}, NP=${SERVER_NP}"
echo "  Client: PPN=${CLIENT_PPN}, NP=${CLIENT_NP}"
echo ""
echo "Benchmark Parameters:"
echo "  DATA_SIZE: $((DATA_SIZE / (1 << 20))) MiB"
echo "  NUM_TRANSFERS: ${NUM_TRANSFERS}"
echo "  WINDOW_SIZE: ${WINDOW_SIZE}"
echo "  Total per client: $((DATA_SIZE * NUM_TRANSFERS / (1 << 20))) MiB"
echo "=========================================="

# Prepare output directory
mkdir -p "${JOB_OUTPUT_DIR}"
cp "$0" "${JOB_OUTPUT_DIR}"
cp "${PBS_NODEFILE}" "${JOB_OUTPUT_DIR}/nodelist"
printenv >"${JOB_OUTPUT_DIR}/env.txt"

# Prepare registry directory
mkdir -p "${REGISTRY_DIR}"

# Prepare data directory on all nodes
echo "Creating data directory on all nodes..."
for node in $(cat "${PBS_NODEFILE}" | sort -u); do
    ssh "$node" "mkdir -p ${DATA_DIR}" || true
done

# MPI Configuration: Use ob1/tcp to avoid UCX context conflicts
# mpi_example manages UCX directly for Socket + InfiniBand communication
# Note: vader (shared memory) is disabled to avoid errors with high ppn values
cmd_mpirun_common=(
  mpirun
  "${nqsii_mpiopts_array[@]}"
  --mca pml ob1
  --mca btl tcp,self
  --mca btl_openib_allow_ib 0
  -x PATH
)

echo "MPI Configuration: Using ob1/tcp to avoid UCX context conflicts"

# Rust logging
export RUST_LOG=info
export RUST_BACKTRACE=1

# Run MPI example with separate server and client mpirun
echo ""
echo "=========================================="
echo "Running Remote READ Benchmark"
echo "=========================================="
echo "Server: ${SERVER_NP} processes (PPN=${SERVER_PPN})"
echo "Client: ${CLIENT_NP} processes (PPN=${CLIENT_PPN})"
echo "=========================================="
echo ""

# Server command: ppn=1 on all nodes
cmd_server=(
  "${cmd_mpirun_common[@]}"
  -np "${SERVER_NP}"
  --map-by "ppr:${SERVER_PPN}:node"
  --bind-to none
  -x RUST_LOG
  -x RUST_BACKTRACE
  -x DATA_SIZE="${DATA_SIZE}"
  "${MPI_EXAMPLE_PREFIX}/mpi_example"
  server
  "${REGISTRY_DIR}"
  "${DATA_DIR}"
)

# Client command: ppn=CLIENT_PPN on all nodes
cmd_client=(
  "${cmd_mpirun_common[@]}"
  -np "${CLIENT_NP}"
  --map-by "ppr:${CLIENT_PPN}:node"
  --bind-to none
  -x RUST_LOG
  -x RUST_BACKTRACE
  -x DATA_SIZE="${DATA_SIZE}"
  -x NUM_TRANSFERS="${NUM_TRANSFERS}"
  -x WINDOW_SIZE="${WINDOW_SIZE}"
  "${MPI_EXAMPLE_PREFIX}/mpi_example"
  client
  "${REGISTRY_DIR}"
  "${SERVER_NP}"
)

echo "Server command: ${cmd_server[@]}"
echo ""
echo "Client command: ${cmd_client[@]}"
echo ""

# Start servers in background
echo "Starting servers..."
"${cmd_server[@]}" \
  > "${JOB_OUTPUT_DIR}/server_stdout.log" \
  2> "${JOB_OUTPUT_DIR}/server_stderr.log" &
SERVER_PID=$!

# Wait for server socket files to appear
echo "Waiting for servers to be ready..."
MAX_WAIT=60
WAIT_COUNT=0
while [ $WAIT_COUNT -lt $MAX_WAIT ]; do
    SOCKET_COUNT=$(find "${REGISTRY_DIR}" -name "server_*.socket" 2>/dev/null | wc -l)
    if [ "$SOCKET_COUNT" -ge "$SERVER_NP" ]; then
        echo "All ${SERVER_NP} servers are ready"
        break
    fi
    sleep 1
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ $((WAIT_COUNT % 10)) -eq 0 ]; then
        echo "  Waiting... ($SOCKET_COUNT/$SERVER_NP servers ready)"
    fi
done

if [ $WAIT_COUNT -ge $MAX_WAIT ]; then
    echo "ERROR: Timeout waiting for servers to start"
    echo "Server stderr:"
    cat "${JOB_OUTPUT_DIR}/server_stderr.log" || true
    kill $SERVER_PID 2>/dev/null || true
    exit 1
fi

# Start clients
echo "Starting clients..."
"${cmd_client[@]}" \
  > "${JOB_OUTPUT_DIR}/client_stdout.log" \
  2> "${JOB_OUTPUT_DIR}/client_stderr.log"
CLIENT_EXIT_CODE=$?

# Wait for servers to finish (they should exit after clients disconnect)
echo "Waiting for servers to finish..."
wait $SERVER_PID
SERVER_EXIT_CODE=$?

echo "Server exit code: ${SERVER_EXIT_CODE}"
echo "Client exit code: ${CLIENT_EXIT_CODE}"

# Combined exit code
if [ $CLIENT_EXIT_CODE -ne 0 ]; then
    EXIT_CODE=$CLIENT_EXIT_CODE
elif [ $SERVER_EXIT_CODE -ne 0 ]; then
    EXIT_CODE=$SERVER_EXIT_CODE
else
    EXIT_CODE=0
fi

echo ""
echo "=========================================="
echo "Remote READ Benchmark Completed"
echo "=========================================="
echo "Exit code: ${EXIT_CODE}"
echo ""

if [ ${EXIT_CODE} -eq 0 ]; then
  echo "Benchmark PASSED"
  echo ""
  echo "Client output summary:"
  tail -100 "${JOB_OUTPUT_DIR}/client_stdout.log"
else
  echo "Benchmark FAILED"
  echo ""
  echo "SERVER STDOUT:"
  cat "${JOB_OUTPUT_DIR}/server_stdout.log"
  echo ""
  echo "SERVER STDERR:"
  cat "${JOB_OUTPUT_DIR}/server_stderr.log"
  echo ""
  echo "CLIENT STDOUT:"
  cat "${JOB_OUTPUT_DIR}/client_stdout.log"
  echo ""
  echo "CLIENT STDERR:"
  cat "${JOB_OUTPUT_DIR}/client_stderr.log"
fi

# Cleanup data files on all nodes
echo ""
echo "Cleaning up data files..."
for node in $(cat "${PBS_NODEFILE}" | sort -u); do
    ssh "$node" "rm -rf ${DATA_DIR}" || true
done

echo "=========================================="
echo "Full logs available at:"
echo "  ${JOB_OUTPUT_DIR}/server_stdout.log"
echo "  ${JOB_OUTPUT_DIR}/server_stderr.log"
echo "  ${JOB_OUTPUT_DIR}/client_stdout.log"
echo "  ${JOB_OUTPUT_DIR}/client_stderr.log"
echo "=========================================="

# Copy PBS stdout/stderr to job output directory
# PBS files are in SCRIPT_DIR (jobs directory), named like: mpi-example-job.sh.o<jobid> and mpi-example-job.sh.e<jobid>
PBS_STDOUT="${SCRIPT_DIR}/mpi-example-job.sh.o${JOBID}"
PBS_STDERR="${SCRIPT_DIR}/mpi-example-job.sh.e${JOBID}"
if [ -f "${PBS_STDOUT}" ]; then
    cp "${PBS_STDOUT}" "${JOB_OUTPUT_DIR}/pbs_stdout.log"
fi
if [ -f "${PBS_STDERR}" ]; then
    cp "${PBS_STDERR}" "${JOB_OUTPUT_DIR}/pbs_stderr.log"
fi

exit ${EXIT_CODE}
