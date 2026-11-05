#!/usr/bin/env bash
# Run the full benchmark suite against one DICOMweb server, which must start empty.
#
#   bench/run.sh <label> <dicomweb-url> [dataset]
#
# Results are appended to bench/results/<date>-<label>.jsonl; compare runs with
# `python3 bench/summarize.py bench/results/*.jsonl`.
#
# Environment: CLIENT_CPUS (default 6-11), DURATION (default 20s), WARMUP (default 3s),
# CONCURRENCY (space separated, default "1 16"), SCENARIOS (default: all), WADO_URL (WADO-URI
# endpoint, default /wado at the root of the DICOMweb URL's origin), SKIP_STOW (reads only, on a
# server already loaded).
set -euo pipefail

label=${1:?label}
url=${2:?dicomweb url}
dataset=${3:-bench/datasets/mixed}
cd "$(dirname "$0")/.."

out=bench/results/$(date +%F)-${label}.jsonl
bench=(taskset -c "${CLIENT_CPUS:-6-11}" ./target/release/fortitude-bench)
scenarios=${SCENARIOS:-qido-patient qido-name-wildcard qido-date-modality qido-series qido-instances series-metadata instance frame wado-uri rendered}

cargo build --quiet --release -p fortitude-bench

[ -n "${SKIP_STOW:-}" ] || "${bench[@]}" stow --url "$url" --dataset "$dataset" --concurrency 8 --batch 20 \
    --label "$label" --out "$out"

for scenario in $scenarios; do
    for concurrency in ${CONCURRENCY:-1 16}; do
        "${bench[@]}" read --url "$url" --dataset "$dataset" --scenario "$scenario" \
            --concurrency "$concurrency" --duration "${DURATION:-20s}" --warmup "${WARMUP:-3s}" \
            ${WADO_URL:+--wado-url "$WADO_URL"} --label "$label" --out "$out"
    done
done
