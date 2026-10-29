#!/usr/bin/env bash
# Start a release build of Fortitude on an empty store for benchmarks, pinned to SERVER_CPUS.
# DIMSE listens on DIMSE_PORT (default 4245), with the benchmark tool's sink (SINK on port
# 11120) as C-MOVE destination.
#
#   bench/serve-fortitude.sh [port]     (default 8045)
set -euo pipefail
cd "$(dirname "$0")/.."
port=${1:-8045}
cargo build --quiet --release -p fortitude
rm -rf bench/var/fortitude
exec env FORTITUDE_HTTP__LISTEN="0.0.0.0:$port" FORTITUDE_STORAGE__PATH=bench/var/fortitude \
    FORTITUDE_DIMSE__LISTEN="0.0.0.0:${DIMSE_PORT:-4245}" \
    FORTITUDE_DIMSE__PEERS='[{ae_title="SINK", host="127.0.0.1", port=11120}]' \
    FORTITUDE_LOG__FILTER="${LOG:-warn}" taskset -c "${SERVER_CPUS:-0-5}" ./target/release/fortitude
