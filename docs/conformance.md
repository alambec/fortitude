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
| `…/instances/{instance}/rendered`, `…/frames/{list}/rendered` | `image/jpeg` (default), `image/png`; multipart for several frames |
| `{root}/studies/{study}/thumbnail`, `…/series/{series}/thumbnail`, `…/instances/{instance}/thumbnail`, `…/frames/{frame}/thumbnail` | `image/jpeg` (default), `image/png` |

**Instances** are returned byte for byte as stored, in study order (series number, instance
number). Each part's `Content-Type` carries its `transfer-syntax`. When the request names no
transfer syntax, the stored one is used (other archives do the same; PS3.18's default would
be Explicit VR Little Endian). When a specific transfer syntax is requested, instances stored
in another one are transcoded (see *Codecs* below); `406` when that is not possible.

**Frames** are read directly from the stored file through byte offsets computed at ingest,
without decoding:
- With `transfer-syntax=*`, frames are returned in the stored transfer syntax. The part
  media type is the requested one (`application/octet-stream`, or e.g. `image/jpeg`), with a
  `transfer-syntax` parameter.
- `application/octet-stream` without `transfer-syntax` (or with Explicit VR Little Endian)
  means uncompressed little endian pixels: compressed frames are decoded; `406` when the
  codec is not available.
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

**Rendered resources and thumbnails** decode the frame and encode it as JPEG (quality 90 by
default) or PNG, 8 bits per sample:
- Query parameters: `window=center,width[,function]` (linear window; the function is
  ignored), `viewport=width,height` (the image is scaled down to fit, keeping its aspect
  ratio; never scaled up), `quality=1..100`. `annotation` and `charset` are ignored.
- Without `window`, the instance's VOI LUT or first window is applied, or the values are
  min-max normalized.
- A rendered instance is its first frame.
- Thumbnails fit in 128×128 pixels unless a viewport is given. The thumbnail of a series is
  its middle instance (by instance number); that of a study is the thumbnail of its first
  series.
- Instances without pixel data answer `404`; pixel data that cannot be decoded answers `406`.

**Errors**: `404` for unknown resources (including a series requested under the wrong
study), `400` for an invalid frame list, `406` when no accepted representation can be
produced.

## WADO-URI (PS3.18 §9)

`GET {wado_root}?requestType=WADO&studyUID=…&seriesUID=…&objectUID=…` (`http.wado_root`,
default `/wado`).

| Parameter | Support |
|---|---|
| `contentType` | `application/dicom`, `image/jpeg`, `image/png` (first supported of a comma-separated list; `406` if none). Default: `image/jpeg` for instances with pixel data, `application/dicom` otherwise. |
| `transferSyntax` | With `application/dicom`: the instance is transcoded when stored in another transfer syntax. |
| `frameNumber` | Frame to render (default 1). |
| `rows`, `columns` | Maximum size of the rendered image; one of them bounds both dimensions. |
| `windowCenter`, `windowWidth` | Window applied when rendering (both required). |
| `imageQuality` | JPEG quality, 1 to 100 (default 90). |
| `anonymize=yes` | Not supported: `400`. |
| `charset`, `annotation`, `region`, `presentationUID`, `presentationSeriesUID` | Ignored. |

Missing `requestType=WADO` or UIDs answer `400`; unknown objects `404`.

## Codecs

Pixel data is decoded (for rendering, uncompressed frames, and transcoding) with dicom-rs:

| Transfer syntax | Decoding |
|---|---|
| Implicit and Explicit VR Little Endian, Deflated Explicit VR Little Endian | yes |
| JPEG Baseline (Process 1), JPEG Lossless (Process 14, SV1) | yes |
| JPEG Extended (Process 2 & 4), 8-bit | yes; 12-bit is not supported |
| JPEG 2000 (lossless and lossy) | yes |
| RLE Lossless | yes |
| JPEG-LS | not supported |
| Video (MPEG-2, MPEG-4, HEVC), JPEG XL, HTJ2K | not supported |

Pixel data that cannot be decoded is still stored and served as is (`transfer-syntax=*`).
Float and Double Float Pixel Data cannot be rendered. Transcoding targets are the
uncompressed transfer syntaxes; compressed targets are attempted with the encoders of
dicom-rs (JPEG Baseline), and answer `406` when no encoder is available.

## QIDO-RS — Search (PS3.18 §10.6)

| Resource | Level |
|---|---|
| `{root}/studies` | studies |
| `{root}/series`, `{root}/studies/{study}/series` | series |
| `{root}/instances`, `{root}/studies/{study}/instances`, `{root}/studies/{study}/series/{series}/instances` | instances |

Responses are `application/dicom+json` (`application/json` is accepted too); an empty match
is `200` with `[]`.

**Query parameters**
- `{attributeID}={value}`, where the attribute ID is a keyword (`PatientName`), a tag
  (`00100010`), or a sequence path (`RequestAttributesSequence.ScheduledProcedureStepID`).
  Unknown attributes are rejected with `400`.
- `includefield={attributeID}[,…]`, repeatable, or `includefield=all`: every attribute of the
  representative instance except bulk data.
- `fuzzymatching=true`: person names match when the value is the beginning of any name
  component.
- `limit`, `offset`. Results are capped at `dicomweb.qido_max_results` (default 10,000). When
  the cap truncates a result set, the response has the header
  `Warning: 299 fortitude: "The number of results exceeded the maximum supported by the server. …"`.

**Matching** (PS3.4 §C.2.2.2)

| Value | Matching |
|---|---|
| empty, `*` | universal: matches everything, and the attribute is returned |
| `a,b` or `a\b` on UIDs | UID list |
| `a\b` on other VRs | any of the values |
| contains `*` or `?` | wildcard (not on UIDs, which answer `400`) |
| `a-b`, `a-`, `-b` on DA, TM, DT | inclusive range |
| anything else | single value |

- Matching is case sensitive, except for person names.
- Attributes of a deeper level are matched against the children of each result: for example
  `/studies?Modality=MR` or `/studies?SOPInstanceUID=…`.
- ModalitiesInStudy matches the modalities of the study's series.

**Indexed attributes** are matched in the database:
- **Study:** Study Instance UID, Study Date, Study Time, Accession Number, Study ID, Study
  Description, Referring Physician's Name, Patient's Name, Patient ID, Issuer of Patient ID,
  Patient's Birth Date, Patient's Sex.
- **Series:** Series Instance UID, Modality, Series Number, Series Description, Series Date,
  Series Time, Body Part Examined, Performed Procedure Step Start Date and Time.
- **Instance:** SOP Instance UID, SOP Class UID, Instance Number.

Any other attribute is matched against the stored metadata of the first instance received
for each study or series (or of the instance itself). This is slower: pagination then
happens after filtering.

**Returned attributes**
- **Studies:** Specific Character Set, Study Date, Study Time, Accession Number, Referring
  Physician's Name, Timezone Offset From UTC, Study Description, Patient's Name, Patient ID,
  Issuer of Patient ID, Patient's Birth Date, Patient's Sex, Study Instance UID, Study ID,
  Modalities in Study (sorted), Number of Study Related Series and Instances, Instance
  Availability (`ONLINE`), Retrieve URL.
- **Series:** Specific Character Set, Series Date, Series Time, Modality, Timezone Offset
  From UTC, Series Description, Performed Procedure Step Start Date and Time, Request
  Attributes Sequence, Body Part Examined, Series Instance UID, Series Number, Number of
  Series Related Instances, Instance Availability, Retrieve URL. Study attributes are added
  when the request does not name the study.
- **Instances:** Specific Character Set, SOP Class UID, SOP Instance UID, Timezone Offset
  From UTC, Instance Number, Rows, Columns, Bits Allocated, Number of Frames, Instance
  Availability, Retrieve URL. Series and study attributes are added when the request does
  not name them.
- Also returned: attributes requested with `includefield`, and those used as matching keys.

Study and series attributes are taken from the first instance received for the study or
series.

## Deletion

PS3.18 defines no deletion transaction. Like other archives, Fortitude accepts `DELETE` on
`{root}/studies/{study}`, `{root}/studies/{study}/series/{series}` and
`{root}/studies/{study}/series/{series}/instances/{instance}`:
- `204 No Content` when instances were deleted, `404` when the resource does not exist.
- Their files are removed. Series and studies left empty are removed.
- Counts and Modalities in Study of the remaining ones are updated.
- Deleted instances can be stored again.

## Security

When credentials are configured (`auth.users` for HTTP Basic, `auth.tokens` for Bearer),
every service except `/health` and `/metrics` requires them:
- `401` without valid credentials.
- `403` for read-only credentials on methods other than `GET`, `HEAD` and `OPTIONS`.

There is no per-study access control. TLS is expected to be terminated by a reverse proxy.

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
