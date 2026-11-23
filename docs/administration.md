# Administration

## Endpoints

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

The `/admin/` endpoints need credentials when authentication is enabled; their `GET` requests
are open to read-only users.

## Web explorer

The web explorer searches patients (grouped from matching studies), studies or series by
patient ID, name, accession number, study description, study date and modality, and opens a
study, series or instance by UID. A series shows its instances as a list or in a viewer that
scrolls through its slices (one per frame) with the mouse wheel, the slider or the arrow keys,
with the tags of the instance shown. Patients, studies and series download as ZIP archives
(`{study}/{series}/{instance}.dcm` entries, stored as is), instances as DICOM files; they can
also be sent to a modality. Its upload page stores files or whole folders with
STOW-RS, one request per file, and reports each failure. Its modalities page registers,
verifies, queries and retrieves from modalities, and follows the sends (see below). Text fields match prefixes (IDs) or substrings (name,
description) unless they contain `*` or `?`; like QIDO-RS, only patient names match
case-insensitively. It is served only when `auth.users` is configured, and only to those
users (the browser asks for one): Bearer tokens do not open it.

## Integrity checks

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

## Retention

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

## Modalities

Fortitude verifies (C-ECHO), queries (C-FIND), retrieves from (C-MOVE) and sends stored
instances to (C-STORE) other DICOM nodes: PACS, scanners, workstations. They are those of `dimse.peers`, fixed by the configuration
file, and those registered through the API or the web explorer, kept in the index (they
survive restarts). Both are C-MOVE destinations of Fortitude's own DIMSE service, and allowed
to associate with it when `allow_unknown_peers` is off. A modality is identified by its AE
title; one of the configuration file cannot be changed or removed at runtime.

```sh
curl -X PUT -H 'Content-Type: application/json' localhost:8080/admin/modalities/PACS \
    -d '{"host": "10.0.0.7", "port": 104, "description": "Main PACS"}'
curl localhost:8080/admin/modalities/PACS/echo
curl 'localhost:8080/admin/modalities/PACS/find?level=STUDY&PatientName=DOE*&StudyDate=20240101-'
curl -X POST -H 'Content-Type: application/json' localhost:8080/admin/modalities/PACS/move \
    -d '{"level": "STUDY", "keys": {"StudyInstanceUID": "1.2.3"}}'
curl localhost:8080/admin/moves
curl -X POST -H 'Content-Type: application/json' localhost:8080/admin/modalities/PACS/store \
    -d '{"level": "SERIES", "keys": {"StudyInstanceUID": "1.2.3", "SeriesInstanceUID": "1.2.3.4"}}'
curl localhost:8080/admin/sends
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

## Metrics

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
