# Fortitude

A fast DICOM server written in Rust, for viewers, modalities and archives.

Fortitude aims to be faster than Orthanc on the workloads viewers and archives depend on
(ingest, searches, metadata and frames), with a deliberately smaller feature set.

- **DICOMweb**: STOW-RS, WADO-RS, QIDO-RS, WADO-URI, rendered images and thumbnails.
- **DIMSE**: C-ECHO, C-STORE, C-FIND, C-MOVE and C-GET; queries, retrievals and sends to
  other DICOM nodes.
- **Storage**: SQLite or PostgreSQL index; files on disk or in an S3-compatible bucket.
- **Web explorer**: search, browse, download, upload (files, folders, ZIP archives), send and delete studies.
- **Operations**: integrity checks, retention by age, Prometheus metrics.

Fortitude is open source, under the [GNU AGPL-3.0](LICENSE). Alambec also offers commercial
licenses and paid support: see [License](#license).

> **Not a medical device.** Fortitude is not certified for diagnostic or clinical use. It comes
> with no warranty: you are responsible for validating it, and for the security of the health
> data it stores.

## Quick start

```yaml
# docker-compose.yaml
services:
  fortitude:
    image: ghcr.io/alambec/fortitude:0.2
    ports:
      - "8080:8080"   # HTTP: DICOMweb and web explorer
      - "104:104"     # DIMSE
    environment:
      FORTITUDE_AUTH__USERS: '[{name="admin",password="change-me"}]'
      FORTITUDE_DIMSE__LISTEN: "0.0.0.0:104"
    volumes:
      - fortitude-data:/var/lib/fortitude
    restart: unless-stopped
volumes:
  fortitude-data:
```

```sh
docker compose up -d
```

- **Web explorer**: <http://localhost:8080/>, as `admin`.
- **DICOMweb**: `http://localhost:8080/dicom-web`, the root to give your viewer (OHIF, for
  example), with the same credentials.
- **DIMSE**: AE title `FORTITUDE`, port 104.

```sh
dcmsend -aec FORTITUDE localhost 104 image.dcm                    # store a file (DCMTK)
curl -u admin:change-me http://localhost:8080/dicom-web/studies   # list the studies
```

The image is built for x86_64. Its tags follow the releases: `0.2.0`, `0.2` and `latest`.

## Configuration

Settings come from the defaults, then a TOML file (`--config`, or the `FORTITUDE_CONFIG`
variable), then `FORTITUDE_*` environment variables, with `__` between the section and the key:
`[index] backend = "postgres"` is `FORTITUDE_INDEX__BACKEND=postgres`.

| Setting | Default | |
|---|---|---|
| `auth.users`, `auth.tokens` | none | HTTP Basic users and Bearer tokens (`read_only` for read access only). Without any, authentication is **disabled**, and the web explorer is not served. |
| `dimse.listen` | unset | Address of the DIMSE service (e.g. `0.0.0.0:104`); unset, it is disabled. |
| `dimse.ae_title` | `FORTITUDE` | Fortitude's AE title. |
| `dimse.peers` | none | Known DICOM nodes (AE title, host, port): C-MOVE destinations, and nodes to query or send to. They can also be registered from the web explorer. |
| `index.backend` | `sqlite` | `postgres`, with `index.postgres_url`. |
| `storage.backend` | `filesystem` | `s3`, with the `storage.s3` section. |
| `http.public_url` | from the request | Base of the URLs in responses, behind a reverse proxy. |
| `retention.max_age_days` | `0` (keep) | Delete studies stored longer ago. |

[docs/configuration.md](docs/configuration.md) lists every setting, with a Docker Compose
example using a configuration file and PostgreSQL.

**Before exposing it beyond a trusted network:** configure users or tokens, put it behind a
reverse proxy that terminates TLS (Fortitude serves plain HTTP), and restrict the DIMSE port at
the network level (DIMSE has no authentication beyond AE titles).

## Documentation

- [Configuration](docs/configuration.md): every setting, PostgreSQL, S3.
- [Administration](docs/administration.md): the HTTP endpoints, the web explorer, integrity
  checks, retention, modalities and metrics.
- [Conformance statement](docs/conformance.md): the DICOMweb and DIMSE services in detail.
- [Development](docs/development.md): building from source, the code layout, tests and
  benchmarks.
- [Roadmap](docs/roadmap.md): the design decisions and phases.

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
contact@alambec.com.

The licenses of the third-party software in Fortitude are in
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt), also in the Docker image
(`/usr/share/doc/fortitude/`).
