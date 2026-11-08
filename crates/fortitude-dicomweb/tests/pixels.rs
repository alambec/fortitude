//! Pixel data that dicom-pixeldata alone does not decode or render correctly: JPEG-LS, 12-bit
//! JPEG Extended, big endian samples, native YBR, 32-bit and floating point samples, signed
//! samples without a window, and attributes encoded with VR UN.

mod common;

use axum::http::StatusCode;
use common::{App, app};
use dicom_core::value::{PixelFragmentSequence, Value};
use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::tags;
use fortitude_testkit::jpeg::fixture;
use fortitude_testkit::{EXPLICIT_VR_LE, IMPLICIT_VR_LE, Instance, JPEG_EXTENDED, Pixels};

const EXPLICIT_VR_BE: &str = "1.2.840.10008.1.2.2";
const JPEG_LS_NEAR_LOSSLESS: &str = "1.2.840.10008.1.2.4.81";
const RLE_LOSSLESS: &str = "1.2.840.10008.1.2.5";
const DECODED: &str =
    r#"multipart/related; type="application/octet-stream"; transfer-syntax=1.2.840.10008.1.2.1"#;

const ROWS: u16 = 8;
const COLUMNS: u16 = 8;
const PIXELS: usize = ROWS as usize * COLUMNS as usize;

/// An 8x8 image of `pixels` with these image attributes (US values).
fn image(sop: &str, transfer_syntax: &str, attributes: &[(Tag, u16)], pixels: Pixels) -> Instance {
    let mut instance = Instance::new("3.1", "3.1.1", sop).with(|i| {
        i.rows = ROWS;
        i.columns = COLUMNS;
        i.transfer_syntax = transfer_syntax.into();
        i.pixels = pixels;
    });
    for &(tag, value) in attributes {
        instance = instance.element(tag, VR::US, value);
    }
    instance
}

fn native(vr: VR, value: PrimitiveValue) -> Pixels {
    Pixels::Element(DataElement::new(tags::PIXEL_DATA, vr, value))
}

fn encapsulated(frames: Vec<Vec<u8>>) -> Pixels {
    Pixels::Element(DataElement::new(
        tags::PIXEL_DATA,
        VR::OB,
        Value::PixelSequence(PixelFragmentSequence::new(Vec::<u32>::new(), frames)),
    ))
}

fn bits(allocated: u16, signed: bool) -> [(Tag, u16); 4] {
    [
        (tags::BITS_ALLOCATED, allocated),
        (tags::BITS_STORED, allocated),
        (tags::HIGH_BIT, allocated - 1),
        (tags::PIXEL_REPRESENTATION, u16::from(signed)),
    ]
}

fn path(instance: &Instance) -> String {
    format!(
        "/studies/{}/series/{}/instances/{}",
        instance.study_uid, instance.series_uid, instance.sop_uid
    )
}

async fn stored(instances: &[&Instance]) -> App {
    let app = app();
    let reply = app.stow("/studies", instances).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    app
}

/// Frame `frame` decoded to Explicit VR Little Endian samples.
async fn decoded(app: &App, instance: &Instance, frame: u32) -> Vec<u8> {
    let reply = app
        .get(&format!("{}/frames/{frame}", path(instance)), DECODED)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let mut parts = reply.parts().await;
    assert_eq!(
        parts[0].0,
        "application/octet-stream; transfer-syntax=1.2.840.10008.1.2.1"
    );
    parts.remove(0).1
}

/// Frame 1 rendered as PNG, as RGB pixels.
async fn rendered(app: &App, instance: &Instance) -> image::RgbImage {
    let reply = app
        .get(
            &format!("{}/frames/1/rendered", path(instance)),
            "image/png",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    image::load_from_memory(&reply.body).expect("PNG").to_rgb8()
}

/// Luma of the first and last pixels of a gradient, which must span the whole range.
fn assert_full_range(image: &image::RgbImage) {
    let (width, height) = image.dimensions();
    assert_eq!((width, height), (u32::from(COLUMNS), u32::from(ROWS)));
    assert_eq!(image.get_pixel(0, 0).0[0], 0);
    assert_eq!(image.get_pixel(width - 1, height - 1).0[0], 255);
}

fn le16(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[tokio::test]
async fn jpeg_ls_near_lossless() {
    let source: Vec<u16> = (0..PIXELS as u16).map(|i| i * 900).collect();
    let codestream = charls::CharLS::default()
        .encode(
            charls::FrameInfo {
                width: COLUMNS.into(),
                height: ROWS.into(),
                bits_per_sample: 16,
                component_count: 1,
            },
            3,
            &le16(&source),
        )
        .expect("JPEG-LS encoding");
    let mut fragment = codestream;
    if fragment.len() % 2 == 1 {
        fragment.push(0);
    }
    let instance = image(
        "3.1.1.1",
        JPEG_LS_NEAR_LOSSLESS,
        &bits(16, false),
        encapsulated(vec![fragment]),
    );
    let app = stored(&[&instance]).await;
    let samples = decoded(&app, &instance, 1).await;
    for (got, want) in samples.chunks_exact(2).zip(&source) {
        let got = u16::from_le_bytes([got[0], got[1]]);
        assert!(got.abs_diff(*want) <= 3, "{got} vs {want}");
    }
    assert_full_range(&rendered(&app, &instance).await);
}

#[tokio::test]
async fn big_endian() {
    let values: Vec<u16> = (0..PIXELS as u16).map(|i| i * 1000 + 1).collect();
    let wide = image(
        "3.1.1.1",
        EXPLICIT_VR_BE,
        &bits(16, false),
        native(VR::OW, PrimitiveValue::U16(values.clone().into())),
    );
    // 8-bit OW samples are stored by 16-bit words, two pixels each; frames of 3x3 pixels
    // start on odd bytes.
    let narrow_pixels: Vec<u8> = (0..18).map(|i| i * 10).collect();
    let words = narrow_pixels
        .chunks(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    let narrow = image(
        "3.1.1.2",
        EXPLICIT_VR_BE,
        &bits(8, false),
        native(VR::OW, PrimitiveValue::U16(words)),
    )
    .with(|i| {
        i.rows = 3;
        i.columns = 3;
        i.frames = 2;
    });
    let app = stored(&[&wide, &narrow]).await;
    assert_eq!(decoded(&app, &wide, 1).await, le16(&values));
    assert_full_range(&rendered(&app, &wide).await);
    assert_eq!(decoded(&app, &narrow, 1).await, narrow_pixels[..9]);
    assert_eq!(decoded(&app, &narrow, 2).await, narrow_pixels[9..]);
}

#[tokio::test]
async fn native_ybr() {
    // Pure green: Y 150, Cb 44, Cr 21.
    let full = image(
        "3.1.1.1",
        fortitude_testkit::EXPLICIT_VR_LE,
        &[
            (tags::SAMPLES_PER_PIXEL, 3),
            (tags::PLANAR_CONFIGURATION, 0),
        ],
        native(VR::OB, PrimitiveValue::from([150, 44, 21].repeat(PIXELS))),
    )
    .with(|i| {
        i.extra.push(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("YBR_FULL"),
        ))
    });
    // Two frames of pure green, as Y1 Y2 Cb Cr per pair of pixels.
    let subsampled = image(
        "3.1.1.2",
        fortitude_testkit::EXPLICIT_VR_LE,
        &[
            (tags::SAMPLES_PER_PIXEL, 3),
            (tags::PLANAR_CONFIGURATION, 0),
        ],
        native(
            VR::OB,
            PrimitiveValue::from([150, 150, 44, 21].repeat(PIXELS)),
        ),
    )
    .with(|i| {
        i.frames = 2;
        i.extra.push(DataElement::new(
            tags::PHOTOMETRIC_INTERPRETATION,
            VR::CS,
            PrimitiveValue::from("YBR_FULL_422"),
        ));
    });
    let app = stored(&[&full, &subsampled]).await;
    for instance in [&full, &subsampled] {
        let image = rendered(&app, instance).await;
        assert_eq!(image.dimensions(), (u32::from(COLUMNS), u32::from(ROWS)));
        assert_eq!(image.get_pixel(5, 5).0, [0, 255, 1], "{}", instance.sop_uid);
    }
    let last = app
        .get(
            &format!("{}/frames/2/rendered", path(&subsampled)),
            "image/png",
        )
        .await;
    assert_eq!(last.status, StatusCode::OK);
    // Frames are returned as stored: two samples per pixel.
    assert_eq!(decoded(&app, &subsampled, 2).await.len(), PIXELS * 2);
}

#[tokio::test]
async fn wide_samples() {
    // RT Dose: 32-bit unsigned, without window.
    let dose: Vec<u32> = (0..PIXELS as u32).map(|i| i * 50_000_000).collect();
    let dose_le: Vec<u8> = dose.iter().flat_map(|v| v.to_le_bytes()).collect();
    let native_dose = image(
        "3.1.1.1",
        fortitude_testkit::IMPLICIT_VR_LE,
        &bits(32, false),
        native(VR::OW, PrimitiveValue::U32(dose.clone().into())),
    );
    // The same as RLE Lossless: one segment per byte, most significant first, literal runs.
    let mut header = [0u32; 16];
    header[0] = 4;
    let mut segments = Vec::new();
    for byte in (0..4).rev() {
        header[4 - byte] = 64 + segments.len() as u32;
        segments.push(PIXELS as u8 - 1);
        segments.extend(dose.iter().map(|v| (v >> (8 * byte)) as u8));
    }
    let mut frame: Vec<u8> = header.iter().flat_map(|w| w.to_le_bytes()).collect();
    frame.extend(segments);
    let rle_dose = image(
        "3.1.1.2",
        RLE_LOSSLESS,
        &bits(32, false),
        encapsulated(vec![frame]),
    );
    // Parametric map: 32-bit floats, without window.
    let map = Instance::new("3.1", "3.1.1", "3.1.1.3").with(|i| {
        i.rows = ROWS;
        i.columns = COLUMNS;
        i.pixels = Pixels::Element(DataElement::new(
            tags::FLOAT_PIXEL_DATA,
            VR::OF,
            PrimitiveValue::F32((0..PIXELS).map(|i| i as f32 * 0.01 - 0.3).collect()),
        ));
    });
    let map = map.element(tags::BITS_ALLOCATED, VR::US, 32u16);
    let app = stored(&[&native_dose, &rle_dose, &map]).await;
    assert_eq!(decoded(&app, &rle_dose, 1).await, dose_le);
    for instance in [&native_dose, &rle_dose, &map] {
        assert_full_range(&rendered(&app, instance).await);
    }
}

#[tokio::test]
async fn signed_without_window() {
    let values: Vec<i16> = (0..PIXELS as i16).map(|i| i * 30 - 1024).collect();
    let instance = image(
        "3.1.1.1",
        fortitude_testkit::EXPLICIT_VR_LE,
        &bits(16, true),
        native(VR::OW, PrimitiveValue::I16(values.into())),
    );
    let app = stored(&[&instance]).await;
    assert_full_range(&rendered(&app, &instance).await);
}

#[tokio::test]
async fn unknown_vr() {
    // As written by converters that do not know the attributes: VR UN, Implicit VR values.
    let instance = Instance::new("3.1", "3.1.1", "3.1.1.1")
        .element(tags::SERIES_INSTANCE_UID, VR::UN, b"3.1.1\0".to_vec())
        .element(tags::PATIENT_ID, VR::UN, b"UN-42 ".to_vec())
        .element(tags::ROWS, VR::UN, 4u16.to_le_bytes().to_vec());
    let app = stored(&[&instance]).await;
    let found = app
        .get("/studies?PatientID=UN-42", "application/dicom+json")
        .await
        .json();
    assert_eq!(found.as_array().map(Vec::len), Some(1));
    let metadata = app
        .get(
            &format!("{}/metadata", path(&instance)),
            "application/dicom+json",
        )
        .await
        .json();
    assert_eq!(
        metadata[0]["0020000E"],
        serde_json::json!({"vr": "UI", "Value": ["3.1.1"]})
    );
    assert_eq!(metadata[0]["00280010"]["vr"], "US");
    let reply = app
        .get(&format!("{}/rendered", path(&instance)), "image/png")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn deflated() {
    let instance = Instance::new("3.1", "3.1.1", "3.1.1.1").with(|i| {
        i.rows = ROWS;
        i.columns = COLUMNS;
        i.frames = 2;
        i.transfer_syntax = "1.2.840.10008.1.2.1.99".into();
    });
    let app = stored(&[&instance]).await;
    assert_eq!(decoded(&app, &instance, 2).await, instance.pixels(1));
    let as_stored = app
        .get(
            &format!("{}/frames/1", path(&instance)),
            r#"multipart/related; type="application/octet-stream"; transfer-syntax=*"#,
        )
        .await;
    assert_eq!(as_stored.status, StatusCode::NOT_FOUND);
    assert_eq!(
        rendered(&app, &instance).await.dimensions(),
        (u32::from(COLUMNS), u32::from(ROWS))
    );
    let report = Instance::new("3.1", "3.1.2", "3.1.2.1").with(|i| {
        i.pixels = Pixels::None;
        i.transfer_syntax = "1.2.840.10008.1.2.1.99".into();
    });
    let app = stored(&[&report]).await;
    let reply = app
        .get(&format!("{}/rendered", path(&report)), "image/png")
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

/// Fragments of the JPEG fixtures `names`, one frame each.
fn jpeg_frames(names: &[&str]) -> Pixels {
    encapsulated(
        names
            .iter()
            .map(|name| {
                let mut stream = fixture(name).0;
                if stream.len() % 2 == 1 {
                    stream.push(0);
                }
                stream
            })
            .collect(),
    )
}

/// A 61x45 image of 12-bit samples in JPEG Extended, with these frames.
fn jpeg_12_bit(sop: &str, names: &[&str], samples_per_pixel: u16, signed: bool) -> Instance {
    let mut instance = image(
        sop,
        JPEG_EXTENDED,
        &[
            (tags::BITS_ALLOCATED, 16),
            (tags::BITS_STORED, 12),
            (tags::HIGH_BIT, 11),
            (tags::PIXEL_REPRESENTATION, u16::from(signed)),
            (tags::SAMPLES_PER_PIXEL, samples_per_pixel),
        ],
        jpeg_frames(names),
    )
    .with(|i| {
        i.rows = 45;
        i.columns = 61;
        i.frames = names.len() as u32;
    });
    if samples_per_pixel == 3 {
        instance = instance
            .element(tags::PLANAR_CONFIGURATION, VR::US, 0u16)
            .element(tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "YBR_FULL_422");
    }
    instance
}

/// The instance transcoded to `transfer_syntax` by WADO-RS.
async fn transcoded(
    app: &App,
    instance: &Instance,
    transfer_syntax: &str,
) -> dicom_object::DefaultDicomObject {
    let accept = format!(
        r#"multipart/related; type="application/dicom"; transfer-syntax={transfer_syntax}"#
    );
    let reply = app.get(&path(instance), &accept).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let parts = reply.parts().await;
    assert_eq!(
        parts[0].0,
        format!("application/dicom; transfer-syntax={transfer_syntax}")
    );
    let obj = dicom_object::OpenFileOptions::new()
        .from_reader(&parts[0].1[..])
        .expect("valid DICOM");
    assert_eq!(
        obj.meta().transfer_syntax().trim_end_matches('\0'),
        transfer_syntax
    );
    obj
}

fn text(obj: &dicom_object::DefaultDicomObject, tag: Tag) -> String {
    obj.element(tag)
        .expect("element")
        .to_str()
        .expect("text")
        .trim()
        .to_owned()
}

#[tokio::test]
async fn jpeg_extended_12_bit_gray() {
    let names = ["12-gray", "12-gray-restart"];
    let instance = jpeg_12_bit("3.1.1.1", &names, 1, false);
    let app = stored(&[&instance]).await;
    let expected: Vec<Vec<u8>> = names.iter().map(|n| fixture(n).1.to_le_bytes()).collect();
    assert_eq!(decoded(&app, &instance, 1).await, expected[0]);
    assert_eq!(decoded(&app, &instance, 2).await, expected[1]);

    // Rendered with the range of values as window: the square is at the top of the range.
    let image = rendered(&app, &instance).await;
    assert_eq!(image.dimensions(), (61, 45));
    assert!(
        image.get_pixel(20, 15).0[0] >= 250,
        "{:?}",
        image.get_pixel(20, 15)
    );
    let last = app
        .get(
            &format!("{}/frames/2/rendered", path(&instance)),
            "image/jpeg",
        )
        .await;
    assert_eq!(last.status, StatusCode::OK);
    let thumbnail = app
        .get(&format!("{}/thumbnail", path(&instance)), "image/jpeg")
        .await;
    assert_eq!(thumbnail.status, StatusCode::OK);

    // Transcoded instances hold all the frames.
    for transfer_syntax in [EXPLICIT_VR_LE, IMPLICIT_VR_LE] {
        let obj = transcoded(&app, &instance, transfer_syntax).await;
        let pixels = obj.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap();
        assert_eq!(pixels.as_ref(), expected.concat(), "{transfer_syntax}");
        assert_eq!(text(&obj, tags::NUMBER_OF_FRAMES), "2");
        assert_eq!(text(&obj, tags::PHOTOMETRIC_INTERPRETATION), "MONOCHROME2");
    }

    // Frames as stored, untouched.
    let as_stored = app
        .get(
            &format!("{}/frames/1", path(&instance)),
            &format!(r#"multipart/related; type="image/jpeg"; transfer-syntax={JPEG_EXTENDED}"#),
        )
        .await;
    assert_eq!(as_stored.status, StatusCode::OK);
    let mut stream = fixture(names[0]).0;
    if stream.len() % 2 == 1 {
        stream.push(0);
    }
    assert_eq!(as_stored.parts().await[0].1, stream);
}

#[tokio::test]
async fn jpeg_extended_12_bit_signed() {
    let instance = jpeg_12_bit("3.1.1.1", &["12-gray"], 1, true);
    let app = stored(&[&instance]).await;
    // Samples are sign extended from bit 11.
    let expected: Vec<u16> = fixture("12-gray")
        .1
        .samples
        .iter()
        .map(|&s| ((s << 4) as i16 >> 4) as u16)
        .collect();
    assert!(expected.iter().any(|&s| (s as i16) < 0));
    assert_eq!(decoded(&app, &instance, 1).await, le16(&expected));
    assert_eq!(rendered(&app, &instance).await.dimensions(), (61, 45));
}

#[tokio::test]
async fn jpeg_extended_12_bit_color() {
    let instance = jpeg_12_bit("3.1.1.1", &["12-ycbcr-420"], 3, false);
    let app = stored(&[&instance]).await;
    // Converted to RGB, as the photometric interpretation is YBR_FULL_422.
    let expected = fixture("12-ycbcr-420").1.to_le_bytes();
    assert_eq!(decoded(&app, &instance, 1).await, expected);
    // A red square on a colored background.
    let image = rendered(&app, &instance).await;
    assert_eq!(image.dimensions(), (61, 45));
    let [red, green, blue] = image.get_pixel(20, 15).0;
    assert!(
        red > 200 && green < 50 && blue > 200,
        "{red} {green} {blue}"
    );

    let obj = transcoded(&app, &instance, EXPLICIT_VR_LE).await;
    assert_eq!(text(&obj, tags::PHOTOMETRIC_INTERPRETATION), "RGB");
    assert_eq!(
        obj.element(tags::PLANAR_CONFIGURATION)
            .unwrap()
            .to_int::<u16>()
            .unwrap(),
        0
    );
    let pixels = obj.element(tags::PIXEL_DATA).unwrap().to_bytes().unwrap();
    assert_eq!(pixels.as_ref(), expected);
}

#[tokio::test]
async fn jpeg_extended_12_bit_invalid() {
    // Frame of 61x45 pixels, Rows and Columns of 8x8.
    let wrong_size = image(
        "3.1.1.1",
        JPEG_EXTENDED,
        &[
            (tags::BITS_ALLOCATED, 16),
            (tags::BITS_STORED, 12),
            (tags::HIGH_BIT, 11),
        ],
        jpeg_frames(&["12-gray"]),
    );
    // Truncated in its headers.
    let mut stream = fixture("12-gray").0;
    stream.truncate(100);
    let truncated = jpeg_12_bit("3.1.1.2", &[], 1, false).with(|i| {
        i.frames = 1;
        i.pixels = encapsulated(vec![stream]);
    });
    let app = stored(&[&wrong_size, &truncated]).await;
    for instance in [&wrong_size, &truncated] {
        let reply = app
            .get(
                &format!("{}/frames/1/rendered", path(instance)),
                "image/png",
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_ACCEPTABLE,
            "{}",
            instance.sop_uid
        );
        let reply = app
            .get(&format!("{}/frames/1", path(instance)), DECODED)
            .await;
        assert_eq!(
            reply.status,
            StatusCode::NOT_ACCEPTABLE,
            "{}",
            instance.sop_uid
        );
    }
}
