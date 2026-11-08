mod common;

use axum::http::StatusCode;
use common::{App, BASE, app};
use dicom_core::VR;
use fortitude_testkit::{EXPLICIT_VR_LE, Instance, JPEG_BASELINE, Pixels};

const MULTIPART_DICOM: &str = r#"multipart/related; type="application/dicom""#;
const ANY_OCTETS: &str = r#"multipart/related; type="application/octet-stream"; transfer-syntax=*"#;

struct Fixture {
    app: App,
    series: Vec<Instance>,
    jpeg: Instance,
}

async fn fixture() -> Fixture {
    let app = app();
    let series: Vec<Instance> = fortitude_testkit::series("1.2.3", "1.2.3.1", 3)
        .into_iter()
        .rev()
        .map(|i| {
            i.with(|i| i.frames = 2).element(
                dicom_core::Tag(0x0009, 0x1001),
                VR::OB,
                vec![7u8; 100],
            )
        })
        .collect();
    let jpeg = Instance::new("1.2.3", "1.2.3.2", "1.2.3.2.1").with(|i| {
        i.transfer_syntax = JPEG_BASELINE.into();
        i.pixels = Pixels::Encapsulated {
            fragments_per_frame: 2,
        };
        i.frames = 3;
        i.series_number = Some(2);
    });
    let mut all: Vec<&Instance> = series.iter().collect();
    all.push(&jpeg);
    assert_eq!(app.stow("/studies", &all).await.status, StatusCode::OK);
    Fixture { app, series, jpeg }
}

fn by_number(series: &[Instance], number: i32) -> &Instance {
    series
        .iter()
        .find(|i| i.instance_number == Some(number))
        .unwrap()
}

#[tokio::test]
async fn retrieves_instances_in_order() {
    let f = fixture().await;
    let reply = f.app.get("/studies/1.2.3", MULTIPART_DICOM).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.headers["content-length"],
        reply.body.len().to_string().as_str()
    );
    let parts = reply.parts().await;
    assert_eq!(parts.len(), 4);
    for (i, (content_type, body)) in parts.iter().take(3).enumerate() {
        assert_eq!(
            content_type,
            &format!("application/dicom; transfer-syntax={EXPLICIT_VR_LE}")
        );
        assert_eq!(body, &by_number(&f.series, i as i32 + 1).to_bytes());
    }
    assert_eq!(parts[3].1, f.jpeg.to_bytes());

    let series = f
        .app
        .get("/studies/1.2.3/series/1.2.3.2", "*/*")
        .await
        .parts()
        .await;
    assert_eq!(series.len(), 1);

    let single = f
        .app
        .get(
            "/studies/1.2.3/series/1.2.3.1/instances/1.2.3.1.2",
            "application/dicom",
        )
        .await;
    assert_eq!(single.status, StatusCode::OK);
    assert_eq!(single.body, by_number(&f.series, 2).to_bytes());
}

#[tokio::test]
async fn retrieval_errors() {
    let f = fixture().await;
    assert_eq!(
        f.app.get("/studies/9.9", MULTIPART_DICOM).await.status,
        StatusCode::NOT_FOUND
    );
    // Existing series, wrong study.
    assert_eq!(
        f.app
            .get("/studies/9.9/series/1.2.3.1", MULTIPART_DICOM)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let explicit = format!("{MULTIPART_DICOM}; transfer-syntax={EXPLICIT_VR_LE}");
    assert_eq!(
        f.app
            .get("/studies/1.2.3/series/1.2.3.2", &explicit)
            .await
            .status,
        StatusCode::NOT_ACCEPTABLE
    );
    assert_eq!(
        f.app
            .get("/studies/1.2.3/series/1.2.3.1", &explicit)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        f.app.get("/studies/1.2.3", "application/json").await.status,
        StatusCode::NOT_ACCEPTABLE
    );
    assert_eq!(
        f.app
            .get("/studies/1.2.3", "application/dicom")
            .await
            .status,
        StatusCode::NOT_ACCEPTABLE
    );
}

#[tokio::test]
async fn metadata_at_every_level() {
    let f = fixture().await;
    let reply = f
        .app
        .get(
            "/studies/1.2.3/series/1.2.3.1/metadata",
            "application/dicom+json",
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type(), "application/dicom+json");
    let json = reply.json();
    let instances = json.as_array().unwrap();
    assert_eq!(instances.len(), 3);
    let numbers: Vec<i64> = instances
        .iter()
        .map(|i| i["00200013"]["Value"][0].as_i64().unwrap())
        .collect();
    assert_eq!(numbers, [1, 2, 3]);
    assert_eq!(
        instances[0]["7FE00010"]["BulkDataURI"],
        format!("{BASE}/studies/1.2.3/series/1.2.3.1/instances/1.2.3.1.1/bulk/7FE00010")
    );
    assert_eq!(
        instances[0]["00091001"]["BulkDataURI"],
        format!("{BASE}/studies/1.2.3/series/1.2.3.1/instances/1.2.3.1.1/bulk/00091001")
    );

    let study = f.app.get("/studies/1.2.3/metadata", "*/*").await.json();
    assert_eq!(study.as_array().unwrap().len(), 4);
    let instance = f
        .app
        .get(
            "/studies/1.2.3/series/1.2.3.2/instances/1.2.3.2.1/metadata",
            "*/*",
        )
        .await
        .json();
    assert_eq!(instance[0]["00080018"]["Value"][0], "1.2.3.2.1");
    assert_eq!(
        f.app.get("/studies/1.2.4/metadata", "*/*").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn retrieves_native_frames() {
    let f = fixture().await;
    let instance = by_number(&f.series, 2);
    let path = "/studies/1.2.3/series/1.2.3.1/instances/1.2.3.1.2/frames";
    let reply = f.app.get(&format!("{path}/2,1"), ANY_OCTETS).await;
    assert_eq!(reply.status, StatusCode::OK);
    let parts = reply.parts().await;
    assert_eq!(
        parts[0],
        (
            format!("application/octet-stream; transfer-syntax={EXPLICIT_VR_LE}"),
            instance.frame(1)
        )
    );
    assert_eq!(parts[1].1, instance.frame(0));

    let raw = r#"multipart/related; type="application/octet-stream""#;
    assert_eq!(
        f.app.get(&format!("{path}/1"), raw).await.parts().await[0].1,
        instance.frame(0)
    );
    let single = f
        .app
        .get(&format!("{path}/1"), "application/octet-stream")
        .await;
    assert_eq!(single.body, instance.frame(0));
    assert_eq!(
        f.app.get(&format!("{path}/3"), ANY_OCTETS).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.app.get(&format!("{path}/0"), ANY_OCTETS).await.status,
        StatusCode::BAD_REQUEST
    );
}

/// Frame locations are cached by series: instances stored after a series was cached are
/// still found, and deleted ones are not.
#[tokio::test]
async fn frames_follow_stores_and_deletes() {
    let f = fixture().await;
    let frame = |sop: &str| format!("/studies/1.2.3/series/1.2.3.1/instances/{sop}/frames/1");
    let cached = f.app.get(&frame("1.2.3.1.1"), ANY_OCTETS).await;
    assert_eq!(cached.status, StatusCode::OK);

    let late = Instance::new("1.2.3", "1.2.3.1", "1.2.3.1.9").with(|i| i.instance_number = Some(9));
    assert_eq!(
        f.app.stow("/studies", &[&late]).await.status,
        StatusCode::OK
    );
    let reply = f.app.get(&frame("1.2.3.1.9"), ANY_OCTETS).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.parts().await[0].1, late.frame(0));

    let deleted = f
        .app
        .store
        .delete(fortitude_store::retrieve::Scope::instance(
            "1.2.3",
            "1.2.3.1",
            "1.2.3.1.2",
        ))
        .await
        .unwrap();
    assert_eq!(deleted, 1);
    assert_eq!(
        f.app.get(&frame("1.2.3.1.2"), ANY_OCTETS).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.app.get(&frame("1.2.3.1.3"), ANY_OCTETS).await.status,
        StatusCode::OK
    );
    assert_eq!(
        f.app.get(&frame("1.2.3.1.404"), ANY_OCTETS).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn retrieves_encapsulated_frames() {
    let f = fixture().await;
    let path = "/studies/1.2.3/series/1.2.3.2/instances/1.2.3.2.1/frames/3";
    let parts = f.app.get(path, ANY_OCTETS).await.parts().await;
    assert_eq!(
        parts[0],
        (
            format!("application/octet-stream; transfer-syntax={JPEG_BASELINE}"),
            f.jpeg.frame(2)
        )
    );
    let jpeg = f
        .app
        .get(path, r#"multipart/related; type="image/jpeg""#)
        .await
        .parts()
        .await;
    assert_eq!(
        jpeg[0].0,
        format!("image/jpeg; transfer-syntax={JPEG_BASELINE}")
    );
    let raw = r#"multipart/related; type="application/octet-stream""#;
    assert_eq!(
        f.app.get(path, raw).await.status,
        StatusCode::NOT_ACCEPTABLE
    );
}

#[tokio::test]
async fn retrieves_bulk_data() {
    let f = fixture().await;
    let instance = "/studies/1.2.3/series/1.2.3.1/instances/1.2.3.1.3";
    let pixels = f
        .app
        .get(&format!("{instance}/bulk/7FE00010"), ANY_OCTETS)
        .await
        .parts()
        .await;
    let expected: Vec<u8> = (0..2)
        .flat_map(|i| by_number(&f.series, 3).frame(i))
        .collect();
    assert_eq!(pixels.len(), 1);
    assert_eq!(pixels[0].1, expected);

    let private = f
        .app
        .get(&format!("{instance}/bulk/00091001"), "*/*")
        .await
        .parts()
        .await;
    assert_eq!(private[0].1, vec![7u8; 100]);
    let missing = f.app.get(&format!("{instance}/bulk/00091002"), "*/*").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let fragments = f
        .app
        .get(
            "/studies/1.2.3/series/1.2.3.2/instances/1.2.3.2.1/bulk/7FE00010",
            ANY_OCTETS,
        )
        .await;
    assert_eq!(fragments.parts().await.len(), 3);
}
