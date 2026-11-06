//! Assembly of the HTTP application.

use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router, middleware};
use fortitude_dicomweb::DicomWeb;
use fortitude_store::Store;
use metrics_exporter_prometheus::PrometheusHandle;
use tower_http::trace::TraceLayer;

use crate::auth::{self, Auth};
use crate::config::Config;
use crate::integrity::Checker;
use crate::{admin, metrics, ui};

/// The application: DICOMweb, WADO-URI and administration services, behind authentication
/// when configured, and the web explorer, for configured users only; `/health` and `/metrics`
/// stay public.
pub fn build(
    config: &Config,
    store: Arc<Store>,
    integrity: Arc<Checker>,
    metrics_handle: Option<PrometheusHandle>,
) -> Router {
    let dicomweb = DicomWeb {
        store: store.clone(),
        config: Arc::new(fortitude_dicomweb::Config {
            root: config.http.dicomweb_root.clone(),
            public_url: config.http.public_url.clone(),
            stow_concurrency: config.dicomweb.stow_concurrency,
            max_instance_size: config
                .dicomweb
                .max_instance_size_mib
                .saturating_mul(1 << 20),
            qido_max_results: config.dicomweb.qido_max_results,
        }),
    };

    let mut services = Router::new()
        .merge(fortitude_dicomweb::wado_uri_router(
            &config.http.wado_root,
            dicomweb.clone(),
        ))
        .nest(
            &config.http.dicomweb_root,
            fortitude_dicomweb::router(dicomweb),
        )
        .nest("/admin", admin::router(store.clone(), integrity));
    if let Some(auth) = Auth::new(&config.auth) {
        services = services.layer(middleware::from_fn_with_state(auth, auth::require));
    }
    // Only for users: without any, it is not served.
    if let Some(auth) = Auth::users(&config.auth).filter(|_| config.http.ui) {
        services = services.merge(
            ui::router(store, &config.http.dicomweb_root, &config.http.wado_root)
                .layer(middleware::from_fn_with_state(auth, auth::require)),
        );
    }
    // Outermost, so that rejected requests are counted too.
    services = services.layer(middleware::from_fn(metrics::track));

    let mut app = Router::new().route("/health", get(health)).merge(services);
    if let Some(handle) = metrics_handle {
        app = app.route("/metrics", get(metrics::render).with_state(handle));
    }
    app.layer(TraceLayer::new_for_http())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use std::sync::OnceLock;

    use tower::ServiceExt;

    use super::*;
    use crate::config::{Token, User};

    struct Harness {
        app: Router,
        _dir: tempfile::TempDir,
    }

    /// Metrics are recorded while futures are polled, so tests share one global recorder.
    fn metrics_handle() -> PrometheusHandle {
        static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
        HANDLE
            .get_or_init(|| {
                let recorder = metrics::recorder();
                let handle = recorder.handle();
                ::metrics::set_global_recorder(recorder).expect("first recorder");
                handle
            })
            .clone()
    }

    fn harness(configure: impl FnOnce(&mut Config)) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.path = dir.path().to_owned();
        configure(&mut config);
        let store = Store::open(fortitude_store::StoreOptions {
            path: config.storage.path.clone(),
            fsync: false,
            files: fortitude_testkit::file_backend(),
            index: fortitude_testkit::postgres_index().map_or(
                fortitude_store::IndexBackend::Sqlite,
                fortitude_store::IndexBackend::Postgres,
            ),
            index_readers: 2,
            inline_binary_max: 64,
        })
        .unwrap();
        let integrity = Checker::new(store.clone(), config.integrity.clone());
        let app = build(&config, store, integrity, Some(metrics_handle()));
        Harness { app, _dir: dir }
    }

    impl Harness {
        async fn send(&self, request: Request<Body>) -> (StatusCode, String) {
            let response = self.app.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        }

        async fn stow(&self, authorization: Option<&str>) -> StatusCode {
            let instance = fortitude_testkit::Instance::new("1.2", "1.2.3", "1.2.3.4");
            self.stow_instances(&[instance], authorization).await
        }

        async fn stow_instances(
            &self,
            instances: &[fortitude_testkit::Instance],
            authorization: Option<&str>,
        ) -> StatusCode {
            let mut body = Vec::new();
            for instance in instances {
                body.extend(b"--b\r\nContent-Type: application/dicom\r\n\r\n");
                body.extend(instance.to_bytes());
                body.extend(b"\r\n");
            }
            body.extend(b"--b--\r\n");
            let mut request = Request::post("/dicom-web/studies").header(
                header::CONTENT_TYPE,
                "multipart/related; type=\"application/dicom\"; boundary=b",
            );
            if let Some(authorization) = authorization {
                request = request.header(header::AUTHORIZATION, authorization);
            }
            self.send(request.body(Body::from(body)).unwrap()).await.0
        }
    }

    fn get(path: &str, authorization: Option<&str>) -> Request<Body> {
        let mut request = Request::get(path);
        if let Some(authorization) = authorization {
            request = request.header(header::AUTHORIZATION, authorization);
        }
        request.body(Body::empty()).unwrap()
    }

    fn basic(user: &str, password: &str) -> String {
        format!("Basic {}", BASE64.encode(format!("{user}:{password}")))
    }

    /// Configures a user, which the web explorer requires.
    fn with_user(config: &mut Config) {
        config.auth.users = vec![User {
            name: "user".into(),
            password: "pass".into(),
            read_only: false,
        }];
    }

    fn user() -> String {
        basic("user", "pass")
    }

    #[tokio::test]
    async fn integrity_check_on_request() {
        let h = harness(|_| {});
        assert_eq!(h.stow(None).await, StatusCode::OK);
        let (status, body) = h.send(get("/admin/integrity", None)).await;
        assert_eq!(status, StatusCode::OK);
        let state: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(state, serde_json::json!({ "running": false, "last": null }));

        let start = || {
            Request::post("/admin/integrity?remove_orphans=false")
                .body(Body::empty())
                .unwrap()
        };
        let (status, body) = h.send(start()).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let state: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(state["running"], true);
        assert_eq!(h.send(start()).await.0, StatusCode::CONFLICT);
        let state = loop {
            let (_, body) = h.send(get("/admin/integrity", None)).await;
            let state: serde_json::Value = serde_json::from_str(&body).unwrap();
            if state["running"] == false {
                break state;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        let report = &state["last"];
        assert_eq!(report["remove_orphans"], false);
        assert_eq!(
            (&report["files"], &report["instances"]),
            (&1.into(), &1.into())
        );
        assert_eq!(report["missing"], 0);
        assert_eq!(report["problems"], serde_json::json!([]));
        assert!(report.get("study_uid").is_none());

        // A study, or a series, is checked at once.
        let check = |query: &str| {
            Request::post(format!("/admin/integrity?{query}"))
                .body(Body::empty())
                .unwrap()
        };
        let (status, body) = h.send(check("study=1.2")).await;
        assert_eq!(status, StatusCode::OK);
        let report: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(report["study_uid"], "1.2");
        assert_eq!(
            (&report["files"], &report["missing"]),
            (&1.into(), &0.into())
        );
        let (status, body) = h.send(check("study=1.2&series=1.2.3")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let report: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(report["series_uid"], "1.2.3");
        assert_eq!(h.send(check("study=9.9")).await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            h.send(check("study=1.2&series=9.9")).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            h.send(check("series=1.2.3")).await.0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn open_without_credentials() {
        let h = harness(|_| {});
        assert_eq!(h.stow(None).await, StatusCode::OK);
        let (status, body) = h.send(get("/admin/statistics", None)).await;
        assert_eq!(status, StatusCode::OK);
        let statistics: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(statistics["instances"], 1);
        assert_eq!(statistics["studies"], 1);
        let delete = Request::delete("/dicom-web/studies/1.2")
            .body(Body::empty())
            .unwrap();
        assert_eq!(h.send(delete).await.0, StatusCode::NO_CONTENT);
        let delete = Request::delete("/dicom-web/studies/1.2")
            .body(Body::empty())
            .unwrap();
        assert_eq!(h.send(delete).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn credentials_and_read_only_access() {
        let h = harness(|c| {
            c.auth.users = vec![
                User {
                    name: "admin".into(),
                    password: "s3cret".into(),
                    read_only: false,
                },
                User {
                    name: "viewer".into(),
                    password: "look".into(),
                    read_only: true,
                },
            ];
            c.auth.tokens = vec![Token {
                token: "t0ken".into(),
                read_only: false,
            }];
        });
        assert_eq!(h.send(get("/health", None)).await.0, StatusCode::OK);
        let (status, _) = h.send(get("/dicom-web/studies", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            h.send(get("/dicom-web/studies", Some(&basic("admin", "wrong"))))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            h.send(get("/wado?requestType=WADO", Some("Bearer nope")))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );

        assert_eq!(
            h.stow(Some(&basic("viewer", "look"))).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            h.stow(Some(&basic("admin", "s3cret"))).await,
            StatusCode::OK
        );
        assert_eq!(
            h.send(get("/dicom-web/studies", Some(&basic("viewer", "look"))))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            h.send(get("/admin/statistics", Some("Bearer t0ken")))
                .await
                .0,
            StatusCode::OK
        );
        let start_integrity = Request::post("/admin/integrity")
            .header(header::AUTHORIZATION, basic("viewer", "look"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(h.send(start_integrity).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn web_explorer() {
        let user = user();
        let user = Some(user.as_str());
        let h = harness(|c| {
            with_user(c);
            c.http.dicomweb_root = "/dw".into();
        });
        let response = h.app.clone().oneshot(get("/", user)).await.unwrap();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers()[header::LOCATION], "/ui/");
        let (status, body) = h.send(get("/ui/", user)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains(r#"<meta name="dicomweb-root" content="/dw">"#),
            "{body}"
        );
        assert!(body.contains(r#"<meta name="wado-root" content="/wado">"#));
        for asset in ["/ui/app.js", "/ui/style.css"] {
            assert_eq!(h.send(get(asset, user)).await.0, StatusCode::OK);
        }

        let (status, body) = h
            .send(get(
                "/ui/api/names?tags=00100010,7FE00010,00091010,zz&uids=1.2.840.10008.5.1.4.1.1.2,1.2.3",
                user,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
        let names: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            names,
            serde_json::json!({
                "tags": { "00100010": "PatientName", "7FE00010": "PixelData" },
                "uids": { "1.2.840.10008.5.1.4.1.1.2": "CT Image Storage" },
            })
        );

        let h = harness(|c| {
            with_user(c);
            c.http.ui = false;
        });
        assert_eq!(h.send(get("/ui/", user)).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn archives() {
        use dicom_core::VR;
        use dicom_dictionary_std::tags;
        use fortitude_testkit::Instance;

        let user = user();
        let h = harness(with_user);
        let instances = [
            Instance::new("1.1", "1.1.1", "1.1.1.1"),
            Instance::new("1.1", "1.1.1", "1.1.1.2").with(|i| i.instance_number = Some(2)),
            Instance::new("1.1", "1.1.2", "1.1.2.1").with(|i| i.series_number = Some(2)),
            Instance::new("1.2", "1.2.1", "1.2.1.1").element(
                tags::ISSUER_OF_PATIENT_ID,
                VR::LO,
                "HOSP",
            ),
            Instance::new("2.1", "2.1.1", "2.1.1.1").with(|i| i.patient_id = "PAT/2".into()),
            // Streamed rather than read at once.
            Instance::new("3.1", "3.1.1", "3.1.1.1").with(|i| {
                i.patient_id = "PAT-3".into();
                (i.rows, i.columns) = (2048, 2100);
            }),
        ];
        assert_eq!(
            h.stow_instances(&instances, Some(&user)).await,
            StatusCode::OK
        );

        let archive = |query: &str| {
            let request = get(&format!("/ui/api/archive?{query}"), Some(&user));
            let app = h.app.clone();
            async move {
                let response = app.oneshot(request).await.unwrap();
                let status = response.status();
                let headers = response.headers().clone();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (status, headers, body)
            }
        };
        let entries = |body: &[u8]| -> Vec<String> {
            crate::archive::tests::read(body)
                .into_iter()
                .map(|(name, data)| {
                    let sop = name.rsplit('/').next().unwrap().trim_end_matches(".dcm");
                    let instance = instances.iter().find(|i| i.sop_uid == sop).unwrap();
                    assert_eq!(data, instance.to_bytes(), "{name}");
                    name
                })
                .collect()
        };

        let (status, headers, body) = archive("study=1.1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/zip");
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            "attachment; filename=\"1.1.zip\""
        );
        assert_eq!(headers[header::CONTENT_LENGTH], body.len().to_string());
        assert_eq!(
            entries(&body),
            [
                "1.1/1.1.1/1.1.1.1.dcm",
                "1.1/1.1.1/1.1.1.2.dcm",
                "1.1/1.1.2/1.1.2.1.dcm"
            ]
        );

        let (status, headers, body) = archive("study=1.1&series=1.1.2").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            "attachment; filename=\"1.1.2.zip\""
        );
        assert_eq!(entries(&body), ["1.1/1.1.2/1.1.2.1.dcm"]);

        let (status, _, body) = archive("patient=PAT-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(entries(&body).len(), 4);
        let (_, _, body) = archive("patient=PAT-1&issuer=HOSP").await;
        assert_eq!(entries(&body), ["1.2/1.2.1/1.2.1.1.dcm"]);
        let (status, headers, body) = archive("patient=PAT%2F2").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            "attachment; filename=\"PAT_2.zip\""
        );
        assert_eq!(entries(&body), ["2.1/2.1.1/2.1.1.1.dcm"]);
        let (status, headers, body) = archive("study=3.1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_LENGTH], body.len().to_string());
        assert_eq!(entries(&body), ["3.1/3.1.1/3.1.1.1.dcm"]);

        for query in [
            "study=9",
            "study=1.1&series=1.2.1",
            "patient=PAT-1&issuer=X",
        ] {
            assert_eq!(archive(query).await.0, StatusCode::NOT_FOUND, "{query}");
        }
        for query in ["", "series=1.1.1", "patient=", "study=1.1&patient=PAT-1"] {
            assert_eq!(archive(query).await.0, StatusCode::BAD_REQUEST, "{query}");
        }
    }

    #[tokio::test]
    async fn web_explorer_requires_a_user() {
        let h = harness(|c| {
            c.auth.users = vec![User {
                name: "viewer".into(),
                password: "look".into(),
                read_only: true,
            }];
            c.auth.tokens = vec![Token {
                token: "t0ken".into(),
                read_only: false,
            }];
        });
        let response = h.app.clone().oneshot(get("/ui/", None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .starts_with("Basic ")
        );
        assert_eq!(
            h.send(get("/ui/", Some(&basic("viewer", "look")))).await.0,
            StatusCode::OK
        );
        for path in ["/", "/ui/", "/ui/app.js", "/ui/api/archive?study=1.2"] {
            assert_eq!(
                h.send(get(path, Some("Bearer t0ken"))).await.0,
                StatusCode::UNAUTHORIZED,
                "{path}"
            );
        }
        assert_eq!(
            h.send(get("/ui/api/archive?study=1.2", None)).await.0,
            StatusCode::UNAUTHORIZED
        );
        // Tokens still open the other services.
        assert_eq!(
            h.send(get("/dicom-web/studies", Some("Bearer t0ken")))
                .await
                .0,
            StatusCode::OK
        );

        // Without users, the explorer is not served, with or without other credentials.
        let h = harness(|_| {});
        assert_eq!(h.send(get("/ui/", None)).await.0, StatusCode::NOT_FOUND);
        assert_eq!(h.send(get("/", None)).await.0, StatusCode::NOT_FOUND);
        let h = harness(|c| {
            c.auth.tokens = vec![Token {
                token: "t0ken".into(),
                read_only: false,
            }];
        });
        assert_eq!(
            h.send(get("/ui/", Some("Bearer t0ken"))).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn metrics_are_exported() {
        let h = harness(|_| {});
        h.stow(None).await;
        h.send(get("/dicom-web/studies?PatientID=x", None)).await;
        let (status, body) = h.send(get("/metrics", None)).await;
        assert_eq!(status, StatusCode::OK);
        // Other tests record into the same recorder: check series, not counts.
        assert!(body.contains(r#"fortitude_http_requests_total{method="GET",route="/dicom-web/studies",status="200"}"#), "{body}");
        assert!(
            body.contains(r#"fortitude_stow_instances_total{outcome="stored"}"#),
            "{body}"
        );
        assert!(
            body.contains("fortitude_http_request_duration_seconds_bucket"),
            "{body}"
        );
    }
}
