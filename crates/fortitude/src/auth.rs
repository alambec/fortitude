//! HTTP authentication: Basic (users) and Bearer (tokens), each optionally read-only. The web
//! explorer accepts users only.
//!
//! Credentials are compared in constant time. Read-only credentials may only use `GET`,
//! `HEAD` and `OPTIONS`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use subtle::ConstantTimeEq;

use crate::config::AuthConfig;

#[derive(Clone)]
pub struct Auth(Arc<Credentials>);

struct Credentials {
    /// `name:password` of each user, with its read-only flag.
    basic: Vec<(Vec<u8>, bool)>,
    bearer: Vec<(Vec<u8>, bool)>,
}

impl Auth {
    /// `None` when no credential is configured: authentication is disabled.
    pub fn new(config: &AuthConfig) -> Option<Self> {
        let basic: Vec<_> = config
            .users
            .iter()
            .map(|u| {
                (
                    format!("{}:{}", u.name, u.password).into_bytes(),
                    u.read_only,
                )
            })
            .collect();
        let bearer: Vec<_> = config
            .tokens
            .iter()
            .map(|t| (t.token.clone().into_bytes(), t.read_only))
            .collect();
        (!basic.is_empty() || !bearer.is_empty())
            .then(|| Auth(Arc::new(Credentials { basic, bearer })))
    }

    /// Users only, without tokens; `None` when no user is configured.
    pub fn users(config: &AuthConfig) -> Option<Self> {
        Self::new(&AuthConfig {
            users: config.users.clone(),
            tokens: Vec::new(),
        })
    }

    /// The read-only flag of the request's credentials, or `None` when they are missing or
    /// unknown.
    fn check(&self, authorization: Option<&HeaderValue>) -> Option<bool> {
        let value = authorization?.to_str().ok()?;
        let (scheme, credentials) = value.split_once(' ')?;
        let (presented, known) = if scheme.eq_ignore_ascii_case("basic") {
            (BASE64.decode(credentials.trim()).ok()?, &self.0.basic)
        } else if scheme.eq_ignore_ascii_case("bearer") {
            (credentials.trim().as_bytes().to_vec(), &self.0.bearer)
        } else {
            return None;
        };
        // Compare with every credential, so that timing does not tell which one matched.
        known.iter().fold(None, |found, (secret, read_only)| {
            let matches: bool = secret.ct_eq(&presented).into();
            found.or(matches.then_some(*read_only))
        })
    }

    fn challenge(&self) -> &'static str {
        if self.0.basic.is_empty() {
            "Bearer"
        } else {
            "Basic realm=\"fortitude\", charset=\"UTF-8\""
        }
    }
}

pub async fn require(State(auth): State<Auth>, request: Request, next: Next) -> Response {
    let reading = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    match auth.check(request.headers().get(header::AUTHORIZATION)) {
        Some(true) if !reading => (StatusCode::FORBIDDEN, "read-only credentials").into_response(),
        Some(_) => next.run(request).await,
        None => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, auth.challenge())],
            "authentication required",
        )
            .into_response(),
    }
}
