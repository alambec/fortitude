//! Shared harness: a router on a temporary store, and request helpers.
#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use fortitude_dicomweb::{Config, DicomWeb};
use fortitude_store::{Store, StoreOptions};
use fortitude_testkit::Instance;
use tower::ServiceExt;

pub const BOUNDARY: &str = "test-boundary-42";
pub const HOST: &str = "pacs.test";
pub const BASE: &str = "http://pacs.test/dicom-web";

pub struct App {
    pub router: Router,
    pub store: Arc<Store>,
    _dir: tempfile::TempDir,
}

pub fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(StoreOptions {
        path: dir.path().to_owned(),
        fsync: false,
        index_readers: 2,
        inline_binary_max: 64,
    })
    .unwrap();
    let config = Config {
        root: "/dicom-web".into(),
        public_url: None,
        stow_concurrency: 4,
        max_instance_size: 1 << 24,
    };
    let router = fortitude_dicomweb::router(DicomWeb {
        store: store.clone(),
        config: Arc::new(config),
    });
    App {
        router,
        store,
        _dir: dir,
    }
}

pub fn multipart(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(
            format!("--{BOUNDARY}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(part);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "invalid JSON ({e}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub fn content_type(&self) -> &str {
        self.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }
}

impl App {
    pub async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        Reply {
            status,
            headers,
            body,
        }
    }

    pub async fn stow(&self, path: &str, instances: &[&Instance]) -> Reply {
        let parts: Vec<Vec<u8>> = instances.iter().map(|i| i.to_bytes()).collect();
        self.stow_raw(
            path,
            &format!("multipart/related; type=\"application/dicom\"; boundary={BOUNDARY}"),
            multipart(&parts),
        )
        .await
    }

    pub async fn stow_raw(&self, path: &str, content_type: &str, body: Vec<u8>) -> Reply {
        self.send(
            Request::post(path)
                .header("host", HOST)
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
    }

    pub async fn get(&self, path: &str, accept: &str) -> Reply {
        self.send(
            Request::get(path)
                .header("host", HOST)
                .header("accept", accept)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }
}
