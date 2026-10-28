"""Print benchmark results (JSONL files written by fortitude-bench) as comparison tables.

    python3 bench/summarize.py bench/results/*.jsonl
"""

import json
import sys
from collections import defaultdict


def main(paths: list[str]) -> None:
    stow, read = [], defaultdict(dict)
    labels = []
    for path in paths:
        with open(path) as f:
            for line in f:
                r = json.loads(line)
                if r["label"] not in labels:
                    labels.append(r["label"])
                if r["kind"] == "stow":
                    stow.append(r)
                else:
                    read[(r["scenario"], r["concurrency"])][r["label"]] = r

    print("## STOW-RS\n")
    print("| label | instances | failed | inst/s | MiB/s | p50 ms | p99 ms |")
    print("|---|---:|---:|---:|---:|---:|---:|")
    for r in stow:
        print(
            f"| {r['label']} | {r['instances']} | {r['failed_instances']} | {r['instances_per_s']:.0f} "
            f"| {r['mib_per_s']:.0f} | {r['p50_ms']:.1f} | {r['p99_ms']:.1f} |"
        )

    print("\n## Reads (req/s · p50 ms · p99 ms · errors)\n")
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
