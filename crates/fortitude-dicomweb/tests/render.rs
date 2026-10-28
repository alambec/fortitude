mod common;

use axum::http::StatusCode;
use common::{App, app};
use fortitude_testkit::{EXPLICIT_VR_LE, Instance, JPEG_BASELINE, Pixels};

const JPEG_PATH: &str = "/studies/2.1/series/2.1.1/instances/2.1.1.1";
const NATIVE_PATH: &str = "/studies/2.1/series/2.1.2/instances/2.1.2.1";

struct Fixture {
    app: App,
    jpeg: Instance,
    native: Instance,
}

async fn fixture() -> Fixture {
    let app = app();
    let image = |i: &mut Instance| {
        i.rows = 16;
        i.columns = 16;
        i.frames = 2;
    };
    let jpeg = Instance::new("2.1", "2.1.1", "2.1.1.1").with(|i| {
        image(i);
        i.pixels = Pixels::Jpeg;
        i.transfer_syntax = JPEG_BASELINE.into();
    });
    let native = Instance::new("2.1", "2.1.2", "2.1.2.1").with(|i| {
        image(i);
        i.series_number = Some(2);
    });
    let report = Instance::new("2.1", "2.1.3", "2.1.3.1").with(|i| {
        i.pixels = Pixels::None;
        i.modality = "SR".into();
        i.series_number = Some(3);
    });
    assert_eq!(
        app.stow("/studies", &[&jpeg, &native, &report])
            .await
            .status,
        StatusCode::OK
    );
    Fixture { app, jpeg, native }
}

fn dimensions(bytes: &[u8]) -> (u32, u32) {
    let image = image::load_from_memory(bytes).expect("decodable image");
    (image.width(), image.height())
}

#[tokio::test]
async fn renders_frames() {
    let f = fixture().await;
    for path in [NATIVE_PATH, JPEG_PATH] {
        let reply = f.app.get(&format!("{path}/rendered"), "image/jpeg").await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        assert_eq!(reply.content_type(), "image/jpeg");
        assert_eq!(dimensions(&reply.body), (16, 16));
    }
    let png = f
        .app
        .get(
            &format!("{NATIVE_PATH}/rendered?viewport=8,8&window=100,200"),
            "image/png",
        )
        .await;
    assert_eq!(png.content_type(), "image/png");
    assert_eq!(dimensions(&png.body), (8, 8));

    let frame = f
        .app
        .get(&format!("{NATIVE_PATH}/frames/2/rendered"), "*/*")
        .await;
    assert_eq!(dimensions(&frame.body), (16, 16));
    let both = f
        .app
        .get(&format!("{JPEG_PATH}/frames/1,2/rendered"), "*/*")
        .await;
    assert_eq!(both.parts().await.len(), 2);

    assert_eq!(
        f.app
            .get(&format!("{NATIVE_PATH}/frames/3/rendered"), "*/*")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.app
            .get(&format!("{NATIVE_PATH}/rendered"), "application/dicom")
            .await
            .status,
        StatusCode::NOT_ACCEPTABLE
    );
    assert_eq!(
        f.app
            .get(&format!("{NATIVE_PATH}/rendered?quality=0"), "*/*")
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let report = f
        .app
        .get(
            "/studies/2.1/series/2.1.3/instances/2.1.3.1/rendered",
            "*/*",
        )
        .await;
    assert_eq!(report.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn thumbnails() {
    let f = fixture().await;
    let study = f.app.get("/studies/2.1/thumbnail", "*/*").await;
    assert_eq!(study.status, StatusCode::OK);
    assert_eq!(dimensions(&study.body), (16, 16));
    let small = f
        .app
        .get("/studies/2.1/series/2.1.2/thumbnail?viewport=4,4", "*/*")
        .await;
    assert_eq!(dimensions(&small.body), (4, 4));
    let frame = f
        .app
        .get(&format!("{JPEG_PATH}/frames/2/thumbnail"), "image/png")
        .await;
    assert_eq!(frame.content_type(), "image/png");
    assert_eq!(
        f.app
            .get("/studies/2.1/series/2.1.3/thumbnail", "*/*")
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.app.get("/studies/9.9/thumbnail", "*/*").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn decodes_compressed_frames_on_request() {
    let f = fixture().await;
    let raw = r#"multipart/related; type="application/octet-stream""#;
    let parts = f
        .app
        .get(&format!("{JPEG_PATH}/frames/2"), raw)
        .await
        .parts()
        .await;
    assert_eq!(
        parts[0].0,
        format!("application/octet-stream; transfer-syntax={EXPLICIT_VR_LE}")
    );
    let expected = f.jpeg.pixels(1);
    assert_eq!(parts[0].1.len(), expected.len());
    let error = parts[0]
        .1
        .iter()
        .zip(&expected)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    assert!(error <= 12, "lossy decoding error {error}");

    let stored = r#"multipart/related; type="application/octet-stream"; transfer-syntax=*"#;
    let parts = f
        .app
        .get(&format!("{JPEG_PATH}/frames/2"), stored)
        .await
        .parts()
        .await;
    // Fragments are stored padded to an even length.
    let mut codestream = f.jpeg.frame(1);
    if codestream.len() % 2 == 1 {
        codestream.push(0);
    }
    assert_eq!(parts[0].1, codestream);
}

#[tokio::test]
async fn transcodes_instances_on_request() {
    let f = fixture().await;
    let explicit =
        format!(r#"multipart/related; type="application/dicom"; transfer-syntax={EXPLICIT_VR_LE}"#);
    let parts = f
        .app
        .get("/studies/2.1/series/2.1.1", &explicit)
        .await
        .parts()
        .await;
    assert_eq!(
        parts[0].0,
        format!("application/dicom; transfer-syntax={EXPLICIT_VR_LE}")
    );
    let obj = dicom_object::OpenFileOptions::new()
        .from_reader(&parts[0].1[..])
        .expect("valid DICOM");
    assert_eq!(
        obj.meta().transfer_syntax().trim_end_matches('\0'),
        EXPLICIT_VR_LE
    );
    let pixels = obj
        .element(dicom_dictionary_std::tags::PIXEL_DATA)
        .unwrap()
        .to_bytes()
        .unwrap();
    assert_eq!(pixels.len(), 2 * 16 * 16);

    // Already in the requested transfer syntax: returned as stored.
    let parts = f
        .app
        .get("/studies/2.1/series/2.1.2", &explicit)
        .await
        .parts()
        .await;
    assert_eq!(parts[0].1, f.native.to_bytes());

    let single = f
        .app
        .get(
            JPEG_PATH,
            &format!("application/dicom; transfer-syntax={EXPLICIT_VR_LE}"),
        )
        .await;
    assert_eq!(single.status, StatusCode::OK);
    assert_eq!(
        single.content_type(),
        format!("application/dicom; transfer-syntax={EXPLICIT_VR_LE}")
    );
}

#[tokio::test]
async fn wado_uri() {
    let f = fixture().await;
    let uri = |object: &str, extra: &str| {
        let series = &object[..object.rfind('.').unwrap()];
        format!("/wado?requestType=WADO&studyUID=2.1&seriesUID={series}&objectUID={object}{extra}")
    };
    let dicom = f
        .app
        .get(&uri("2.1.2.1", "&contentType=application/dicom"), "*/*")
        .await;
    assert_eq!(dicom.status, StatusCode::OK);
    assert_eq!(dicom.content_type(), "application/dicom");
    assert_eq!(dicom.body, f.native.to_bytes());

    let default = f.app.get(&uri("2.1.1.1", ""), "*/*").await;
    assert_eq!(default.content_type(), "image/jpeg");
    assert_eq!(dimensions(&default.body), (16, 16));
    let report = f.app.get(&uri("2.1.3.1", ""), "*/*").await;
    assert_eq!(report.content_type(), "application/dicom");

    let scaled = f
        .app
        .get(
            &uri("2.1.1.1", "&contentType=image/png&rows=8&frameNumber=2"),
            "*/*",
        )
        .await;
    assert_eq!(scaled.content_type(), "image/png");
    assert_eq!(dimensions(&scaled.body), (8, 8));

    let transcoded = f
        .app
        .get(
            &uri(
                "2.1.1.1",
                &format!("&contentType=application/dicom&transferSyntax={EXPLICIT_VR_LE}"),
            ),
            "*/*",
        )
        .await;
    let obj = dicom_object::OpenFileOptions::new()
        .from_reader(&transcoded.body[..])
        .expect("valid DICOM");
    assert_eq!(
        obj.meta().transfer_syntax().trim_end_matches('\0'),
        EXPLICIT_VR_LE
    );

    assert_eq!(
        f.app.get("/wado?studyUID=2.1", "*/*").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.app
            .get(&uri("2.1.1.1", "&anonymize=yes"), "*/*")
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.app.get(&uri("2.1.1.9", ""), "*/*").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.app
            .get(&uri("2.1.1.1", "&contentType=text/html"), "*/*")
            .await
            .status,
        StatusCode::NOT_ACCEPTABLE
    );
}
