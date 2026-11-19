# Configuration

Settings come from three layers, each overriding the previous one:

1. the defaults (see [All settings](#all-settings));
2. a TOML file, given with `--config <path>` or the `FORTITUDE_CONFIG` variable;
3. `FORTITUDE_*` environment variables, with `__` between the section and the key:
   `[index] backend = "postgres"` is `FORTITUDE_INDEX__BACKEND=postgres`. Arrays and tables take
   TOML values: `FORTITUDE_AUTH__USERS='[{name="admin",password="change-me"}]'`.

Credentials are plain text: keep the file readable only by the server, or inject them through
the environment.

The Docker image sets `FORTITUDE_STORAGE__PATH=/var/lib/fortitude` (its volume) and
`FORTITUDE_LOG__FORMAT=json`: as environment variables, they override the file.

## Docker Compose with a configuration file and PostgreSQL

```yaml
# docker-compose.yaml
services:
  fortitude:
    image: ghcr.io/alambec/fortitude:0.1
    ports:
      - "8042:8042"
      - "4242:4242"
    environment:
      FORTITUDE_CONFIG: /etc/fortitude/fortitude.toml
    volumes:
      - ./fortitude.toml:/etc/fortitude/fortitude.toml:ro
      - fortitude-data:/var/lib/fortitude
    depends_on:
      db:
        condition: service_healthy
    restart: unless-stopped
  db:
    image: postgres:17
    environment:
      POSTGRES_USER: fortitude
      POSTGRES_PASSWORD: secret
      POSTGRES_DB: fortitude
    volumes:
      - db-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U fortitude"]
      interval: 2s
    restart: unless-stopped
volumes:
  fortitude-data:
  db-data:
```

```toml
# fortitude.toml
[index]
backend = "postgres"
postgres_url = "postgres://fortitude:secret@db:5432/fortitude"

[dimse]
listen = "0.0.0.0:4242"
[[dimse.peers]]
ae_title = "VIEWER"
host = "10.0.0.5"
port = 104

[[auth.users]]
name = "admin"
password = "change-me"
```

The container runs as the user 10001: the file must be readable by it.

## All settings

The values are the defaults, except the commented lines and the examples of `dimse.peers` and
`auth`.

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

## PostgreSQL

With `backend = "postgres"`, the index lives in the database of `postgres_url`, which must
exist; Fortitude creates its tables on first start (in the connection's `search_path`
schema, `public` by default). Connections are not encrypted yet: keep the database on a
trusted network.

The index schema has no upgrades yet: an index (SQLite file or PostgreSQL schema) created with
another schema version is refused at startup, and must be re-created. The one exception is the
table of registered modalities, added to an index of schema 4 on startup.

## S3

With `storage.backend = "s3"`, DICOM files are stored as objects of the bucket, under
`prefix` (`<prefix>/ab/cd/<uuid>.dcm`); the bucket must exist, and the server checks that it
can list it on startup. The service must support ranged `GetObject` (frames, rendering and
headers are read by range), as AWS S3 and the usual compatible stores do. Instances are
uploaded with a single `PutObject` (up to `max_instance_size_mib`, below the 5 GiB limit).
