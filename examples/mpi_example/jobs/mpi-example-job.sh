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
# - NUM_TRANSFERS (optional, default: 256)
# - WINDOW_SIZE (optional, default: 32)

source "$SCRIPT_DIR/common.sh"

JOB_START=$(timestamp)
NNODES=$(wc --lines "${PBS_NODEFILE}" | awk '{print $1}')
JOBID=$(echo "$PBS_JOBID" | cut -d : -f 2)
JOB_OUTPUT_DIR="${OUTPUT_DIR}/${JOB_START}-${JOBID}-${NNODES}"
REGISTRY_DIR="${JOB_OUTPUT_DIR}/registry"

# Split nodes: half for servers, half for clients
SERVER_NODEFILE="${JOB_OUTPUT_DIR}/server_nodes"
CLIENT_NODEFILE="${JOB_OUTPUT_DIR}/client_nodes"

# Data directory - use local SSD if available
if [ -d "/local" ]; then
    DATA_DIR="/local/${USER}/mpi_read_bench"
else
    DATA_DIR="${JOB_OUTPUT_DIR}/data"
fi

# Benchmark parameters (with defaults)
: ${DATA_SIZE:=$((4 << 20))}
: ${NUM_TRANSFERS:=1024}
: ${WINDOW_SIZE:=64}

# ==============================================================================
# UCX Configuration for MPI + UCX Integration Test
# ==============================================================================

# Force InfiniBand + TCP configuration for Socket connection mode
# export UCX_TLS="tcp,rc_mlx5,sm,self"
# export UCX_MEMTYPE_CACHE="n"
# export UCX_NET_DEVICES="all"
# export UCX_PROTOS="^ud,dc"

# # Timeout and retry settings
# export UCX_RC_TIMEOUT=2.0s
# export UCX_RC_RETRY_COUNT=16
# export UCX_RC_TIMEOUT_MULTIPLIER=4.0

# # Active Message settings
# export UCX_AM_MAX_SHORT=128
# export UCX_AM_MAX_EAGER=8192
# export UCX_RNDV_THRESH=inf

# # RDMA settings
# export UCX_ZCOPY_THRESH=0
# export UCX_RNDV_SCHEME=get_zcopy
# export UCX_IB_NUM_PATHS=2
# export UCX_RC_MLX5_TM_ENABLE=y
# export UCX_RC_MLX5_RX_QUEUE_LEN=4096

# # Memory registration cache
# export UCX_RCACHE_ENABLE=y

# # Flow control
# export UCX_RC_FC_ENABLE=y
# export UCX_RC_MAX_NUM_EPS=-1

# # Network layer
# export UCX_IB_SEG_SIZE=8192
# export UCX_RC_PATH_MTU=4096

# # Progress settings
# export UCX_ADAPTIVE_PROGRESS=y
# export UCX_ASYNC_MAX_EVENTS=256
# export UCX_USE_MT_MUTEX=n

# # Logging level (set to WARN for production, DEBUG/TRACE for debugging)
# export UCX_LOG_LEVEL="WARN"

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
echo "Benchmark Parameters:"
echo "  DATA_SIZE: $((DATA_SIZE / (1 << 20))) MiB"
echo "  NUM_TRANSFERS: ${NUM_TRANSFERS}"
echo "  WINDOW_SIZE: ${WINDOW_SIZE}"
echo "  Total per client: $((DATA_SIZE * NUM_TRANSFERS / (1 << 20))) MiB"
echo "=========================================="

# Prepare output directory
mkdir -p "${JOB_OUTPUT_DIR}"
cp "$0" "${JOB_OUTPUT_DIR}"
cp "${PBS_NODEFILE}" "${JOB_OUTPUT_DIR}"
printenv >"${JOB_OUTPUT_DIR}/env.txt"

# Prepare registry directory
mkdir -p "${REGISTRY_DIR}"

# Split nodes into servers and clients (half and half)
UNIQUE_NODES=$(cat "${PBS_NODEFILE}" | sort -u)
NUM_UNIQUE_NODES=$(echo "$UNIQUE_NODES" | wc -l)
NUM_SERVER_NODES=$((NUM_UNIQUE_NODES / 2))
NUM_CLIENT_NODES=$((NUM_UNIQUE_NODES - NUM_SERVER_NODES))

echo "$UNIQUE_NODES" | head -n "$NUM_SERVER_NODES" > "${SERVER_NODEFILE}"
echo "$UNIQUE_NODES" | tail -n "$NUM_CLIENT_NODES" > "${CLIENT_NODEFILE}"

echo "Node allocation:"
echo "  Total unique nodes: ${NUM_UNIQUE_NODES}"
echo "  Server nodes: ${NUM_SERVER_NODES}"
echo "  Client nodes: ${NUM_CLIENT_NODES}"
echo ""
echo "Server nodes:"
cat "${SERVER_NODEFILE}"
echo ""
echo "Client nodes:"
cat "${CLIENT_NODEFILE}"
echo ""

# Prepare data directory on server nodes only
echo "Creating data directory on server nodes..."
for node in $(cat "${SERVER_NODEFILE}"); do
    ssh "$node" "mkdir -p ${DATA_DIR}" || true
done

# MPI Configuration: Use ob1/tcp to avoid UCX context conflicts
# mpi_example manages UCX directly for Socket + InfiniBand communication
cmd_mpirun=(
  mpirun
  "${nqsii_mpiopts_array[@]}"
  --mca pml ob1
  --mca btl tcp,vader,self
  --mca btl_openib_allow_ib 0
  -x UCX_TLS
  -x UCX_NET_DEVICES
  -x UCX_MEMTYPE_CACHE
  -x UCX_PROTOS
  -x UCX_LOG_LEVEL
  -x UCX_RNDV_THRESH
  -x UCX_RNDV_SCHEME
  -x UCX_RC_TIMEOUT
  -x UCX_RC_RETRY_COUNT
  -x UCX_RC_TIMEOUT_MULTIPLIER
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
echo "Server processes: ${NUM_SERVER_NODES}"
echo "Client processes: ${NUM_CLIENT_NODES}"
echo "=========================================="
echo ""

# Server command
cmd_server=(
  "${cmd_mpirun[@]}"
  --hostfile "${SERVER_NODEFILE}"
  -np "${NUM_SERVER_NODES}"
  --bind-to core
  -x RUST_LOG
  -x RUST_BACKTRACE
  -x DATA_SIZE="${DATA_SIZE}"
  "${MPI_EXAMPLE_PREFIX}/mpi_example"
  server
  "${REGISTRY_DIR}"
  "${DATA_DIR}"
)

# Client command
cmd_client=(
  "${cmd_mpirun[@]}"
  --hostfile "${CLIENT_NODEFILE}"
  -np "${NUM_CLIENT_NODES}"
  --bind-to core
  -x RUST_LOG
  -x RUST_BACKTRACE
  -x DATA_SIZE="${DATA_SIZE}"
  -x NUM_TRANSFERS="${NUM_TRANSFERS}"
  -x WINDOW_SIZE="${WINDOW_SIZE}"
  "${MPI_EXAMPLE_PREFIX}/mpi_example"
  client
  "${REGISTRY_DIR}"
  "${NUM_SERVER_NODES}"
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
    if [ "$SOCKET_COUNT" -ge "$NUM_SERVER_NODES" ]; then
        echo "All ${NUM_SERVER_NODES} servers are ready"
        break
    fi
    sleep 1
    WAIT_COUNT=$((WAIT_COUNT + 1))
    if [ $((WAIT_COUNT % 10)) -eq 0 ]; then
        echo "  Waiting... ($SOCKET_COUNT/$NUM_SERVER_NODES servers ready)"
    fi
done

if [ $WAIT_COUNT -ge $MAX_WAIT ]; then
    echo "ERROR: Timeout waiting for servers to start"
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

# Cleanup data files on server nodes
echo ""
echo "Cleaning up data files..."
for node in $(cat "${SERVER_NODEFILE}"); do
    ssh "$node" "rm -rf ${DATA_DIR}" || true
done

echo "=========================================="
echo "Full logs available at:"
echo "  ${JOB_OUTPUT_DIR}/server_stdout.log"
echo "  ${JOB_OUTPUT_DIR}/server_stderr.log"
echo "  ${JOB_OUTPUT_DIR}/client_stdout.log"
echo "  ${JOB_OUTPUT_DIR}/client_stderr.log"
echo "=========================================="

exit ${EXIT_CODE}
