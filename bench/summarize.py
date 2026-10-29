"""Print benchmark results (JSONL files written by fortitude-bench) as comparison tables.

    python3 bench/summarize.py bench/results/*.jsonl
"""

import json
import sys
from collections import defaultdict


def main(paths: list[str]) -> None:
    stow, read = [], defaultdict(dict)
    dimse_store, dimse_find, dimse_move = [], defaultdict(dict), []
    labels = []
    for path in paths:
        with open(path) as f:
            for line in f:
                r = json.loads(line)
                if r["label"] not in labels:
                    labels.append(r["label"])
                if r["kind"] == "stow":
                    stow.append(r)
                elif r["kind"] == "dimse-store":
                    dimse_store.append(r)
                elif r["kind"] == "dimse-find":
                    dimse_find[(r["scenario"], r["concurrency"])][r["label"]] = r
                elif r["kind"] == "dimse-move":
                    dimse_move.append(r)
                else:
                    read[(r["scenario"], r["concurrency"])][r["label"]] = r

    if stow:
        print("## STOW-RS\n")
        print("| label | instances | failed | inst/s | MiB/s | p50 ms | p99 ms |")
        print("|---|---:|---:|---:|---:|---:|---:|")
        for r in stow:
            print(
                f"| {r['label']} | {r['instances']} | {r['failed_instances']} | {r['instances_per_s']:.0f} "
                f"| {r['mib_per_s']:.0f} | {r['p50_ms']:.1f} | {r['p99_ms']:.1f} |"
            )

    print_reads(read, labels, "Reads")
    if dimse_store:
        print("\n## C-STORE\n")
        print("| label | assoc. | instances | failed | inst/s | MiB/s | p50 ms | p99 ms |")
        print("|---|---:|---:|---:|---:|---:|---:|---:|")
        for r in dimse_store:
            print(
                f"| {r['label']} | {r['concurrency']} | {r['instances']} | {r['failed_instances']} "
                f"| {r['instances_per_s']:.0f} | {r['mib_per_s']:.0f} | {r['p50_ms']:.1f} | {r['p99_ms']:.1f} |"
            )
    if dimse_find:
        print_reads(dimse_find, labels, "C-FIND")
    if dimse_move:
        print("\n## C-MOVE (every study to a sink SCP)\n")
        print("| label | assoc. | instances | failed | inst/s | MiB/s | p50 ms/study | p99 ms/study |")
        print("|---|---:|---:|---:|---:|---:|---:|---:|")
        for r in dimse_move:
            print(
                f"| {r['label']} | {r['concurrency']} | {r['instances']} | {r['failed_instances']} "
                f"| {r['instances_per_s']:.0f} | {r['mib_per_s']:.0f} | {r['p50_ms']:.1f} | {r['p99_ms']:.1f} |"
            )


def print_reads(read, labels, title):
    if not read:
        return
    labels = [l for l in labels if any(l in by_label for by_label in read.values())]
    print(f"\n## {title} (req/s · p50 ms · p99 ms · errors)\n")
    print("| scenario | conc. | " + " | ".join(labels) + " |")
    print("|---|---:|" + "---:|" * len(labels))
    for (scenario, concurrency), by_label in sorted(read.items()):
        cells = []
        for label in labels:
            r = by_label.get(label)
            if r is None:
                cells.append("–")
                continue
            errors = f" · **{r['errors']} err**" if r["errors"] else ""
            cells.append(f"{r['requests_per_s']:.0f} · {r['p50_ms']:.1f} · {r['p99_ms']:.1f}{errors}")
        print(f"| {scenario} | {concurrency} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main(sys.argv[1:])
