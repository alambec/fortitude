# Fortitude roadmap

Goal: a DICOM server that outperforms Orthanc on the workloads that matter to viewers and
archives (ingest, QIDO-RS, WADO-RS metadata and frames), while staying deliberately smaller.

## Decisions

| Topic | Decision |
|---|---|
| Deployment | Single node: SQLite index (WAL) + local filesystem. The index sits behind a narrow trait so that a PostgreSQL backend can be added later. |
| Protocols | DICOMweb first (STOW-RS, WADO-RS, QIDO-RS, WADO-URI). DIMSE (C-STORE, C-FIND, C-MOVE via `dicom-ul`) later. |
| Duplicates | An instance whose SOP Instance UID is already stored is **discarded**. The STOW-RS response still lists it in ReferencedSOPSequence (it is retrievable at its RetrieveURL) with the implementation-specific Warning Reason **`0xB111` "Duplicate SOP Instance, discarded"** (PS3.18 §I.2.1 allows additional warning codes; must appear in the conformance statement), so the response status is 202. |
| Out of scope | Lua, plugin SDK, jobs engine, built-in web UI (use OHIF), Orthanc REST API compatibility. |
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
   every level, frames, bulk data; `transfer-syntax=*` (stored syntax) first.
3. **QIDO-RS** — single value, UID list, wildcard, range (DA/TM/DT), case-insensitive PN;
   `includefield`, `limit`/`offset`, `fuzzymatching`; queries across all studies/series/instances.
4. **WADO-URI + rendering** — `application/dicom`, then JPEG/PNG rendering with windowing
   (shared with WADO-RS `/rendered` and `/thumbnail`); decompression to Explicit VR Little Endian.
   JPEG 2000 / JPEG-LS codecs behind cargo features.
5. **Operations** — admin API (delete, statistics), authentication, Prometheus metrics; then
   PostgreSQL index, DIMSE.

## Benchmarks

`bench/` holds the load generator, the Orthanc baselines (PostgreSQL and SQLite index, tuned
per the Orthanc Book) and the results. Every phase is measured against the same dataset and
scenarios; see the [README](../README.md#benchmarks).
