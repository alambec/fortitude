mod common;

use axum::http::StatusCode;
use common::{BASE, BOUNDARY, app, multipart};
use fortitude_testkit::Instance;
use serde_json::Value;

fn sequence<'a>(json: &'a Value, tag: &str) -> &'a [Value] {
    json[tag]["Value"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

#[tokio::test]
async fn stores_instances() {
    let app = app();
    let instances: Vec<Instance> = fortitude_testkit::series("1.2.3", "1.2.3.1", 3);
    let reply = app
        .stow("/studies", &instances.iter().collect::<Vec<_>>())
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type(), "application/dicom+json");
    let json = reply.json();
    assert_eq!(
        json["00081190"]["Value"][0],
        format!("{BASE}/studies/1.2.3")
    );
    let referenced = sequence(&json, "00081199");
    assert_eq!(referenced.len(), 3);
    assert_eq!(referenced[0]["00081155"]["Value"][0], "1.2.3.1.1");
    assert_eq!(
        referenced[0]["00081190"]["Value"][0],
        format!("{BASE}/studies/1.2.3/series/1.2.3.1/instances/1.2.3.1.1")
    );
    assert!(json.get("00081198").is_none());
}

#[tokio::test]
async fn duplicates_are_reported_as_warnings() {
    let app = app();
    let instance = Instance::new("1.2.3", "1.2.3.1", "1.2.3.1.1");
    assert_eq!(
        app.stow("/studies", &[&instance]).await.status,
        StatusCode::OK
    );
    let reply = app.stow("/studies", &[&instance]).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let referenced = sequence(&reply.json(), "00081199").to_vec();
    assert_eq!(referenced.len(), 1);
    assert_eq!(referenced[0]["00081196"]["Value"][0], 0xB111);
}

#[tokio::test]
async fn failures_are_reported_per_instance() {
    let app = app();
    let good = Instance::new("1.2.3", "1.2.3.1", "1.2.3.1.1");
    let bad = Instance::new("1.02.abc", "1.2.3.1", "1.2.3.1.2");
    let reply = app.stow("/studies", &[&good, &bad]).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let json = reply.json();
    let failed = sequence(&json, "00081198");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["00081155"]["Value"][0], "1.2.3.1.2");
    assert_eq!(failed[0]["00081197"]["Value"][0], 0xA900);

    let reply = app.stow("/studies", &[&bad]).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn study_path_rejects_other_studies() {
    let app = app();
    let instance = Instance::new("1.2.3", "1.2.3.1", "1.2.3.1.1");
    let reply = app.stow("/studies/1.2.4", &[&instance]).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        sequence(&reply.json(), "00081198")[0]["00081197"]["Value"][0],
        0xA900
    );
    assert_eq!(
        app.stow("/studies/1.2.3", &[&instance]).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn non_dicom_parts_are_other_failures() {
    let app = app();
    let instance = Instance::new("1.2.3", "1.2.3.1", "1.2.3.1.1").to_bytes();
    let mut body = multipart(&[instance]);
    let extra = format!("--{BOUNDARY}\r\nContent-Type: application/json\r\n\r\n{{}}\r\n");
    body.splice(0..0, extra.into_bytes());
    let reply = app
        .stow_raw(
            "/studies",
            &format!("multipart/related; boundary={BOUNDARY}"),
            body,
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(
        sequence(&reply.json(), "0008119A")[0]["00081197"]["Value"][0],
        0xC000
    );
}

#[tokio::test]
async fn rejects_unsupported_requests() {
    let app = app();
    let reply = app
        .stow_raw("/studies", "application/dicom", Vec::new())
        .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let reply = app
        .stow_raw(
            "/studies",
            "multipart/related; type=\"application/dicom+json\"; boundary=x",
            Vec::new(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let reply = app
        .stow_raw(
            "/studies",
            "multipart/related; type=\"application/dicom\"",
            Vec::new(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let reply = app
        .stow_raw(
            "/studies",
            &format!("multipart/related; boundary={BOUNDARY}"),
            b"--nope".to_vec(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}
