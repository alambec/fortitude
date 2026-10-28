# DICOMweb conformance statement

This document describes how Fortitude implements DICOM PS3.18. It grows with each phase of
the [roadmap](roadmap.md).

## STOW-RS — Store Instances (PS3.18 §10.5)

| Resource | Method |
|---|---|
| `{root}/studies` | `POST` |
| `{root}/studies/{StudyInstanceUID}` | `POST` |

**Request**
- `Content-Type: multipart/related; type="application/dicom"; boundary=…`. The `type`
  parameter may be omitted. Other representations (`application/dicom+json` or
  `application/dicom+xml` with bulk data) are rejected with `415 Unsupported Media Type`.
- Each part is a DICOM Part-10 instance. Parts with another `Content-Type` are reported in
  Other Failures Sequence (0008,119A) with Failure Reason `C000`.
- With `{StudyInstanceUID}` in the path, instances of other studies are rejected (`A900`).

**Stored data**
- Instances are stored byte for byte as received; no attribute is coerced.
- Required attributes: SOP Class UID, SOP Instance UID, Study Instance UID and Series
  Instance UID, all syntactically valid UIDs.
- A series is bound to the study of its first instance. An instance claiming the same series
  under another study is rejected (`A900`).

**Duplicates.** An instance whose SOP Instance UID is already stored is **discarded**: the
stored copy is kept unchanged. It is reported in Referenced SOP Sequence (0008,1199), with its
Retrieve URL, and with the implementation-specific Warning Reason below.

| Warning Reason (0008,1196) | Meaning |
|---|---|
| `B111` (45329) | Duplicate SOP Instance: already stored, the received copy was discarded. |

**Failure reasons**

| Failure Reason (0008,1197) | Cause |
|---|---|
| `C000` | The part could not be parsed as a DICOM Part-10 instance, or is not `application/dicom`. |
| `A900` | Missing or invalid required UID, study mismatch with the request path, or series bound to another study. |
| `A700` | The file could not be written (out of resources). |
| `0110` | Internal processing failure (index). |

**Response status**

| Status | When |
|---|---|
| `200 OK` | Every instance was stored, without warnings. |
| `202 Accepted` | At least one instance is stored or already present, and there are warnings (duplicates) or failures. |
| `409 Conflict` | No instance is stored. |
| `400 Bad Request` | Malformed multipart body, before any instance could be read. |
| `415 Unsupported Media Type` | Request is not `multipart/related; type="application/dicom"`. |

The response body is `application/dicom+json`. Retrieve URL (0008,1190) is given at the top
level when every stored instance belongs to the same study, and for each referenced
instance.

## DICOM JSON

- All strings are UTF-8. Specific Character Set (0008,0005), when present, is returned as
  `ISO_IR 192`.
- Binary values up to `storage.inline_binary_max` bytes (default 1024) are returned as
  `InlineBinary`; larger ones, and pixel data, as `BulkDataURI`.
- URLs use `http.public_url` when configured, else the request's `X-Forwarded-Proto`,
  `X-Forwarded-Host` or `Host` headers.
