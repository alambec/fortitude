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
use crate::modalities::{self, Modalities};
use crate::retention::Retention;
use crate::{admin, metrics, ui};

/// The application: DICOMweb, WADO-URI and administration services, behind authentication
/// when configured, and the web explorer, for configured users only; `/health` and `/metrics`
/// stay public.
pub fn build(
    config: &Config,
    store: Arc<Store>,
    integrity: Arc<Checker>,
    retention: Arc<Retention>,
    modalities: Arc<Modalities>,
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
        .nest(
            "/admin",
            admin::router(admin::Admin {
                store: store.clone(),
                integrity,
                retention,
            })
            .merge(modalities::router(modalities)),
        );
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
        store: Arc<Store>,
        dimse: Arc<fortitude_dimse::Config>,
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
            location_cache_bytes: 1 << 20,
            location_cache_ttl: std::time::Duration::from_secs(60),
            inline_binary_max: 64,
        })
        .unwrap();
        let integrity = Checker::new(store.clone(), config.integrity.clone());
        let retention = Retention::new(store.clone(), config.retention.clone());
        let dimse = Arc::new(config.dimse.service(1 << 20).unwrap());
        let modalities = Modalities::new(store.clone(), dimse.clone(), &config.dimse);
        let app = build(
            &config,
            store.clone(),
            integrity,
            retention,
            modalities,
            Some(metrics_handle()),
        );
        Harness {
            app,
            store,
            dimse,
            _dir: dir,
        }
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
        // Checksums are verified when configured, or asked for.
        assert_eq!(report["verify_checksums"], false);
        assert_eq!(report["checksums_verified"], 0);
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
        // Unless asked not to.
        assert_eq!(
            (&report["verify_checksums"], &report["checksums_verified"]),
            (&true.into(), &1.into())
        );
        let (_, body) = h.send(check("study=1.2&verify_checksums=false")).await;
        let report: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(report["checksums_verified"], 0);
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
    async fn retention_on_request() {
        let post = |query: &str| {
            Request::post(format!("/admin/retention{query}"))
                .body(Body::empty())
                .unwrap()
        };
        let finished = |h: &Harness| {
            let h = h.app.clone();
            async move {
                loop {
                    let response = h.clone().oneshot(get("/admin/retention", None)).await;
                    let body = axum::body::to_bytes(response.unwrap().into_body(), usize::MAX);
                    let state: serde_json::Value =
                        serde_json::from_slice(&body.await.unwrap()).unwrap();
                    if state["running"] == false {
                        return state["last"].clone();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        };

        // Not configured: dry runs only, with an age.
        let h = harness(|_| {});
        assert_eq!(h.stow(None).await, StatusCode::OK);
        assert_eq!(h.send(post("")).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            h.send(post("?dry_run=true&max_age_days=0")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            h.send(post("?max_age_days=1")).await.0,
            StatusCode::BAD_REQUEST
        );
        let (status, _) = h.send(post("?dry_run=true&max_age_days=1")).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let report = finished(&h).await;
        assert_eq!(report["dry_run"], true);
        assert_eq!(report["max_age_days"], 1.0);
        assert_eq!(
            (&report["studies"], &report["expired"]),
            (&0.into(), &serde_json::json!([]))
        );

        let h = harness(|c| c.retention.max_age_days = 3650);
        assert_eq!(h.stow(None).await, StatusCode::OK);
        assert_eq!(h.send(post("")).await.0, StatusCode::ACCEPTED);
        let report = finished(&h).await;
        assert_eq!(
            (&report["dry_run"], &report["studies"]),
            (&false.into(), &0.into())
        );
        let (_, body) = h.send(get("/admin/statistics", None)).await;
        let statistics: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(statistics["studies"], 1);
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
        // The source-code offer of the AGPL.
        let source = format!(
            r#"href="https://github.com/alambec/fortitude/tree/v{}""#,
            env!("CARGO_PKG_VERSION")
        );
        assert!(body.contains(&source), "{body}");
        assert!(!body.contains("{{"), "{body}");
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

    fn json(method: &str, path: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn modalities() {
        use tokio::net::TcpListener;

        let local_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = local_listener.local_addr().unwrap();
        // A remote Fortitude with one study, to which the local one is a move destination.
        let remote = harness(|c| {
            c.dimse.ae_title = "REMOTE".into();
            c.dimse.peers = vec![crate::config::Peer {
                ae_title: "LOCAL".into(),
                host: "127.0.0.1".into(),
                port: local.port(),
                description: String::new(),
            }];
        });
        assert_eq!(remote.stow(None).await, StatusCode::OK);
        let remote_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_port = remote_listener.local_addr().unwrap().port();
        let dimse = |h: &Harness| fortitude_dimse::Dimse {
            store: h.store.clone(),
            config: h.dimse.clone(),
        };
        tokio::spawn(fortitude_dimse::serve(
            remote_listener,
            dimse(&remote),
            std::future::pending(),
        ));
        let h = harness(|c| {
            c.dimse.ae_title = "LOCAL".into();
            c.dimse.listen = Some(local);
            c.dimse.timeout_secs = 5;
            c.dimse.peers = vec![crate::config::Peer {
                ae_title: "CONFIGURED".into(),
                host: "127.0.0.1".into(),
                port: 1,
                description: "from the file".into(),
            }];
        });
        tokio::spawn(fortitude_dimse::serve(
            local_listener,
            dimse(&h),
            std::future::pending(),
        ));
        let body = |text: String| serde_json::from_str::<serde_json::Value>(&text).unwrap();

        let (status, list) = h.send(get("/admin/modalities", None)).await;
        assert_eq!(status, StatusCode::OK);
        let list = body(list);
        assert_eq!(
            (&list["ae_title"], &list["listening"]),
            (&"LOCAL".into(), &true.into())
        );
        assert_eq!(list["modalities"][0]["source"], "config");
        assert_eq!(list["modalities"][0]["description"], "from the file");

        let registration = serde_json::json!({ "host": "127.0.0.1", "port": remote_port });
        let (status, _) = h
            .send(json(
                "PUT",
                "/admin/modalities/REMOTE",
                registration.clone(),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _) = h
            .send(json(
                "PUT",
                "/admin/modalities/REMOTE",
                registration.clone(),
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
        for (path, registration) in [
            (
                "/admin/modalities/REMOTE",
                serde_json::json!({ "host": "127.0.0.1", "port": 0 }),
            ),
            (
                "/admin/modalities/REMOTE",
                serde_json::json!({ "host": "a b", "port": 104 }),
            ),
            (
                "/admin/modalities/REMOTE",
                serde_json::json!({ "port": 104 }),
            ),
            (
                "/admin/modalities/A_VERY_LONG_AE_TITLE",
                registration.clone(),
            ),
        ] {
            let (status, text) = h.send(json("PUT", path, registration)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {text}");
        }
        let (status, _) = h
            .send(json(
                "PUT",
                "/admin/modalities/CONFIGURED",
                registration.clone(),
            ))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let offline = serde_json::json!({ "host": "127.0.0.1", "port": 1, "description": "off" });
        let (status, _) = h
            .send(json("PUT", "/admin/modalities/OFF%20LINE", offline))
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let (_, list) = h.send(get("/admin/modalities", None)).await;
        let list = body(list);
        let titles: Vec<_> = list["modalities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["ae_title"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(titles, ["CONFIGURED", "OFF LINE", "REMOTE"]);
        // Registered modalities are peers of the DIMSE service too.
        assert!(h.dimse.peers.get("REMOTE").is_some());

        let (status, echo) = h.send(get("/admin/modalities/REMOTE/echo", None)).await;
        assert_eq!(status, StatusCode::OK, "{echo}");
        assert!(body(echo)["milliseconds"].is_number());
        let (status, reason) = h.send(get("/admin/modalities/OFF%20LINE/echo", None)).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(reason.contains("OFF LINE at 127.0.0.1:1"), "{reason}");
        assert_eq!(
            h.send(get("/admin/modalities/NOBODY/echo", None)).await.0,
            StatusCode::NOT_FOUND
        );

        let (status, found) = h
            .send(get(
                "/admin/modalities/REMOTE/find?level=STUDY&PatientID=PAT*&includefield=00080090",
                None,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{found}");
        let found = body(found);
        assert_eq!(found["truncated"], false);
        let study = &found["matches"][0];
        assert_eq!(study["0020000D"]["Value"][0], "1.2");
        assert_eq!(study["00201208"]["Value"][0], 1);
        assert!(study.get("00080090").is_some());
        let (status, found) = h
            .send(get(
                "/admin/modalities/REMOTE/find?level=SERIES&StudyInstanceUID=1.2",
                None,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body(found)["matches"][0]["0020000E"]["Value"][0], "1.2.3");
        for query in [
            "PatientID=1",
            "level=STUDY&Nonsense=1",
            "level=FRAME",
            "level=STUDY&limit=x",
        ] {
            let path = format!("/admin/modalities/REMOTE/find?{query}");
            assert_eq!(
                h.send(get(&path, None)).await.0,
                StatusCode::BAD_REQUEST,
                "{query}"
            );
        }

        // Retrieval into the local store.
        let request =
            serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": "1.2" } });
        let (status, job) = h
            .send(json("POST", "/admin/modalities/REMOTE/move", request))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{job}");
        let job = body(job);
        assert_eq!(
            (&job["destination"], &job["state"]),
            (&"LOCAL".into(), &"running".into())
        );
        let id = job["id"].as_u64().unwrap();
        let job = loop {
            let (_, moves) = h.send(get("/admin/moves", None)).await;
            let job = body(moves)["moves"][0].clone();
            if job["state"] != "running" {
                break job;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(job["id"], id);
        assert_eq!(job["state"], "succeeded", "{job}");
        assert_eq!((&job["completed"], &job["failed"]), (&1.into(), &0.into()));
        assert_eq!(job["status"], "0000");
        let (_, statistics) = h.send(get("/admin/statistics", None)).await;
        assert_eq!(body(statistics)["instances"], 1);
        let cancel = |id: u64| {
            Request::post(format!("/admin/moves/{id}/cancel"))
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(h.send(cancel(id)).await.0, StatusCode::CONFLICT);
        assert_eq!(h.send(cancel(999)).await.0, StatusCode::NOT_FOUND);

        // A destination the modality does not know.
        let request = serde_json::json!({
            "level": "SERIES",
            "keys": { "StudyInstanceUID": "1.2", "SeriesInstanceUID": "1.2.3" },
            "destination": "ELSEWHERE",
        });
        let (status, _) = h
            .send(json("POST", "/admin/modalities/REMOTE/move", request))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let job = loop {
            let (_, moves) = h.send(get("/admin/moves", None)).await;
            let job = body(moves)["moves"][0].clone();
            if job["state"] != "running" {
                break job;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(
            (&job["state"], &job["status"]),
            (&"failed".into(), &"A801".into())
        );
        for request in [
            serde_json::json!({ "level": "SERIES", "keys": { "StudyInstanceUID": "1.2" } }),
            serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": "" } }),
            serde_json::json!({ "level": "STUDY", "keys": {} , "other": 1 }),
        ] {
            let (status, text) = h
                .send(json("POST", "/admin/modalities/REMOTE/move", request))
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        }

        // Sending the retrieved study back (C-STORE): the remote has it already, which is
        // not a failure.
        let wait_send = || async {
            loop {
                let (_, sends) = h.send(get("/admin/sends", None)).await;
                let job = body(sends)["sends"][0].clone();
                if job["state"] != "running" {
                    break job;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        let request = serde_json::json!({
            "level": "SERIES",
            "keys": { "StudyInstanceUID": "1.2", "SeriesInstanceUID": "1.2.3\\9.9" },
        });
        let (status, job) = h
            .send(json("POST", "/admin/modalities/REMOTE/store", request))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{job}");
        let job = body(job);
        assert_eq!(
            (&job["modality"], &job["remaining"], &job["state"]),
            (&"REMOTE".into(), &1.into(), &"running".into())
        );
        assert!(job.get("destination").is_none());
        let id = job["id"].as_u64().unwrap();
        let job = wait_send().await;
        assert_eq!(job["id"], id);
        assert_eq!(job["state"], "succeeded", "{job}");
        assert_eq!((&job["completed"], &job["failed"]), (&1.into(), &0.into()));
        assert_eq!(job["failures"], serde_json::json!([]));
        let cancel = |id: u64| {
            Request::post(format!("/admin/sends/{id}/cancel"))
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(h.send(cancel(id)).await.0, StatusCode::CONFLICT);
        assert_eq!(h.send(cancel(999)).await.0, StatusCode::NOT_FOUND);

        // A modality that cannot be reached.
        let request =
            serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": "1.2" } });
        let (status, _) = h
            .send(json("POST", "/admin/modalities/OFF%20LINE/store", request))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let job = wait_send().await;
        assert_eq!(job["state"], "failed", "{job}");
        assert!(
            job["error"]
                .as_str()
                .unwrap()
                .contains("cannot associate with OFF LINE"),
            "{job}"
        );
        assert_eq!(job["remaining"], 1);

        let (status, _) = h
            .send(json(
                "POST",
                "/admin/modalities/REMOTE/store",
                serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": "9.9" } }),
            ))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            h.send(json(
                "POST",
                "/admin/modalities/NOBODY/store",
                serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": "1.2" } }),
            ))
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        for request in [
            serde_json::json!({ "level": "SERIES", "keys": { "StudyInstanceUID": "1.2" } }),
            serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": " " } }),
            serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": "1.2", "Modality": "CT" } }),
            serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": "1.2" }, "destination": "X" }),
            serde_json::json!({ "level": "FRAME", "keys": { "StudyInstanceUID": "1.2" } }),
        ] {
            let (status, text) = h
                .send(json("POST", "/admin/modalities/REMOTE/store", request))
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        }

        let remove = |path: &str| Request::delete(path).body(Body::empty()).unwrap();
        assert_eq!(
            h.send(remove("/admin/modalities/REMOTE")).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            h.send(remove("/admin/modalities/REMOTE")).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            h.send(remove("/admin/modalities/CONFIGURED")).await.0,
            StatusCode::CONFLICT
        );
        assert!(h.dimse.peers.get("REMOTE").is_none());
        assert_eq!(
            h.send(get("/admin/modalities/REMOTE/echo", None)).await.0,
            StatusCode::NOT_FOUND
        );

        // Without the DIMSE service, Fortitude cannot be the destination.
        let h = harness(|_| {});
        let (status, _) = h
            .send(json("PUT", "/admin/modalities/REMOTE", registration))
            .await;
        assert_eq!(status, StatusCode::CREATED);
        let request =
            serde_json::json!({ "level": "STUDY", "keys": { "StudyInstanceUID": "1.2" } });
        let (status, _) = h
            .send(json("POST", "/admin/modalities/REMOTE/move", request))
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
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
