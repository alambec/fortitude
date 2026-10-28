# Fortitude

High performance DICOM(web) server written in Rust.

Fortitude aims to be faster than Orthanc on the workloads viewers and archives depend on
(ingest, QIDO-RS, WADO-RS metadata and frames) with a deliberately smaller feature set.
See [docs/roadmap.md](docs/roadmap.md) for the design decisions and phases.

## Layout

| Path | Content |
|---|---|
| `crates/fortitude-core` | Identifiers, indexed attributes, QIDO query model and matching, DICOM JSON encoder |
| `crates/fortitude-store` | File layout, ingest, SQLite index and its writer |
| `crates/fortitude-dicomweb` | STOW-RS, WADO-RS, QIDO-RS, WADO-URI routes |
| `crates/fortitude` | Server binary: configuration, logging, wiring |
| `bench/` | Load generator, Orthanc baselines, results |

## Running

```sh
cargo run --release -p fortitude -- --config fortitude.toml
```

Configuration is layered: defaults, then the TOML file, then `FORTITUDE_*` environment
variables with `__` between sections.

```toml
[http]
listen = "0.0.0.0:8042"
dicomweb_root = "/dicom-web"
wado_root = "/wado"        # WADO-URI
# public_url = "https://pacs.example.org"   # default: from Host / X-Forwarded-* headers

[storage]
path = "./data"            # files/ and index.sqlite
fsync = false              # flush each file before acknowledging it
inline_binary_max = 1024   # larger binary attributes are served as BulkDataURI

[index]
readers = 0                # read connections; 0 = one per CPU

[dicomweb]
stow_concurrency = 4       # instances of one STOW-RS request stored in parallel
max_instance_size_mib = 4096
qido_max_results = 10000   # QIDO-RS results per request (0 = no limit); page with offset

[log]
filter = "info"
format = "text"   # or "json"
```

`GET /health` reports liveness. The supported DICOMweb services are described in the
[conformance statement](docs/conformance.md).

## Benchmarks

```sh
cargo build --release -p fortitude-bench

# 1. Build a dataset from seed files (UIDs, patients and dates are rewritten per copy).
#    --tiny replaces pixel data by an 8x8 image to reach index-scale datasets cheaply.
./target/release/fortitude-bench generate --seed <dir> [--seed <dir>...] \
    --out bench/datasets/mixed --copies 10

# 2. Start the Orthanc baselines (PostgreSQL on :8043, SQLite on :8044).
docker compose -f bench/orthanc/docker-compose.yml up -d

# 3. Run the suite against an empty server, then compare.
bench/run.sh orthanc-pg http://localhost:8043/dicom-web
bench/serve-fortitude.sh &        # empty store on :8045, pinned to SERVER_CPUS
bench/run.sh fortitude http://localhost:8045/dicom-web
python3 bench/summarize.py bench/results/*.jsonl

# 4. Check that both servers agree (search results and metadata).
python3 bench/diff_qido.py http://localhost:8043/dicom-web http://localhost:8045/dicom-web \
    bench/datasets/mixed/manifest.jsonl
python3 bench/diff_metadata.py http://localhost:8043/dicom-web http://localhost:8045/dicom-web \
    bench/datasets/mixed/manifest.jsonl
```

Servers are pinned to CPUs 0-5 and the load generator to 6-11 (`SERVER_CPUS`, `CLIENT_CPUS`).

## License

Proprietary — Copyright (c) 2026 Alambec. All rights reserved. See [LICENSE](LICENSE).
