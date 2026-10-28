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

## WADO-RS — Retrieve (PS3.18 §10.4)

| Resource | Representations |
|---|---|
| `{root}/studies/{study}` | `multipart/related; type="application/dicom"` |
| `{root}/studies/{study}/series/{series}` | same |
| `{root}/studies/{study}/series/{series}/instances/{instance}` | same, or `application/dicom` (single part) |
| `…/metadata` at the three levels above | `application/dicom+json` (also served for `application/json`) |
| `…/instances/{instance}/frames/{list}` | `multipart/related` of `application/octet-stream` or the compressed media type; single part for one frame |
| `…/instances/{instance}/bulk/{path}` | `multipart/related; type="application/octet-stream"`, or single part |

**Instances** are returned byte for byte as stored, in study order (series number, instance
number). Each part's `Content-Type` carries the stored `transfer-syntax`. When the request
names no transfer syntax, the stored one is used (other archives do the same; PS3.18's
default would be Explicit VR Little Endian). A specific transfer syntax is accepted only when
it is the stored one; otherwise the response is `406 Not Acceptable` (transcoding comes
with phase 4).

**Frames** are read directly from the stored file through byte offsets computed at ingest,
without decoding:
- With `transfer-syntax=*`, frames are returned in the stored transfer syntax. The part
  media type is the requested one (`application/octet-stream`, or e.g. `image/jpeg`), with a
  `transfer-syntax` parameter.
- `application/octet-stream` without `transfer-syntax` means uncompressed little endian. It
  is served for natively encoded little endian instances, and `406` for compressed ones
  (until phase 4).
- Compressed media types (`image/jpeg`, `image/jls`, `image/jp2`, `image/jpx`,
  `image/jphc`, `image/dicom-rle`, video types) are served when they match the stored
  transfer syntax (PS3.18 Table 8.7.3-5).
- Frames of encapsulated pixel data are found through the Basic or Extended Offset Table,
  one fragment per frame, or JPEG / JPEG 2000 start markers. Instances whose fragments
  cannot be attributed to frames answer `404` for frame requests.
- Float and Double Float Pixel Data frames are supported. Native YBR_FULL_422 frames account
  for chroma subsampling.

**Bulk data** paths are those of the `BulkDataURI`s in metadata: `GGGGEEEE`, or
`GGGGEEEE/item/GGGGEEEE…` within sequences. Pixel data is returned as one part (native)
or one part per frame (encapsulated).

**Errors**: `404` for unknown resources (including a series requested under the wrong
study), `400` for an invalid frame list, `406` when no accepted representation can be
produced.

## DICOM JSON

- All strings are UTF-8. Specific Character Set (0008,0005), when present, is returned as
  `ISO_IR 192`.
- Binary values up to `storage.inline_binary_max` bytes (default 1024) are returned as
  `InlineBinary`; larger ones, and pixel data, as `BulkDataURI`.
- URLs use `http.public_url` when configured, else the request's `X-Forwarded-Proto`,
  `X-Forwarded-Host` or `Host` headers.
- DS and IS values are JSON numbers (PS3.18 F.2.3), keeping their original decimal text
  when it is a valid JSON number.
- Padding is trimmed from string values. Attributes whose value is only padding have no
  `Value` member.
- Private attributes of implicit VR instances have no known VR and are returned as `UN`
  (binary).
- Attributes after Pixel Data (e.g. Data Set Trailing Padding) are not part of metadata.
