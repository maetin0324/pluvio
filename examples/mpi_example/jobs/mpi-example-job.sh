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

source "$SCRIPT_DIR/common.sh"

JOB_START=$(timestamp)
NNODES=$(wc --lines "${PBS_NODEFILE}" | awk '{print $1}')
JOBID=$(echo "$PBS_JOBID" | cut -d : -f 2)
JOB_OUTPUT_DIR="${OUTPUT_DIR}/${JOB_START}-${JOBID}-${NNODES}"
REGISTRY_DIR="${JOB_OUTPUT_DIR}/registry"

# ==============================================================================
# UCX Configuration for MPI + UCX Integration Test
# ==============================================================================

# Force InfiniBand + TCP configuration for Socket connection mode
export UCX_TLS="tcp,rc_mlx5,sm,self"
export UCX_MEMTYPE_CACHE="n"
export UCX_NET_DEVICES="all"
export UCX_PROTOS="^ud,dc"

# Timeout and retry settings
export UCX_RC_TIMEOUT=2.0s
export UCX_RC_RETRY_COUNT=16
export UCX_RC_TIMEOUT_MULTIPLIER=4.0

# Active Message settings
export UCX_AM_MAX_SHORT=128
export UCX_AM_MAX_EAGER=8192
export UCX_RNDV_THRESH=inf

# RDMA settings
export UCX_ZCOPY_THRESH=0
export UCX_RNDV_SCHEME=get_zcopy
export UCX_IB_NUM_PATHS=2
export UCX_RC_MLX5_TM_ENABLE=y
export UCX_RC_MLX5_RX_QUEUE_LEN=4096

# Memory registration cache
export UCX_RCACHE_ENABLE=y

# Flow control
export UCX_RC_FC_ENABLE=y
export UCX_RC_MAX_NUM_EPS=-1

# Network layer
export UCX_IB_SEG_SIZE=8192
export UCX_RC_PATH_MTU=4096

# Progress settings
export UCX_ADAPTIVE_PROGRESS=y
export UCX_ASYNC_MAX_EVENTS=256
export UCX_USE_MT_MUTEX=n

export UCX_LOG_LEVEL="TRACE"

IFS=" " read -r -a nqsii_mpiopts_array <<<"$NQSII_MPIOPTS"

echo "=========================================="
echo "MPI Example Job Configuration"
echo "=========================================="
echo "Job ID: ${JOBID}"
echo "Nodes: ${NNODES}"
echo "Output: ${JOB_OUTPUT_DIR}"
echo "Registry: ${REGISTRY_DIR}"
echo "Binary: ${MPI_EXAMPLE_PREFIX}/mpi_example"
echo "=========================================="

# Prepare output directory
mkdir -p "${JOB_OUTPUT_DIR}"
cp "$0" "${JOB_OUTPUT_DIR}"
cp "${PBS_NODEFILE}" "${JOB_OUTPUT_DIR}"
printenv >"${JOB_OUTPUT_DIR}/env.txt"

# Prepare registry directory
mkdir -p "${REGISTRY_DIR}"

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
  -x UCX_LOG_LEVEL
  -x PATH
)

echo "MPI Configuration: Using ob1/tcp to avoid UCX context conflicts"

# Rust logging
export RUST_LOG=info
export RUST_BACKTRACE=1

# Run MPI example
# N processes total (N/2 servers + N/2 clients)
echo ""
echo "=========================================="
echo "Running MPI Example Test"
echo "=========================================="
echo "Total processes: ${NNODES}"
echo "Server ranks (even): ~$((NNODES / 2))"
echo "Client ranks (odd): ~$((NNODES / 2))"
echo "Data size: 1 MiB x 128 transfers = 128 MiB per client"
echo "Pipeline window: 32"
echo "=========================================="
echo ""

cmd_mpi_example=(
  "${cmd_mpirun[@]}"
  # -np "${NNODES}"
  --bind-to core
  # -map-by ppr:2:node
  -x RUST_LOG
  -x RUST_BACKTRACE
  "${MPI_EXAMPLE_PREFIX}/mpi_example"
  "${REGISTRY_DIR}"
)

echo "Command: ${cmd_mpi_example[@]}"
echo ""

"${cmd_mpi_example[@]}" \
  > "${JOB_OUTPUT_DIR}/stdout.log" \
  2> "${JOB_OUTPUT_DIR}/stderr.log"

EXIT_CODE=$?

echo ""
echo "=========================================="
echo "MPI Example Test Completed"
echo "=========================================="
echo "Exit code: ${EXIT_CODE}"
echo ""

if [ ${EXIT_CODE} -eq 0 ]; then
  echo "Test PASSED"
  echo ""
  echo "Output summary:"
  tail -50 "${JOB_OUTPUT_DIR}/stdout.log"
else
  echo "Test FAILED"
  echo ""
  echo "STDOUT:"
  cat "${JOB_OUTPUT_DIR}/stdout.log"
  echo ""
  echo "STDERR:"
  cat "${JOB_OUTPUT_DIR}/stderr.log"
fi

echo "=========================================="
echo "Full logs available at:"
echo "  ${JOB_OUTPUT_DIR}/stdout.log"
echo "  ${JOB_OUTPUT_DIR}/stderr.log"
echo "=========================================="

exit ${EXIT_CODE}
