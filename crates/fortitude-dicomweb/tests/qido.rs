mod common;

use axum::http::StatusCode;
use common::{App, BASE, app};
use dicom_dictionary_std::tags;
use fortitude_testkit::Instance;
use serde_json::Value;

const JSON: &str = "application/dicom+json";

fn instance(study: &str, series: &str, n: i32) -> Instance {
    Instance::new(study, series, &format!("{series}.{n}")).with(|i| i.instance_number = Some(n))
}

/// Study 1.1: DOE^JANE, 2024, CT (2 instances) + SR; study 1.2: SMITH^JOHN, 2023, CT;
/// study 1.3: doe^john, 2022, MR.
async fn fixture() -> App {
    let app = app();
    let a = |i: Instance| {
        i.with(|i| {
            i.patient_name = "Doe^Jane".into();
            i.patient_id = "PAT-1".into();
            i.study_date = "20240115".into();
        })
        .element(tags::PATIENT_AGE, dicom_core::VR::AS, "045Y")
        .element(tags::STUDY_DESCRIPTION, dicom_core::VR::LO, "Head CT")
    };
    let studies = [
        a(instance("1.1", "1.1.1", 2)),
        a(instance("1.1", "1.1.1", 1)),
        a(instance("1.1", "1.1.2", 1).with(|i| {
            i.modality = "SR".into();
            i.series_number = Some(2);
        })),
        instance("1.2", "1.2.1", 1).with(|i| {
            i.patient_name = "Smith^John".into();
            i.patient_id = "PAT-2".into();
            i.study_date = "20230610".into();
        }),
        instance("1.3", "1.3.1", 1).with(|i| {
            i.patient_name = "doe^john".into();
            i.patient_id = "PAT-3".into();
            i.study_date = "20220101".into();
            i.modality = "MR".into();
        }),
    ];
    let reply = app
        .stow("/studies", &studies.iter().collect::<Vec<_>>())
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    app
}

async fn search(app: &App, path: &str) -> Vec<Value> {
    let reply = app.get(path, JSON).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "{path}: {}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(reply.content_type(), JSON);
    reply.json().as_array().unwrap().clone()
}

fn uids(results: &[Value], tag: &str) -> Vec<String> {
    results
        .iter()
        .map(|r| r[tag]["Value"][0].as_str().unwrap().to_owned())
        .collect()
}

/// Study UIDs of results, sorted: instances of a request are stored concurrently, so the
/// insertion order (the result order) is not deterministic.
fn studies(results: &[Value]) -> Vec<String> {
    sorted(uids(results, "0020000D"))
}

fn sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

#[tokio::test]
async fn study_results_have_default_and_computed_attributes() {
    let app = fixture().await;
    let results = search(&app, "/studies").await;
    assert_eq!(studies(&results), ["1.1", "1.2", "1.3"]);
    let first = results
        .iter()
        .find(|r| r["0020000D"]["Value"][0] == "1.1")
        .unwrap();
    assert_eq!(first["00100010"]["Value"][0]["Alphabetic"], "Doe^Jane");
    assert_eq!(first["00081030"]["Value"][0], "Head CT");
    assert_eq!(first["00080061"]["Value"], serde_json::json!(["CT", "SR"]));
    assert_eq!(first["00201206"]["Value"][0], 2);
    assert_eq!(first["00201208"]["Value"][0], 3);
    assert_eq!(first["00080056"]["Value"][0], "ONLINE");
    assert_eq!(first["00081190"]["Value"][0], format!("{BASE}/studies/1.1"));
    assert!(
        first.get("00101010").is_none(),
        "PatientAge is not a default attribute"
    );
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "attributes are in tag order");
}

#[tokio::test]
async fn study_matching() {
    let app = fixture().await;
    let check = |query: &'static str, expected: &'static [&'static str]| {
        let app = &app;
        async move {
            let results = search(app, &format!("/studies?{query}")).await;
            assert_eq!(studies(&results), expected, "{query}");
        }
    };
    check("PatientName=DOE*", &["1.1", "1.3"]).await;
    check("PatientName=doe%5Ejane", &["1.1"]).await;
    check("PatientName=jo&fuzzymatching=true", &["1.2", "1.3"]).await;
    check("PatientID=PAT-2", &["1.2"]).await;
    check("PatientID=PAT-?", &["1.1", "1.2", "1.3"]).await;
    check("00100020=PAT-3", &["1.3"]).await;
    check("StudyDate=20230101-20241231", &["1.1", "1.2"]).await;
    check("StudyDate=-20221231", &["1.3"]).await;
    check("StudyInstanceUID=1.3,1.1", &["1.1", "1.3"]).await;
    check("ModalitiesInStudy=SR", &["1.1"]).await;
    check("ModalitiesInStudy=CT%5CMR", &["1.1", "1.2", "1.3"]).await;
    check("Modality=MR", &["1.3"]).await;
    check("SOPInstanceUID=1.2.1.1", &["1.2"]).await;
    check("PatientAge=045Y", &["1.1"]).await;
    check("PatientAge=04*&PatientID=PAT-1", &["1.1"]).await;
    check("PatientName=NOBODY", &[]).await;

    // Pages follow the order of the full result list.
    let all = uids(&search(&app, "/studies").await, "0020000D");
    let page = uids(&search(&app, "/studies?limit=1&offset=1").await, "0020000D");
    assert_eq!(page, all[1..2]);
    let residual_page = uids(
        &search(&app, "/studies?PatientAge=&limit=2&offset=1").await,
        "0020000D",
    );
    assert_eq!(residual_page, all[1..3]);
}

#[tokio::test]
async fn includefield() {
    let app = fixture().await;
    let results = search(
        &app,
        "/studies?includefield=PatientAge&StudyInstanceUID=1.1",
    )
    .await;
    assert_eq!(results[0]["00101010"]["Value"][0], "045Y");
    let all = search(&app, "/studies?includefield=all&StudyInstanceUID=1.1").await;
    assert_eq!(all[0]["00101010"]["Value"][0], "045Y");
    assert!(
        all[0].get("7FE00010").is_none(),
        "bulk data is not returned"
    );
    let matched = search(&app, "/studies?PatientAge=045Y").await;
    assert_eq!(
        matched[0]["00101010"]["Value"][0], "045Y",
        "matching keys are returned"
    );
}

#[tokio::test]
async fn series_searches() {
    let app = fixture().await;
    let in_study = search(&app, "/studies/1.1/series").await;
    assert_eq!(uids(&in_study, "0020000E"), ["1.1.1", "1.1.2"]);
    assert_eq!(in_study[0]["00201209"]["Value"][0], 2);
    assert_eq!(
        in_study[0]["00081190"]["Value"][0],
        format!("{BASE}/studies/1.1/series/1.1.1")
    );
    assert!(
        in_study[0].get("00100010").is_none(),
        "no study attributes under a study"
    );

    let all = search(&app, "/series?Modality=CT").await;
    assert_eq!(sorted(uids(&all, "0020000E")), ["1.1.1", "1.2.1"]);
    let smith = all
        .iter()
        .find(|r| r["0020000E"]["Value"][0] == "1.2.1")
        .unwrap();
    assert_eq!(smith["00100010"]["Value"][0]["Alphabetic"], "Smith^John");
    assert_eq!(
        uids(&search(&app, "/series?PatientID=PAT-3").await, "0020000E"),
        ["1.3.1"]
    );
    assert_eq!(
        uids(&search(&app, "/series?SeriesNumber=2").await, "0020000E"),
        ["1.1.2"]
    );
    assert!(search(&app, "/studies/9.9/series").await.is_empty());
}

#[tokio::test]
async fn instance_searches() {
    let app = fixture().await;
    let in_series = search(&app, "/studies/1.1/series/1.1.1/instances").await;
    assert_eq!(uids(&in_series, "00080018"), ["1.1.1.1", "1.1.1.2"]);
    assert!(
        in_series[0].get("00080060").is_none(),
        "no series attributes under a series"
    );
    assert_eq!(
        in_series[0]["00081190"]["Value"][0],
        format!("{BASE}/studies/1.1/series/1.1.1/instances/1.1.1.1")
    );
    let in_study = search(&app, "/studies/1.1/instances?Modality=SR").await;
    assert_eq!(uids(&in_study, "00080018"), ["1.1.2.1"]);
    assert_eq!(in_study[0]["00080060"]["Value"][0], "SR");

    let all = search(&app, "/instances?PatientName=SMITH*").await;
    assert_eq!(uids(&all, "00080018"), ["1.2.1.1"]);
    assert_eq!(all[0]["0020000D"]["Value"][0], "1.2");
    assert_eq!(all[0]["0020000E"]["Value"][0], "1.2.1");
    assert_eq!(
        uids(
            &search(&app, "/instances?InstanceNumber=2").await,
            "00080018"
        ),
        ["1.1.1.2"]
    );
}

#[tokio::test]
async fn invalid_searches() {
    let app = fixture().await;
    for query in [
        "NotAnAttribute=1",
        "limit=x",
        "fuzzymatching=maybe",
        "SeriesNumber=two",
        "StudyInstanceUID=1.*",
    ] {
        let reply = app.get(&format!("/series?{query}"), JSON).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
    }
    assert_eq!(
        app.get("/studies", "application/dicom+xml").await.status,
        StatusCode::NOT_ACCEPTABLE
    );
}

#[tokio::test]
async fn results_are_capped() {
    let app = app();
    let instances: Vec<Instance> = fortitude_testkit::series("1.5", "1.5.1", 51);
    app.stow("/studies", &instances.iter().collect::<Vec<_>>())
        .await;
    let reply = app.get("/instances", JSON).await;
    assert_eq!(reply.json().as_array().unwrap().len(), 50);
    assert!(
        reply.headers["warning"]
            .to_str()
            .unwrap()
            .starts_with("299 ")
    );
    let reply = app.get("/instances?limit=10", JSON).await;
    assert_eq!(reply.json().as_array().unwrap().len(), 10);
    assert!(reply.headers.get("warning").is_none());
    let reply = app.get("/instances?offset=50", JSON).await;
    assert_eq!(reply.json().as_array().unwrap().len(), 1);
}
