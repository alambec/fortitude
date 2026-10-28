#!/usr/bin/env bash
# Start a release build of Fortitude on an empty store for benchmarks, pinned to SERVER_CPUS.
#
#   bench/serve-fortitude.sh [port]     (default 8045)
set -euo pipefail
cd "$(dirname "$0")/.."
port=${1:-8045}
cargo build --quiet --release -p fortitude
rm -rf bench/var/fortitude
exec env FORTITUDE_HTTP__LISTEN="0.0.0.0:$port" FORTITUDE_STORAGE__PATH=bench/var/fortitude \
    FORTITUDE_LOG__FILTER="${LOG:-warn}" taskset -c "${SERVER_CPUS:-0-5}" ./target/release/fortitude
