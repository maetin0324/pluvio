#!/bin/bash
set -euo pipefail
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE:-$0}" )" &> /dev/null && pwd )"
source "${SCRIPT_DIR}/common.sh"
TIMESTAMP="$(timestamp)"

# default params
: ${ELAPSTIM_REQ:="0:30:00"}
: ${LABEL:=read_bench}

# Benchmark parameters (can be overridden by environment variables)
: ${DATA_SIZE:=$((4 << 20))}        # 4 MiB per transfer (matching BenchFS chunk size)
: ${NUM_TRANSFERS:=1024}            # Number of transfers per client
: ${WINDOW_SIZE:=64}                # Concurrent requests window
: ${CLIENT_PPN:=1}                  # Client processes per node (server always ppn=1)

JOB_FILE="$(remove_ext "$(this_file)")-job.sh"
MPI_EXAMPLE_DIR="$(to_fullpath "$(this_directory)/..")"
OUTPUT_DIR="${MPI_EXAMPLE_DIR}/results/${TIMESTAMP}-${LABEL}"
MPI_EXAMPLE_PREFIX="${MPI_EXAMPLE_DIR}/target/release"

# Debug: Print paths
echo "=========================================="
echo "MPI Remote READ Benchmark Job Submission"
echo "=========================================="
echo "MPI_EXAMPLE_DIR: $MPI_EXAMPLE_DIR"
echo "MPI_EXAMPLE_PREFIX: $MPI_EXAMPLE_PREFIX"
echo ""
echo "Architecture:"
echo "  - All nodes run both server (ppn=1) and client (ppn=${CLIENT_PPN})"
echo "  - Servers and clients run as separate mpirun processes"
echo ""
echo "Benchmark Parameters:"
echo "  DATA_SIZE: $((DATA_SIZE / (1 << 20))) MiB"
echo "  NUM_TRANSFERS: $NUM_TRANSFERS"
echo "  WINDOW_SIZE: $WINDOW_SIZE"
echo "  CLIENT_PPN: $CLIENT_PPN"
echo "  Total per client: $((DATA_SIZE * NUM_TRANSFERS / (1 << 20))) MiB"
echo ""
echo "Checking binary:"
ls -la "${MPI_EXAMPLE_PREFIX}/mpi_example" || echo "ERROR: Binary not found at ${MPI_EXAMPLE_PREFIX}/mpi_example"
echo "=========================================="

mkdir -p "${OUTPUT_DIR}"

nnodes_list=(
  16
  # 4 8 16
)
niter=1

param_set_list=(
  "
  NQSV_MPI_VER=4.1.8/gcc11.4.0-cuda12.8.1
  "
)

for nnodes in "${nnodes_list[@]}"; do
  for ((iter=0; iter<niter; iter++)); do
    for param_set in "${param_set_list[@]}"; do
      eval "$param_set"

      cmd_qsub=(
        qsub
        -A NBBG
        -q gen_S
        -l elapstim_req="${ELAPSTIM_REQ}"
        -T openmpi
        -v NQSV_MPI_VER="${NQSV_MPI_VER}"
        -b "$nnodes"
        -v OUTPUT_DIR="$OUTPUT_DIR"
        -v SCRIPT_DIR="$SCRIPT_DIR"
        -v LABEL="$LABEL"
        -v MPI_EXAMPLE_PREFIX="$MPI_EXAMPLE_PREFIX"
        -v DATA_SIZE="$DATA_SIZE"
        -v NUM_TRANSFERS="$NUM_TRANSFERS"
        -v WINDOW_SIZE="$WINDOW_SIZE"
        -v CLIENT_PPN="$CLIENT_PPN"
        "${JOB_FILE}"
      )
      echo "${cmd_qsub[@]}"
      "${cmd_qsub[@]}"
    done
  done
done
