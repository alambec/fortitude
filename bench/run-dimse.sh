#!/usr/bin/env bash
# Run the DIMSE benchmark suite against one server, which must start empty and know the
# benchmark tool's sink (AE title SINK, port 11120 on this host) as a C-MOVE destination.
#
#   bench/run-dimse.sh <label> <host:port> [called-ae] [dataset]
#
# C-STORE of the first STORE_SINGLE instances over one association, then of the rest over
# STORE_CONCURRENCY associations; C-FIND scenarios; C-MOVE of every study. Results are
# appended to bench/results/<date>-<label>.jsonl.
#
# Environment: CLIENT_CPUS (default 6-11), DURATION (default 20s), WARMUP (default 3s),
# CONCURRENCY (C-FIND, default "1 16"), MOVE_CONCURRENCY (default "1 4"),
# STORE_SINGLE (default 300), STORE_CONCURRENCY (default 8), SCENARIOS (default: all).
set -euo pipefail

label=${1:?label}
addr=${2:?host:port}
ae=${3:-FORTITUDE}
dataset=${4:-bench/datasets/mixed}
cd "$(dirname "$0")/.."

out=bench/results/$(date +%F)-${label}.jsonl
bench=(taskset -c "${CLIENT_CPUS:-6-11}" ./target/release/fortitude-bench)
common=(--addr "$addr" --ae "$ae" --dataset "$dataset" --label "$label" --out "$out")
scenarios=${SCENARIOS:-patient name-wildcard date-modality series instances}
single=${STORE_SINGLE:-300}

cargo build --quiet --release -p fortitude-bench

"${bench[@]}" dimse-store "${common[@]}" --concurrency 1 --limit "$single"
"${bench[@]}" dimse-store "${common[@]}" --concurrency "${STORE_CONCURRENCY:-8}" --skip "$single"

for scenario in $scenarios; do
    for concurrency in ${CONCURRENCY:-1 16}; do
        "${bench[@]}" dimse-find "${common[@]}" --scenario "$scenario" \
            --concurrency "$concurrency" --duration "${DURATION:-20s}" --warmup "${WARMUP:-3s}"
    done
done

for concurrency in ${MOVE_CONCURRENCY:-1 4}; do
    "${bench[@]}" dimse-move "${common[@]}" --concurrency "$concurrency"
done
