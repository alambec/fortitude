//! Pixel data decoding: rendering frames as JPEG or PNG, decoding frames to native pixels,
//! and transcoding instances.
//!
//! A frame is decoded from a one-frame object built from the stored header and that frame's
//! bytes, read at the offsets recorded at ingest; only when frames cannot be located
//! (deflated data sets, unattributable fragments) is the whole file read. Files are read
//! asynchronously from the file store, then decoded on the blocking thread pool.

use std::io::Cursor;
use std::sync::Arc;

use dicom_core::value::{PixelFragmentSequence, Value};
use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::tags;
use dicom_encoding::TransferSyntaxIndex;
use dicom_object::DefaultDicomObject;
use dicom_pixeldata::{
    BitDepthOption, ConvertOptions, PixelDecoder, Transcode, VoiLutOption, WindowLevel,
};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_transfer_syntax_registry::entries::{
    DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN, EXPLICIT_VR_BIG_ENDIAN, EXPLICIT_VR_LITTLE_ENDIAN,
    IMPLICIT_VR_LITTLE_ENDIAN, RLE_LOSSLESS,
};
use image::DynamicImage;
use image::imageops::FilterType;

use crate::Store;
use crate::convert::{self, SampleFormat};
use crate::index::IndexError;
use crate::ingest::resolve_unknown_vrs;
use crate::pixel::{PixelLayout, Range, Sample};
use crate::retrieve::{PixelSource, Scope};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg { quality: u8 },
    Png,
}

impl ImageFormat {
    pub fn media_type(self) -> &'static str {
        match self {
            ImageFormat::Jpeg { .. } => "image/jpeg",
            ImageFormat::Png => "image/png",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderOptions {
    /// 0-based frame.
    pub frame: usize,
    /// Window center and width; the instance's own VOI transformation when absent.
    pub window: Option<(f64, f64)>,
    /// Maximum width and height; the aspect ratio is kept.
    pub viewport: Option<(u32, u32)>,
    pub format: ImageFormat,
}

/// What frames are decoded for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Render,
    /// Native samples, as described by the instance's attributes.
    Decode,
}

#[derive(Debug, thiserror::Error)]
pub enum ImagingError {
    #[error("not found")]
    NotFound,
    #[error("the instance has no pixel data")]
    NoPixelData,
    #[error("frame {0} does not exist")]
    NoFrame(usize),
    /// The pixel data cannot be decoded (codec not available) or encoded as requested.
    #[error("{0}")]
    Unsupported(String),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("{0}")]
    Failed(String),
}

impl Store {
    pub async fn render(
        self: &Arc<Self>,
        scope: Scope,
        options: RenderOptions,
    ) -> Result<Vec<u8>, ImagingError> {
        let source = self.pixels(scope).await?.ok_or(ImagingError::NotFound)?;
        let (obj, frame) = self
            .frame_objects(&source, &[options.frame], Purpose::Render)
            .await?
            .swap_remove(0);
        blocking(move || render(&obj, frame, options)).await
    }

    /// Frames (0-based) decoded to native little endian pixels.
    pub async fn decoded_frames(
        self: &Arc<Self>,
        scope: Scope,
        frames: Vec<usize>,
    ) -> Result<Vec<Vec<u8>>, ImagingError> {
        let source = self.pixels(scope).await?.ok_or(ImagingError::NotFound)?;
        let objects = self
            .frame_objects(&source, &frames, Purpose::Decode)
            .await?;
        blocking(move || {
            objects
                .iter()
                .map(|(obj, frame)| decode_frame(obj, *frame))
                .collect()
        })
        .await
    }

    /// The stored file at `path` in another transfer syntax, as a Part-10 stream.
    pub async fn transcoded(
        self: &Arc<Self>,
        path: &str,
        transfer_syntax: &str,
    ) -> Result<Vec<u8>, ImagingError> {
        let ts = TransferSyntaxRegistry.get(transfer_syntax).ok_or_else(|| {
            ImagingError::Unsupported(format!("unknown transfer syntax {transfer_syntax}"))
        })?;
        let obj = self.files().object(path).await.map_err(failed)?;
        blocking(move || transcode(obj, ts)).await
    }

    /// For each of `frames`, an object holding only that frame, as frame 0, or the whole
    /// object when frames cannot be located; with the index of the frame to decode in it.
    ///
    /// Native frames are made little endian, and for rendering, native YBR frames are
    /// converted to RGB.
    async fn frame_objects(
        &self,
        source: &PixelSource,
        frames: &[usize],
        purpose: Purpose,
    ) -> Result<Vec<(Arc<DefaultDicomObject>, u32)>, ImagingError> {
        let Some(layout) = source.layout.as_ref() else {
            // Pixel data of deflated data sets is not located at ingest: inflate the file.
            if source.transfer_syntax != DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.uid() {
                return Err(ImagingError::NoPixelData);
            }
            let mut obj = self.files().object(&source.path).await.map_err(failed)?;
            let pixel_data = [
                tags::PIXEL_DATA,
                tags::FLOAT_PIXEL_DATA,
                tags::DOUBLE_FLOAT_PIXEL_DATA,
            ]
            .into_iter()
            .any(|tag| obj.element_opt(tag).ok().flatten().is_some());
            if !pixel_data {
                return Err(ImagingError::NoPixelData);
            }
            resolve_unknown_vrs(&mut obj);
            let obj = Arc::new(obj);
            return Ok(frames.iter().map(|&f| (obj.clone(), f as u32)).collect());
        };
        if layout.frame_count() == 0 {
            let mut obj = self.files().object(&source.path).await.map_err(failed)?;
            resolve_unknown_vrs(&mut obj);
            let obj = Arc::new(obj);
            return Ok(frames.iter().map(|&f| (obj.clone(), f as u32)).collect());
        }
        let mut header = self
            .files()
            .header(&source.path, layout.header_length())
            .await
            .map_err(failed)?;
        resolve_unknown_vrs(&mut header);
        header.put_str(tags::NUMBER_OF_FRAMES, VR::IS, "1");
        // The offset tables describe all frames, not the one kept.
        header.remove_element(tags::EXTENDED_OFFSET_TABLE);
        header.remove_element(tags::EXTENDED_OFFSET_TABLE_LENGTHS);
        let bits_allocated = header
            .element_opt(tags::BITS_ALLOCATED)
            .ok()
            .flatten()
            .and_then(|e| e.to_int::<u16>().ok())
            .unwrap_or(8);
        let mut ranges: Vec<_> = frames
            .iter()
            .map(|&frame| layout.frame(frame).ok_or(ImagingError::NoFrame(frame)))
            .collect::<Result<_, _>>()?;
        let (tag, vr) = match layout {
            PixelLayout::Native {
                sample: Sample::Float,
                ..
            } => (tags::FLOAT_PIXEL_DATA, VR::OF),
            PixelLayout::Native {
                sample: Sample::Double,
                ..
            } => (tags::DOUBLE_FLOAT_PIXEL_DATA, VR::OD),
            _ if bits_allocated > 8 => (tags::PIXEL_DATA, VR::OW),
            _ => (tags::PIXEL_DATA, VR::OB),
        };
        let native = !layout.is_encapsulated();
        let value_offset = layout.header_length().unwrap_or(0);
        // Explicit VR Big Endian samples are swapped to little endian, by words aligned on
        // the start of the value; the VR, in the element header, tells whether 8-bit samples
        // are swapped (OW).
        let swap = if native && source.transfer_syntax == EXPLICIT_VR_BIG_ENDIAN.uid() {
            let stored_vr = self
                .files()
                .read_parts(
                    &source.path,
                    vec![vec![Range {
                        offset: value_offset.saturating_sub(8),
                        length: 2,
                    }]],
                )
                .await
                .map_err(failed)?;
            header
                .meta_mut()
                .set_transfer_syntax(&EXPLICIT_VR_LITTLE_ENDIAN);
            convert::big_endian_unit(bits_allocated, stored_vr[0] == b"OW")
        } else {
            1
        };
        // Each native frame (one range) is read from and to word boundaries: the bytes read
        // before it, and its length.
        let mut frame_bytes = Vec::with_capacity(ranges.len());
        for parts in &mut ranges {
            if let [range] = parts.as_mut_slice() {
                let lead = (range.offset - value_offset) % swap as u64;
                frame_bytes.push((lead as usize, range.length as usize));
                range.offset -= lead;
                range.length = (range.length + lead).next_multiple_of(swap as u64);
            }
        }
        // dicom-pixeldata supports neither native YBR_FULL_422 nor (correctly) the YBR_FULL
        // to RGB conversion: rendered frames are converted to RGB here.
        let photometric = header
            .element_opt(tags::PHOTOMETRIC_INTERPRETATION)
            .ok()
            .flatten()
            .and_then(|e| e.to_str().ok())
            .map(|p| p.trim_matches([' ', '\0']).to_owned())
            .unwrap_or_default();
        let to_rgb = native
            && purpose == Purpose::Render
            && bits_allocated == 8
            && matches!(photometric.as_str(), "YBR_FULL" | "YBR_FULL_422");
        let planar = header
            .element_opt(tags::PLANAR_CONFIGURATION)
            .ok()
            .flatten()
            .and_then(|e| e.to_int::<u16>().ok())
            == Some(1);
        if to_rgb {
            header.put_str(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "RGB");
            if photometric == "YBR_FULL_422" {
                header.put(DataElement::new(
                    tags::PLANAR_CONFIGURATION,
                    VR::US,
                    PrimitiveValue::from(0u16),
                ));
            }
        }
        let mut data = self
            .files()
            .read_parts(&source.path, ranges)
            .await
            .map_err(failed)?;
        if native {
            for (bytes, (lead, length)) in data.iter_mut().zip(frame_bytes) {
                convert::swap_bytes(bytes, swap);
                bytes.truncate(lead + length);
                bytes.drain(..lead);
                if to_rgb {
                    if photometric == "YBR_FULL_422" {
                        *bytes = convert::upsample_422(bytes, 1);
                    }
                    convert::ybr_full_to_rgb(bytes, planar && photometric == "YBR_FULL");
                }
            }
        }
        Ok(data
            .into_iter()
            .map(|bytes| {
                let element = if native {
                    DataElement::new(tag, vr, PrimitiveValue::from(bytes))
                } else {
                    DataElement::new(
                        tags::PIXEL_DATA,
                        VR::OB,
                        Value::PixelSequence(PixelFragmentSequence::new(
                            Vec::<u32>::new(),
                            vec![bytes],
                        )),
                    )
                };
                let mut obj = header.clone();
                obj.put(element);
                (Arc::new(obj), 0)
            })
            .collect())
    }

    /// The instance representing a scope in thumbnails: the middle instance of the series, or
    /// of the first series of the study.
    pub async fn representative(
        self: &Arc<Self>,
        scope: Scope,
    ) -> Result<Option<Scope>, IndexError> {
        if scope.instance.is_some() {
            return Ok(Some(scope));
        }
        let instances = self.instances(scope).await?;
        let Some(first) = instances.first() else {
            return Ok(None);
        };
        let series: Vec<_> = instances
            .iter()
            .filter(|i| i.series_uid == first.series_uid)
            .collect();
        let middle = series[series.len() / 2];
        Ok(Some(Scope::instance(
            &middle.study_uid,
            &middle.series_uid,
            &middle.sop_uid,
        )))
    }
}

/// Whether instances can be transcoded to `transfer_syntax`.
pub fn can_transcode_to(transfer_syntax: &str) -> bool {
    TransferSyntaxRegistry
        .get(transfer_syntax)
        .is_some_and(|ts| {
            !ts.is_unsupported() && (ts.is_codec_free() || ts.is_encapsulated_pixel_data())
        })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ImagingError> + Send + 'static,
) -> Result<T, ImagingError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ImagingError::Failed(format!("decoding task failed: {e}")))?
}

fn unsupported(e: impl std::fmt::Display) -> ImagingError {
    ImagingError::Unsupported(e.to_string())
}

fn failed(e: std::io::Error) -> ImagingError {
    ImagingError::Failed(e.to_string())
}

fn render(
    obj: &DefaultDicomObject,
    frame: u32,
    options: RenderOptions,
) -> Result<Vec<u8>, ImagingError> {
    let format = SampleFormat::of(obj);
    let mut image = if format.is_imageable() {
        let decoded = obj.decode_pixel_data_frame(frame).map_err(unsupported)?;
        let mut convert = ConvertOptions::new().with_bit_depth(BitDepthOption::Force8Bit);
        let window = options.window.or_else(|| {
            let data = decoded.frame_data(0).ok()?;
            convert::auto_window(obj, format, data)
        });
        if let Some((center, width)) = window {
            convert = convert.with_voi_lut(VoiLutOption::Custom(WindowLevel { center, width }));
        }
        decoded
            .to_dynamic_image_with_options(0, &convert)
            .map_err(unsupported)?
    } else {
        let data = decode_frame(obj, frame)?;
        convert::wide_image(obj, format, &data, options.window)
            .map_err(ImagingError::Unsupported)?
    };
    if let Some((width, height)) = options.viewport
        && (width < image.width() || height < image.height())
    {
        image = image.resize(width.max(1), height.max(1), FilterType::Triangle);
    }
    encode(image, options.format)
}

fn encode(image: DynamicImage, format: ImageFormat) -> Result<Vec<u8>, ImagingError> {
    let mut out = Vec::new();
    let image = match image {
        DynamicImage::ImageLuma8(_) | DynamicImage::ImageRgb8(_) => image,
        DynamicImage::ImageLuma16(_)
        | DynamicImage::ImageLumaA8(_)
        | DynamicImage::ImageLumaA16(_) => DynamicImage::ImageLuma8(image.to_luma8()),
        _ => DynamicImage::ImageRgb8(image.to_rgb8()),
    };
    match (format, &image) {
        // jpeg-encoder (SIMD) is about three times faster than the image crate's encoder.
        (ImageFormat::Jpeg { quality }, DynamicImage::ImageLuma8(pixels)) => {
            jpeg(
                &mut out,
                quality,
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                jpeg_encoder::ColorType::Luma,
            )?;
        }
        (ImageFormat::Jpeg { quality }, _) => {
            let pixels = image.to_rgb8();
            jpeg(
                &mut out,
                quality,
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                jpeg_encoder::ColorType::Rgb,
            )?;
        }
        (ImageFormat::Png, _) => {
            image
                .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
                .map_err(unsupported)?;
        }
    }
    Ok(out)
}

fn jpeg(
    out: &mut Vec<u8>,
    quality: u8,
    pixels: &[u8],
    width: u32,
    height: u32,
    color: jpeg_encoder::ColorType,
) -> Result<(), ImagingError> {
    let (width, height) = match (u16::try_from(width), u16::try_from(height)) {
        (Ok(w), Ok(h)) => (w, h),
        _ => return Err(ImagingError::Unsupported("image too large for JPEG".into())),
    };
    jpeg_encoder::Encoder::new(out, quality.clamp(1, 100))
        .encode(pixels, width, height, color)
        .map_err(unsupported)
}

/// Frame `frame` of `obj` as native little endian samples.
fn decode_frame(obj: &DefaultDicomObject, frame: u32) -> Result<Vec<u8>, ImagingError> {
    let format = SampleFormat::of(obj);
    let ts = obj.meta().transfer_syntax();
    let int = |tag| {
        obj.element_opt(tag)
            .ok()
            .flatten()
            .and_then(|e| e.to_int::<usize>().ok())
    };
    let pixels = int(tags::ROWS).unwrap_or(0) * int(tags::COLUMNS).unwrap_or(0);
    let samples_per_pixel = int(tags::SAMPLES_PER_PIXEL).unwrap_or(1);
    let subsampled = obj
        .element_opt(tags::PHOTOMETRIC_INTERPRETATION)
        .ok()
        .flatten()
        .and_then(|e| e.to_str().ok())
        .is_some_and(|p| p.trim_matches([' ', '\0']) == "YBR_FULL_422");
    let packed =
        matches!(format, SampleFormat::Integer { bits_allocated, .. } if bits_allocated % 8 != 0);
    if !packed && (ts == EXPLICIT_VR_LITTLE_ENDIAN.uid() || ts == IMPLICIT_VR_LITTLE_ENDIAN.uid()) {
        // Native samples are already little endian; also the only way to get float samples.
        let element = [
            tags::PIXEL_DATA,
            tags::FLOAT_PIXEL_DATA,
            tags::DOUBLE_FLOAT_PIXEL_DATA,
        ]
        .into_iter()
        .find_map(|tag| obj.element_opt(tag).ok().flatten())
        .ok_or(ImagingError::NoPixelData)?;
        let bytes = element.to_bytes().map_err(unsupported)?;
        // Native YBR_FULL_422 stores two samples per pixel.
        let samples = if subsampled { 2 } else { samples_per_pixel };
        let length = pixels * samples * format.bytes();
        let start = frame as usize * length;
        return bytes
            .get(start..start + length)
            .map(<[u8]>::to_vec)
            .ok_or(ImagingError::NoFrame(frame as usize));
    }
    if ts == RLE_LOSSLESS.uid() && format.bytes() > 2 {
        // dicom-pixeldata decodes RLE Lossless of up to 16 bits per sample.
        let fragment = obj
            .element_opt(tags::PIXEL_DATA)
            .ok()
            .flatten()
            .and_then(|e| e.value().fragments()?.get(frame as usize).cloned())
            .ok_or(ImagingError::NoFrame(frame as usize))?;
        return convert::decode_rle(&fragment, pixels, samples_per_pixel, format.bytes())
            .map_err(ImagingError::Unsupported);
    }
    let decoded = obj.decode_pixel_data_frame(frame).map_err(unsupported)?;
    Ok(decoded.frame_data(0).map_err(unsupported)?.to_vec())
}

fn transcode(
    mut obj: DefaultDicomObject,
    ts: &dicom_encoding::TransferSyntax,
) -> Result<Vec<u8>, ImagingError> {
    obj.transcode(ts).map_err(unsupported)?;
    let mut out = Vec::new();
    obj.write_all(&mut out).map_err(unsupported)?;
    Ok(out)
}
