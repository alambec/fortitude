//! Modalities: the remote application entities (PACS, scanners, workstations) Fortitude
//! verifies (C-ECHO), queries (C-FIND), retrieves from (C-MOVE) and sends stored instances to
//! (C-STORE), as a DIMSE service user.
//!
//! They are those of the configuration file (`dimse.peers`), which cannot be changed here,
//! and those registered through this API, kept in the index. Both are C-MOVE destinations of
//! Fortitude's own DIMSE service, and allowed to associate when unknown peers are not.
//!
//! Retrievals and sends run in the background, one association each; the last ones of each
//! kind are kept, with their progress, until the server stops.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use dicom_core::Tag;
use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_dictionary_std::{StandardDataDictionary, tags};
use dicom_object::InMemDicomObject;
use fortitude_dimse::Peer;
use fortitude_dimse::remote::{self, Level, RemoteError};
use fortitude_store::Store;
use fortitude_store::modalities::Modality;
use fortitude_store::retrieve::Selection;
use serde::{Deserialize, Serialize};

use crate::config::{self, DimseConfig};

/// Retrievals kept, running or not, and sends.
const MAX_TRANSFERS: usize = 100;
/// Matches of a query when the request does not say.
const DEFAULT_FIND_LIMIT: usize = 1000;

/// Return keys of a query at each level, besides the requested ones.
const RETURN_KEYS: &[(Level, &[Tag])] = &[
    (
        Level::Patient,
        &[
            tags::PATIENT_NAME,
            tags::PATIENT_ID,
            tags::ISSUER_OF_PATIENT_ID,
            tags::PATIENT_BIRTH_DATE,
            tags::PATIENT_SEX,
            tags::NUMBER_OF_PATIENT_RELATED_STUDIES,
        ],
    ),
    (
        Level::Study,
        &[
            tags::STUDY_INSTANCE_UID,
            tags::PATIENT_NAME,
            tags::PATIENT_ID,
            tags::PATIENT_BIRTH_DATE,
            tags::PATIENT_SEX,
            tags::STUDY_DATE,
            tags::STUDY_TIME,
            tags::ACCESSION_NUMBER,
            tags::STUDY_ID,
            tags::STUDY_DESCRIPTION,
            tags::REFERRING_PHYSICIAN_NAME,
            tags::MODALITIES_IN_STUDY,
            tags::NUMBER_OF_STUDY_RELATED_SERIES,
            tags::NUMBER_OF_STUDY_RELATED_INSTANCES,
        ],
    ),
    (
        Level::Series,
        &[
            tags::STUDY_INSTANCE_UID,
            tags::SERIES_INSTANCE_UID,
            tags::MODALITY,
            tags::SERIES_NUMBER,
            tags::SERIES_DESCRIPTION,
            tags::SERIES_DATE,
            tags::SERIES_TIME,
            tags::BODY_PART_EXAMINED,
            tags::NUMBER_OF_SERIES_RELATED_INSTANCES,
        ],
    ),
    (
        Level::Image,
        &[
            tags::STUDY_INSTANCE_UID,
            tags::SERIES_INSTANCE_UID,
            tags::SOP_INSTANCE_UID,
            tags::SOP_CLASS_UID,
            tags::INSTANCE_NUMBER,
            tags::ROWS,
            tags::COLUMNS,
            tags::NUMBER_OF_FRAMES,
        ],
    ),
];

/// The unique key a retrieval at each level requires.
fn unique_key(level: Level) -> Tag {
    match level {
        Level::Patient => tags::PATIENT_ID,
        Level::Study => tags::STUDY_INSTANCE_UID,
        Level::Series => tags::SERIES_INSTANCE_UID,
        Level::Image => tags::SOP_INSTANCE_UID,
    }
}

pub struct Modalities {
    store: Arc<Store>,
    /// The DIMSE configuration, whose peers this keeps up to date.
    dimse: Arc<fortitude_dimse::Config>,
    /// Peers of the configuration file, with their descriptions.
    configured: Vec<config::Peer>,
    /// Whether Fortitude's DIMSE service listens, to receive the instances it retrieves.
    listening: bool,
    /// Serializes changes, so that the peers follow the index.
    changes: tokio::sync::Mutex<()>,
    /// Retrievals (C-MOVE).
    moves: Transfers,
    /// Sends (C-STORE).
    sends: Transfers,
}

/// A modality, as listed.
#[derive(Debug, Clone, Serialize)]
struct Entry {
    ae_title: String,
    host: String,
    port: u16,
    description: String,
    /// `config` (read-only) or `registered`.
    source: &'static str,
}

impl Modalities {
    pub fn new(
        store: Arc<Store>,
        dimse: Arc<fortitude_dimse::Config>,
        config: &DimseConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            dimse,
            configured: config.peers.clone(),
            listening: config.listen.is_some(),
            changes: tokio::sync::Mutex::new(()),
            moves: Transfers::default(),
            sends: Transfers::default(),
        })
    }

    /// Add the registered modalities to the peers.
    pub async fn load(&self) -> Result<(), fortitude_store::index::IndexError> {
        let _changing = self.changes.lock().await;
        self.refresh().await
    }

    async fn refresh(&self) -> Result<(), fortitude_store::index::IndexError> {
        let entries = self.entries().await?;
        let peers = entries
            .into_iter()
            .map(|e| Peer {
                ae_title: e.ae_title,
                host: e.host,
                port: e.port,
            })
            .collect();
        self.dimse.peers.set(peers);
        Ok(())
    }

    /// Configured modalities, then registered ones (those with the AE title of a configured
    /// one are ignored).
    async fn entries(&self) -> Result<Vec<Entry>, fortitude_store::index::IndexError> {
        let mut entries: Vec<Entry> = self
            .configured
            .iter()
            .map(|p| Entry {
                ae_title: p.ae_title.clone(),
                host: p.host.clone(),
                port: p.port,
                description: p.description.clone(),
                source: "config",
            })
            .collect();
        for m in self.store.modalities().await? {
            if !self.is_configured(&m.ae_title) {
                entries.push(Entry {
                    ae_title: m.ae_title,
                    host: m.host,
                    port: m.port,
                    description: m.description,
                    source: "registered",
                });
            }
        }
        Ok(entries)
    }

    fn is_configured(&self, ae_title: &str) -> bool {
        self.configured.iter().any(|p| p.ae_title == ae_title)
    }
}

pub fn router(modalities: Arc<Modalities>) -> Router {
    Router::new()
        .route("/modalities", get(list))
        .route("/modalities/{ae}", put(register).delete(remove))
        .route("/modalities/{ae}/echo", get(echo))
        .route("/modalities/{ae}/find", get(find))
        .route("/modalities/{ae}/move", post(start_move))
        .route("/modalities/{ae}/store", post(start_send))
        .route("/moves", get(moves))
        .route("/moves/{id}/cancel", post(cancel_move))
        .route("/sends", get(sends))
        .route("/sends/{id}/cancel", post(cancel_send))
        .with_state(modalities)
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "modalities: index error");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

fn remote_error(e: &RemoteError) -> Response {
    let status = match e {
        RemoteError::Invalid(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, e.to_string()).into_response()
}

fn count(command: &'static str, result: &Result<impl Sized, RemoteError>) {
    let outcome = if result.is_ok() { "success" } else { "failure" };
    metrics::counter!("fortitude_dimse_scu_requests_total", "command" => command, "outcome" => outcome)
        .increment(1);
}

/// Fortitude's AE title, whether it can receive retrieved instances, and the modalities.
async fn list(State(modalities): State<Arc<Modalities>>) -> Response {
    match modalities.entries().await {
        Ok(entries) => Json(serde_json::json!({
            "ae_title": modalities.dimse.ae_title,
            "listening": modalities.listening,
            "modalities": entries,
        }))
        .into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    host: String,
    port: u16,
    #[serde(default)]
    description: String,
}

/// Register a modality, or change a registered one: `201 Created` or `200 OK`; `409
/// Conflict` for one of the configuration file.
async fn register(
    State(modalities): State<Arc<Modalities>>,
    Path(ae_title): Path<String>,
    body: Result<Json<Registration>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(registration) = match body {
        Ok(body) => body,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    let ae_title = ae_title.trim().to_owned();
    let host = registration.host.trim().to_owned();
    let description = registration.description.trim().to_owned();
    if !config::valid_ae_title(&ae_title) {
        return (
            StatusCode::BAD_REQUEST,
            "invalid AE title (1 to 16 ASCII characters, without backslash)",
        )
            .into_response();
    }
    if host.is_empty()
        || host.len() > 253
        || host
            .chars()
            .any(|c| c.is_whitespace() || c == '/' || c.is_control())
    {
        return (StatusCode::BAD_REQUEST, "invalid host").into_response();
    }
    if registration.port == 0 {
        return (StatusCode::BAD_REQUEST, "invalid port").into_response();
    }
    if description.chars().count() > 200 {
        return (
            StatusCode::BAD_REQUEST,
            "description longer than 200 characters",
        )
            .into_response();
    }
    if modalities.is_configured(&ae_title) {
        return (
            StatusCode::CONFLICT,
            "this modality is set in the configuration file",
        )
            .into_response();
    }
    let modality = Modality {
        ae_title,
        host,
        port: registration.port,
        description,
    };
    let _changing = modalities.changes.lock().await;
    let created = match modalities.store.put_modality(modality.clone()).await {
        Ok(created) => created,
        Err(e) => return internal(e),
    };
    if let Err(e) = modalities.refresh().await {
        return internal(e);
    }
    tracing::info!(
        ae_title = modality.ae_title,
        host = modality.host,
        port = modality.port,
        "modality {}",
        if created { "registered" } else { "updated" }
    );
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let entry = Entry {
        ae_title: modality.ae_title,
        host: modality.host,
        port: modality.port,
        description: modality.description,
        source: "registered",
    };
    (status, Json(entry)).into_response()
}

/// Remove a registered modality: `204`, `404` when it is not registered, `409` for one of the
/// configuration file.
async fn remove(
    State(modalities): State<Arc<Modalities>>,
    Path(ae_title): Path<String>,
) -> Response {
    if modalities.is_configured(&ae_title) {
        return (
            StatusCode::CONFLICT,
            "this modality is set in the configuration file",
        )
            .into_response();
    }
    let _changing = modalities.changes.lock().await;
    match modalities.store.delete_modality(&ae_title).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "unknown modality").into_response(),
        Err(e) => return internal(e),
    }
    if let Err(e) = modalities.refresh().await {
        return internal(e);
    }
    tracing::info!(ae_title, "modality removed");
    StatusCode::NO_CONTENT.into_response()
}

/// The peer with this AE title, or `404`.
#[allow(clippy::result_large_err)]
fn peer(modalities: &Modalities, ae_title: &str) -> Result<Peer, Response> {
    modalities
        .dimse
        .peers
        .get(ae_title)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "unknown modality").into_response())
}

/// C-ECHO: `200` with the round-trip time, association included, or `502` with the reason.
async fn echo(State(modalities): State<Arc<Modalities>>, Path(ae_title): Path<String>) -> Response {
    let peer = match peer(&modalities, &ae_title) {
        Ok(peer) => peer,
        Err(response) => return response,
    };
    let result = remote::echo(&modalities.dimse, &peer).await;
    count("C-ECHO", &result);
    match result {
        Ok(elapsed) => Json(serde_json::json!({
            "ae_title": peer.ae_title,
            "milliseconds": (elapsed.as_secs_f64() * 1000.0).round(),
        }))
        .into_response(),
        Err(e) => {
            tracing::info!(ae_title, error = %e, "C-ECHO failed");
            remote_error(&e)
        }
    }
}

/// An attribute by keyword (`PatientID`) or tag (`00100020`).
fn attribute(name: &str) -> Option<Tag> {
    if name.len() == 8
        && let Ok(value) = u32::from_str_radix(name, 16)
    {
        return Some(Tag((value >> 16) as u16, value as u16));
    }
    StandardDataDictionary.by_name(name).map(|e| e.tag())
}

/// `?level=STUDY&PatientID=…&StudyDate=20240101-&includefield=…&limit=…`: C-FIND at
/// `level` (`PATIENT`, `STUDY`, `SERIES` or `IMAGE`), with the other parameters as matching
/// keys (keywords or tags, values as in C-FIND: `*` and `?` wildcards, date ranges), the
/// usual return keys of the level and those of `includefield`. Answers `{"matches": […],
/// "truncated": …}`, matches in DICOM JSON.
async fn find(
    State(modalities): State<Arc<Modalities>>,
    Path(ae_title): Path<String>,
    RawQuery(raw): RawQuery,
) -> Response {
    let peer = match peer(&modalities, &ae_title) {
        Ok(peer) => peer,
        Err(response) => return response,
    };
    let mut level = None;
    let mut limit = DEFAULT_FIND_LIMIT;
    let mut keys: Vec<(Tag, String)> = Vec::new();
    for (name, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        let value = value.trim();
        match name.as_ref() {
            "level" => match Level::parse(value) {
                Some(parsed) => level = Some(parsed),
                None => return (StatusCode::BAD_REQUEST, "invalid level").into_response(),
            },
            "limit" => match value.parse() {
                Ok(parsed) => limit = parsed,
                Err(_) => return (StatusCode::BAD_REQUEST, "invalid limit").into_response(),
            },
            "includefield" => {
                for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    match attribute(name) {
                        Some(tag) => keys.push((tag, String::new())),
                        None => {
                            return (StatusCode::BAD_REQUEST, format!("unknown attribute {name}"))
                                .into_response();
                        }
                    }
                }
            }
            name => match attribute(name) {
                Some(tag) => keys.push((tag, value.to_owned())),
                None => {
                    return (StatusCode::BAD_REQUEST, format!("unknown attribute {name}"))
                        .into_response();
                }
            },
        }
    }
    let Some(level) = level else {
        return (StatusCode::BAD_REQUEST, "level is required").into_response();
    };
    let max = modalities.dimse.max_find_results;
    if max > 0 && (limit == 0 || limit > max) {
        limit = max;
    }
    let defaults = RETURN_KEYS
        .iter()
        .find(|(l, _)| *l == level)
        .map_or(&[][..], |(_, keys)| keys);
    let mut identifier = InMemDicomObject::new_empty();
    for &tag in defaults {
        identifier.put(remote::key(tag, "").expect("standard attribute"));
    }
    for (tag, value) in &keys {
        // A matching key replaces the return key of the same attribute, not the reverse.
        if value.is_empty() && identifier.get(*tag).is_some() {
            continue;
        }
        match remote::key(*tag, value) {
            Some(element) => identifier.put(element),
            None => {
                return (StatusCode::BAD_REQUEST, format!("unknown attribute {tag}"))
                    .into_response();
            }
        };
    }
    let result = remote::find(&modalities.dimse, &peer, level, identifier, limit).await;
    count("C-FIND", &result);
    let found = match result {
        Ok(found) => found,
        Err(e) => {
            tracing::info!(ae_title, error = %e, "C-FIND failed");
            return remote_error(&e);
        }
    };
    match dicom_json::to_value(&found.matches[..]) {
        Ok(mut matches) => {
            numbers(&mut matches);
            Json(serde_json::json!({
                "matches": matches,
                "truncated": found.truncated,
            }))
            .into_response()
        }
        Err(e) => internal(format!("cannot encode the matches: {e}")),
    }
}

/// Integer and decimal strings (IS, DS) as JSON numbers, as the DICOM JSON model wants them
/// (PS3.18 §F.2.3), in DICOM JSON objects and their sequences.
fn numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => items.iter_mut().for_each(numbers),
        serde_json::Value::Object(object) => {
            for attribute in object.values_mut() {
                let vr = attribute
                    .get("vr")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let numeric = matches!(vr, "IS" | "DS");
                let sequence = vr == "SQ";
                let Some(serde_json::Value::Array(values)) = attribute.get_mut("Value") else {
                    continue;
                };
                for v in values {
                    if sequence {
                        numbers(v);
                    } else if numeric && let serde_json::Value::String(text) = v {
                        let number = text
                            .trim()
                            .parse::<i64>()
                            .map(serde_json::Value::from)
                            .ok()
                            .or_else(|| {
                                text.trim()
                                    .parse::<f64>()
                                    .ok()
                                    .and_then(serde_json::Number::from_f64)
                                    .map(serde_json::Value::Number)
                            });
                        if let Some(number) = number {
                            *v = number;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransferState {
    Running,
    /// Every sub-operation (instance) succeeded.
    Succeeded,
    /// Some sub-operations failed, or completed with warnings.
    Partial,
    Cancelled,
    Failed,
}

/// An instance a send could not store.
#[derive(Debug, Clone, Serialize)]
struct FailedInstance {
    sop_instance_uid: String,
    reason: String,
}

/// A retrieval (C-MOVE from `modality` to `destination`) or a send (C-STORE to `modality`).
#[derive(Debug, Clone, Serialize)]
struct Transfer {
    id: u64,
    modality: String,
    /// Retrievals only.
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<String>,
    level: &'static str,
    keys: BTreeMap<String, String>,
    state: TransferState,
    /// Final status of a retrieval (hexadecimal).
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    remaining: Option<u16>,
    completed: u16,
    failed: u16,
    warning: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Sends only: the first instances that were not stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    failures: Option<Vec<FailedInstance>>,
    started_at: u64,
    finished_at: Option<u64>,
    #[serde(skip)]
    cancel: Arc<AtomicBool>,
}

impl Transfer {
    fn new(modality: &str, level: Level, keys: BTreeMap<String, String>) -> Self {
        Self {
            id: 0,
            modality: modality.to_owned(),
            destination: None,
            level: level.as_str(),
            keys,
            state: TransferState::Running,
            status: None,
            remaining: None,
            completed: 0,
            failed: 0,
            warning: 0,
            error: None,
            failures: None,
            started_at: now(),
            finished_at: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn progress(&mut self, progress: &remote::Progress) {
        self.remaining = progress.remaining;
        self.completed = progress.completed;
        self.failed = progress.failed;
        self.warning = progress.warning;
    }
}

/// The last transfers of one kind, running or not.
#[derive(Default)]
struct Transfers(Mutex<TransferList>);

#[derive(Default)]
struct TransferList {
    next_id: u64,
    /// Newest last.
    list: VecDeque<Transfer>,
}

impl Transfers {
    fn lock(&self) -> std::sync::MutexGuard<'_, TransferList> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add `transfer`, numbered here; the oldest finished ones go beyond the limit.
    fn add(&self, mut transfer: Transfer) -> Transfer {
        let mut transfers = self.lock();
        transfers.next_id += 1;
        transfer.id = transfers.next_id;
        transfers.list.push_back(transfer.clone());
        while transfers.list.len() > MAX_TRANSFERS {
            match transfers
                .list
                .iter()
                .position(|t| t.state != TransferState::Running)
            {
                Some(i) => {
                    transfers.list.remove(i);
                }
                None => break,
            }
        }
        transfer
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut Transfer)) {
        if let Some(transfer) = self.lock().list.iter_mut().find(|t| t.id == id) {
            f(transfer);
        }
    }

    /// Newest first.
    fn list(&self) -> Vec<Transfer> {
        self.lock().list.iter().rev().cloned().collect()
    }

    /// Ask a running transfer to stop: `202`, `404` when unknown, `409` when it is over.
    fn cancel(&self, id: u64, name: &str) -> Response {
        let transfers = self.lock();
        match transfers.list.iter().find(|t| t.id == id) {
            None => (StatusCode::NOT_FOUND, format!("unknown {name}")).into_response(),
            Some(t) if t.state != TransferState::Running => {
                (StatusCode::CONFLICT, format!("this {name} is over")).into_response()
            }
            Some(t) => {
                t.cancel.store(true, Ordering::Relaxed);
                (StatusCode::ACCEPTED, Json(t.clone())).into_response()
            }
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveRequest {
    level: String,
    /// Unique keys by keyword or tag: `StudyInstanceUID`, and those of the levels above
    /// and of the level itself.
    keys: BTreeMap<String, String>,
    /// AE title the instances are sent to; Fortitude's own by default.
    destination: Option<String>,
}

/// `400` naming the unique key `level` requires.
fn missing_unique_key(level: Level) -> Response {
    let name = StandardDataDictionary
        .by_tag(unique_key(level))
        .map_or("unique key", |e| e.alias());
    (
        StatusCode::BAD_REQUEST,
        format!("{name} is required at this level"),
    )
        .into_response()
}

/// Start a C-MOVE from the modality: `202 Accepted` with the retrieval (see `GET
/// /admin/moves`).
async fn start_move(
    State(modalities): State<Arc<Modalities>>,
    Path(ae_title): Path<String>,
    body: Result<Json<MoveRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    let peer = match peer(&modalities, &ae_title) {
        Ok(peer) => peer,
        Err(response) => return response,
    };
    let Some(level) = Level::parse(&request.level) else {
        return (StatusCode::BAD_REQUEST, "invalid level").into_response();
    };
    let mut identifier = InMemDicomObject::new_empty();
    for (name, value) in &request.keys {
        let value = value.trim();
        let element = attribute(name).and_then(|tag| remote::key(tag, value));
        match element {
            Some(element) if !value.is_empty() => identifier.put(element),
            Some(_) => {
                return (StatusCode::BAD_REQUEST, format!("{name} has no value")).into_response();
            }
            None => {
                return (StatusCode::BAD_REQUEST, format!("unknown attribute {name}"))
                    .into_response();
            }
        };
    }
    if identifier.get(unique_key(level)).is_none() {
        return missing_unique_key(level);
    }
    let own = &modalities.dimse.ae_title;
    let destination = request
        .destination
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| own.clone());
    if !config::valid_ae_title(&destination) {
        return (StatusCode::BAD_REQUEST, "invalid destination AE title").into_response();
    }
    if destination == *own && !modalities.listening {
        return (
            StatusCode::CONFLICT,
            "the DIMSE service is disabled (dimse.listen): Fortitude cannot receive the instances",
        )
            .into_response();
    }

    let job = modalities.moves.add(Transfer {
        destination: Some(destination.clone()),
        ..Transfer::new(&peer.ae_title, level, request.keys)
    });
    tracing::info!(
        id = job.id,
        modality = peer.ae_title,
        destination,
        level = job.level,
        "C-MOVE started"
    );
    let (id, cancel) = (job.id, job.cancel.clone());
    let task = modalities.clone();
    tokio::spawn(async move {
        let result = remote::retrieve(
            &task.dimse,
            &peer,
            level,
            identifier,
            &destination,
            |progress| {
                task.moves.update(id, |job| job.progress(progress));
                cancel.load(Ordering::Relaxed)
            },
        )
        .await;
        count("C-MOVE", &result);
        task.moves.update(id, |job| {
            job.finished_at = Some(now());
            match &result {
                Ok(moved) => {
                    let p = moved.progress;
                    job.progress(&p);
                    job.status = Some(format!("{:04X}", moved.status));
                    job.state = if moved.status == fortitude_dimse::command::status::CANCEL {
                        TransferState::Cancelled
                    } else if p.failed > 0 || p.warning > 0 || moved.status != 0 {
                        TransferState::Partial
                    } else {
                        TransferState::Succeeded
                    };
                }
                Err(e) => {
                    if let RemoteError::Failed { status, .. } = e {
                        job.status = Some(format!("{status:04X}"));
                    }
                    job.error = Some(e.to_string());
                    job.state = TransferState::Failed;
                }
            }
        });
        match &result {
            Ok(moved) => tracing::info!(
                id,
                completed = moved.progress.completed,
                failed = moved.progress.failed,
                status = format!("{:04X}", moved.status),
                "C-MOVE finished"
            ),
            Err(e) => tracing::info!(id, error = %e, "C-MOVE failed"),
        }
    });
    (StatusCode::ACCEPTED, Json(job)).into_response()
}

/// The retrievals, newest first.
async fn moves(State(modalities): State<Arc<Modalities>>) -> Response {
    Json(serde_json::json!({ "moves": modalities.moves.list() })).into_response()
}

/// Cancel a running retrieval (a C-CANCEL is sent with the next response of the modality):
/// `202`, `404` when unknown, `409` when it is over.
async fn cancel_move(State(modalities): State<Arc<Modalities>>, Path(id): Path<u64>) -> Response {
    modalities.moves.cancel(id, "retrieval")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    level: String,
    /// `PatientID` (and `IssuerOfPatientID`), `StudyInstanceUID`, `SeriesInstanceUID`,
    /// `SOPInstanceUID`, by keyword or tag: the key of the level is required, the others
    /// narrow the selection. UIDs can be lists separated by backslashes.
    keys: BTreeMap<String, String>,
}

/// The stored instances `keys` select, as for a C-MOVE received by Fortitude.
#[allow(clippy::result_large_err)]
fn selection(level: Level, keys: &BTreeMap<String, String>) -> Result<Selection, Response> {
    let mut selection = Selection::default();
    for (name, value) in keys {
        let value = value.trim();
        let Some(tag) = attribute(name) else {
            return Err(
                (StatusCode::BAD_REQUEST, format!("unknown attribute {name}")).into_response(),
            );
        };
        if value.is_empty() {
            return Err((StatusCode::BAD_REQUEST, format!("{name} has no value")).into_response());
        }
        let uids = || {
            value
                .split('\\')
                .map(|u| u.trim().to_owned())
                .filter(|u| !u.is_empty())
                .collect()
        };
        match tag {
            tags::PATIENT_ID => selection.patient_id = Some(value.to_owned()),
            tags::ISSUER_OF_PATIENT_ID => selection.issuer_of_patient_id = Some(value.to_owned()),
            tags::STUDY_INSTANCE_UID => selection.studies = uids(),
            tags::SERIES_INSTANCE_UID => selection.series = uids(),
            tags::SOP_INSTANCE_UID => selection.instances = uids(),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("{name} does not select instances: use PatientID, IssuerOfPatientID, StudyInstanceUID, SeriesInstanceUID or SOPInstanceUID"),
                )
                    .into_response());
            }
        }
    }
    let present = match level {
        Level::Patient => selection.patient_id.is_some(),
        Level::Study => !selection.studies.is_empty(),
        Level::Series => !selection.series.is_empty(),
        Level::Image => !selection.instances.is_empty(),
    };
    if present {
        Ok(selection)
    } else {
        Err(missing_unique_key(level))
    }
}

/// Send stored instances to the modality (C-STORE): `202 Accepted` with the send (see `GET
/// /admin/sends`), `404` when nothing stored matches.
async fn start_send(
    State(modalities): State<Arc<Modalities>>,
    Path(ae_title): Path<String>,
    body: Result<Json<SendRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(e) => return (StatusCode::BAD_REQUEST, e.body_text()).into_response(),
    };
    let peer = match peer(&modalities, &ae_title) {
        Ok(peer) => peer,
        Err(response) => return response,
    };
    let Some(level) = Level::parse(&request.level) else {
        return (StatusCode::BAD_REQUEST, "invalid level").into_response();
    };
    let selection = match selection(level, &request.keys) {
        Ok(selection) => selection,
        Err(response) => return response,
    };
    let instances = match modalities.store.select(selection).await {
        Ok(instances) => instances,
        Err(e) => return internal(e),
    };
    if instances.is_empty() {
        return (StatusCode::NOT_FOUND, "no stored instance matches").into_response();
    }

    let job = modalities.sends.add(Transfer {
        remaining: Some(u16::try_from(instances.len()).unwrap_or(u16::MAX)),
        failures: Some(Vec::new()),
        ..Transfer::new(&peer.ae_title, level, request.keys)
    });
    tracing::info!(
        id = job.id,
        modality = peer.ae_title,
        level = job.level,
        instances = instances.len(),
        "C-STORE send started"
    );
    let (id, cancel) = (job.id, job.cancel.clone());
    let task = modalities.clone();
    tokio::spawn(async move {
        let result = remote::send(&task.dimse, &peer, &task.store, instances, |progress| {
            task.sends.update(id, |job| job.progress(progress));
            cancel.load(Ordering::Relaxed)
        })
        .await;
        count("C-STORE", &result);
        task.sends.update(id, |job| {
            job.finished_at = Some(now());
            match &result {
                Ok(sent) => {
                    let p = sent.progress;
                    job.progress(&p);
                    job.failures = Some(
                        sent.failures
                            .iter()
                            .map(|f| FailedInstance {
                                sop_instance_uid: f.sop_instance_uid.clone(),
                                reason: f.reason.clone(),
                            })
                            .collect(),
                    );
                    job.error = sent.failures.first().map(|f| {
                        if p.failed > 1 {
                            format!("{} and {} more", f.reason, p.failed - 1)
                        } else {
                            f.reason.clone()
                        }
                    });
                    job.state = if sent.cancelled {
                        TransferState::Cancelled
                    } else if p.failed > 0 && p.completed == 0 && p.warning == 0 {
                        TransferState::Failed
                    } else if p.failed > 0 || p.warning > 0 {
                        TransferState::Partial
                    } else {
                        TransferState::Succeeded
                    };
                }
                Err(e) => {
                    job.error = Some(e.to_string());
                    job.state = TransferState::Failed;
                }
            }
        });
        match &result {
            Ok(sent) => tracing::info!(
                id,
                completed = sent.progress.completed,
                failed = sent.progress.failed,
                cancelled = sent.cancelled,
                "C-STORE send finished"
            ),
            Err(e) => tracing::info!(id, error = %e, "C-STORE send failed"),
        }
    });
    (StatusCode::ACCEPTED, Json(job)).into_response()
}

/// The sends, newest first.
async fn sends(State(modalities): State<Arc<Modalities>>) -> Response {
    Json(serde_json::json!({ "sends": modalities.sends.list() })).into_response()
}

/// Stop a running send after the instance being sent: `202`, `404` when unknown, `409` when
/// it is over.
async fn cancel_send(State(modalities): State<Arc<Modalities>>, Path(id): Path<u64>) -> Response {
    modalities.sends.cancel(id, "send")
}
