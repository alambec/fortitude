# Conformance statement

This document describes how Fortitude implements the DICOMweb services (PS3.18) and the
DIMSE services (PS3.4, PS3.7, PS3.8, [below](#dimse)). It grows with each phase of the
[roadmap](roadmap.md).

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
- Standard top-level attributes encoded with VR UN (as some converters write them) are read
  with their dictionary VR, for the required UIDs, the index and the metadata; the stored
  file keeps them as received.
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
- Float and Double Float Pixel Data frames are supported. Native YBR_FULL_422 and
  YBR_PARTIAL_422 frames account for chroma subsampling.
- Frames of 1-bit samples are returned packed, from the first bit of their first byte. When
  frames do not start on a byte (their number of pixels is not a multiple of 8), they are
  extracted from the bit stream (uncompressed little endian instances only).
- Decoded frames of Explicit VR Big Endian instances are byte-swapped to little endian (by
  sample, or by 16-bit word for 8-bit OW data).
- Frames of Deflated Explicit VR Little Endian instances are only available decoded (the file
  is inflated): `transfer-syntax=*` answers `404`.

**Bulk data** paths are those of the `BulkDataURI`s in metadata: `GGGGEEEE`, or
`GGGGEEEE/item/GGGGEEEE…` within sequences. Pixel data is returned as one part (native)
or one part per frame (encapsulated).

**Rendered resources and thumbnails** decode the frame and encode it as JPEG (quality 90 by
default) or PNG, 8 bits per sample:
- Query parameters: `window=center,width[,function]` (linear window; the function is
  ignored), `viewport=width,height` (the image is scaled down to fit, keeping its aspect
  ratio; never scaled up), `quality=1..100`. `annotation` and `charset` are ignored.
- Without `window`, the instance's VOI LUT or first window is applied, or the range of the
  frame's values (after the modality rescale) is used as window.
- Monochrome samples of 1 to 32 bits, Float and Double Float Pixel Data, and RGB samples of
  up to 32 bits are rendered. Native YBR_FULL, YBR_FULL_422 and YBR_PARTIAL_422 frames are
  converted to RGB, and PALETTE COLOR frames through their palette (segmented palettes are not supported: `406`).
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

Pixel data is decoded (for rendering, uncompressed frames, and transcoding) with dicom-rs,
CharLS for JPEG-LS, and Fortitude's own decoder for 12-bit JPEG Extended:

| Transfer syntax | Decoding |
|---|---|
| Implicit and Explicit VR Little Endian, Explicit VR Big Endian, Deflated Explicit VR Little Endian | yes |
| JPEG Baseline (Process 1), JPEG Lossless (Process 14, SV1) | yes |
| JPEG Extended (Process 2 & 4), 8 and 12 bits | yes (sequential DCT with Huffman coding; samples identical to libjpeg-turbo's) |
| JPEG 2000 (lossless and lossy) | yes |
| RLE Lossless | yes (8, 16 and 32 bits per sample) |
| JPEG-LS (lossless and near-lossless) | yes |
| HTJ2K (lossless and lossy) | yes |
| Video (MPEG-2, MPEG-4, HEVC), JPEG XL | not supported |

Pixel data that cannot be decoded is still stored and served as is (`transfer-syntax=*`).
Transcoding targets are the uncompressed transfer syntaxes; compressed targets are attempted with the encoders of
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

The web explorer (`/ui/`) accepts `auth.users` only, not tokens, and is not served when no
user is configured, even with authentication otherwise disabled.

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

## DIMSE

Fortitude is a DIMSE service class provider when `dimse.listen` is set (disabled by default).
It initiates associations for C-MOVE sub-operations, and as a service class user of its
peers (C-ECHO, C-FIND, C-MOVE, C-STORE), on request through the [administration API](#scu) (whether or not `dimse.listen` is
set).

| | |
|---|---|
| AE title | `dimse.ae_title` (default `FORTITUDE`) |
| Implementation Class UID | `2.25.331980162813764169807831877674859356756` |
| Implementation Version Name | `FORTITUDE_<version>` |
| Maximum PDU length received | `dimse.max_pdu_length` (default 262144) |
| Concurrent associations | `dimse.max_associations` (default 64); more are rejected (transient, local limit exceeded) |
| Asynchronous operations, extended negotiation | Not supported (not negotiated) |
| SCP/SCU role selection | Accepted as proposed for storage SOP classes (the requestor may take the SCP role, to receive C-GET sub-operations); for other SOP classes, the SCP role of the requestor is refused |
| Timeouts | `dimse.timeout_secs` (default 30) for each network read or write, and to connect to a peer; `dimse.retrieve_timeout_secs` (default 600) between two responses to a C-MOVE Fortitude requested |

### SOP classes (SCP)

| SOP class | UID |
|---|---|
| Verification | `1.2.840.10008.1.1` |
| Patient Root Query/Retrieve – FIND | `1.2.840.10008.5.1.4.1.2.1.1` |
| Patient Root Query/Retrieve – MOVE | `1.2.840.10008.5.1.4.1.2.1.2` |
| Study Root Query/Retrieve – FIND | `1.2.840.10008.5.1.4.1.2.2.1` |
| Study Root Query/Retrieve – MOVE | `1.2.840.10008.5.1.4.1.2.2.2` |
| Patient Root Query/Retrieve – GET | `1.2.840.10008.5.1.4.1.2.1.3` |
| Study Root Query/Retrieve – GET | `1.2.840.10008.5.1.4.1.2.2.3` |
| Every standard storage SOP class (`1.2.840.10008.5.1.4.1.1.*`, retired ones included) | |
| Storage SOP classes listed in `dimse.extra_sop_classes` (e.g. private ones) | |

The Modality Worklist is not supported; its presentation contexts are refused.

### Association acceptance

- With `dimse.check_called_ae_title`, the called AE title must be `dimse.ae_title`.
- With `dimse.allow_unknown_peers = false`, the calling AE title must be one of
  `dimse.peers`. AE titles are not authentication: restrict access to the DIMSE port at the
  network level.
- **Transfer syntaxes.** Every transfer syntax known to dicom-rs is accepted (uncompressed,
  deflated, JPEG, JPEG-LS, JPEG 2000, HTJ2K, RLE, MPEG/HEVC). When a presentation context
  proposes several: for storage SOP classes, the first non-native one in the requestor's
  order (the sender's data is then not decompressed); for verification, query/retrieve, and
  storage SOP classes for which the requestor takes the SCP role (Fortitude sends on them),
  the first native one (Explicit/Implicit VR Little Endian, Explicit VR Big Endian, Deflated).

### C-ECHO

Always answered with Success.

### C-STORE

- The received data set is stored byte for byte, behind a file meta group built from the
  command (Affected SOP Class/Instance UID, the presentation context's transfer syntax,
  calling AE title as Source Application Entity Title). No attribute is coerced.
- Ingest, required attributes and duplicates are those of STOW-RS: an instance already
  stored is discarded and answered with Success.

| Status | Cause |
|---|---|
| `0000` | Stored, or already stored (duplicate). |
| `A900` | Missing or invalid required UID, or series bound to another study. |
| `C000` | The data set cannot be parsed. |
| `A700` | The file cannot be written. |
| `0110` | Index failure. |

Failures carry an Error Comment (0000,0902). A data set larger than
`dicomweb.max_instance_size_mib` aborts the association.

### C-FIND

- Levels: `PATIENT` (Patient Root only), `STUDY`, `SERIES`, `IMAGE`. Another level is answered
  with `A900`.
- Matching is that of QIDO-RS: single value, list of UIDs, wildcards `*` and `?`, DA/TM/DT
  ranges, case-insensitive person names, on any attribute (indexed ones are matched in the
  index, others against the stored metadata). Queries are relational: keys of any level can
  be combined, and higher-level unique keys are not required. Sequence keys are return keys
  only.
- Each response contains Specific Character Set `ISO_IR 192` (UTF-8), Query/Retrieve Level,
  and every requested key: with its value, or empty when the match has none. Retrieve AE
  Title is `dimse.ae_title`. Computed keys: Modalities in Study, Number of Study Related
  Series/Instances, Number of Series Related Instances, Number of Patient Related
  Studies/Series/Instances.
- The patient level groups the matching studies by Patient ID and Issuer of Patient ID.
- At most `dimse.max_find_results` matches (default 10000, 0 for no limit) are returned;
  beyond, the query ends with `A700` after them.
- C-CANCEL is honored between responses (final status `FE00`).

### C-MOVE

- The identifier selects instances by unique keys: Patient ID (`PATIENT`), Study Instance
  UID (`STUDY`), Series Instance UID (`SERIES`), SOP Instance UID (`IMAGE`), each possibly a
  list. The key of the level is required (`A900` otherwise); keys of higher levels narrow the
  selection.
- The Move Destination must be one of `dimse.peers` (`A801` otherwise). One association is
  opened to it per C-MOVE, with Fortitude's AE title as calling AE title.
- Proposed presentation contexts: each stored (SOP class, transfer syntax) on its own, and
  Explicit and Implicit VR Little Endian per SOP class (at most 128). Instances are sent as
  stored when their transfer syntax is accepted, otherwise transcoded to the accepted
  uncompressed syntax; an instance that cannot be decoded (e.g. JPEG-LS, see *Codecs*) fails.
- Sub-operations carry Move Originator AE Title and Message ID. A pending response follows
  each sub-operation; C-CANCEL is honored between them.

| Final status | When |
|---|---|
| `0000` | Every sub-operation succeeded (or nothing matched). |
| `B000` | Some sub-operations failed or completed with a warning; Failed SOP Instance UID List is returned. |
| `FE00` | Cancelled. |
| `A801` | Unknown move destination. |
| `A702` | The destination cannot be reached or accepts no proposed context. |
| `A900` | Invalid identifier. |

### C-GET

- The identifier selects instances as for C-MOVE (same keys, same `A900` failures).
- Instances are sent with C-STORE on the requestor's association, on the presentation
  contexts of storage SOP classes for which it took the SCP role (role selection). Each is
  sent as stored when a context of its SOP class accepted its transfer syntax, otherwise
  transcoded to an accepted Explicit or Implicit VR Little Endian context; an instance
  without either fails, as does one that cannot be decoded.
- Sub-operations are sent one at a time, each after the response to the previous one. A
  pending response follows each sub-operation; C-CANCEL is honored between them, including
  one received while waiting for a sub-operation's response.

| Final status | When |
|---|---|
| `0000` | Every sub-operation succeeded (or nothing matched). |
| `B000` | Some sub-operations failed or completed with a warning; Failed SOP Instance UID List is returned. |
| `FE00` | Cancelled. |
| `A900` | Invalid identifier. |

### SCU

Fortitude verifies, queries and retrieves from its peers, and sends them stored instances,
on request (`/admin/modalities`, see [Administration](administration.md#modalities)): those of
`dimse.peers` and those registered at runtime. Each request opens an association of its own,
with `dimse.ae_title` as calling AE title and `dimse.max_pdu_length` as maximum PDU length,
proposing Explicit and Implicit VR Little Endian (and, to send instances, their stored
transfer syntaxes).

| SOP class | UID | Used for |
|---|---|---|
| Verification | `1.2.840.10008.1.1` | C-ECHO |
| Study Root Query/Retrieve – FIND | `1.2.840.10008.5.1.4.1.2.2.1` | C-FIND at the `STUDY`, `SERIES` and `IMAGE` levels |
| Patient Root Query/Retrieve – FIND | `1.2.840.10008.5.1.4.1.2.1.1` | C-FIND at the `PATIENT` level, and at the others when the peer refuses Study Root |
| Study Root Query/Retrieve – MOVE | `1.2.840.10008.5.1.4.1.2.2.2` | C-MOVE at the `STUDY`, `SERIES` and `IMAGE` levels |
| Patient Root Query/Retrieve – MOVE | `1.2.840.10008.5.1.4.1.2.1.2` | C-MOVE at the `PATIENT` level, and at the others when the peer refuses Study Root |
| Storage SOP classes of the instances sent | | C-STORE |

- **C-FIND.** The identifier holds the matching keys given, and the usual return keys of the
  level (empty). Values are sent as given (wildcards, ranges); Specific Character Set
  `ISO_IR 192` is added when a value is not ASCII. Responses are decoded with their own
  Specific Character Set. Beyond the requested number of matches, a C-CANCEL is sent; a
  final `A700` after some matches is reported as a truncated result.
- **C-MOVE.** The identifier holds the unique keys given; the key of the level is required.
  The Move Destination is `dimse.ae_title` by default (the peer must know Fortitude's
  address, and `dimse.listen` must be set), or another AE title the peer knows. Pending
  responses update the progress of the retrieval; cancelling it sends a C-CANCEL with the
  next one. Final statuses `0000`, `B000` and `FE00` end it as succeeded, partial or
  cancelled; any other fails it.
- **C-STORE.** The instances selected (by patient, study, series or instance, as for a
  C-MOVE Fortitude receives) are sent over one association, one request at a time, each
  after the response to the previous one. Presentation contexts are proposed as for C-MOVE
  sub-operations: each stored (SOP class, transfer syntax) on its own, and Explicit and
  Implicit VR Little Endian per SOP class (at most 128); instances go out as stored when
  their transfer syntax is accepted, otherwise transcoded to the accepted uncompressed
  syntax. Success and warning statuses (`0001`, `Bxxx`) count as sent; any other fails the
  instance, which is listed with its status, and the send goes on. A send fails as a whole
  when the peer cannot be reached or accepts none of the SOP classes; when the association
  breaks, the instances not sent yet fail with it. Cancelling stops after the instance
  being sent, and releases the association.
- C-STORE requests received on the C-ECHO, C-FIND and C-MOVE associations are refused
  (`A700`).
