//! Minimal walker over the raw bytes of a Part-10 file.
//!
//! dicom-object does not expose byte positions, so this walker skips over the top-level data
//! set to locate the pixel data element, and the fragments of encapsulated pixel data. This
//! is what lets frames be served with a single positioned read, without parsing.

use dicom_core::Tag;
use dicom_dictionary_std::tags;

const ITEM: Tag = Tag(0xFFFE, 0xE000);
const ITEM_END: Tag = Tag(0xFFFE, 0xE00D);
const SEQUENCE_END: Tag = Tag(0xFFFE, 0xE0DD);
const UNDEFINED: u32 = 0xFFFF_FFFF;

/// Pixel data attributes, any of which carries the frames of an image.
pub const PIXEL_TAGS: [Tag; 3] = [
    tags::FLOAT_PIXEL_DATA,
    tags::DOUBLE_FLOAT_PIXEL_DATA,
    tags::PIXEL_DATA,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Encoding {
    explicit: bool,
    little: bool,
}

const EXPLICIT_LE: Encoding = Encoding {
    explicit: true,
    little: true,
};
const IMPLICIT_LE: Encoding = Encoding {
    explicit: false,
    little: true,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelElement {
    pub tag: Tag,
    /// Value representation as written in the file, if explicit.
    pub vr: Option<[u8; 2]>,
    /// Offset of the first value byte (or of the first item, for encapsulated data).
    pub value_offset: u64,
    pub value: PixelValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PixelValue {
    Native {
        length: u64,
    },
    Encapsulated {
        offset_table: Vec<u32>,
        fragments: Vec<Fragment>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment {
    /// Offset of the item header, which Basic Offset Table entries refer to.
    pub item_offset: u64,
    /// Offset of the fragment bytes.
    pub offset: u64,
    pub length: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WalkError {
    #[error("not a DICOM Part-10 stream")]
    NotPart10,
    #[error("truncated or malformed data set at byte {0}")]
    Malformed(usize),
}

/// Locate the top-level pixel data element of a Part-10 file.
///
/// Returns `Ok(None)` when there is no pixel data, or when the transfer syntax does not
/// allow positioned access (deflated data sets).
pub fn find_pixel_data(
    file: &[u8],
    transfer_syntax: &str,
) -> Result<Option<PixelElement>, WalkError> {
    let encoding = match transfer_syntax.trim_end_matches(['\0', ' ']) {
        "1.2.840.10008.1.2" => IMPLICIT_LE,
        "1.2.840.10008.1.2.2" => Encoding {
            explicit: true,
            little: false,
        },
        "1.2.840.10008.1.2.1.99" | "1.2.840.10008.1.2.4.95" => return Ok(None),
        _ => EXPLICIT_LE,
    };
    let reader = Reader { buf: file };
    let mut pos = dataset_start(file)?;
    while pos < file.len() {
        let header = reader.header(pos, encoding)?;
        let value_offset = pos + header.len;
        if PIXEL_TAGS.contains(&header.tag) {
            let value = if header.length == UNDEFINED {
                reader.fragments(value_offset)?
            } else {
                reader.check(value_offset, header.length as usize)?;
                PixelValue::Native {
                    length: u64::from(header.length),
                }
            };
            return Ok(Some(PixelElement {
                tag: header.tag,
                vr: header.vr,
                value_offset: value_offset as u64,
                value,
            }));
        }
        pos = reader.skip_value(value_offset, &header, encoding)?;
    }
    Ok(None)
}

/// Offset of the data set, after the preamble, magic code and file meta group.
fn dataset_start(file: &[u8]) -> Result<usize, WalkError> {
    let mut pos = if file.get(128..132) == Some(b"DICM") {
        132
    } else if file.get(0..4) == Some(b"DICM") {
        4
    } else {
        return Err(WalkError::NotPart10);
    };
    let reader = Reader { buf: file };
    while file.len() >= pos + 4 && reader.u16(pos, true)? == 0x0002 {
        let header = reader.header(pos, EXPLICIT_LE)?;
        pos = reader.skip_value(pos + header.len, &header, EXPLICIT_LE)?;
    }
    Ok(pos)
}

struct Header {
    tag: Tag,
    vr: Option<[u8; 2]>,
    length: u32,
    /// Header size in bytes.
    len: usize,
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl Reader<'_> {
    fn check(&self, pos: usize, len: usize) -> Result<(), WalkError> {
        match pos.checked_add(len) {
            Some(end) if end <= self.buf.len() => Ok(()),
            _ => Err(WalkError::Malformed(pos)),
        }
    }

    fn u16(&self, pos: usize, little: bool) -> Result<u16, WalkError> {
        self.check(pos, 2)?;
        let bytes = [self.buf[pos], self.buf[pos + 1]];
        Ok(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    }

    fn u32(&self, pos: usize, little: bool) -> Result<u32, WalkError> {
        self.check(pos, 4)?;
        let bytes = [
            self.buf[pos],
            self.buf[pos + 1],
            self.buf[pos + 2],
            self.buf[pos + 3],
        ];
        Ok(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn tag(&self, pos: usize, little: bool) -> Result<Tag, WalkError> {
        Ok(Tag(self.u16(pos, little)?, self.u16(pos + 2, little)?))
    }

    fn header(&self, pos: usize, encoding: Encoding) -> Result<Header, WalkError> {
        let tag = self.tag(pos, encoding.little)?;
        if tag.group() == 0xFFFE || !encoding.explicit {
            return Ok(Header {
                tag,
                vr: None,
                length: self.u32(pos + 4, encoding.little)?,
                len: 8,
            });
        }
        self.check(pos + 4, 2)?;
        let vr = [self.buf[pos + 4], self.buf[pos + 5]];
        if matches!(
            &vr,
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
        ) {
            Ok(Header {
                tag,
                vr: Some(vr),
                length: self.u32(pos + 8, encoding.little)?,
                len: 12,
            })
        } else {
            Ok(Header {
                tag,
                vr: Some(vr),
                length: u32::from(self.u16(pos + 6, encoding.little)?),
                len: 8,
            })
        }
    }

    /// Position after the value of an element whose header ends at `pos`.
    fn skip_value(
        &self,
        pos: usize,
        header: &Header,
        encoding: Encoding,
    ) -> Result<usize, WalkError> {
        if header.length != UNDEFINED {
            self.check(pos, header.length as usize)?;
            return Ok(pos + header.length as usize);
        }
        // Undefined length: a sequence, or UN whose content is implicit VR little endian.
        let inner = if header.vr == Some(*b"UN") {
            IMPLICIT_LE
        } else {
            encoding
        };
        self.skip_items(pos, inner)
    }

    /// Skip the items of an undefined length sequence starting at `pos`.
    fn skip_items(&self, mut pos: usize, encoding: Encoding) -> Result<usize, WalkError> {
        loop {
            let tag = self.tag(pos, encoding.little)?;
            let length = self.u32(pos + 4, encoding.little)?;
            pos += 8;
            match tag {
                SEQUENCE_END => return Ok(pos),
                ITEM if length == UNDEFINED => pos = self.skip_item_elements(pos, encoding)?,
                ITEM => {
                    self.check(pos, length as usize)?;
                    pos += length as usize;
                }
                _ => return Err(WalkError::Malformed(pos - 8)),
            }
        }
    }

    /// Skip the elements of an undefined length item, up to its delimiter.
    fn skip_item_elements(&self, mut pos: usize, encoding: Encoding) -> Result<usize, WalkError> {
        loop {
            let header = self.header(pos, encoding)?;
            if header.tag == ITEM_END {
                return Ok(pos + 8);
            }
            pos = self.skip_value(pos + header.len, &header, encoding)?;
        }
    }

    /// Read the items of encapsulated pixel data (always little endian).
    fn fragments(&self, mut pos: usize) -> Result<PixelValue, WalkError> {
        let mut offset_table = Vec::new();
        let mut fragments = Vec::new();
        let mut first = true;
        loop {
            let tag = self.tag(pos, true)?;
            let length = self.u32(pos + 4, true)?;
            match tag {
                SEQUENCE_END => {
                    return Ok(PixelValue::Encapsulated {
                        offset_table,
                        fragments,
                    });
                }
                ITEM if length != UNDEFINED => {
                    self.check(pos + 8, length as usize)?;
                    if first {
                        offset_table = self.buf[pos + 8..pos + 8 + length as usize]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|c| u32::from_le_bytes(*c))
                            .collect();
                    } else {
                        fragments.push(Fragment {
                            item_offset: pos as u64,
                            offset: (pos + 8) as u64,
                            length,
                        });
                    }
                    first = false;
                    pos += 8 + length as usize;
                }
                _ => return Err(WalkError::Malformed(pos)),
            }
        }
    }
}
