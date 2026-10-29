//! Conversion between network data sets and Part-10 files (PS3.10 §7).
//!
//! A received data set is stored untouched behind a file meta group built from the C-STORE
//! command; a stored file is sent without its preamble and meta group.

/// Implementation Class UID (0002,0012) of the files Fortitude writes.
pub const IMPLEMENTATION_CLASS_UID: &str = "2.25.331980162813764169807831877674859356756";
/// Implementation Version Name (0002,0013), at most 16 characters.
pub const IMPLEMENTATION_VERSION_NAME: &str = concat!("FORTITUDE_", env!("CARGO_PKG_VERSION"));

/// Identification of a received instance, written in its file meta group.
#[derive(Debug, Clone, Copy)]
pub struct Meta<'a> {
    pub sop_class_uid: &'a str,
    pub sop_instance_uid: &'a str,
    pub transfer_syntax: &'a str,
    /// Calling AE title of the association the instance was received on.
    pub source_ae_title: &'a str,
}

/// Preamble, magic code and file meta group (Explicit VR Little Endian) for a data set
/// encoded with `meta.transfer_syntax`, with room reserved for `data_set_len` bytes.
pub fn header(meta: &Meta<'_>, data_set_len: usize) -> Vec<u8> {
    let mut group = Vec::with_capacity(256);
    group.extend([0x02, 0x00, 0x01, 0x00, b'O', b'B', 0, 0, 2, 0, 0, 0, 0, 1]);
    push_short(&mut group, 0x0002, b"UI", meta.sop_class_uid, 0);
    push_short(&mut group, 0x0003, b"UI", meta.sop_instance_uid, 0);
    push_short(&mut group, 0x0010, b"UI", meta.transfer_syntax, 0);
    push_short(&mut group, 0x0012, b"UI", IMPLEMENTATION_CLASS_UID, 0);
    push_short(&mut group, 0x0013, b"SH", IMPLEMENTATION_VERSION_NAME, b' ');
    if !meta.source_ae_title.is_empty() {
        push_short(&mut group, 0x0016, b"AE", meta.source_ae_title, b' ');
    }

    let mut out = Vec::with_capacity(132 + 12 + group.len() + data_set_len);
    out.resize(128, 0);
    out.extend(b"DICM");
    out.extend([0x02, 0x00, 0x00, 0x00, b'U', b'L', 4, 0]);
    out.extend((group.len() as u32).to_le_bytes());
    out.extend(group);
    out
}

fn push_short(out: &mut Vec<u8>, element: u16, vr: &[u8; 2], value: &str, padding: u8) {
    let padded = value.len() + value.len() % 2;
    out.extend(0x0002_u16.to_le_bytes());
    out.extend(element.to_le_bytes());
    out.extend(vr);
    out.extend((padded as u16).to_le_bytes());
    out.extend(value.as_bytes());
    if padded > value.len() {
        out.push(padding);
    }
}

/// Offset of the data set in a Part-10 file: after the preamble (if any), the magic code
/// and the file meta group.
pub fn data_set_offset(file: &[u8]) -> Option<usize> {
    let mut pos = if file.get(128..132) == Some(b"DICM") {
        132
    } else if file.get(0..4) == Some(b"DICM") {
        4
    } else {
        return None;
    };
    // The meta group is always Explicit VR Little Endian.
    while let Some(header) = file.get(pos..pos + 8) {
        if u16::from_le_bytes([header[0], header[1]]) != 0x0002 {
            break;
        }
        let vr = &header[4..6];
        let long = matches!(
            vr,
            b"OB"
                | b"OD"
                | b"OF"
                | b"OL"
                | b"OV"
                | b"OW"
                | b"SQ"
                | b"SV"
                | b"UC"
                | b"UN"
                | b"UR"
                | b"UT"
                | b"UV"
        );
        let (header_len, length) = if long {
            let bytes = file.get(pos + 8..pos + 12)?;
            (12, u32::from_le_bytes(bytes.try_into().ok()?) as usize)
        } else {
            (8, u16::from_le_bytes([header[6], header[7]]) as usize)
        };
        pos = pos.checked_add(header_len + length)?;
    }
    (pos <= file.len()).then_some(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_a_valid_meta_group() {
        let meta = Meta {
            sop_class_uid: "1.2.840.10008.5.1.4.1.1.2",
            sop_instance_uid: "1.2.3",
            transfer_syntax: "1.2.840.10008.1.2",
            source_ae_title: "MODALITY",
        };
        let mut file = header(&meta, 0);
        let data_set = [
            0x08, 0x00, 0x18, 0x00, 6, 0, 0, 0, b'1', b'.', b'2', b'.', b'3', 0,
        ];
        file.extend(data_set);
        assert_eq!(data_set_offset(&file), Some(file.len() - data_set.len()));

        let object = dicom_object::OpenFileOptions::new()
            .from_reader(&file[..])
            .expect("readable file");
        assert_eq!(object.meta().media_storage_sop_instance_uid(), "1.2.3");
        assert_eq!(object.meta().transfer_syntax(), "1.2.840.10008.1.2");
        assert_eq!(
            object.meta().source_application_entity_title.as_deref(),
            Some("MODALITY")
        );
        assert_eq!(
            object
                .element(dicom_dictionary_std::tags::SOP_INSTANCE_UID)
                .unwrap()
                .to_str()
                .unwrap(),
            "1.2.3"
        );
    }

    #[test]
    fn rejects_files_without_magic_code() {
        assert_eq!(data_set_offset(&[0; 200]), None);
    }
}
