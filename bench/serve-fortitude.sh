#!/usr/bin/env bash
# Start a release build of Fortitude on an empty store for benchmarks, pinned to SERVER_CPUS.
# DIMSE listens on DIMSE_PORT (default 4245), with the benchmark tool's sink (SINK on port
# 11120) as C-MOVE destination.
#
# With INDEX=postgres, the index goes to database "fortitude", recreated empty, on the
# PostgreSQL server of the Orthanc baselines (bench/orthanc/docker-compose.yml, port 5433).
#
#   bench/serve-fortitude.sh [port]     (default 8045)
set -euo pipefail
cd "$(dirname "$0")/.."
port=${1:-8045}
cargo build --quiet --release -p fortitude
rm -rf bench/var/fortitude
index=()
if [ "${INDEX:-sqlite}" = postgres ]; then
    compose=(docker compose -f bench/orthanc/docker-compose.yml)
    "${compose[@]}" up -d --wait postgres
    for sql in "DROP DATABASE IF EXISTS fortitude WITH (FORCE)" "CREATE DATABASE fortitude"; do
        "${compose[@]}" exec -T postgres psql -q -U postgres -c "$sql"
    done
    index=(FORTITUDE_INDEX__BACKEND=postgres
        FORTITUDE_INDEX__POSTGRES_URL="host=127.0.0.1 port=5433 user=postgres password=postgres dbname=fortitude")
fi
exec env "${index[@]}" FORTITUDE_HTTP__LISTEN="0.0.0.0:$port" FORTITUDE_STORAGE__PATH=bench/var/fortitude \
    FORTITUDE_DIMSE__LISTEN="0.0.0.0:${DIMSE_PORT:-4245}" \
    FORTITUDE_DIMSE__PEERS='[{ae_title="SINK", host="127.0.0.1", port=11120}]' \
    FORTITUDE_LOG__FILTER="${LOG:-warn}" taskset -c "${SERVER_CPUS:-0-5}" ./target/release/fortitude
