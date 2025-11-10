#!/bin/bash
set -euo pipefail
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE:-$0}" )" &> /dev/null && pwd )"
source "${SCRIPT_DIR}/common.sh"
TIMESTAMP="$(timestamp)"

# default params
: ${ELAPSTIM_REQ:="0:05:00"}
: ${LABEL:=default}

JOB_FILE="$(remove_ext "$(this_file)")-job.sh"
MPI_EXAMPLE_DIR="$(to_fullpath "$(this_directory)/..")"
OUTPUT_DIR="${MPI_EXAMPLE_DIR}/results/${TIMESTAMP}-${LABEL}"
MPI_EXAMPLE_PREFIX="${MPI_EXAMPLE_DIR}/target/release"

# Debug: Print paths
echo "=========================================="
echo "MPI Example Job Submission"
echo "=========================================="
echo "MPI_EXAMPLE_DIR: $MPI_EXAMPLE_DIR"
echo "MPI_EXAMPLE_PREFIX: $MPI_EXAMPLE_PREFIX"
echo ""
echo "Checking binary:"
ls -la "${MPI_EXAMPLE_PREFIX}/mpi_example" || echo "ERROR: Binary not found at ${MPI_EXAMPLE_PREFIX}/mpi_example"
echo "=========================================="

mkdir -p "${OUTPUT_DIR}"

nnodes_list=(
  4
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
        "${JOB_FILE}"
      )
      echo "${cmd_qsub[@]}"
      "${cmd_qsub[@]}"
    done
  done
done
