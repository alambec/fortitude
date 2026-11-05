# Fortitude roadmap

Goal: a DICOM server that outperforms Orthanc on the workloads that matter to viewers and
archives (ingest, QIDO-RS, WADO-RS metadata and frames), while staying deliberately smaller.

## Decisions

| Topic | Decision |
|---|---|
| Deployment | Single node: local filesystem, and an index in SQLite (WAL, the default) or PostgreSQL. Both run the same SQL through a thin connection layer, with the same writer and reader pool. |
| Protocols | DICOMweb first (STOW-RS, WADO-RS, QIDO-RS, WADO-URI), then DIMSE as SCP (C-ECHO, C-STORE, C-FIND, C-MOVE), with `dicom-ul` for PDU encoding. |
| Duplicates | An instance whose SOP Instance UID is already stored is **discarded**. The STOW-RS response still lists it in ReferencedSOPSequence (it is retrievable at its RetrieveURL) with the implementation-specific Warning Reason **`0xB111` "Duplicate SOP Instance, discarded"** (PS3.18 §I.2.1 allows additional warning codes; must appear in the conformance statement), so the response status is 202. |
| Out of scope | Lua, plugin SDK, jobs engine, a built-in viewer (use OHIF; the explorer only searches and browses), Orthanc REST API compatibility. |
| License | Proprietary, Alambec. |

## Performance principles

1. **Do the work once, at ingest.** Store each instance's DICOM JSON pre-encoded (bulk data
   replaced by BulkDataURI) and the byte offsets of pixel data and frame fragments.
   Series metadata becomes a concatenation of stored fragments; a frame becomes one `pread`.
2. **Flat, typed index.** One table per level (patient, study, series, instance) with indexed
   columns for the QIDO-RS required matching attributes, and denormalized study/series counts.
3. **Never parse what is not needed.** Ingest reads the header up to Pixel Data; the original
   Part-10 bytes are written untouched.
4. **Group commit.** A single writer thread batches index inserts of concurrent STOW requests
   into one transaction.
5. **Stream everything.** Multipart bodies are parsed and produced incrementally; memory use
   does not grow with study size.

## Phases

0. **Foundation** — workspace, CI, config, benchmark tool, Orthanc baseline. *(done)*
1. **Ingest + STOW-RS** — streamed multipart, header parsing, offsets, pre-encoded JSON,
   group-committing SQLite writer, Specific Character Set to UTF-8, duplicate policy above.
   *(done)*
2. **WADO-RS** — study/series/instance retrieval (multipart `application/dicom`), metadata at
   every level, frames, bulk data; `transfer-syntax=*` (stored syntax) first. *(done)*
3. **QIDO-RS** — single value, UID list, wildcard, range (DA/TM/DT), case-insensitive PN;
   `includefield`, `limit`/`offset`, `fuzzymatching`; queries across all studies/series/instances.
   *(done)*
4. **WADO-URI + rendering** — `application/dicom`, then JPEG/PNG rendering with windowing
   (shared with WADO-RS `/rendered` and `/thumbnail`); decompression to Explicit VR Little Endian.
   JPEG 2000 and JPEG-LS codecs, always built in. *(done)*
5. **Operations** — admin API (delete, statistics), authentication, Prometheus metrics, Docker
   image. *(done)*
6. **DIMSE** — C-ECHO, C-STORE (same ingest as STOW-RS), C-FIND (same search as QIDO-RS,
   Patient and Study Root), C-MOVE to configured peers, sending stored files as they are.
   *(done)*
7. **PostgreSQL index** — `index.backend = "postgres"`: same schema (text in the `C`
   collation, so that indexes serve prefix `LIKE` and ranges compare bytewise like SQLite),
   same group-committing writer and reader pool. The test suite runs against both. *(done)*
8. **S3 file storage** — `storage.backend = "s3"`: files as objects of a bucket (AWS or
   compatible), through `object_store`. Frames and headers are read with ranged GETs
   (close ranges coalesced into one request), files are streamed, scope deletions use
   `DeleteObjects`. The test suite runs against both backends. *(done)*
9. **Web explorer** — `/ui/`: search patients, studies and series, open a study, series or
   instance by UID, browse down to an instance's tags and rendered frames, download patients,
   studies and series as ZIP archives, upload files and folders. A static page
   embedded in the binary, over QIDO-RS, WADO-RS, STOW-RS and WADO-URI only (plus a dictionary lookup
   for tag and SOP class names, and the ZIP archives). *(done)*

## Next

- **12-bit JPEG Extended** decoding (jpeg-decoder supports 8-bit DCT only), and a faster
  JPEG 2000 decoder.
- **DIMSE**: C-GET, storage commitment, TLS.
- **Retention and integrity**: periodic check of files against the index, orphan file
  cleanup after crashes.

## Benchmarks

`bench/` holds the load generator, the Orthanc baselines (PostgreSQL and SQLite index, tuned
per the Orthanc Book) and the results. Every phase is measured against the same dataset and
scenarios; see the [README](../README.md#benchmarks).
