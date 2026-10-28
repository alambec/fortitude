//! Frame layout of stored pixel data: where each frame lives in the file.
//!
//! Computed once at ingest from the walker output and the image attributes, then stored in
//! the index as a compact binary blob.

use dicom_dictionary_std::tags;

use crate::walker::{Fragment, PixelElement, PixelValue};

/// A byte range of the stored file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PixelLayout {
    /// Native pixel data: frames are contiguous, `frame_length` bytes each
    /// (0 when frames are not byte aligned).
    Native {
        offset: u64,
        length: u64,
        frame_length: u64,
        frames: u32,
        sample: Sample,
    },
    /// Encapsulated pixel data: the fragments of each frame. Empty when fragments could not
    /// be attributed to frames.
    Encapsulated { frames: Vec<Vec<Range>> },
}

/// Type of native samples, after the attribute holding them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sample {
    /// Pixel Data (7FE0,0010).
    Integer,
    /// Float Pixel Data (7FE0,0008).
    Float,
    /// Double Float Pixel Data (7FE0,0009).
    Double,
}

impl Sample {
    /// Kind byte of the encoded layout.
    fn code(self) -> u8 {
        match self {
            Sample::Integer => 1,
            Sample::Float => 3,
            Sample::Double => 4,
        }
    }
}

/// Image attributes needed to compute frame boundaries.
#[derive(Debug, Clone, Copy, Default)]
pub struct ImageInfo {
    pub rows: u32,
    pub columns: u32,
    pub samples_per_pixel: u32,
    pub bits_allocated: u32,
    pub frames: u32,
    /// Native YBR_FULL_422: chroma is subsampled, two samples per pixel are stored.
    pub ybr_422: bool,
}

impl PixelLayout {
    pub fn new(element: &PixelElement, info: ImageInfo, extended_offsets: Option<&[u64]>) -> Self {
        let frames = info.frames.max(1);
        match &element.value {
            PixelValue::Native { length } => {
                let samples = if info.ybr_422 {
                    2
                } else {
                    info.samples_per_pixel.max(1)
                };
                let bits = u64::from(info.rows)
                    * u64::from(info.columns)
                    * u64::from(samples)
                    * u64::from(info.bits_allocated);
                let frame_length = if bits % 8 == 0 && bits / 8 * u64::from(frames) <= *length {
                    bits / 8
                } else {
                    0
                };
                let sample = match element.tag {
                    tags::FLOAT_PIXEL_DATA => Sample::Float,
                    tags::DOUBLE_FLOAT_PIXEL_DATA => Sample::Double,
                    _ => Sample::Integer,
                };
                PixelLayout::Native {
                    offset: element.value_offset,
                    length: *length,
                    frame_length,
                    frames,
                    sample,
                }
            }
            PixelValue::Encapsulated {
                offset_table,
                fragments,
            } => PixelLayout::Encapsulated {
                frames: split_frames(frames as usize, offset_table, fragments, extended_offsets),
            },
        }
    }

    pub fn frame_count(&self) -> usize {
        match self {
            PixelLayout::Native {
                frame_length: 0, ..
            } => 0,
            PixelLayout::Native { frames, .. } => *frames as usize,
            PixelLayout::Encapsulated { frames } => frames.len(),
        }
    }

    /// Byte ranges of frame `index` (0-based), whose concatenation is the frame.
    pub fn frame(&self, index: usize) -> Option<Vec<Range>> {
        match self {
            PixelLayout::Native {
                offset,
                frame_length,
                frames,
                ..
            } => (*frame_length > 0 && index < *frames as usize).then(|| {
                vec![Range {
                    offset: offset + index as u64 * frame_length,
                    length: *frame_length,
                }]
            }),
            PixelLayout::Encapsulated { frames } => frames.get(index).cloned(),
        }
    }

    /// Byte ranges of the whole pixel data value: native bytes, or every fragment.
    pub fn all(&self) -> Vec<Range> {
        match self {
            PixelLayout::Native { offset, length, .. } => vec![Range {
                offset: *offset,
                length: *length,
            }],
            PixelLayout::Encapsulated { frames } => frames.iter().flatten().copied().collect(),
        }
    }

    pub fn is_encapsulated(&self) -> bool {
        matches!(self, PixelLayout::Encapsulated { .. })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            PixelLayout::Native {
                offset,
                length,
                frame_length,
                frames,
                sample,
            } => {
                out.push(sample.code());
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&length.to_le_bytes());
                out.extend_from_slice(&frame_length.to_le_bytes());
                out.extend_from_slice(&frames.to_le_bytes());
            }
            PixelLayout::Encapsulated { frames } => {
                out.push(2);
                out.extend_from_slice(&(frames.len() as u32).to_le_bytes());
                for frame in frames {
                    out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
                    for range in frame {
                        out.extend_from_slice(&range.offset.to_le_bytes());
                        out.extend_from_slice(&range.length.to_le_bytes());
                    }
                }
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut cursor = Cursor(bytes);
        match cursor.u8()? {
            code @ (1 | 3 | 4) => Some(PixelLayout::Native {
                offset: cursor.u64()?,
                length: cursor.u64()?,
                frame_length: cursor.u64()?,
                frames: cursor.u32()?,
                sample: match code {
                    3 => Sample::Float,
                    4 => Sample::Double,
                    _ => Sample::Integer,
                },
            }),
            2 => {
                let count = cursor.u32()? as usize;
                let mut frames = Vec::with_capacity(count.min(bytes.len()));
                for _ in 0..count {
                    let ranges = cursor.u32()? as usize;
                    let mut frame = Vec::with_capacity(ranges.min(bytes.len()));
                    for _ in 0..ranges {
                        frame.push(Range {
                            offset: cursor.u64()?,
                            length: cursor.u64()?,
                        });
                    }
                    frames.push(frame);
                }
                Some(PixelLayout::Encapsulated { frames })
            }
            _ => None,
        }
    }
}

/// Attribute encapsulated fragments to frames (PS3.5 §A.4), trying in turn: a single frame,
/// one fragment per frame, the (Extended) Basic Offset Table, and JPEG/JPEG 2000 start
/// markers at the beginning of fragments.
fn split_frames(
    frames: usize,
    offset_table: &[u32],
    fragments: &[Fragment],
    extended_offsets: Option<&[u64]>,
) -> Vec<Vec<Range>> {
    let range = |f: &Fragment| Range {
        offset: f.offset,
        length: u64::from(f.length),
    };
    if fragments.is_empty() {
        return Vec::new();
    }
    if frames == 1 {
        return vec![fragments.iter().map(range).collect()];
    }
    if fragments.len() == frames {
        return fragments.iter().map(|f| vec![range(f)]).collect();
    }
    let first_item = fragments[0].item_offset;
    let table: Option<Vec<u64>> = match extended_offsets {
        Some(offsets) if offsets.len() == frames => Some(offsets.to_vec()),
        _ if offset_table.len() == frames => {
            Some(offset_table.iter().map(|o| u64::from(*o)).collect())
        }
        _ => None,
    };
    if let Some(table) = table {
        let mut result = vec![Vec::new(); frames];
        for fragment in fragments {
            let relative = fragment.item_offset - first_item;
            let frame = table
                .partition_point(|start| *start <= relative)
                .saturating_sub(1);
            result[frame].push(range(fragment));
        }
        if result.iter().all(|f| !f.is_empty()) {
            return result;
        }
    }
    Vec::new()
}

/// Split fragments on codestream start markers, given the first bytes of each fragment.
pub fn split_on_markers(frames: usize, fragments: &[(Range, [u8; 4])]) -> Vec<Vec<Range>> {
    let starts = |head: &[u8; 4]| {
        head[..2] == [0xFF, 0xD8] // JPEG, JPEG-LS SOI
            || head[..2] == [0xFF, 0x4F] // JPEG 2000 codestream SOC
            || head == &[0x00, 0x00, 0x00, 0x0C] // JP2 signature box
    };
    let mut result: Vec<Vec<Range>> = Vec::new();
    for (range, head) in fragments {
        if starts(head) || result.is_empty() {
            result.push(vec![*range]);
        } else if let Some(last) = result.last_mut() {
            last.push(*range);
        }
    }
    if result.len() == frames {
        result
    } else {
        Vec::new()
    }
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(item_offset: u64, length: u32) -> Fragment {
        Fragment {
            item_offset,
            offset: item_offset + 8,
            length,
        }
    }

    #[test]
    fn native_frames_are_contiguous() {
        let element = PixelElement {
            tag: dicom_dictionary_std::tags::PIXEL_DATA,
            vr: Some(*b"OW"),
            value_offset: 1000,
            value: PixelValue::Native {
                length: 2 * 4 * 4 * 3,
            },
        };
        let info = ImageInfo {
            rows: 4,
            columns: 4,
            samples_per_pixel: 1,
            bits_allocated: 16,
            frames: 3,
            ybr_422: false,
        };
        let layout = PixelLayout::new(&element, info, None);
        assert_eq!(layout.frame_count(), 3);
        assert_eq!(
            layout.frame(2),
            Some(vec![Range {
                offset: 1064,
                length: 32
            }])
        );
        assert_eq!(layout.frame(3), None);
        assert_eq!(PixelLayout::decode(&layout.encode()), Some(layout));
    }

    #[test]
    fn native_ybr_422_stores_two_samples_per_pixel() {
        let element = PixelElement {
            tag: dicom_dictionary_std::tags::PIXEL_DATA,
            vr: Some(*b"OB"),
            value_offset: 0,
            value: PixelValue::Native {
                length: 4 * 4 * 2 * 2,
            },
        };
        let info = ImageInfo {
            rows: 4,
            columns: 4,
            samples_per_pixel: 3,
            bits_allocated: 8,
            frames: 2,
            ybr_422: true,
        };
        assert_eq!(
            PixelLayout::new(&element, info, None).frame(1),
            Some(vec![Range {
                offset: 32,
                length: 32
            }])
        );
    }

    #[test]
    fn fragments_follow_the_offset_table() {
        // Frame 0: fragments at items 0 and 1; frame 1: fragment at item 2.
        let fragments = [fragment(500, 10), fragment(518, 6), fragment(532, 4)];
        let frames = split_frames(2, &[0, 32], &fragments, None);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].len(), 2);
        assert_eq!(
            frames[1],
            vec![Range {
                offset: 540,
                length: 4
            }]
        );

        let layout = PixelLayout::Encapsulated { frames };
        assert_eq!(PixelLayout::decode(&layout.encode()), Some(layout));
    }

    #[test]
    fn unattributable_fragments_give_no_frames() {
        let fragments = [fragment(500, 10), fragment(518, 6), fragment(532, 4)];
        assert!(split_frames(2, &[], &fragments, None).is_empty());
    }

    #[test]
    fn markers_split_jpeg_fragments() {
        let r = |offset| Range { offset, length: 1 };
        let frames = split_on_markers(
            2,
            &[
                (r(1), [0xFF, 0xD8, 0, 0]),
                (r(2), [1, 2, 3, 4]),
                (r(3), [0xFF, 0xD8, 0xFF, 0]),
            ],
        );
        assert_eq!(frames, vec![vec![r(1), r(2)], vec![r(3)]]);
    }
}
