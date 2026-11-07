//! Fortitude as a service user of its peers (modalities, PACS): verification (C-ECHO),
//! queries (C-FIND) and retrievals (C-MOVE) on the Patient Root and Study Root models, and
//! storage (C-STORE) of stored instances.
//!
//! Each operation opens an association of its own with Fortitude's AE title as calling AE
//! title, proposing Explicit and Implicit VR Little Endian (and, to send instances, their
//! stored transfer syntaxes). Queries and retrievals use the Study Root model, or the Patient
//! Root one at the patient level and with peers that only accept it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_encoding::TransferSyntaxIndex;
use dicom_object::InMemDicomObject;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use fortitude_store::Store;
use fortitude_store::retrieve::StoredInstance;
use futures::StreamExt;

use crate::command::{Command, field, status};
use crate::retrieve::{Destination, Plan, READ_AHEAD, prepare};
use crate::scu::{Options, Response, Scu};
use crate::{Config, Peer};

const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";
const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";
const UNCOMPRESSED: &[&str] = &[EXPLICIT_VR_LE, IMPLICIT_VR_LE];

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// The peer could not be reached, or refused the association.
    #[error("cannot associate with {0}")]
    Unreachable(String),
    /// The peer accepted the association, but not the service.
    #[error("the peer does not accept {0}")]
    Unsupported(&'static str),
    /// The peer answered with a failure status.
    #[error("{}", describe_failure(*status, comment.as_deref()))]
    Failed {
        status: u16,
        comment: Option<String>,
    },
    /// The association broke, or the peer did not follow the protocol.
    #[error("{0}")]
    Protocol(String),
    /// The request cannot be sent (e.g. a value that cannot be encoded).
    #[error("{0}")]
    Invalid(String),
}

fn describe_failure(status: u16, comment: Option<&str>) -> String {
    match comment {
        Some(comment) => format!("{} ({status:04X}): {comment}", describe(status)),
        None => format!("{} ({status:04X})", describe(status)),
    }
}

/// The meaning of a status of a C-ECHO, C-FIND or C-MOVE response (PS3.4 Annex C, PS3.7
/// Annex C).
pub fn describe(status: u16) -> &'static str {
    match status {
        status::SUCCESS => "success",
        status::PENDING | 0xFF01 => "pending",
        status::CANCEL => "cancelled",
        status::SUBOPERATIONS_WARNING => "completed with failures or warnings",
        0xA701 => "out of resources: unable to calculate the number of matches",
        status::SUBOPERATIONS_REFUSED => "out of resources: unable to perform sub-operations",
        s if s & 0xFF00 == 0xA700 => "out of resources",
        status::MOVE_DESTINATION_UNKNOWN => "move destination unknown",
        s if s & 0xFF00 == 0xA900 => "identifier does not match SOP class",
        0x0122 => "SOP class not supported",
        0x0124 => "not authorized",
        status::UNRECOGNIZED_OPERATION => "unrecognized operation",
        s if s & 0xF000 == 0xC000 => "unable to process",
        _ => "failure",
    }
}

/// Query/Retrieve Level (0008,0052).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Patient,
    Study,
    Series,
    Image,
}

impl Level {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "PATIENT" => Some(Self::Patient),
            "STUDY" => Some(Self::Study),
            "SERIES" => Some(Self::Series),
            "IMAGE" | "INSTANCE" => Some(Self::Image),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Patient => "PATIENT",
            Self::Study => "STUDY",
            Self::Series => "SERIES",
            Self::Image => "IMAGE",
        }
    }
}

/// An association with `peer`, proposing `contexts`.
async fn associate(
    config: &Config,
    peer: &Peer,
    contexts: &[&str],
    response_timeout: Option<Duration>,
) -> Result<Scu, RemoteError> {
    let mut options = Options::new(&config.ae_title, &peer.ae_title, config.timeout);
    options.max_pdu_length = config.max_pdu_length;
    options.response_timeout = response_timeout;
    for abstract_syntax in contexts {
        options = options.context(abstract_syntax, UNCOMPRESSED);
    }
    Scu::open((peer.host.as_str(), peer.port), &options)
        .await
        .map_err(|e| {
            RemoteError::Unreachable(format!(
                "{} at {}:{}: {}",
                peer.ae_title,
                peer.host,
                peer.port,
                chain(&e)
            ))
        })
}

/// An error and its sources, which `dicom-ul` errors keep apart from their message.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(e) = source {
        let message = e.to_string();
        if !text.contains(&message) {
            text = format!("{text}: {message}");
        }
        source = e.source();
    }
    text
}

fn protocol(error: crate::association::Error) -> RemoteError {
    RemoteError::Protocol(chain(&error))
}

/// Release the association, or abort it after an error.
async fn finish<T>(scu: Scu, result: Result<T, RemoteError>) -> Result<T, RemoteError> {
    match &result {
        Ok(_) | Err(RemoteError::Failed { .. }) => {
            if let Err(e) = scu.release().await {
                tracing::debug!(error = %e, "release failed");
            }
        }
        Err(_) => scu.abort().await,
    }
    result
}

/// Verify the connection with `peer`: returns the time taken, association included.
pub async fn echo(config: &Config, peer: &Peer) -> Result<Duration, RemoteError> {
    let started = Instant::now();
    let mut scu = associate(config, peer, &[uids::VERIFICATION], None).await?;
    let result = async {
        let (context, _) = scu
            .context(uids::VERIFICATION, None)
            .ok_or(RemoteError::Unsupported("the Verification service"))?;
        let command = Command::default()
            .with_u16(tags::COMMAND_FIELD, field::C_ECHO_RQ)
            .with_uid(tags::AFFECTED_SOP_CLASS_UID, uids::VERIFICATION);
        let response = scu
            .request_each(context, command, None, refuse, |_| false)
            .await
            .map_err(protocol)?;
        check(&response, &[status::SUCCESS])?;
        Ok(started.elapsed())
    }
    .await;
    finish(scu, result).await
}

/// C-STORE requests are not expected on these associations.
fn refuse(_: &Command, _: &str, _: Vec<u8>) -> u16 {
    status::OUT_OF_RESOURCES
}

/// The final response, unless its status is not one of `expected` (or a warning).
fn check(response: &Response, expected: &[u16]) -> Result<u16, RemoteError> {
    let status = response
        .command
        .status()
        .ok_or_else(|| RemoteError::Protocol("response without status".into()))?;
    if expected.contains(&status) || status::is_warning(status) {
        Ok(status)
    } else {
        Err(RemoteError::Failed {
            status,
            comment: response.command.string(tags::ERROR_COMMENT),
        })
    }
}

/// The models to propose for an operation at `level`: Study Root first, Patient Root as a
/// fallback; Patient Root only at the patient level.
fn models(level: Level, study_root: &'static str, patient_root: &'static str) -> Vec<&'static str> {
    match level {
        Level::Patient => vec![patient_root],
        _ => vec![study_root, patient_root],
    }
}

/// The first model of `models` the peer accepts: its presentation context and transfer
/// syntax.
fn model_context(scu: &Scu, models: &[&'static str]) -> Option<(&'static str, u8, String)> {
    models.iter().find_map(|model| {
        scu.context(model, None)
            .map(|(id, ts)| (*model, id, ts.to_owned()))
    })
}

/// The identifier of a request: `keys`, with the Query/Retrieve Level, and the Specific
/// Character Set when a value is not ASCII.
fn encode(
    mut identifier: InMemDicomObject,
    level: Level,
    transfer_syntax: &str,
) -> Result<Vec<u8>, RemoteError> {
    let ascii = identifier.iter().all(|element| {
        element
            .value()
            .to_str()
            .map_or(true, |text| text.is_ascii())
    });
    identifier.put(DataElement::new(
        tags::QUERY_RETRIEVE_LEVEL,
        VR::CS,
        PrimitiveValue::from(level.as_str()),
    ));
    if !ascii {
        identifier.put(DataElement::new(
            tags::SPECIFIC_CHARACTER_SET,
            VR::CS,
            PrimitiveValue::from("ISO_IR 192"),
        ));
    }
    let ts = TransferSyntaxRegistry.get(transfer_syntax).ok_or_else(|| {
        RemoteError::Protocol(format!("unknown transfer syntax {transfer_syntax}"))
    })?;
    let mut data = Vec::new();
    identifier
        .write_dataset_with_ts(&mut data, ts)
        .map_err(|e| RemoteError::Invalid(format!("cannot encode the identifier: {e}")))?;
    Ok(data)
}

/// The matches of a query.
#[derive(Debug, Default)]
pub struct Found {
    /// Response identifiers, without Query/Retrieve Level nor Specific Character Set.
    pub matches: Vec<InMemDicomObject>,
    /// The peer had more matches than requested, or than it returns at once.
    pub truncated: bool,
}

/// Query `peer` at `level`: the keys of `identifier` with a value are matching keys, all of
/// them return keys. At most `limit` matches are returned (0: no limit); the request is
/// cancelled beyond them.
pub async fn find(
    config: &Config,
    peer: &Peer,
    level: Level,
    identifier: InMemDicomObject,
    limit: usize,
) -> Result<Found, RemoteError> {
    let models = models(
        level,
        uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND,
        uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND,
    );
    let mut scu = associate(config, peer, &models, None).await?;
    let result = async {
        let (model, context, transfer_syntax) = model_context(&scu, &models)
            .ok_or(RemoteError::Unsupported("the Query/Retrieve FIND models"))?;
        let ts = TransferSyntaxRegistry
            .get(&transfer_syntax)
            .ok_or_else(|| {
                RemoteError::Protocol(format!("unknown transfer syntax {transfer_syntax}"))
            })?;
        let data = encode(identifier, level, &transfer_syntax)?;
        let command = Command::default()
            .with_u16(tags::COMMAND_FIELD, field::C_FIND_RQ)
            .with_uid(tags::AFFECTED_SOP_CLASS_UID, model)
            .with_u16(tags::PRIORITY, 0);
        let mut found = Found::default();
        let mut errors = 0;
        let response = scu
            .request_each(context, command, Some(&data), refuse, |pending| {
                if limit > 0 && found.matches.len() >= limit {
                    found.truncated = true;
                    return true;
                }
                let decoded = pending
                    .data
                    .as_deref()
                    .map(|data| InMemDicomObject::read_dataset_with_ts(data, ts));
                match decoded {
                    Some(Ok(mut object)) => {
                        object.remove_element(tags::QUERY_RETRIEVE_LEVEL);
                        object.remove_element(tags::SPECIFIC_CHARACTER_SET);
                        found.matches.push(object);
                    }
                    Some(Err(e)) => {
                        errors += 1;
                        tracing::debug!(error = %e, "C-FIND: unreadable match");
                    }
                    None => errors += 1,
                }
                false
            })
            .await
            .map_err(protocol)?;
        if errors > 0 {
            tracing::info!(
                peer = peer.ae_title,
                errors,
                "C-FIND: matches without a readable identifier"
            );
        }
        match check(&response, &[status::SUCCESS, status::CANCEL]) {
            Ok(_) => Ok(found),
            // Fortitude itself, among others, stops at its maximum number of matches.
            Err(RemoteError::Failed { status, .. })
                if status & 0xFF00 == 0xA700 && !found.matches.is_empty() =>
            {
                found.truncated = true;
                Ok(found)
            }
            Err(e) => Err(e),
        }
    }
    .await;
    finish(scu, result).await
}

/// Counts of the sub-operations of a retrieval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    /// Unknown until the peer reports it.
    pub remaining: Option<u16>,
    pub completed: u16,
    pub failed: u16,
    pub warning: u16,
}

impl Progress {
    fn update(&mut self, command: &Command) {
        for (count, tag) in [
            (&mut self.completed, tags::NUMBER_OF_COMPLETED_SUBOPERATIONS),
            (&mut self.failed, tags::NUMBER_OF_FAILED_SUBOPERATIONS),
            (&mut self.warning, tags::NUMBER_OF_WARNING_SUBOPERATIONS),
        ] {
            if let Some(value) = command.u16(tag) {
                *count = value;
            }
        }
    }
}

/// The end of a retrieval.
#[derive(Debug, Clone)]
pub struct Moved {
    /// Status of the final response: success, warning (some sub-operations failed) or
    /// cancel.
    pub status: u16,
    pub progress: Progress,
}

/// Ask `peer` to send the instances designated by the unique keys of `identifier` at
/// `level` to `destination` (an AE title the peer knows), and wait until it has. Each
/// pending response is passed to `on_progress`, which returns whether to cancel.
pub async fn retrieve(
    config: &Config,
    peer: &Peer,
    level: Level,
    identifier: InMemDicomObject,
    destination: &str,
    mut on_progress: impl FnMut(&Progress) -> bool,
) -> Result<Moved, RemoteError> {
    let models = models(
        level,
        uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE,
        uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE,
    );
    let mut scu = associate(config, peer, &models, Some(config.retrieve_timeout)).await?;
    let result = async {
        let (model, context, transfer_syntax) = model_context(&scu, &models)
            .ok_or(RemoteError::Unsupported("the Query/Retrieve MOVE models"))?;
        let data = encode(identifier, level, &transfer_syntax)?;
        let command = Command::default()
            .with_u16(tags::COMMAND_FIELD, field::C_MOVE_RQ)
            .with_uid(tags::AFFECTED_SOP_CLASS_UID, model)
            .with_u16(tags::PRIORITY, 0)
            .with_text(tags::MOVE_DESTINATION, destination);
        let mut progress = Progress::default();
        let response = scu
            .request_each(context, command, Some(&data), refuse, |pending| {
                progress.update(&pending.command);
                progress.remaining = pending
                    .command
                    .u16(tags::NUMBER_OF_REMAINING_SUBOPERATIONS)
                    .or(progress.remaining);
                on_progress(&progress)
            })
            .await
            .map_err(protocol)?;
        progress.update(&response.command);
        // Only a cancelled or failed retrieval has remaining sub-operations at the end.
        progress.remaining = response
            .command
            .u16(tags::NUMBER_OF_REMAINING_SUBOPERATIONS)
            .filter(|r| *r > 0);
        let status = check(&response, &[status::SUCCESS, status::CANCEL])?;
        Ok(Moved { status, progress })
    }
    .await;
    finish(scu, result).await
}

/// Failures kept in [`Sent::failures`]; the others are only counted.
pub const MAX_FAILURES: usize = 100;

/// An instance a send could not store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub sop_instance_uid: String,
    pub reason: String,
}

/// The end of a send.
#[derive(Debug, Clone, Default)]
pub struct Sent {
    /// `remaining` is the number of instances not sent, when cancelled.
    pub progress: Progress,
    pub cancelled: bool,
    /// The first [`MAX_FAILURES`] instances that were not stored, in order.
    pub failures: Vec<Failure>,
}

/// The meaning of a failure status of a C-STORE response (PS3.4 Annex B).
fn describe_store_failure(status: u16) -> &'static str {
    match status {
        s if s & 0xFF00 == 0xA700 => "out of resources",
        s if s & 0xFF00 == 0xA900 => "data set does not match SOP class",
        s if s & 0xF000 == 0xC000 => "cannot understand",
        0x0122 => "SOP class not supported",
        0x0124 => "not authorized",
        0x0117 => "invalid SOP instance",
        status::UNRECOGNIZED_OPERATION => "unrecognized operation",
        _ => "failure",
    }
}

/// Send stored `instances` to `peer` with C-STORE requests, over one association proposing
/// their SOP classes, each in its stored transfer syntax and in Explicit and Implicit VR
/// Little Endian (as for C-MOVE: files go out as stored, unless the peer only accepts an
/// uncompressed syntax). `on_progress` is called before the first instance and after each
/// one, and returns whether to stop; the association is then released.
///
/// Fails when the peer cannot be reached or accepts none of the SOP classes; otherwise the
/// instances it refused are counted and listed as [failures](Sent::failures). When the
/// association breaks, the instances not sent yet fail with it.
pub async fn send(
    config: &Config,
    peer: &Peer,
    store: &Arc<Store>,
    instances: Vec<StoredInstance>,
    mut on_progress: impl FnMut(&Progress) -> bool,
) -> Result<Sent, RemoteError> {
    let total = instances.len();
    let mut sent = Sent {
        progress: Progress {
            remaining: Some(u16::try_from(total).unwrap_or(u16::MAX)),
            ..Progress::default()
        },
        ..Sent::default()
    };
    if total == 0 {
        sent.progress.remaining = None;
        return Ok(sent);
    }
    let mut destination = Destination::open(config, peer, &instances)
        .await
        .map_err(|e| {
            RemoteError::Unreachable(format!(
                "{} at {}:{}: {e}",
                peer.ae_title, peer.host, peer.port
            ))
        })?;
    let plans: Vec<(StoredInstance, Plan)> = instances
        .into_iter()
        .map(|instance| {
            let plan = destination.plan(&instance);
            (instance, plan)
        })
        .collect();
    if plans.iter().all(|(_, plan)| plan.is_none()) {
        destination.release().await;
        return Err(RemoteError::Unsupported(
            "the SOP classes of these instances",
        ));
    }

    let uids: Vec<String> = plans.iter().map(|(i, _)| i.sop_uid.clone()).collect();
    let fail = |sent: &mut Sent, sop_instance_uid: &str, reason: String| {
        sent.progress.failed = sent.progress.failed.saturating_add(1);
        if sent.failures.len() < MAX_FAILURES {
            sent.failures.push(Failure {
                sop_instance_uid: sop_instance_uid.to_owned(),
                reason,
            });
        }
        metrics::counter!("fortitude_dimse_sent_instances_total", "outcome" => "failed")
            .increment(1);
    };
    let mut processed = 0;
    let mut broken = None;
    let mut loads = futures::stream::iter(plans)
        .map(|plan| prepare(store, plan))
        .buffered(READ_AHEAD);
    sent.cancelled = on_progress(&sent.progress);
    while !sent.cancelled {
        let Some((instance, plan, loaded)) = loads.next().await else {
            break;
        };
        processed += 1;
        sent.progress.remaining = Some(u16::try_from(total - processed).unwrap_or(u16::MAX));
        let outcome = match (plan, loaded) {
            (Some((pc, _)), Ok((bytes, offset))) => {
                let request = Command::default()
                    .with_u16(tags::COMMAND_FIELD, field::C_STORE_RQ)
                    // 1 to 65535, then 1 again.
                    .with_u16(tags::MESSAGE_ID, ((processed - 1) % 0xFFFF + 1) as u16)
                    .with_u16(tags::PRIORITY, 0)
                    .with_uid(tags::AFFECTED_SOP_CLASS_UID, &instance.sop_class_uid)
                    .with_uid(tags::AFFECTED_SOP_INSTANCE_UID, &instance.sop_uid)
                    .with_data_set(true);
                let stored = destination.store(pc, &request, &bytes[offset..]).await;
                if let Err(e) = &stored {
                    broken = Some(e.clone());
                }
                stored
            }
            (_, Err(e)) => Err(e),
            (None, Ok(_)) => Err(
                "no presentation context accepted for its SOP class and transfer syntax".to_owned(),
            ),
        };
        match outcome {
            Ok(s) if s == status::SUCCESS || status::is_warning(s) => {
                if s == status::SUCCESS {
                    sent.progress.completed = sent.progress.completed.saturating_add(1);
                } else {
                    sent.progress.warning = sent.progress.warning.saturating_add(1);
                }
                metrics::counter!("fortitude_dimse_sent_instances_total", "outcome" => "sent")
                    .increment(1);
            }
            Ok(s) => {
                let reason = format!("{} ({s:04X})", describe_store_failure(s));
                fail(&mut sent, &instance.sop_uid, reason);
            }
            Err(reason) => fail(&mut sent, &instance.sop_uid, reason),
        }
        if broken.is_some() {
            break;
        }
        sent.cancelled = on_progress(&sent.progress);
    }
    drop(loads);
    if let Some(e) = broken {
        destination.abort().await;
        // The instances not sent yet fail with the association.
        let reason = format!("not sent: the association broke ({e})");
        for uid in &uids[processed..] {
            fail(&mut sent, uid, reason.clone());
        }
        sent.progress.remaining = None;
    } else {
        destination.release().await;
        let remaining = total - processed;
        sent.progress.remaining =
            (remaining > 0).then(|| u16::try_from(remaining).unwrap_or(u16::MAX));
    }
    tracing::debug!(
        peer = peer.ae_title,
        completed = sent.progress.completed,
        failed = sent.progress.failed,
        "C-STORE: send finished"
    );
    Ok(sent)
}

/// A key of a query, typed with the VR of the standard dictionary.
pub fn key(tag: dicom_core::Tag, value: &str) -> Option<DataElement<InMemDicomObject>> {
    use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
    let vr = dicom_dictionary_std::StandardDataDictionary
        .by_tag(tag)?
        .vr()
        .relaxed();
    Some(if vr == VR::SQ {
        DataElement::new(
            tag,
            VR::SQ,
            dicom_core::value::Value::from(
                dicom_core::value::DataSetSequence::<InMemDicomObject>::empty(),
            ),
        )
    } else if value.is_empty() {
        DataElement::new(tag, vr, PrimitiveValue::Empty)
    } else {
        DataElement::new(tag, vr, PrimitiveValue::from(value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_statuses() {
        assert_eq!(Level::parse("study"), Some(Level::Study));
        assert_eq!(Level::parse("INSTANCE"), Some(Level::Image));
        assert_eq!(Level::parse("frame"), None);
        assert_eq!(describe(0xA801), "move destination unknown");
        assert_eq!(describe(0xC123), "unable to process");
        assert_eq!(
            RemoteError::Failed {
                status: 0xA900,
                comment: Some("bad key".into())
            }
            .to_string(),
            "identifier does not match SOP class (A900): bad key"
        );
    }

    #[test]
    fn keys_take_the_vr_of_the_dictionary() {
        let date = key(tags::STUDY_DATE, "20240101-").unwrap();
        assert_eq!(date.vr(), VR::DA);
        assert_eq!(date.to_str().unwrap(), "20240101-");
        assert_eq!(key(tags::PATIENT_NAME, "").unwrap().vr(), VR::PN);
    }

    #[test]
    fn identifiers_declare_their_character_set_when_needed() {
        let ts = TransferSyntaxRegistry.get(EXPLICIT_VR_LE).unwrap();
        let read = |data: Vec<u8>| InMemDicomObject::read_dataset_with_ts(&data[..], ts).unwrap();
        let ascii = InMemDicomObject::from_element_iter([key(tags::PATIENT_ID, "P1").unwrap()]);
        let object = read(encode(ascii, Level::Study, EXPLICIT_VR_LE).unwrap());
        assert_eq!(
            object
                .get(tags::QUERY_RETRIEVE_LEVEL)
                .unwrap()
                .to_str()
                .unwrap(),
            "STUDY"
        );
        assert!(object.get(tags::SPECIFIC_CHARACTER_SET).is_none());

        let name =
            InMemDicomObject::from_element_iter([key(tags::PATIENT_NAME, "Gérard*").unwrap()]);
        let object = read(encode(name, Level::Patient, EXPLICIT_VR_LE).unwrap());
        assert_eq!(
            object.get(tags::PATIENT_NAME).unwrap().to_str().unwrap(),
            "Gérard*"
        );
        assert_eq!(
            object
                .get(tags::SPECIFIC_CHARACTER_SET)
                .unwrap()
                .to_str()
                .unwrap(),
            "ISO_IR 192"
        );
    }
}
