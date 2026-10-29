#!/usr/bin/env bash
# Start a release build of Fortitude on an empty store for benchmarks, pinned to SERVER_CPUS.
# DIMSE listens on DIMSE_PORT (default 4245), with the benchmark tool's sink (SINK on port
# 11120) as C-MOVE destination.
#
# With INDEX=postgres, the index goes to database "fortitude", recreated empty, on the
# PostgreSQL server of the Orthanc baselines (bench/orthanc/docker-compose.yml, port 5433).
#
# With FILES=s3, files go to bucket "fortitude", recreated empty, on a Versity S3 gateway
# (container fortitude-bench-s3, port 7071, objects in its tmpfs).
#
# SERVER_BIN runs another build (e.g. one of main, for A/B comparisons).
#
#   bench/serve-fortitude.sh [port]     (default 8045)
set -euo pipefail
cd "$(dirname "$0")/.."
port=${1:-8045}
bin=${SERVER_BIN:-./target/release/fortitude}
[ -n "${SERVER_BIN:-}" ] || cargo build --quiet --release -p fortitude
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
files=()
if [ "${FILES:-filesystem}" = s3 ]; then
    docker rm -f fortitude-bench-s3 >/dev/null 2>&1 || true
    docker run -d --name fortitude-bench-s3 -p 127.0.0.1:7071:7070 --tmpfs /data:size=8g \
        -e ROOT_ACCESS_KEY=bench -e ROOT_SECRET_KEY=bench-secret \
        versity/versitygw:v1.10.0 posix /data >/dev/null
    until curl -sf --aws-sigv4 aws:amz:us-east-1:s3 --user bench:bench-secret \
        -X PUT http://127.0.0.1:7071/fortitude -o /dev/null; do sleep 0.5; done
    files=(FORTITUDE_STORAGE__BACKEND=s3 FORTITUDE_STORAGE__S3__BUCKET=fortitude
        FORTITUDE_STORAGE__S3__ENDPOINT=http://127.0.0.1:7071
        FORTITUDE_STORAGE__S3__ACCESS_KEY_ID=bench FORTITUDE_STORAGE__S3__SECRET_ACCESS_KEY=bench-secret)
fi
exec env "${index[@]}" "${files[@]}" FORTITUDE_HTTP__LISTEN="0.0.0.0:$port" FORTITUDE_STORAGE__PATH=bench/var/fortitude \
    FORTITUDE_DIMSE__LISTEN="0.0.0.0:${DIMSE_PORT:-4245}" \
    FORTITUDE_DIMSE__PEERS='[{ae_title="SINK", host="127.0.0.1", port=11120}]' \
    FORTITUDE_LOG__FILTER="${LOG:-warn}" taskset -c "${SERVER_CPUS:-0-5}" "$bin"
