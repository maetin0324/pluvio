#!/bin/bash
# Integration test driver for pluvio_collective.
#
# Usage:
#   scripts/run_integration_tests.sh                   # debug build, 2 procs
#   PROCS=4 PORT=14100 scripts/run_integration_tests.sh
#
# Requires UCX >= 1.18 to be discoverable by the dynamic loader. If your
# system libucp.so.0 is older than that (e.g. Ubuntu 24.04 ships 1.16),
# install a newer UCX system-wide (e.g. via /etc/ld.so.conf.d/), or export
# LD_LIBRARY_PATH yourself before invoking this script.

set -euo pipefail

PROCS=${PROCS:-2}
PORT=${PORT:-14100}
PROFILE=${PROFILE:-debug}

# Disable MPI internal async progress threads so Pluvio's reactor controls
# UCX progress.
export MPICH_ASYNC_PROGRESS=0
export MPIR_CVAR_ASYNC_PROGRESS=0
export OMPI_MCA_pml_ucx_progress_iterations=0

cargo_build_args=()
if [[ "$PROFILE" == "release" ]]; then
    cargo_build_args+=("--release")
fi
cargo build "${cargo_build_args[@]}" -p pluvio_collective --tests

# Honour CARGO_TARGET_DIR if set, otherwise default to ./target (cargo's
# default). Avoids hard-coding "target/" while staying portable.
target_dir="${CARGO_TARGET_DIR:-target}"
deps_dir="${target_dir}/${PROFILE}/deps"

run_one() {
    local pattern="$1"
    local bin
    bin=$(ls -t ${deps_dir}/${pattern}-* 2>/dev/null | grep -v '\.d$' | head -1)
    if [[ -z "$bin" ]]; then
        echo "[run_integration_tests] could not find $pattern test binary in $deps_dir" >&2
        exit 2
    fi
    echo "[run_integration_tests] running $pattern via mpiexec -n $PROCS"
    mpiexec -n "$PROCS" --oversubscribe \
        -x PLUVIO_COLL_ROOT_HOST=127.0.0.1 \
        -x PLUVIO_COLL_ROOT_PORT="$PORT" \
        "$bin" --ignored --nocapture --test-threads=1
}

run_one mpi_allreduce_2proc
PORT=$((PORT + 1))
run_one ucx_allreduce_2proc
PORT=$((PORT + 1))
run_one cross_check
PORT=$((PORT + 1))
run_one scatter_2proc
PORT=$((PORT + 1))
run_one ucx_pipelined_allreduce_2proc
PORT=$((PORT + 1))
run_one ucx_recursive_doubling_2proc
PORT=$((PORT + 1))
run_one ucx_allgather_2proc
PORT=$((PORT + 1))
run_one ucx_broadcast_2proc
echo "[run_integration_tests] all tests passed"
