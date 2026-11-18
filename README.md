# Fortitude

High performance DICOM(web) server written in Rust.

Fortitude aims to be faster than Orthanc on the workloads viewers and archives depend on
(ingest, QIDO-RS, WADO-RS metadata and frames) with a deliberately smaller feature set.
See [docs/roadmap.md](docs/roadmap.md) for the design decisions and phases.

Fortitude is open source, under the [GNU Affero General Public License v3.0](LICENSE) (AGPL-3.0).
Alambec also offers commercial licenses, to use it without the obligations of the AGPL, and paid
support: see [License](#license).

> **Not a medical device.** Fortitude is not certified for diagnostic or clinical use. It comes
> with no warranty: you are responsible for validating it, and for the security of the health
> data it stores.

## Layout

| Path | Content |
|---|---|
| `crates/fortitude-core` | Identifiers, indexed attributes, QIDO query model and matching, DICOM JSON encoder |
| `crates/fortitude-store` | File layout, ingest, index (SQLite or PostgreSQL) and its writer |
| `crates/fortitude-dicomweb` | STOW-RS, WADO-RS, QIDO-RS, WADO-URI routes |
| `crates/fortitude-dimse` | DIMSE SCP: C-ECHO, C-STORE, C-FIND, C-MOVE, C-GET; SCU of the modalities: C-ECHO, C-FIND, C-MOVE, C-STORE |
| `crates/fortitude` | Server binary: configuration, logging, wiring, web explorer (`ui/`) |
| `bench/` | Load generator, Orthanc baselines, results |

## Running

```sh
cargo run --release -p fortitude -- --config fortitude.toml
# or
docker build -t fortitude .
docker run -p 8042:8042 -v fortitude-data:/var/lib/fortitude fortitude
```

Building needs CMake and a C++ compiler: CharLS, the JPEG-LS codec, is built from source.

Each version tag `vX.Y.Z` publishes the image (x86_64) as `ghcr.io/alambec/fortitude`, tagged
`X.Y.Z`, `X.Y` and `latest`:

```yaml
# docker-compose.yaml
services:
  fortitude:
    image: ghcr.io/alambec/fortitude:0.1
    ports: ["8042:8042"]
    environment:
      FORTITUDE_AUTH__USERS: '[{name="admin",password="change-me"}]'
    volumes: ["fortitude-data:/var/lib/fortitude"]
volumes:
  fortitude-data:
```

Configuration is layered: defaults, then the TOML file, then `FORTITUDE_*` environment
variables with `__` between sections.

```toml
[http]
listen = "0.0.0.0:8042"
dicomweb_root = "/dicom-web"
wado_root = "/wado"        # WADO-URI
# public_url = "https://pacs.example.org"   # default: from Host / X-Forwarded-* headers
ui = true                  # web explorer at /ui/, for auth.users only

[storage]
path = "./data"            # files/ (filesystem backend), and index.sqlite with the SQLite index
backend = "filesystem"     # where DICOM files go: "filesystem" or "s3"
fsync = false              # flush each file before acknowledging it (filesystem)
inline_binary_max = 1024   # larger binary attributes are served as BulkDataURI

[storage.s3]               # with backend = "s3"
# bucket = "dicom"         # required
# prefix = "fortitude/"    # key prefix of the files in the bucket
# region = "eu-west-3"     # default: AWS_REGION, else us-east-1
# endpoint = "http://minio:9000"   # S3-compatible service; default: AWS
# access_key_id = "…"      # default: AWS_* variables, web identity, instance metadata
# secret_access_key = "…"
virtual_hosted_style = false   # https://bucket.endpoint instead of https://endpoint/bucket
bulk_delete = true         # DeleteObjects requests; false for services lacking them

[index]
backend = "sqlite"         # or "postgres"
readers = 0                # read connections; 0 = one per CPU
location_cache_mib = 64    # pixel data locations of recently read series (0 = disabled)
location_cache_ttl_secs = 300   # how long a series' locations are kept after loading
# postgres_url = "postgres://fortitude:secret@db:5432/fortitude"   # or "host=db user=… dbname=…"

[dicomweb]
stow_concurrency = 4       # instances of one STOW-RS request stored in parallel
max_instance_size_mib = 4096
qido_max_results = 10000   # QIDO-RS results per request (0 = no limit); page with offset

[dimse]                    # C-ECHO, C-STORE, C-FIND, C-MOVE, C-GET (SCP); C-ECHO, C-FIND, C-MOVE, C-STORE (SCU)
# listen = "0.0.0.0:4242"  # unset: DIMSE SCP disabled (modalities can still be queried)
ae_title = "FORTITUDE"
check_called_ae_title = false
allow_unknown_peers = true # false: only the peers below may associate
max_associations = 64
max_pdu_length = 262144
timeout_secs = 30
retrieve_timeout_secs = 600  # between two responses of a modality to a C-MOVE Fortitude sent
max_find_results = 10000   # 0 = no limit
extra_sop_classes = []     # e.g. private storage SOP classes
[[dimse.peers]]            # modalities: C-MOVE destinations, queried, retrieved from and sent to
ae_title = "VIEWER"
host = "10.0.0.5"
port = 104
description = "Viewing station, room 2"   # optional, shown in the web explorer

[integrity]                # checks of the stored files against the index
interval_hours = 24        # first check one interval after the start; 0 = only on request
orphan_grace_minutes = 60  # unreferenced files younger than this are kept (min. 5)
remove_orphans = true      # false = only count orphan files
verify_checksums = false   # true = read every file and verify its checksum (reads all storage)

[retention]                # deletion of whole studies by age
max_age_days = 0           # delete studies whose series were all stored longer ago; 0 = keep
interval_hours = 24        # first run one interval after the start; 0 = only on request

[metrics]
enabled = true             # Prometheus metrics on /metrics

# Without any user or token, authentication is disabled. Only users open the web explorer:
# without any, it is not served.
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

With `backend = "postgres"`, the index lives in the database of `postgres_url`, which must
exist; Fortitude creates its tables on first start (in the connection's `search_path`
schema, `public` by default). Connections are not encrypted yet: keep the database on a
trusted network.

Until the first release, the index schema changes without upgrades: an index (SQLite file or
PostgreSQL schema) created by another version is refused at startup, and must be re-created.
The one exception is the table of registered modalities, added to an index of the previous
version (schema 4) on startup.

With `storage.backend = "s3"`, DICOM files are stored as objects of the bucket, under
`prefix` (`<prefix>/ab/cd/<uuid>.dcm`); the bucket must exist, and the server checks that it
can list it on startup. The service must support ranged `GetObject` (frames, rendering and
headers are read by range), as AWS S3 and the usual compatible stores do. Instances are
uploaded with a single `PutObject` (up to `max_instance_size_mib`, below the 5 GiB limit).

| Endpoint | Purpose |
|---|---|
| `{dicomweb_root}/…` | DICOMweb: STOW-RS, WADO-RS, QIDO-RS, rendered resources, thumbnails; `DELETE` on studies, series and instances |
| `{wado_root}` | WADO-URI |
| `GET /ui/` (`/` redirects to it) | Web explorer: search, browse, download and upload |
| `GET /admin/statistics` | Number of studies, series and instances, and stored size |
| `POST /admin/integrity` | Start an integrity check (`?remove_orphans=false`: dry run; `?verify_checksums=true`: verify checksums); `409` if one is running |
| `POST /admin/integrity?study=…[&series=…]` | Check one study or series now, checksums included (unless `verify_checksums=false`), and return its report (`404` if not stored) |
| `GET /admin/integrity` | Whether a check is running, and the report of the last one |
| `POST /admin/retention` | Start a retention run (`?dry_run=true[&max_age_days=N]`: list only); `409` if one is running |
| `GET /admin/retention` | Whether a retention run is in progress, and the report of the last one |
| `GET /admin/modalities` | Fortitude's AE title, whether its DIMSE service listens, and the modalities |
| `PUT /admin/modalities/{ae}`, `DELETE …` | Register (or change) a modality: `{"host": "…", "port": 104, "description": "…"}`; remove it |
| `GET /admin/modalities/{ae}/echo` | C-ECHO: round-trip time, or `502` with the reason |
| `GET /admin/modalities/{ae}/find?level=STUDY&PatientID=…` | C-FIND, matches in DICOM JSON |
| `POST /admin/modalities/{ae}/move` | Start a C-MOVE: `{"level": "STUDY", "keys": {"StudyInstanceUID": "…"}, "destination": "…"}` |
| `GET /admin/moves`, `POST /admin/moves/{id}/cancel` | Retrievals and their progress; cancel one |
| `POST /admin/modalities/{ae}/store` | Send stored instances (C-STORE): `{"level": "STUDY", "keys": {"StudyInstanceUID": "…"}}` |
| `GET /admin/sends`, `POST /admin/sends/{id}/cancel` | Sends and their progress; cancel one |
| `GET /health` | Liveness (never authenticated) |
| `GET /metrics` | Prometheus metrics (never authenticated) |

The web explorer searches patients (grouped from matching studies), studies or series by
patient ID, name, accession number, study description, study date and modality, and opens a
study, series or instance by UID. Patients, studies and series download as ZIP archives
(`{study}/{series}/{instance}.dcm` entries, stored as is), instances as DICOM files; they can
also be sent to a modality. Its upload page stores files or whole folders with
STOW-RS, one request per file, and reports each failure. Its modalities page registers,
verifies, queries and retrieves from modalities, and follows the sends (see below). Text fields match prefixes (IDs) or substrings (name,
description) unless they contain `*` or `?`; like QIDO-RS, only patient names match
case-insensitively. It is served only when `auth.users` is configured, and only to those
users (the browser asks for one): Bearer tokens do not open it.

### Integrity checks

The index and the files are not updated in one transaction, so a crash or a failed removal
can leave **orphan** files (stored, but referenced by no instance), and files can go
**missing** or be truncated behind the server's back. The integrity check lists the storage
one key prefix at a time (`00/` to `ff/`, so memory stays bounded whatever the archive size)
and compares it with the index:

- orphan files older than `orphan_grace_minutes` are removed (younger ones may belong to an
  ingest in progress), unless `remove_orphans = false`;
- instances whose file is missing, or not of the size recorded at ingest, are logged and
  reported with their UIDs, never deleted: delete them (`DELETE` on DICOMweb) or restore
  their files;
- with `verify_checksums = true` (or `?verify_checksums=true`), every file is also read and
  its CRC-32 compared with the one recorded at ingest: files corrupted with their size
  intact (bit rot, a faulty disk, a bad copy or restore) are reported in the same way, as
  `checksum_mismatch`.

Verifying checksums reads the whole storage (from S3, every object is downloaded), so it is
off by default: run it on request, or enable it with a long `interval_hours`.

Files whose key was not written by Fortitude (`ab/cd/<uuid>.dcm`) are left alone. Each
suspect file is checked again before it is reported or removed, so checks can run while
the server stores and deletes instances. The report of the last check is returned by
`GET /admin/integrity` (up to 1000 problems are listed; the counts include all of them).
A single study or series is checked on request, at once: `POST
/admin/integrity?study={uid}[&series={uid}]` returns its report (with `study_uid` and
`series_uid`), counting its missing files, size and checksum mismatches (checksums are
verified unless `verify_checksums=false`). Orphan files belong to no study, so only the full
check finds them.

The check relies on an index of `instances(path, size, crc32)`, which it reads without
reading the instance rows.

```json
{"running": false, "last": {"started_at": 1793232000, "duration_seconds": 41.2,
  "remove_orphans": true, "verify_checksums": true, "files": 1203311, "instances": 1203310,
  "orphans": 1, "orphans_size": 524288, "orphans_removed": 1, "recent_files": 0,
  "foreign_files": 0, "missing": 1, "size_mismatches": 0, "checksums_verified": 1203308,
  "checksum_mismatches": 1,
  "problems": [{"kind": "missing", "study_uid": "1.2.3", "series_uid": "1.2.3.4",
    "sop_uid": "1.2.3.4.5", "path": "3f/a2/3fa2….dcm", "expected_size": 524416,
    "actual_size": null},
   {"kind": "checksum_mismatch", "study_uid": "1.2.3", "series_uid": "1.2.3.4",
    "sop_uid": "1.2.3.4.6", "path": "8c/01/8c01….dcm", "expected_size": 524416,
    "actual_size": 524416, "expected_crc32": "5d3e0f7a", "actual_crc32": "c41b9e02"}],
  "errors": []}}
```

### Retention

With `retention.max_age_days` set, studies are deleted, whole, once **all** their series were
stored more than that many days ago: a study that receives a new series (priors sent again,
a late series) is kept in full, and its age starts over. The age is that of storage in
Fortitude, not the Study Date. The decision is checked again in the transaction that
deletes each study, so a series stored during a run saves its study. Each deleted study is
logged (UID, patient ID, study date, accession number); files that cannot be removed are
reported, and later removed as orphans by the integrity check.

Preview before enabling it: `POST /admin/retention?dry_run=true&max_age_days=3650` lists
the studies that would be deleted (an age given in the request only applies to dry runs),
and `GET /admin/retention` returns the report:

```json
{"running": false, "last": {"started_at": 1793232000, "duration_seconds": 0.8,
  "dry_run": true, "max_age_days": 3650.0, "cutoff": 1477872000,
  "studies": 1, "instances": 212,
  "expired": [{"study_uid": "1.2.3", "patient_id": "P1", "study_date": "20160402",
    "accession_number": "A17", "instances": 212, "last_stored_at": 1459600000}],
  "errors": []}}
```

Up to 1000 studies are listed; the counts include all of them. Expired studies are found
through an index of `studies.created_at`, so a run reads only the
studies stored before the cutoff.

### Modalities

Fortitude verifies (C-ECHO), queries (C-FIND), retrieves from (C-MOVE) and sends stored
instances to (C-STORE) other DICOM nodes: PACS, scanners, workstations. They are those of `dimse.peers`, fixed by the configuration
file, and those registered through the API or the web explorer, kept in the index (they
survive restarts). Both are C-MOVE destinations of Fortitude's own DIMSE service, and allowed
to associate with it when `allow_unknown_peers` is off. A modality is identified by its AE
title; one of the configuration file cannot be changed or removed at runtime.

```sh
curl -X PUT -H 'Content-Type: application/json' localhost:8042/admin/modalities/PACS \
    -d '{"host": "10.0.0.7", "port": 104, "description": "Main PACS"}'
curl localhost:8042/admin/modalities/PACS/echo
curl 'localhost:8042/admin/modalities/PACS/find?level=STUDY&PatientName=DOE*&StudyDate=20240101-'
curl -X POST -H 'Content-Type: application/json' localhost:8042/admin/modalities/PACS/move \
    -d '{"level": "STUDY", "keys": {"StudyInstanceUID": "1.2.3"}}'
curl localhost:8042/admin/moves
curl -X POST -H 'Content-Type: application/json' localhost:8042/admin/modalities/PACS/store \
    -d '{"level": "SERIES", "keys": {"StudyInstanceUID": "1.2.3", "SeriesInstanceUID": "1.2.3.4"}}'
curl localhost:8042/admin/sends
```

- **Queries** take a level (`PATIENT`, `STUDY`, `SERIES` or `IMAGE`) and matching keys, by
  keyword or tag, with C-FIND values (`*` and `?` wildcards, date ranges, `\`-separated UID
  lists). The usual return keys of the level are always requested; `includefield` adds
  others. At most `limit` matches are returned (1000 by default, `max_find_results` at most);
  `truncated` tells when the modality had more. Lower levels need the unique keys of the
  levels above (e.g. `StudyInstanceUID` for series), as most modalities only support
  hierarchical queries.
- **Retrievals** need the unique key of their level (and, for most modalities, those of the
  levels above). The instances go to Fortitude unless another `destination` is given: the
  modality must know Fortitude's AE title, address and DIMSE port, and `dimse.listen` must be
  set. They run in the background, one association each; `GET /admin/moves` lists the last
  100 with their state (`running`, `succeeded`, `partial`, `cancelled`, `failed`), counts of
  sub-operations, final status and error, until the server restarts.

```json
{"moves": [{"id": 1, "modality": "PACS", "destination": "FORTITUDE", "level": "STUDY",
  "keys": {"StudyInstanceUID": "1.2.3"}, "state": "succeeded", "status": "0000",
  "remaining": null, "completed": 212, "failed": 0, "warning": 0,
  "started_at": 1793232000, "finished_at": 1793232041}]}
```

- **Sends** select stored instances by `PatientID` (with `IssuerOfPatientID` to narrow it),
  `StudyInstanceUID`, `SeriesInstanceUID` or `SOPInstanceUID`, as a C-MOVE received by
  Fortitude does: the key of the level is required, the others narrow the selection, and
  UIDs can be `\`-separated lists (`404` when nothing stored matches). The instances go over
  one association, as stored when the modality accepts their transfer syntax, otherwise
  transcoded to Explicit or Implicit VR Little Endian. `GET /admin/sends` lists the last 100
  sends like the retrievals, with `failures`: the first 100 instances the modality refused,
  with the reason. A send whose modality cannot be reached, or accepts none of the SOP
  classes, fails, as does one whose instances were all refused; one with some refused
  instances is `partial`; cancelling it stops after the instance being sent.

```json
{"sends": [{"id": 1, "modality": "PACS", "level": "SERIES",
  "keys": {"StudyInstanceUID": "1.2.3", "SeriesInstanceUID": "1.2.3.4"}, "state": "partial",
  "remaining": null, "completed": 132, "failed": 1, "warning": 0,
  "error": "out of resources (A700)",
  "failures": [{"sop_instance_uid": "1.2.3.4.5", "reason": "out of resources (A700)"}],
  "started_at": 1793232000, "finished_at": 1793232012}]}
```

Echoes and queries are `GET` requests, open to read-only credentials; registrations,
retrievals and sends are not.

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
| `fortitude_index_read_wait_seconds` (histogram: wait for a read connection) | |
| `fortitude_index_read_seconds` (histogram: index query, database round trip included) | |
| `fortitude_location_cache_lookups_total` (pixel data locations, by frame or bulk data request) | `outcome`: `hit`, `load` (series loaded or joined), `absent` (instance not in the cached series) |
| `fortitude_files_read_seconds` (histogram: frames and other byte ranges of stored files) | |
| `fortitude_dimse_associations_total` | |
| `fortitude_dimse_requests_total` | `command`, `status`: `success`, `warning`, `failure`, `cancel` |
| `fortitude_dimse_request_duration_seconds` (histogram) | `command` |
| `fortitude_dimse_instances_total` (C-STORE) | `outcome`: `stored`, `duplicate`, `failed` |
| `fortitude_dimse_moved_instances_total` (C-MOVE) | `outcome`: `sent`, `failed` |
| `fortitude_dimse_get_instances_total` (C-GET) | `outcome`: `sent`, `failed` |
| `fortitude_dimse_sent_instances_total` (C-STORE to modalities) | `outcome`: `sent`, `failed` |
| `fortitude_dimse_scu_requests_total` (requests to modalities; one per send) | `command`: `C-ECHO`, `C-FIND`, `C-MOVE`, `C-STORE`; `outcome`: `success`, `failure` |
| `fortitude_integrity_orphans`, `_missing`, `_size_mismatches`, `_errors` (gauges: last check) | |
| `fortitude_integrity_checksum_mismatches` (gauge: last check that verified checksums) | |
| `fortitude_integrity_orphans_removed_total` | |
| `fortitude_integrity_last_run_timestamp_seconds`, `_last_run_duration_seconds` (gauges) | |
| `fortitude_retention_deleted_studies_total`, `fortitude_retention_deleted_instances_total` | |
| `fortitude_retention_last_run_timestamp_seconds` (gauge) | |

## Tests

`cargo test --workspace` runs everything against SQLite. With `TEST_POSTGRES_URL` set to
a PostgreSQL connection string, every test store indexes into its own new schema of that
database instead (schemas are left behind; use a disposable database):

```sh
docker run -d --rm --name fortitude-test-pg -e POSTGRES_PASSWORD=test -p 127.0.0.1:5499:5432 postgres:17
TEST_POSTGRES_URL='host=127.0.0.1 port=5499 user=postgres password=test' cargo test --workspace
```

Likewise, with `TEST_S3_URL` set to `http://access_key:secret_key@host:port/bucket`, test
stores keep their files under a new prefix of that bucket (`TEST_S3_BULK_DELETE=false` for
services without `DeleteObjects`):

```sh
docker run -d --rm --name fortitude-test-s3 -p 127.0.0.1:7070:7070 -e ROOT_ACCESS_KEY=test \
    -e ROOT_SECRET_KEY=test-secret --tmpfs /data versity/versitygw:v1.10.0 posix /data
curl --aws-sigv4 aws:amz:us-east-1:s3 --user test:test-secret -X PUT http://127.0.0.1:7070/fortitude
TEST_S3_URL=http://test:test-secret@127.0.0.1:7070/fortitude cargo test --workspace
```

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
# PostgreSQL index, on the baselines' PostgreSQL server:
INDEX=postgres bench/serve-fortitude.sh &
bench/run.sh fortitude-postgres http://localhost:8045/dicom-web
# dcm4chee-arc-light (LDAP + PostgreSQL + WildFly) on :8046:
docker compose -f bench/dcm4chee/docker-compose.yml up -d
WADO_URL=http://localhost:8046/dcm4chee-arc/aets/DCM4CHEE/wado \
    bench/run.sh dcm4chee http://localhost:8046/dcm4chee-arc/aets/DCM4CHEE/rs
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

## Contributing and security

Bug reports and feature requests are welcome as GitHub issues, but pull requests are not accepted:
see [CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities privately, as described in
[SECURITY.md](SECURITY.md).

## License

Copyright 2026 Alambec. Fortitude is free software: you may use, modify and redistribute it under
the terms of the [GNU Affero General Public License, version 3](LICENSE) (AGPL-3.0-only). If you
distribute a modified version, or let users interact with one over a network, you must offer them
its complete source code under the same license.

To use Fortitude without these obligations, for example in a proprietary product or service,
Alambec offers commercial licenses. For a commercial license or paid support, write to
gabriel.couture@alambec.com.

The Docker image includes the licenses of the third-party software in the binary
(`/usr/share/doc/fortitude/THIRD-PARTY-NOTICES.txt`). After a change of the dependencies, regenerate
the file with [cargo-about](https://github.com/EmbarkStudios/cargo-about):

```sh
cargo about generate --locked --fail -m crates/fortitude/Cargo.toml about.hbs -o THIRD-PARTY-NOTICES.txt
```

A new license in the dependencies must be added to `accepted` in `about.toml`, only if it is
permissive.
