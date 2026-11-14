//! Pixel conversions done here rather than by dicom-pixeldata, which does not support them:
//! big endian samples, native YBR_FULL_422 frames, RLE Lossless with more than 16 bits per
//! sample, 1-bit frames that do not start on a byte, and rendering of 32-bit integer and
//! floating point samples and of PALETTE COLOR.

use dicom_core::{PrimitiveValue, Tag};
use dicom_dictionary_std::tags;
use dicom_object::DefaultDicomObject;
use image::{DynamicImage, GrayImage, RgbImage};

/// Number of bytes to reverse in Explicit VR Big Endian pixel data to get little endian
/// samples: each sample when wider than 8 bits, each 16-bit word of OW data otherwise (PS3.5
/// 7.3: OW values are sequences of 16-bit words). 1 when nothing is to be swapped.
pub fn big_endian_unit(bits_allocated: u16, ow: bool) -> usize {
    match bits_allocated {
        8 if ow => 2,
        bits if bits > 8 && bits % 8 == 0 => usize::from(bits / 8),
        _ => 1,
    }
}

/// Reverse the bytes of each `unit`-byte word (a trailing partial word is left as is).
pub fn swap_bytes(data: &mut [u8], unit: usize) {
    if unit > 1 {
        for word in data.chunks_exact_mut(unit) {
            word.reverse();
        }
    }
}

/// A native YBR_FULL_422 frame (Y1 Y2 Cb Cr for each pair of pixels) as YBR_FULL (Y Cb Cr for
/// each pixel), with samples of `sample_bytes` bytes.
pub fn upsample_422(data: &[u8], sample_bytes: usize) -> Vec<u8> {
    let s = sample_bytes;
    let mut out = Vec::with_capacity(data.len() / 4 * 6);
    for pair in data.chunks_exact(4 * s) {
        let (y1, y2, cb, cr) = (
            &pair[..s],
            &pair[s..2 * s],
            &pair[2 * s..3 * s],
            &pair[3 * s..],
        );
        for y in [y1, y2] {
            out.extend_from_slice(y);
            out.extend_from_slice(cb);
            out.extend_from_slice(cr);
        }
    }
    out
}

/// Convert 8-bit YBR_FULL samples to RGB in place (PS3.3 C.7.6.3.1.2), pixel-interleaved or,
/// when `planar`, as three planes.
pub fn ybr_full_to_rgb(data: &mut [u8], planar: bool) {
    let convert = |y: u8, cb: u8, cr: u8| {
        let (y, cb, cr) = (f32::from(y), f32::from(cb) - 128.0, f32::from(cr) - 128.0);
        let channel = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        [
            channel(y + 1.402 * cr),
            channel(y - 0.344_136 * cb - 0.714_136 * cr),
            channel(y + 1.772 * cb),
        ]
    };
    if planar {
        let plane = data.len() / 3;
        for i in 0..plane {
            let rgb = convert(data[i], data[plane + i], data[2 * plane + i]);
            (data[i], data[plane + i], data[2 * plane + i]) = (rgb[0], rgb[1], rgb[2]);
        }
    } else {
        for pixel in data.chunks_exact_mut(3) {
            let rgb = convert(pixel[0], pixel[1], pixel[2]);
            pixel.copy_from_slice(&rgb);
        }
    }
}

/// Decode one RLE Lossless frame (PS3.5 Annex G) to little endian samples, pixel-interleaved
/// (planar configuration 0), for any number of bytes per sample.
pub fn decode_rle(
    frame: &[u8],
    pixels: usize,
    samples_per_pixel: usize,
    sample_bytes: usize,
) -> Result<Vec<u8>, String> {
    let word = |i: usize| -> Option<usize> {
        let bytes = frame.get(i * 4..i * 4 + 4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?) as usize)
    };
    let segments = word(0).ok_or("RLE header missing")?;
    if segments != samples_per_pixel * sample_bytes || segments > 15 {
        return Err(format!(
            "RLE frame has {segments} segments, {} expected",
            samples_per_pixel * sample_bytes
        ));
    }
    let stride = samples_per_pixel * sample_bytes;
    let mut out = vec![0; pixels * stride];
    for segment in 0..segments {
        let start = word(segment + 1).ok_or("RLE header truncated")?;
        let end = if segment + 1 < segments {
            word(segment + 2).ok_or("RLE header truncated")?
        } else {
            frame.len()
        };
        let data = frame
            .get(start..end.max(start))
            .ok_or("RLE segment offset out of range")?;
        // Segments hold the most significant bytes of each sample first.
        let sample = segment / sample_bytes;
        let byte = sample_bytes - 1 - segment % sample_bytes;
        let mut targets = (sample * sample_bytes + byte..out.len()).step_by(stride);
        let mut put = |value: u8| match targets.next() {
            Some(i) => {
                out[i] = value;
                true
            }
            None => false,
        };
        let mut i = 0;
        'segment: while i < data.len() {
            let header = data[i] as i8;
            i += 1;
            match header {
                0.. => {
                    let n = header as usize + 1;
                    let literal = data.get(i..i + n).ok_or("RLE literal run truncated")?;
                    for &value in literal {
                        if !put(value) {
                            break 'segment;
                        }
                    }
                    i += n;
                }
                -127..=-1 => {
                    let value = *data.get(i).ok_or("RLE replicate run truncated")?;
                    for _ in 0..(1 - isize::from(header)) {
                        if !put(value) {
                            break 'segment;
                        }
                    }
                    i += 1;
                }
                -128 => {}
            }
        }
    }
    Ok(out)
}

/// How the samples of a frame are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Integers of `bits_allocated` bits holding `bits_stored` significant bits.
    Integer {
        bits_allocated: u16,
        bits_stored: u16,
        signed: bool,
    },
    Float,
    Double,
}

impl SampleFormat {
    pub fn of(obj: &DefaultDicomObject) -> SampleFormat {
        if obj
            .element_opt(tags::FLOAT_PIXEL_DATA)
            .ok()
            .flatten()
            .is_some()
        {
            return SampleFormat::Float;
        }
        if obj
            .element_opt(tags::DOUBLE_FLOAT_PIXEL_DATA)
            .ok()
            .flatten()
            .is_some()
        {
            return SampleFormat::Double;
        }
        let bits_allocated = int(obj, tags::BITS_ALLOCATED).unwrap_or(8) as u16;
        SampleFormat::Integer {
            bits_allocated,
            bits_stored: int(obj, tags::BITS_STORED)
                .map(|b| b as u16)
                .filter(|b| (1..=bits_allocated).contains(b))
                .unwrap_or(bits_allocated),
            signed: int(obj, tags::PIXEL_REPRESENTATION) == Some(1),
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            SampleFormat::Integer { bits_allocated, .. } => usize::from(bits_allocated).div_ceil(8),
            SampleFormat::Float => 4,
            SampleFormat::Double => 8,
        }
    }

    /// Whether dicom-pixeldata can make an image of these samples.
    pub fn is_imageable(self) -> bool {
        matches!(self, SampleFormat::Integer { bits_allocated, .. } if bits_allocated <= 16)
    }

    /// The smallest and largest finite little endian samples of `data`.
    fn range(self, data: &[u8]) -> Option<(f64, f64)> {
        let finite = |values: &mut dyn Iterator<Item = f64>| {
            let (low, high) = values
                .filter(|v| v.is_finite())
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                    (lo.min(v), hi.max(v))
                });
            (low <= high).then_some((low, high))
        };
        match self {
            SampleFormat::Float => finite(
                &mut data
                    .chunks_exact(4)
                    .map(|b| f64::from(f32::from_le_bytes(b.try_into().unwrap_or_default()))),
            ),
            SampleFormat::Double => finite(
                &mut data
                    .chunks_exact(8)
                    .map(|b| f64::from_le_bytes(b.try_into().unwrap_or_default())),
            ),
            SampleFormat::Integer {
                bits_allocated,
                bits_stored,
                signed,
            } => {
                let unused = 64 - u32::from(bits_stored);
                match usize::from(bits_allocated).div_ceil(8).clamp(1, 8) {
                    1 => integer_range::<1>(data, unused, signed),
                    2 => integer_range::<2>(data, unused, signed),
                    3 => integer_range::<3>(data, unused, signed),
                    4 => integer_range::<4>(data, unused, signed),
                    5 => integer_range::<5>(data, unused, signed),
                    6 => integer_range::<6>(data, unused, signed),
                    7 => integer_range::<7>(data, unused, signed),
                    _ => integer_range::<8>(data, unused, signed),
                }
            }
        }
    }

    /// The little endian samples of `data`.
    fn values(self, data: &[u8]) -> Vec<f64> {
        match self {
            SampleFormat::Float => data
                .chunks_exact(4)
                .map(|b| f64::from(f32::from_le_bytes(b.try_into().unwrap_or_default())))
                .collect(),
            SampleFormat::Double => data
                .chunks_exact(8)
                .map(|b| f64::from_le_bytes(b.try_into().unwrap_or_default()))
                .collect(),
            SampleFormat::Integer {
                bits_allocated,
                bits_stored,
                signed,
            } => {
                let size = usize::from(bits_allocated).div_ceil(8).clamp(1, 8);
                let unused = 64 - u32::from(bits_stored);
                data.chunks_exact(size)
                    .map(|b| {
                        let mut word = [0; 8];
                        word[..size].copy_from_slice(b);
                        // Keep the stored bits (the low ones), sign extended when signed.
                        let raw = u64::from_le_bytes(word) << unused;
                        if signed {
                            ((raw as i64) >> unused) as f64
                        } else {
                            (raw >> unused) as f64
                        }
                    })
                    .collect()
            }
        }
    }
}

/// The smallest and largest of the little endian `SIZE`-byte samples of `data`, of which the
/// low `64 - unused` bits are stored.
fn integer_range<const SIZE: usize>(data: &[u8], unused: u32, signed: bool) -> Option<(f64, f64)> {
    // The stored bits moved to the top of the word compare like the samples (as signed words
    // for signed samples), so the loop is a plain integer minimum and maximum.
    let words = data.chunks_exact(SIZE).map(|b| {
        let mut word = [0; 8];
        word[..SIZE].copy_from_slice(b);
        u64::from_le_bytes(word) << unused
    });
    if signed {
        let (low, high) = words
            .map(|w| w as i64)
            .fold((i64::MAX, i64::MIN), |(lo, hi), w| (lo.min(w), hi.max(w)));
        (low <= high).then(|| ((low >> unused) as f64, (high >> unused) as f64))
    } else {
        let (low, high) = words.fold((u64::MAX, u64::MIN), |(lo, hi), w| (lo.min(w), hi.max(w)));
        (low <= high).then(|| ((low >> unused) as f64, (high >> unused) as f64))
    }
}

/// An 8-bit image of a native little endian frame whose samples dicom-pixeldata cannot make an
/// image of: monochrome samples go through the modality rescale and the window (`window`,
/// else the instance's first one, else the range of values); RGB samples are scaled from
/// their stored bits.
pub fn wide_image(
    obj: &DefaultDicomObject,
    format: SampleFormat,
    data: &[u8],
    window: Option<(f64, f64)>,
) -> Result<DynamicImage, String> {
    let rows = int(obj, tags::ROWS).ok_or("missing Rows")?;
    let columns = int(obj, tags::COLUMNS).ok_or("missing Columns")?;
    let samples_per_pixel = int(obj, tags::SAMPLES_PER_PIXEL).unwrap_or(1);
    let photometric = text(obj, tags::PHOTOMETRIC_INTERPRETATION).unwrap_or_default();
    let pixels = rows as usize * columns as usize;
    let values = format.values(data);
    if values.len() < pixels * samples_per_pixel as usize {
        return Err(format!(
            "frame has {} samples, {} expected",
            values.len(),
            pixels * samples_per_pixel as usize
        ));
    }
    match (samples_per_pixel, photometric.as_str()) {
        (1, "MONOCHROME1" | "MONOCHROME2") => {
            let values = modality_values(obj, format, &values[..pixels]);
            let (low, high) = window
                .or_else(|| stored_window(obj))
                .map(|(center, width)| (center - width / 2.0, center + width / 2.0))
                .unwrap_or_else(|| range(&values));
            let invert = photometric == "MONOCHROME1";
            let scale = if high > low {
                255.0 / (high - low)
            } else {
                0.0
            };
            let luma = values
                .iter()
                .map(|v| {
                    let y = if v.is_nan() {
                        0.0
                    } else {
                        ((v - low) * scale).clamp(0.0, 255.0)
                    };
                    (if invert { 255.0 - y } else { y }).round() as u8
                })
                .collect();
            GrayImage::from_raw(columns, rows, luma)
                .map(DynamicImage::ImageLuma8)
                .ok_or_else(|| "invalid image size".into())
        }
        (3, "RGB") => {
            let max = match format {
                SampleFormat::Integer { bits_stored, .. } => {
                    2f64.powi(i32::from(bits_stored)) - 1.0
                }
                _ => 1.0,
            };
            let planar = int(obj, tags::PLANAR_CONFIGURATION) == Some(1);
            let sample = |pixel: usize, channel: usize| {
                let index = if planar {
                    channel * pixels + pixel
                } else {
                    pixel * 3 + channel
                };
                (values[index] / max * 255.0).clamp(0.0, 255.0).round() as u8
            };
            let rgb = (0..pixels)
                .flat_map(|p| [sample(p, 0), sample(p, 1), sample(p, 2)])
                .collect();
            RgbImage::from_raw(columns, rows, rgb)
                .map(DynamicImage::ImageRgb8)
                .ok_or_else(|| "invalid image size".into())
        }
        (spp, photometric) => Err(format!(
            "cannot render {photometric} pixel data with {spp} samples per pixel and {format:?}"
        )),
    }
}

/// Monochrome `values` through the modality rescale (not applied to float samples, which are
/// real world values).
fn modality_values(obj: &DefaultDicomObject, format: SampleFormat, values: &[f64]) -> Vec<f64> {
    let (slope, intercept) = rescale(obj, format);
    values.iter().map(|v| v * slope + intercept).collect()
}

/// The modality rescale (slope, intercept) of `format` samples.
fn rescale(obj: &DefaultDicomObject, format: SampleFormat) -> (f64, f64) {
    match format {
        SampleFormat::Integer { .. } => (
            float(obj, tags::RESCALE_SLOPE).unwrap_or(1.0),
            float(obj, tags::RESCALE_INTERCEPT).unwrap_or(0.0),
        ),
        _ => (1.0, 0.0),
    }
}

/// The first window (center, width) of the instance.
fn stored_window(obj: &DefaultDicomObject) -> Option<(f64, f64)> {
    let center = float(obj, tags::WINDOW_CENTER)?;
    let width = float(obj, tags::WINDOW_WIDTH).filter(|w| *w > 0.0)?;
    Some((center, width))
}

/// The window (center, width) spanning the values of a monochrome frame (native little endian
/// `data`), for instances without VOI transformation: dicom-pixeldata's own fallback takes
/// signed samples for unsigned ones.
pub fn auto_window(
    obj: &DefaultDicomObject,
    format: SampleFormat,
    data: &[u8],
) -> Option<(f64, f64)> {
    let monochrome = text(obj, tags::PHOTOMETRIC_INTERPRETATION)
        .is_some_and(|p| p == "MONOCHROME1" || p == "MONOCHROME2");
    let has_voi = stored_window(obj).is_some()
        || obj
            .element_opt(tags::VOILUT_SEQUENCE)
            .ok()
            .flatten()
            .is_some();
    if !monochrome || has_voi {
        return None;
    }
    // The rescale is monotonic: the range of rescaled values is that of the samples, rescaled
    // (without making a rescaled copy of the frame, which took most of the rendering time).
    let (low, high) = format.range(data)?;
    let (slope, intercept) = rescale(obj, format);
    let (a, b) = (low * slope + intercept, high * slope + intercept);
    let (low, high) = (a.min(b), a.max(b));
    (low <= high).then(|| ((low + high) / 2.0, (high - low).max(1.0)))
}

/// Smallest and largest finite values.
fn range(values: &[f64]) -> (f64, f64) {
    values
        .iter()
        .filter(|v| v.is_finite())
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        })
}

/// `count` samples of 1 bit (0 or 1) from bit `start` of `data`. The bits of 1-bit pixel data
/// follow each other from the lowest bit of each byte, frames included (PS3.5 8.1.1): a frame
/// whose number of samples is not a multiple of 8 does not end on a byte.
pub fn unpack_bits(data: &[u8], start: usize, count: usize) -> Option<Vec<u8>> {
    if (start + count).div_ceil(8) > data.len() {
        return None;
    }
    Some(
        (start..start + count)
            .map(|i| (data[i / 8] >> (i % 8)) & 1)
            .collect(),
    )
}

/// Samples of 0 and 1 packed in bits, the first in the lowest bit of the first byte.
pub fn pack_bits(samples: &[u8]) -> Vec<u8> {
    samples
        .chunks(8)
        .map(|byte| {
            byte.iter()
                .enumerate()
                .fold(0, |acc, (bit, &sample)| acc | (sample & 1) << bit)
        })
        .collect()
}

/// An 8-bit image of a native little endian frame of 1-bit samples (`data`, from its first bit).
pub fn one_bit_image(obj: &DefaultDicomObject, data: &[u8]) -> Result<DynamicImage, String> {
    let rows = int(obj, tags::ROWS).ok_or("missing Rows")?;
    let columns = int(obj, tags::COLUMNS).ok_or("missing Columns")?;
    if int(obj, tags::SAMPLES_PER_PIXEL).unwrap_or(1) != 1 {
        return Err("1-bit samples need 1 sample per pixel".into());
    }
    let pixels = rows as usize * columns as usize;
    let samples = unpack_bits(data, 0, pixels).ok_or_else(|| {
        format!(
            "frame has {} bytes, {} expected",
            data.len(),
            pixels.div_ceil(8)
        )
    })?;
    let on = if text(obj, tags::PHOTOMETRIC_INTERPRETATION).as_deref() == Some("MONOCHROME1") {
        0
    } else {
        255
    };
    let luma = samples
        .into_iter()
        .map(|s| if s == 1 { on } else { 255 - on })
        .collect();
    GrayImage::from_raw(columns, rows, luma)
        .map(DynamicImage::ImageLuma8)
        .ok_or_else(|| "invalid frame size".into())
}

/// The colors of the palette of a PALETTE COLOR instance (PS3.3 C.7.6.3.1.5 and C.7.6.3.1.6),
/// in 8 bits, and the stored value of its first entry. Segmented palettes are not supported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub colors: Vec<[u8; 3]>,
    pub first: i64,
}

impl Palette {
    pub fn of(obj: &DefaultDicomObject) -> Result<Palette, String> {
        let (red, first) = palette_channel(
            obj,
            tags::RED_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
            tags::RED_PALETTE_COLOR_LOOKUP_TABLE_DATA,
            "Red",
        )?;
        let (green, _) = palette_channel(
            obj,
            tags::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
            tags::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DATA,
            "Green",
        )?;
        let (blue, _) = palette_channel(
            obj,
            tags::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
            tags::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DATA,
            "Blue",
        )?;
        if green.len() != red.len() || blue.len() != red.len() {
            return Err("the red, green and blue palettes have different lengths".into());
        }
        let colors = red
            .into_iter()
            .zip(green)
            .zip(blue)
            .map(|((r, g), b)| [r, g, b])
            .collect();
        Ok(Palette { colors, first })
    }

    /// The RGB image of a native little endian frame of indices (`data`); the values out of
    /// the palette take its first or last entry.
    pub fn image(
        &self,
        obj: &DefaultDicomObject,
        format: SampleFormat,
        data: &[u8],
    ) -> Result<DynamicImage, String> {
        let rows = int(obj, tags::ROWS).ok_or("missing Rows")?;
        let columns = int(obj, tags::COLUMNS).ok_or("missing Columns")?;
        let SampleFormat::Integer {
            bits_allocated: 8 | 16,
            ..
        } = format
        else {
            return Err("PALETTE COLOR needs 8 or 16 bits allocated".into());
        };
        let pixels = rows as usize * columns as usize;
        let values = format.values(data);
        if values.len() < pixels {
            return Err(format!(
                "frame has {} samples, {pixels} expected",
                values.len()
            ));
        }
        let last = self.colors.len() as i64 - 1;
        let rgb = values[..pixels]
            .iter()
            .flat_map(|&v| self.colors[(v as i64 - self.first).clamp(0, last) as usize])
            .collect();
        RgbImage::from_raw(columns, rows, rgb)
            .map(DynamicImage::ImageRgb8)
            .ok_or_else(|| "invalid frame size".into())
    }
}

/// The entries of one palette in 8 bits, and the stored value of its first entry.
fn palette_channel(
    obj: &DefaultDicomObject,
    descriptor_tag: Tag,
    data_tag: Tag,
    name: &str,
) -> Result<(Vec<u8>, i64), String> {
    let descriptor: Vec<i64> = obj
        .element_opt(descriptor_tag)
        .ok()
        .flatten()
        .and_then(|e| e.to_multi_int::<i64>().ok())
        .ok_or_else(|| format!("no {name} Palette Color Lookup Table Descriptor"))?;
    let [entries, first, bits] = descriptor[..] else {
        return Err(format!(
            "invalid {name} Palette Color Lookup Table Descriptor: {descriptor:?}"
        ));
    };
    // 0 entries means 65536; the first stored value mapped is unsigned with US, signed with SS.
    let entries = if entries == 0 {
        65536
    } else {
        entries as u16 as usize
    };
    let first = if first < 0 { first } else { first & 0xFFFF };
    let element = obj.element_opt(data_tag).ok().flatten().ok_or_else(|| {
        format!("no {name} Palette Color Lookup Table Data (segmented palettes are not supported)")
    })?;
    let words: Vec<u16> = match element.value().primitive() {
        Some(PrimitiveValue::U16(words)) => words.to_vec(),
        // 8-bit entries in bytes.
        Some(PrimitiveValue::U8(bytes)) if bits == 8 && bytes.len() == entries => {
            bytes.iter().map(|&b| u16::from(b)).collect()
        }
        Some(PrimitiveValue::U8(bytes)) => bytes
            .chunks_exact(2)
            .map(|w| u16::from_le_bytes([w[0], w[1]]))
            .collect(),
        _ => return Err(format!("invalid {name} Palette Color Lookup Table Data")),
    };
    if words.len() < entries {
        return Err(format!(
            "{} entries in the {name} Palette Color Lookup Table Data, {entries} expected",
            words.len()
        ));
    }
    // 8-bit entries are in the low byte of 16-bit words (or, by some writers, in the high one).
    let high = bits == 16 || words[..entries].iter().any(|&w| w > 255);
    Ok((
        words[..entries]
            .iter()
            .map(|&w| if high { (w >> 8) as u8 } else { w as u8 })
            .collect(),
        first,
    ))
}

fn int(obj: &DefaultDicomObject, tag: Tag) -> Option<u32> {
    obj.element_opt(tag).ok().flatten()?.to_int::<u32>().ok()
}

fn float(obj: &DefaultDicomObject, tag: Tag) -> Option<f64> {
    // The first value of multi-valued attributes (Window Center and Width).
    obj.element_opt(tag)
        .ok()
        .flatten()?
        .to_multi_float64()
        .ok()?
        .first()
        .copied()
        .filter(|v| v.is_finite())
}

fn text(obj: &DefaultDicomObject, tag: Tag) -> Option<String> {
    Some(
        obj.element_opt(tag)
            .ok()
            .flatten()?
            .to_str()
            .ok()?
            .trim_matches([' ', '\0'])
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_endian_units() {
        assert_eq!(big_endian_unit(8, false), 1);
        assert_eq!(big_endian_unit(8, true), 2);
        assert_eq!(big_endian_unit(16, false), 2);
        assert_eq!(big_endian_unit(32, true), 4);
        assert_eq!(big_endian_unit(1, true), 1);
        let mut data = vec![1, 2, 3, 4, 5];
        swap_bytes(&mut data, 2);
        assert_eq!(data, [2, 1, 4, 3, 5]);
    }

    #[test]
    fn upsamples_422() {
        assert_eq!(
            upsample_422(&[10, 20, 1, 2, 30, 40, 3, 4], 1),
            [10, 1, 2, 20, 1, 2, 30, 3, 4, 40, 3, 4]
        );
    }

    #[test]
    fn converts_ybr_full() {
        // Pure red, green and blue, as written by pydicom's RGB to YBR_FULL conversion.
        let mut data = vec![76, 85, 255, 150, 44, 21, 29, 255, 107];
        ybr_full_to_rgb(&mut data, false);
        assert_eq!(data, [254, 0, 0, 0, 255, 1, 0, 0, 254]);
        let mut planar = vec![76, 150, 85, 44, 255, 21];
        ybr_full_to_rgb(&mut planar, true);
        assert_eq!(planar[0], 254);
    }

    /// RLE frame of `segments` (each already PackBits encoded).
    fn rle(segments: &[&[u8]]) -> Vec<u8> {
        let mut header = [0u32; 16];
        header[0] = segments.len() as u32;
        let mut offset = 64;
        for (i, segment) in segments.iter().enumerate() {
            header[i + 1] = offset;
            offset += segment.len() as u32;
        }
        let mut out: Vec<u8> = header.iter().flat_map(|w| w.to_le_bytes()).collect();
        for segment in segments {
            out.extend_from_slice(segment);
        }
        out
    }

    #[test]
    fn decodes_32_bit_rle() {
        // Two 32-bit pixels, 0x00020304 and 0x000B0C0D: one segment per byte, MSB first.
        // The first segment is a replicate run, the others literal runs.
        let frame = rle(&[&[0xFF, 0x00], &[1, 2, 0x0B], &[1, 3, 0x0C], &[1, 4, 0x0D]]);
        let decoded = decode_rle(&frame, 2, 1, 4).expect("decodable");
        assert_eq!(decoded, [4, 3, 2, 0, 0x0D, 0x0C, 0x0B, 0]);
        assert!(decode_rle(&frame, 2, 1, 2).is_err());
        assert!(decode_rle(&frame[..70], 2, 1, 4).is_err());
    }

    #[test]
    fn integer_samples() {
        let unsigned = SampleFormat::Integer {
            bits_allocated: 32,
            bits_stored: 32,
            signed: false,
        };
        assert_eq!(
            unsigned.values(&u32::MAX.to_le_bytes()),
            [f64::from(u32::MAX)]
        );
        let signed12 = SampleFormat::Integer {
            bits_allocated: 16,
            bits_stored: 12,
            signed: true,
        };
        // 0xF800: high bits are not stored; 0x800 is -2048 in 12 bits.
        assert_eq!(signed12.values(&[0x00, 0xF8]), [-2048.0]);
    }

    #[test]
    fn sample_ranges() {
        // Pseudo-random bytes, with garbage in the bits that are not stored.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        for bits_allocated in [1, 8, 12, 16, 24, 32, 64] {
            for bits_stored in [1, 7, 8, 12, 15, 16, 31, 32, 63, 64] {
                for signed in [false, true] {
                    if bits_stored > bits_allocated {
                        continue;
                    }
                    let format = SampleFormat::Integer {
                        bits_allocated,
                        bits_stored,
                        signed,
                    };
                    let expected = range(&format.values(&data));
                    assert_eq!(format.range(&data), Some(expected), "{format:?}");
                }
            }
        }
        let floats: Vec<u8> = [1.5f32, f32::NAN, -3.0, f32::INFINITY, 2.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(SampleFormat::Float.range(&floats), Some((-3.0, 2.0)));
        let doubles: Vec<u8> = [f64::NEG_INFINITY, 0.25, 8.0]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(SampleFormat::Double.range(&doubles), Some((0.25, 8.0)));
        assert_eq!(SampleFormat::Double.range(&f64::NAN.to_le_bytes()), None);
        assert_eq!(SampleFormat::of(&monochrome(&[])).range(&[]), None);
    }

    fn monochrome(elements: &[(Tag, dicom_core::VR, &str)]) -> DefaultDicomObject {
        use dicom_core::{DataElement, PrimitiveValue, VR};
        use dicom_object::{FileMetaTableBuilder, InMemDicomObject};
        let mut obj = InMemDicomObject::new_empty();
        let mut put = |tag, vr, value: &str| {
            obj.put(DataElement::new(tag, vr, PrimitiveValue::from(value)));
        };
        put(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
        put(tags::BITS_ALLOCATED, VR::US, "16");
        put(tags::BITS_STORED, VR::US, "12");
        put(tags::PIXEL_REPRESENTATION, VR::US, "1");
        for (tag, vr, value) in elements {
            put(*tag, *vr, value);
        }
        obj.with_meta(
            FileMetaTableBuilder::new()
                .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.2")
                .media_storage_sop_instance_uid("1.2.3")
                .transfer_syntax("1.2.840.10008.1.2.1"),
        )
        .expect("valid meta")
    }

    #[test]
    fn one_bit_samples() {
        // The second sample of 9 starts at bit 9.
        let samples = [1, 0, 1, 1, 0, 0, 0, 0, 1, 0, 1, 1, 1, 0, 1, 0, 0, 1];
        let packed = pack_bits(&samples);
        assert_eq!(packed, [0b0000_1101, 0b0101_1101, 0b0000_0010]);
        assert_eq!(unpack_bits(&packed, 0, 18).as_deref(), Some(&samples[..]));
        assert_eq!(unpack_bits(&packed, 9, 9).as_deref(), Some(&samples[9..]));
        assert_eq!(unpack_bits(&packed, 9, 16), None);
    }

    #[test]
    fn palettes() {
        use dicom_core::{DataElement, PrimitiveValue, VR};
        let mut obj = monochrome(&[]);
        // 8-bit entries in 16-bit words, from the stored value 10.
        for (descriptor, data, scale) in [
            (
                tags::RED_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
                tags::RED_PALETTE_COLOR_LOOKUP_TABLE_DATA,
                1,
            ),
            (
                tags::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
                tags::GREEN_PALETTE_COLOR_LOOKUP_TABLE_DATA,
                2,
            ),
            (
                tags::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DESCRIPTOR,
                tags::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DATA,
                3,
            ),
        ] {
            let descriptor_value = PrimitiveValue::U16(vec![4, 10, 8].into());
            obj.put(DataElement::new(descriptor, VR::US, descriptor_value));
            let entries = PrimitiveValue::U16((0..4).map(|i| i * scale * 10).collect());
            obj.put(DataElement::new(data, VR::OW, entries));
        }
        let palette = Palette::of(&obj).expect("palette");
        assert_eq!(palette.first, 10);
        assert_eq!(
            palette.colors,
            [[0, 0, 0], [10, 20, 30], [20, 40, 60], [30, 60, 90]]
        );
        obj.put(DataElement::new(
            tags::ROWS,
            VR::US,
            PrimitiveValue::from(1_u16),
        ));
        obj.put(DataElement::new(
            tags::COLUMNS,
            VR::US,
            PrimitiveValue::from(3_u16),
        ));
        let format = SampleFormat::Integer {
            bits_allocated: 8,
            bits_stored: 8,
            signed: false,
        };
        // Below, in and above the palette.
        let image = palette.image(&obj, format, &[2, 11, 200]).expect("image");
        assert_eq!(
            image.to_rgb8().into_raw(),
            [0, 0, 0, 10, 20, 30, 30, 60, 90]
        );
        obj.remove_element(tags::BLUE_PALETTE_COLOR_LOOKUP_TABLE_DATA);
        let error = Palette::of(&obj).expect_err("segmented");
        assert!(error.contains("segmented"), "{error}");
    }

    #[test]
    fn auto_windows() {
        use dicom_core::VR;
        // -2048, 100 and 2047 in 12 bits.
        let data = [0x00, 0x08, 0x64, 0x00, 0xFF, 0x07];
        let window_of = |elements: &[(Tag, VR, &str)], data: &[u8]| {
            let obj = monochrome(elements);
            auto_window(&obj, SampleFormat::of(&obj), data)
        };
        let window = |elements: &[(Tag, VR, &str)]| window_of(elements, &data);
        assert_eq!(window(&[]), Some((-0.5, 4095.0)));
        assert_eq!(
            window(&[
                (tags::RESCALE_SLOPE, VR::DS, "-2"),
                (tags::RESCALE_INTERCEPT, VR::DS, "10"),
            ]),
            // From 4106 (-2048) down to -4084 (2047).
            Some((11.0, 8190.0))
        );
        // Flat frames get a width of 1; empty ones no window.
        assert_eq!(window_of(&[], &data[2..4]), Some((100.0, 1.0)));
        assert_eq!(window_of(&[], &[]), None);
        assert_eq!(
            window(&[
                (tags::WINDOW_CENTER, VR::DS, "40"),
                (tags::WINDOW_WIDTH, VR::DS, "400")
            ]),
            None
        );
        assert_eq!(
            window(&[(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "RGB")]),
            None
        );
    }
}
