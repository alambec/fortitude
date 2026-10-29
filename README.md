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
| `crates/fortitude-dimse` | DIMSE SCP: C-ECHO, C-STORE, C-FIND, C-MOVE |
| `crates/fortitude` | Server binary: configuration, logging, wiring |
| `bench/` | Load generator, Orthanc baselines, results |

## Running

```sh
cargo run --release -p fortitude -- --config fortitude.toml
# or
docker build -t fortitude .
docker run -p 8042:8042 -v fortitude-data:/var/lib/fortitude fortitude
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

[dimse]                    # C-ECHO, C-STORE, C-FIND, C-MOVE (SCP)
# listen = "0.0.0.0:4242"  # unset: DIMSE disabled
ae_title = "FORTITUDE"
check_called_ae_title = false
allow_unknown_peers = true # false: only the peers below may associate
max_associations = 64
max_pdu_length = 262144
timeout_secs = 30
max_find_results = 10000   # 0 = no limit
extra_sop_classes = []     # e.g. private storage SOP classes
[[dimse.peers]]            # C-MOVE destinations
ae_title = "VIEWER"
host = "10.0.0.5"
port = 104

[metrics]
enabled = true             # Prometheus metrics on /metrics

# Without any user or token, authentication is disabled.
[[auth.users]]             # HTTP Basic
name = "admin"
password = "change-me"
[[auth.users]]
name = "viewer"
password = "change-me-too"
read_only = true           # GET, HEAD and OPTIONS only
[[auth.tokens]]            # HTTP Bearer
token = "a-long-random-token"

[log]
filter = "info"
format = "text"   # or "json"
```

Environment variables take precedence over the file, e.g.
`FORTITUDE_AUTH__TOKENS='[{token="…"}]'`. Credentials are plain text: keep the file
readable only by the server, or inject them through the environment.

| Endpoint | Purpose |
|---|---|
| `{dicomweb_root}/…` | DICOMweb: STOW-RS, WADO-RS, QIDO-RS, rendered resources, thumbnails; `DELETE` on studies, series and instances |
| `{wado_root}` | WADO-URI |
| `GET /admin/statistics` | Number of studies, series and instances, and stored size |
| `GET /health` | Liveness (never authenticated) |
| `GET /metrics` | Prometheus metrics (never authenticated) |

The DICOMweb and DIMSE services are described in the [conformance statement](docs/conformance.md).
DIMSE has no authentication beyond AE titles: restrict its port at the network level.

### Metrics

| Metric | Labels |
|---|---|
| `fortitude_http_requests_total` | `method`, `route`, `status` |
| `fortitude_http_request_duration_seconds` (histogram, until response headers) | `method`, `route` |
| `fortitude_stow_instances_total` | `outcome`: `stored`, `duplicate`, `failed` |
| `fortitude_index_batch_size` (histogram: instances per index transaction) | |
| `fortitude_index_commit_seconds` (histogram) | |
| `fortitude_dimse_associations_total` | |
| `fortitude_dimse_requests_total` | `command`, `status`: `success`, `warning`, `failure`, `cancel` |
| `fortitude_dimse_request_duration_seconds` (histogram) | `command` |
| `fortitude_dimse_instances_total` (C-STORE) | `outcome`: `stored`, `duplicate`, `failed` |
| `fortitude_dimse_moved_instances_total` (C-MOVE) | `outcome`: `sent`, `failed` |

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

DIMSE: `bench/run-dimse.sh <label> <host:port> [ae]` (C-STORE, C-FIND, C-MOVE to a sink
run by the tool, which servers must know as `SINK` on port 11120). The Orthanc baselines
listen for DIMSE on 4243 and 4244, Fortitude (`serve-fortitude.sh`) on 4245. Interoperability
checks with pynetdicom and Orthanc are in `tests/interop/`.

Servers are pinned to CPUs 0-5 and the load generator to 6-11 (`SERVER_CPUS`, `CLIENT_CPUS`).

## License

Proprietary — Copyright (c) 2026 Alambec. All rights reserved. See [LICENSE](LICENSE).
