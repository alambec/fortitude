# Development

## Layout

| Path | Content |
|---|---|
| `crates/fortitude-core` | Identifiers, indexed attributes, QIDO query model and matching, DICOM JSON encoder |
| `crates/fortitude-store` | File layout, ingest, index (SQLite or PostgreSQL) and its writer |
| `crates/fortitude-dicomweb` | STOW-RS, WADO-RS, QIDO-RS, WADO-URI routes |
| `crates/fortitude-dimse` | DIMSE SCP: C-ECHO, C-STORE, C-FIND, C-MOVE, C-GET; SCU of the modalities: C-ECHO, C-FIND, C-MOVE, C-STORE |
| `crates/fortitude` | Server binary: configuration, logging, wiring, web explorer (`ui/`) |
| `bench/` | Load generator, Orthanc baselines, results |

## Building

```sh
cargo run --release -p fortitude -- --config fortitude.toml
# or
docker build -t fortitude .
docker run -p 8042:8042 -v fortitude-data:/var/lib/fortitude fortitude
```

Building needs CMake and a C++ compiler: CharLS, the JPEG-LS codec, is built from source.

Each version tag `vX.Y.Z` builds the image (x86_64) and publishes it as
`ghcr.io/alambec/fortitude`, tagged `X.Y.Z`, `X.Y` and `latest`
(`.github/workflows/release.yml`).

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

## Third-party notices

The Docker image includes the licenses of the third-party software in the binary
(`/usr/share/doc/fortitude/THIRD-PARTY-NOTICES.txt`). After a change of the dependencies, regenerate
the file with [cargo-about](https://github.com/EmbarkStudios/cargo-about):

```sh
cargo about generate --locked --fail -m crates/fortitude/Cargo.toml about.hbs -o THIRD-PARTY-NOTICES.txt
```

A new license in the dependencies must be added to `accepted` in `about.toml`, only if it is
permissive.
