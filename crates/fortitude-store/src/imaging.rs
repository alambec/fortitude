//! Pixel data decoding: rendering frames as JPEG or PNG, decoding frames to native pixels,
//! and transcoding instances.
//!
//! A frame is decoded from a one-frame object built from the stored header and that frame's
//! bytes, read at the offsets recorded at ingest; only when frames cannot be located
//! (deflated data sets, unattributable fragments) is the whole file parsed.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use dicom_core::value::{PixelFragmentSequence, Value};
use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::tags;
use dicom_encoding::TransferSyntaxIndex;
use dicom_object::{DefaultDicomObject, OpenFileOptions};
use dicom_pixeldata::{
    BitDepthOption, ConvertOptions, PixelDecoder, Transcode, VoiLutOption, WindowLevel,
};
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use image::DynamicImage;
use image::imageops::FilterType;

use crate::Store;
use crate::index::IndexError;
use crate::pixel::{PixelLayout, Sample};
use crate::retrieve::{PixelSource, Scope, read_ranges};

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
        blocking(move || render(&source, options)).await
    }

    /// Frames (0-based) decoded to native little endian pixels.
    pub async fn decoded_frames(
        self: &Arc<Self>,
        scope: Scope,
        frames: Vec<usize>,
    ) -> Result<Vec<Vec<u8>>, ImagingError> {
        let source = self.pixels(scope).await?.ok_or(ImagingError::NotFound)?;
        blocking(move || {
            frames
                .into_iter()
                .map(|frame| decode_frame(&source, frame))
                .collect()
        })
        .await
    }

    /// The stored file at `path` in another transfer syntax, as a Part-10 stream.
    pub async fn transcoded(path: &Path, transfer_syntax: &str) -> Result<Vec<u8>, ImagingError> {
        let (path, transfer_syntax) = (path.to_owned(), transfer_syntax.to_owned());
        blocking(move || transcode(&path, &transfer_syntax)).await
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

/// An object holding only frame `frame` of `source`, as frame 0, or the whole object when
/// frames cannot be located; with the index of the frame to decode in it.
fn frame_object(
    source: &PixelSource,
    frame: usize,
) -> Result<(DefaultDicomObject, u32), ImagingError> {
    let layout = source.layout.as_ref().ok_or(ImagingError::NoPixelData)?;
    if let PixelLayout::Native {
        sample: Sample::Float | Sample::Double,
        ..
    } = layout
    {
        return Err(ImagingError::Unsupported(
            "float pixel data cannot be rendered".into(),
        ));
    }
    let mut obj = OpenFileOptions::new()
        .read_until(tags::FLOAT_PIXEL_DATA)
        .open_file(&source.path)
        .map_err(|e| ImagingError::Failed(e.to_string()))?;
    // Native YBR_FULL_422 frames are subsampled, which a one-frame object cannot describe.
    let subsampled = !layout.is_encapsulated()
        && obj
            .element_opt(tags::PHOTOMETRIC_INTERPRETATION)
            .ok()
            .flatten()
            .and_then(|e| e.to_str().ok())
            .is_some_and(|p| p.trim() == "YBR_FULL_422");
    if layout.frame_count() == 0 || subsampled {
        if layout.frame_count() > 0 && frame >= layout.frame_count() {
            return Err(ImagingError::NoFrame(frame));
        }
        let obj = dicom_object::open_file(&source.path)
            .map_err(|e| ImagingError::Failed(e.to_string()))?;
        return Ok((obj, frame as u32));
    }
    let ranges = layout.frame(frame).ok_or(ImagingError::NoFrame(frame))?;
    let bytes =
        read_ranges(&source.path, &ranges).map_err(|e| ImagingError::Failed(e.to_string()))?;
    obj.put_str(tags::NUMBER_OF_FRAMES, VR::IS, "1");
    // The offset tables describe all frames, not the one kept.
    obj.remove_element(tags::EXTENDED_OFFSET_TABLE);
    obj.remove_element(tags::EXTENDED_OFFSET_TABLE_LENGTHS);
    let element = if layout.is_encapsulated() {
        DataElement::new(
            tags::PIXEL_DATA,
            VR::OB,
            Value::PixelSequence(PixelFragmentSequence::new(Vec::<u32>::new(), vec![bytes])),
        )
    } else {
        let wide = obj
            .element_opt(tags::BITS_ALLOCATED)
            .ok()
            .flatten()
            .and_then(|e| e.to_int::<u16>().ok())
            > Some(8);
        DataElement::new(
            tags::PIXEL_DATA,
            if wide { VR::OW } else { VR::OB },
            PrimitiveValue::from(bytes),
        )
    };
    obj.put(element);
    Ok((obj, 0))
}

fn render(source: &PixelSource, options: RenderOptions) -> Result<Vec<u8>, ImagingError> {
    let (obj, frame) = frame_object(source, options.frame)?;
    let decoded = obj.decode_pixel_data_frame(frame).map_err(unsupported)?;
    let mut convert = ConvertOptions::new().with_bit_depth(BitDepthOption::Force8Bit);
    if let Some((center, width)) = options.window {
        convert = convert.with_voi_lut(VoiLutOption::Custom(WindowLevel { center, width }));
    }
    let mut image = decoded
        .to_dynamic_image_with_options(0, &convert)
        .map_err(unsupported)?;
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

fn decode_frame(source: &PixelSource, frame: usize) -> Result<Vec<u8>, ImagingError> {
    let (obj, index) = frame_object(source, frame)?;
    let decoded = obj.decode_pixel_data_frame(index).map_err(unsupported)?;
    Ok(decoded.frame_data(0).map_err(unsupported)?.to_vec())
}

fn transcode(path: &Path, transfer_syntax: &str) -> Result<Vec<u8>, ImagingError> {
    let ts = TransferSyntaxRegistry.get(transfer_syntax).ok_or_else(|| {
        ImagingError::Unsupported(format!("unknown transfer syntax {transfer_syntax}"))
    })?;
    let mut obj = dicom_object::open_file(path).map_err(|e| ImagingError::Failed(e.to_string()))?;
    obj.transcode(ts).map_err(unsupported)?;
    let mut out = Vec::new();
    obj.write_all(&mut out).map_err(unsupported)?;
    Ok(out)
}
