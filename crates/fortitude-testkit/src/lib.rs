//! Synthetic DICOM instances for tests.
//!
//! Instances are small but complete Part-10 files, with deterministic pixel data so that
//! tests can check the exact bytes served for each frame.

use dicom_core::value::{PixelFragmentSequence, Value};
use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_object::mem::InMemElement;
use dicom_object::{FileMetaTableBuilder, InMemDicomObject};

pub const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";
pub const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";
pub const JPEG_BASELINE: &str = "1.2.840.10008.1.2.4.50";

#[derive(Debug, Clone)]
pub enum Pixels {
    None,
    /// Uncompressed 8-bit monochrome frames.
    Native,
    /// Encapsulated frames, each split into this many fragments, without offset table.
    Encapsulated {
        fragments_per_frame: usize,
    },
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub study_uid: String,
    pub series_uid: String,
    pub sop_uid: String,
    pub sop_class_uid: String,
    pub transfer_syntax: String,
    pub charset: Option<String>,
    pub patient_id: String,
    pub patient_name: String,
    pub study_date: String,
    pub modality: String,
    pub series_number: Option<i32>,
    pub instance_number: Option<i32>,
    pub rows: u16,
    pub columns: u16,
    pub frames: u32,
    pub pixels: Pixels,
    pub extra: Vec<InMemElement>,
}

impl Instance {
    pub fn new(study_uid: &str, series_uid: &str, sop_uid: &str) -> Self {
        Self {
            study_uid: study_uid.into(),
            series_uid: series_uid.into(),
            sop_uid: sop_uid.into(),
            sop_class_uid: uids::CT_IMAGE_STORAGE.into(),
            transfer_syntax: EXPLICIT_VR_LE.into(),
            charset: None,
            patient_id: "PAT-1".into(),
            patient_name: "Doe^Jane".into(),
            study_date: "20240115".into(),
            modality: "CT".into(),
            series_number: Some(1),
            instance_number: Some(1),
            rows: 4,
            columns: 4,
            frames: 1,
            pixels: Pixels::Native,
            extra: Vec::new(),
        }
    }

    pub fn with(mut self, f: impl FnOnce(&mut Self)) -> Self {
        f(&mut self);
        self
    }

    pub fn element(mut self, tag: Tag, vr: VR, value: impl Into<PrimitiveValue>) -> Self {
        self.extra.push(DataElement::new(tag, vr, value.into()));
        self
    }

    /// Bytes of frame `index` (0-based) as stored: raw pixels, or the encapsulated
    /// codestream (concatenation of its fragments).
    pub fn frame(&self, index: u32) -> Vec<u8> {
        match self.pixels {
            Pixels::Native | Pixels::None => {
                let size = usize::from(self.rows) * usize::from(self.columns);
                (0..size).map(|k| (index as usize * 31 + k) as u8).collect()
            }
            Pixels::Encapsulated { .. } => {
                // A fake JPEG codestream: SOI, payload without any 0xFF, EOI.
                let mut data = vec![0xFF, 0xD8];
                data.extend((0..60).map(|k| ((index as usize * 7 + k) % 200) as u8));
                data.extend([0xFF, 0xD9]);
                data
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut obj = InMemDicomObject::new_empty();
        if let Some(charset) = &self.charset {
            obj.put_str(tags::SPECIFIC_CHARACTER_SET, VR::CS, charset.clone());
        }
        obj.put_str(tags::SOP_CLASS_UID, VR::UI, self.sop_class_uid.clone());
        obj.put_str(tags::SOP_INSTANCE_UID, VR::UI, self.sop_uid.clone());
        obj.put_str(tags::STUDY_DATE, VR::DA, self.study_date.clone());
        obj.put_str(tags::MODALITY, VR::CS, self.modality.clone());
        obj.put_str(tags::PATIENT_NAME, VR::PN, self.patient_name.clone());
        obj.put_str(tags::PATIENT_ID, VR::LO, self.patient_id.clone());
        obj.put_str(tags::STUDY_INSTANCE_UID, VR::UI, self.study_uid.clone());
        obj.put_str(tags::SERIES_INSTANCE_UID, VR::UI, self.series_uid.clone());
        if let Some(n) = self.series_number {
            obj.put_str(tags::SERIES_NUMBER, VR::IS, n.to_string());
        }
        if let Some(n) = self.instance_number {
            obj.put_str(tags::INSTANCE_NUMBER, VR::IS, n.to_string());
        }
        if !matches!(self.pixels, Pixels::None) {
            let us = |tag, value: u16| DataElement::new(tag, VR::US, PrimitiveValue::from(value));
            obj.put(us(tags::SAMPLES_PER_PIXEL, 1));
            obj.put_str(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
            if self.frames > 1 {
                obj.put_str(tags::NUMBER_OF_FRAMES, VR::IS, self.frames.to_string());
            }
            obj.put(us(tags::ROWS, self.rows));
            obj.put(us(tags::COLUMNS, self.columns));
            obj.put(us(tags::BITS_ALLOCATED, 8));
            obj.put(us(tags::BITS_STORED, 8));
            obj.put(us(tags::HIGH_BIT, 7));
            obj.put(us(tags::PIXEL_REPRESENTATION, 0));
        }
        for element in &self.extra {
            obj.put(element.clone());
        }
        match self.pixels {
            Pixels::None => {}
            Pixels::Native => {
                let data: Vec<u8> = (0..self.frames).flat_map(|i| self.frame(i)).collect();
                obj.put(DataElement::new(
                    tags::PIXEL_DATA,
                    VR::OB,
                    PrimitiveValue::from(data),
                ));
            }
            Pixels::Encapsulated {
                fragments_per_frame,
            } => {
                let mut fragments: Vec<Vec<u8>> = Vec::new();
                for i in 0..self.frames {
                    let frame = self.frame(i);
                    let size = frame
                        .len()
                        .div_ceil(fragments_per_frame)
                        .next_multiple_of(2);
                    fragments.extend(frame.chunks(size).map(<[u8]>::to_vec));
                }
                obj.put(DataElement::new(
                    tags::PIXEL_DATA,
                    VR::OB,
                    Value::PixelSequence(PixelFragmentSequence::new(Vec::<u32>::new(), fragments)),
                ));
            }
        }

        let file = obj
            .with_meta(
                FileMetaTableBuilder::new()
                    .transfer_syntax(self.transfer_syntax.clone())
                    .media_storage_sop_class_uid(self.sop_class_uid.clone())
                    .media_storage_sop_instance_uid(self.sop_uid.clone()),
            )
            .expect("valid file meta");
        let mut bytes = Vec::new();
        file.write_all(&mut bytes).expect("encodable instance");
        bytes
    }
}

/// `count` instances of one series, numbered from 1.
pub fn series(study_uid: &str, series_uid: &str, count: u32) -> Vec<Instance> {
    (1..=count)
        .map(|i| {
            Instance::new(study_uid, series_uid, &format!("{series_uid}.{i}"))
                .with(|inst| inst.instance_number = Some(i as i32))
        })
        .collect()
}
