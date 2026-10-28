//! Turn the bytes of a Part-10 instance into an index record.
//!
//! Only the header is parsed (everything before pixel data); pixel data is located by the
//! [walker](crate::walker) and never decoded.

use dicom_core::{Tag, VR};
use dicom_dictionary_std::tags;
use dicom_object::{DefaultDicomObject, OpenFileOptions};
use fortitude_core::attrs::{self, INDEXED, IndexedAttribute, Level};
use fortitude_core::json::{self, EncodeOptions};
use fortitude_core::uid;

use crate::pixel::{ImageInfo, PixelLayout, Range, split_on_markers};
use crate::walker::{self, PixelValue};

/// Everything the index stores about one instance.
#[derive(Debug, Clone)]
pub struct InstanceRecord {
    pub study_uid: String,
    pub series_uid: String,
    pub sop_uid: String,
    pub sop_class_uid: String,
    /// Values of [`INDEXED`] attributes, in the same order.
    pub values: Vec<Option<String>>,
    /// Pre-encoded QIDO-RS default attributes of each level (member lists).
    pub study_attrs: String,
    pub series_attrs: String,
    pub instance_attrs: String,
    /// Pre-encoded metadata of the whole instance (member list).
    pub metadata: String,
    pub transfer_syntax: String,
    pub frames: u32,
    pub pixel: Option<PixelLayout>,
    /// File location relative to the storage root, and size.
    pub path: String,
    pub size: u64,
}

impl InstanceRecord {
    pub fn value(&self, attribute: &IndexedAttribute) -> Option<&str> {
        let index = INDEXED.iter().position(|a| a.tag == attribute.tag)?;
        self.values[index].as_deref()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("cannot parse DICOM data: {0}")]
    Unreadable(String),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy)]
pub struct IngestOptions {
    pub inline_binary_max: usize,
}

/// Identifiers read from an instance, available even when it is rejected later.
#[derive(Debug, Clone, Default)]
pub struct Identifiers {
    pub sop_class_uid: Option<String>,
    pub sop_uid: Option<String>,
}

pub fn parse(
    bytes: &[u8],
    options: IngestOptions,
) -> Result<InstanceRecord, (Identifiers, IngestError)> {
    let obj = OpenFileOptions::new()
        // Stop before Float Pixel Data, the first of the pixel data attributes.
        .read_until(tags::FLOAT_PIXEL_DATA)
        .from_reader(bytes)
        .map_err(|e| {
            (
                Identifiers::default(),
                IngestError::Unreadable(e.to_string()),
            )
        })?;

    let ids = Identifiers {
        sop_class_uid: string(&obj, tags::SOP_CLASS_UID),
        sop_uid: string(&obj, tags::SOP_INSTANCE_UID),
    };
    build(bytes, &obj, options).map_err(|e| (ids, e))
}

fn build(
    bytes: &[u8],
    obj: &DefaultDicomObject,
    options: IngestOptions,
) -> Result<InstanceRecord, IngestError> {
    let required = |tag: Tag, name: &str| {
        let value =
            string(obj, tag).ok_or_else(|| IngestError::Invalid(format!("missing {name}")))?;
        if uid::is_valid(&value) {
            Ok(value)
        } else {
            Err(IngestError::Invalid(format!("invalid {name} {value:?}")))
        }
    };
    let study_uid = required(tags::STUDY_INSTANCE_UID, "StudyInstanceUID")?;
    let series_uid = required(tags::SERIES_INSTANCE_UID, "SeriesInstanceUID")?;
    let sop_uid = required(tags::SOP_INSTANCE_UID, "SOPInstanceUID")?;
    let sop_class_uid = required(tags::SOP_CLASS_UID, "SOPClassUID")?;
    let transfer_syntax = obj
        .meta()
        .transfer_syntax()
        .trim_end_matches(['\0', ' '])
        .to_owned();

    let values = INDEXED.iter().map(|a| string(obj, a.tag)).collect();

    let bulk_prefix = format!(
        "{}/studies/{study_uid}/series/{series_uid}/instances/{sop_uid}/bulk",
        json::BASE_MARKER as char
    );
    let encode = EncodeOptions {
        inline_binary_max: options.inline_binary_max,
        bulk_prefix: &bulk_prefix,
    };
    let level_attrs = |level: Level| {
        let wanted = attrs::default_return(level);
        let mut out = String::new();
        json::encode_members(obj, &encode, |t| wanted.contains(&t), &mut out);
        out
    };

    let info = ImageInfo {
        rows: integer(obj, tags::ROWS),
        columns: integer(obj, tags::COLUMNS),
        samples_per_pixel: integer(obj, tags::SAMPLES_PER_PIXEL),
        bits_allocated: integer(obj, tags::BITS_ALLOCATED),
        frames: integer(obj, tags::NUMBER_OF_FRAMES).max(1),
        ybr_422: string(obj, tags::PHOTOMETRIC_INTERPRETATION).as_deref() == Some("YBR_FULL_422"),
    };

    let mut metadata = String::new();
    json::encode_members(obj, &encode, |_| true, &mut metadata);
    let pixel = match walker::find_pixel_data(bytes, &transfer_syntax) {
        Ok(Some(element)) => {
            let vr = match element.vr {
                Some(vr) => VR::from_binary(vr).unwrap_or(VR::OB),
                None if element.tag == tags::FLOAT_PIXEL_DATA => VR::OF,
                None if element.tag == tags::DOUBLE_FLOAT_PIXEL_DATA => VR::OD,
                None if info.bits_allocated > 8 => VR::OW,
                None => VR::OB,
            };
            json::push_bulk_member(element.tag, vr, &encode, &mut metadata);
            Some(layout(bytes, obj, &element, info))
        }
        Ok(None) => None,
        Err(e) => return Err(IngestError::Unreadable(e.to_string())),
    };

    Ok(InstanceRecord {
        values,
        study_attrs: level_attrs(Level::Study),
        series_attrs: level_attrs(Level::Series),
        instance_attrs: level_attrs(Level::Instance),
        metadata,
        transfer_syntax,
        frames: info.frames,
        pixel,
        path: String::new(),
        size: bytes.len() as u64,
        study_uid,
        series_uid,
        sop_uid,
        sop_class_uid,
    })
}

fn layout(
    bytes: &[u8],
    obj: &DefaultDicomObject,
    element: &walker::PixelElement,
    info: ImageInfo,
) -> PixelLayout {
    let extended: Option<Vec<u64>> = obj
        .element_opt(tags::EXTENDED_OFFSET_TABLE)
        .ok()
        .flatten()
        .map(|e| e.to_bytes().ok().unwrap_or_default())
        .map(|b| {
            b.as_chunks::<8>()
                .0
                .iter()
                .map(|c| u64::from_le_bytes(*c))
                .collect()
        });
    let mut layout = PixelLayout::new(element, info, extended.as_deref());
    if let (PixelLayout::Encapsulated { frames }, PixelValue::Encapsulated { fragments, .. }) =
        (&mut layout, &element.value)
        && frames.is_empty()
    {
        let heads: Vec<(Range, [u8; 4])> = fragments
            .iter()
            .map(|f| {
                let start = f.offset as usize;
                let mut head = [0u8; 4];
                let available = bytes
                    .get(start..start + (f.length as usize).min(4))
                    .unwrap_or_default();
                head[..available.len()].copy_from_slice(available);
                (
                    Range {
                        offset: f.offset,
                        length: u64::from(f.length),
                    },
                    head,
                )
            })
            .collect();
        *frames = split_on_markers(info.frames as usize, &heads);
    }
    layout
}

fn string(obj: &DefaultDicomObject, tag: Tag) -> Option<String> {
    let element = obj.element_opt(tag).ok().flatten()?;
    let value = element.to_str().ok()?;
    let value = value.trim_matches([' ', '\0']);
    (!value.is_empty()).then(|| value.to_owned())
}

fn integer(obj: &DefaultDicomObject, tag: Tag) -> u32 {
    obj.element_opt(tag)
        .ok()
        .flatten()
        .and_then(|e| e.to_int::<u32>().ok())
        .unwrap_or(0)
}
