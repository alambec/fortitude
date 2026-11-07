//! Retrieve service (PS3.4 Annex C): C-MOVE and C-GET on the Patient Root and Study Root
//! models.
//!
//! The identifier selects instances by their unique keys; they are sent with C-STORE
//! sub-operations. For C-MOVE, over one association to the move destination, a configured
//! peer: each stored transfer syntax is proposed on a presentation context of its own, plus
//! Explicit/Implicit VR Little Endian per SOP class. For C-GET, back over the requestor's
//! association, on the storage contexts for which it took the SCP role. Files go out as
//! stored whenever the receiver accepts their transfer syntax; otherwise they are
//! transcoded.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use dicom_core::{DataElement, PrimitiveValue, VR};
use dicom_dictionary_std::tags;
use dicom_encoding::TransferSyntaxIndex;
use dicom_object::InMemDicomObject;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::Pdu;
use dicom_ul::association::Association;
use dicom_ul::association::client::{AsyncClientAssociation, ClientAssociationOptions};
use dicom_ul::pdu::{PDataValueType, PresentationContextResultReason};
use fortitude_store::Store;
use fortitude_store::retrieve::{Selection, StoredInstance};
use futures::StreamExt;
use tokio::net::TcpStream;

use crate::association::{Connection, Error, Message};
use crate::command::{Command, field, status};
use crate::find::{QueryLevel, identifier, string};
use crate::store::truncate;
use crate::{Config, Peer};
use crate::{part10, pdata};

const EXPLICIT_VR_LE: &str = "1.2.840.10008.1.2.1";
const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";
/// Presentation contexts an association can propose (odd IDs from 1 to 255).
const MAX_CONTEXTS: usize = 128;
/// Instances read (or transcoded) ahead of the one being sent.
const READ_AHEAD: usize = 4;

/// The instances designated by the unique keys of a C-MOVE or C-GET identifier, at and
/// above its level; the key of the level itself is required.
fn selection(identifier: &InMemDicomObject, level: QueryLevel) -> Result<Selection, String> {
    let uids = |tag| {
        string(identifier, tag)
            .map(|v| v.split('\\').map(|u| u.trim().to_owned()).collect())
            .unwrap_or_default()
    };
    let mut selection = Selection {
        patient_id: string(identifier, tags::PATIENT_ID),
        ..Selection::default()
    };
    let (required, name) = match level {
        QueryLevel::Patient => (selection.patient_id.is_some(), "PatientID"),
        QueryLevel::Study => {
            selection.studies = uids(tags::STUDY_INSTANCE_UID);
            (!selection.studies.is_empty(), "StudyInstanceUID")
        }
        QueryLevel::Series => {
            selection.studies = uids(tags::STUDY_INSTANCE_UID);
            selection.series = uids(tags::SERIES_INSTANCE_UID);
            (!selection.series.is_empty(), "SeriesInstanceUID")
        }
        QueryLevel::Image => {
            selection.studies = uids(tags::STUDY_INSTANCE_UID);
            selection.series = uids(tags::SERIES_INSTANCE_UID);
            selection.instances = uids(tags::SOP_INSTANCE_UID);
            (!selection.instances.is_empty(), "SOPInstanceUID")
        }
    };
    if required {
        Ok(selection)
    } else {
        Err(format!("missing {name}"))
    }
}

#[derive(Debug, Default)]
struct Counts {
    remaining: u16,
    completed: u16,
    failed: u16,
    warning: u16,
}

impl Counts {
    fn apply(&self, command: Command, with_remaining: bool) -> Command {
        let command = command
            .with_u16(tags::NUMBER_OF_COMPLETED_SUBOPERATIONS, self.completed)
            .with_u16(tags::NUMBER_OF_FAILED_SUBOPERATIONS, self.failed)
            .with_u16(tags::NUMBER_OF_WARNING_SUBOPERATIONS, self.warning);
        if with_remaining {
            command.with_u16(tags::NUMBER_OF_REMAINING_SUBOPERATIONS, self.remaining)
        } else {
            command
        }
    }
}

pub(crate) async fn handle(
    dimse: &crate::Dimse,
    connection: &mut Connection,
    message: Message,
) -> Result<u16, Error> {
    let Message {
        context,
        command,
        data,
    } = message;
    let (abstract_syntax, transfer_syntax) = connection
        .context(context)
        .map(|(a, t)| (a.to_owned(), t.to_owned()))
        .unwrap_or_default();
    let message_id = command.message_id().unwrap_or_default();
    let fail = |status: u16, comment: &str| {
        command
            .response(status, false)
            .with_text(tags::ERROR_COMMENT, truncate(comment, 64))
    };

    let get = command.field() == Some(field::C_GET_RQ);
    let name = if get { "C-GET" } else { "C-MOVE" };
    let patient_root =
        abstract_syntax == crate::PATIENT_ROOT_MOVE || abstract_syntax == crate::PATIENT_ROOT_GET;
    let selection = identifier(data.as_deref(), &transfer_syntax, patient_root)
        .and_then(|(identifier, level)| selection(&identifier, level));
    let selection = match selection {
        Ok(selection) => selection,
        Err(comment) => {
            connection
                .send(context, &fail(status::IDENTIFIER_MISMATCH, &comment), None)
                .await?;
            return Ok(status::IDENTIFIER_MISMATCH);
        }
    };
    let peer = if get {
        None
    } else {
        let destination = command.string(tags::MOVE_DESTINATION).unwrap_or_default();
        let Some(peer) = dimse.config.peer(&destination) else {
            let comment = format!("unknown move destination {destination:?}");
            connection
                .send(
                    context,
                    &fail(status::MOVE_DESTINATION_UNKNOWN, &comment),
                    None,
                )
                .await?;
            return Ok(status::MOVE_DESTINATION_UNKNOWN);
        };
        Some(peer)
    };
    let instances = match dimse.store.select(selection).await {
        Ok(instances) => instances,
        Err(e) => {
            tracing::warn!(error = %e, "{name} failed");
            connection
                .send(
                    context,
                    &fail(status::UNABLE_TO_PROCESS, &e.to_string()),
                    None,
                )
                .await?;
            return Ok(status::UNABLE_TO_PROCESS);
        }
    };
    let mut counts = Counts {
        remaining: u16::try_from(instances.len()).unwrap_or(u16::MAX),
        ..Counts::default()
    };
    if instances.is_empty() {
        let done = counts.apply(command.response(status::SUCCESS, false), false);
        connection.send(context, &done, None).await?;
        return Ok(status::SUCCESS);
    }

    let mut destination = match peer {
        None => None,
        Some(peer) => match Destination::open(&dimse.config, &peer, &instances).await {
            Ok(destination) => Some(destination),
            Err(e) => {
                tracing::warn!(destination = peer.ae_title, error = %e, "C-MOVE: cannot reach the destination");
                counts.failed = counts.remaining;
                let response = counts.apply(
                    fail(
                        status::SUBOPERATIONS_REFUSED,
                        &format!("cannot reach {}: {e}", peer.ae_title),
                    ),
                    false,
                );
                connection.send(context, &response, None).await?;
                return Ok(status::SUBOPERATIONS_REFUSED);
            }
        },
    };
    tracing::debug!(
        destination = destination
            .as_ref()
            .map_or(connection.peer_ae_title(), |d| &d.ae_title),
        instances = instances.len(),
        "{name} started"
    );

    let originator = connection.peer_ae_title().to_owned();
    let plans: Vec<_> = instances
        .iter()
        .map(|instance| {
            let plan = match &destination {
                Some(destination) => destination.plan(instance),
                None => plan(connection.retrieve_contexts(), instance),
            };
            (instance.clone(), plan)
        })
        .collect();
    let mut loads = futures::stream::iter(plans)
        .map(|plan| prepare(&dimse.store, plan))
        .buffered(READ_AHEAD);

    let mut failed_uids = Vec::new();
    let mut cancelled = false;
    let mut broken = false;
    let mut processed = 0;
    while let Some((instance, plan, loaded)) = loads.next().await {
        if connection.cancelled(message_id) {
            cancelled = true;
            break;
        }
        processed += 1;
        counts.remaining = counts.remaining.saturating_sub(1);
        let outcome = match (plan, loaded) {
            (Some((pc, _)), Ok((bytes, offset))) => {
                let request = Command::default()
                    .with_u16(tags::COMMAND_FIELD, field::C_STORE_RQ)
                    // Distinct from the request's own ID, on the same association for C-GET.
                    .with_u16(tags::MESSAGE_ID, message_id.wrapping_add(processed as u16))
                    .with_u16(tags::PRIORITY, 0)
                    .with_uid(tags::AFFECTED_SOP_CLASS_UID, &instance.sop_class_uid)
                    .with_uid(tags::AFFECTED_SOP_INSTANCE_UID, &instance.sop_uid)
                    .with_data_set(true);
                let data = &bytes[offset..];
                match &mut destination {
                    Some(destination) => {
                        let request = request
                            .with_text(tags::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE, &originator)
                            .with_u16(tags::MOVE_ORIGINATOR_MESSAGE_ID, message_id);
                        let sent = destination.store(pc, &request, data).await;
                        broken = sent.is_err();
                        sent
                    }
                    // A failure here breaks the requestor's own association: it is aborted.
                    None => Ok(connection
                        .sub_operation(pc, &request, data, message_id)
                        .await?),
                }
            }
            (_, Err(e)) => Err(e),
            (None, Ok(_)) => Err("no accepted presentation context".to_owned()),
        };
        let outcome = match outcome {
            Ok(status::SUCCESS) => {
                counts.completed += 1;
                "sent"
            }
            Ok(s) if status::is_warning(s) => {
                counts.warning += 1;
                "sent"
            }
            failure => {
                let reason = match failure {
                    Ok(s) => format!("status {s:04X}"),
                    Err(e) => e,
                };
                tracing::info!(
                    sop_uid = instance.sop_uid,
                    reason,
                    "{name}: sub-operation failed"
                );
                counts.failed += 1;
                failed_uids.push(instance.sop_uid.clone());
                "failed"
            }
        };
        if get {
            metrics::counter!("fortitude_dimse_get_instances_total", "outcome" => outcome)
                .increment(1);
        } else {
            metrics::counter!("fortitude_dimse_moved_instances_total", "outcome" => outcome)
                .increment(1);
        }
        if broken {
            break;
        }
        if counts.remaining > 0 {
            let pending = counts.apply(command.response(status::PENDING, false), true);
            connection.send(context, &pending, None).await?;
        }
    }
    drop(loads);
    if let Some(destination) = destination {
        if broken {
            destination.abort().await;
            // The instances not sent yet fail with the association.
            failed_uids.extend(instances[processed..].iter().map(|i| i.sop_uid.clone()));
            counts.failed += counts.remaining;
            counts.remaining = 0;
        } else {
            destination.release().await;
        }
    }

    let final_status = if cancelled {
        status::CANCEL
    } else if counts.failed > 0 || counts.warning > 0 {
        status::SUBOPERATIONS_WARNING
    } else {
        status::SUCCESS
    };
    let identifier =
        (!failed_uids.is_empty()).then(|| failed_identifier(&failed_uids, &transfer_syntax));
    let response = counts.apply(
        command.response(final_status, identifier.is_some()),
        cancelled,
    );
    connection
        .send(context, &response, identifier.as_deref())
        .await?;
    Ok(final_status)
}

/// Identifier of a final C-MOVE or C-GET response: Failed SOP Instance UID List.
fn failed_identifier(uids: &[String], transfer_syntax: &str) -> Vec<u8> {
    let object = InMemDicomObject::from_element_iter([DataElement::new(
        tags::FAILED_SOP_INSTANCE_UID_LIST,
        VR::UI,
        PrimitiveValue::Strs(uids.iter().cloned().collect()),
    )]);
    let mut out = Vec::new();
    if let Some(ts) = TransferSyntaxRegistry.get(transfer_syntax)
        && let Err(e) = object.write_dataset_with_ts(&mut out, ts)
    {
        tracing::warn!(error = %e, "cannot encode Failed SOP Instance UID List");
    }
    out
}

type Plan = Option<(u8, Option<String>)>;

/// An instance with its plan and data set, ready to be sent.
async fn prepare(
    store: &Arc<Store>,
    (instance, plan): (StoredInstance, Plan),
) -> (StoredInstance, Plan, Result<(Bytes, usize), String>) {
    let loaded = match &plan {
        Some((_, transcode)) => load(store, &instance, transcode.as_deref()).await,
        None => Err("no accepted presentation context".to_owned()),
    };
    (instance, plan, loaded)
}

/// The data set of a stored instance, in `transcode` or as stored: the file bytes and the
/// offset of the data set in them.
async fn load(
    store: &Arc<Store>,
    instance: &StoredInstance,
    transcode: Option<&str>,
) -> Result<(Bytes, usize), String> {
    let bytes = match transcode {
        None => store
            .files()
            .read(&instance.path)
            .await
            .map_err(|e| format!("cannot read file: {e}"))?,
        Some(ts) => store
            .transcoded(&instance.path, ts)
            .await
            .map(Bytes::from)
            .map_err(|e| format!("cannot transcode to {ts}: {e}"))?,
    };
    let offset = part10::data_set_offset(&bytes).ok_or("stored file is not Part-10")?;
    Ok((bytes, offset))
}

/// The presentation context to send `instance` on, among the accepted `contexts` (ID, SOP
/// class, transfer syntax), and the transfer syntax to transcode it to when none accepts the
/// stored one.
fn plan<'a>(
    contexts: impl Iterator<Item = (u8, &'a str, &'a str)>,
    instance: &StoredInstance,
) -> Plan {
    let accepted: Vec<_> = contexts
        .filter(|(_, sop_class, _)| *sop_class == instance.sop_class_uid)
        .collect();
    if let Some((id, _, _)) = accepted
        .iter()
        .find(|(_, _, ts)| *ts == instance.transfer_syntax)
    {
        return Some((*id, None));
    }
    accepted
        .iter()
        .find(|(_, _, ts)| is_uncompressed_le(ts))
        .map(|(id, _, ts)| (*id, Some((*ts).to_owned())))
}

/// The association to a move destination.
struct Destination {
    association: AsyncClientAssociation<TcpStream>,
    ae_title: String,
    timeout: Duration,
}

impl Destination {
    async fn open(
        config: &Config,
        peer: &Peer,
        instances: &[StoredInstance],
    ) -> Result<Self, String> {
        let mut options = ClientAssociationOptions::new()
            .calling_ae_title(config.ae_title.clone())
            .called_ae_title(peer.ae_title.clone())
            .max_pdu_length(config.max_pdu_length)
            .connection_timeout(config.timeout)
            .read_timeout(config.timeout)
            .write_timeout(config.timeout);
        for (sop_class, transfer_syntaxes) in proposals(instances) {
            options = options.with_presentation_context(sop_class, transfer_syntaxes);
        }
        let address = (peer.host.as_str(), peer.port);
        let mut association = tokio::time::timeout(
            config.timeout + Duration::from_secs(1),
            options.establish_async(address),
        )
        .await
        .map_err(|_| "timed out".to_owned())?
        .map_err(|e| e.to_string())?;
        if let Err(e) = association.inner_stream().set_nodelay(true) {
            tracing::warn!(error = %e, "cannot set TCP_NODELAY");
        }
        Ok(Self {
            association,
            ae_title: peer.ae_title.clone(),
            timeout: config.timeout,
        })
    }

    /// The presentation context to send `instance` on, and the transfer syntax to transcode
    /// it to when the destination does not accept the stored one.
    fn plan(&self, instance: &StoredInstance) -> Plan {
        let accepted = self
            .association
            .presentation_contexts()
            .iter()
            .filter(|pc| pc.reason == PresentationContextResultReason::Acceptance)
            .map(|pc| {
                (
                    pc.id,
                    pc.abstract_syntax.as_str(),
                    pc.transfer_syntax.as_str(),
                )
            });
        plan(accepted, instance)
    }

    /// Send one C-STORE request; returns the response status. An error leaves the
    /// association unusable.
    async fn store(&mut self, pc: u8, request: &Command, data: &[u8]) -> Result<u16, String> {
        let message_id = request.message_id();
        let max_pdu_length = self.association.peer_max_pdu_length();
        let timeout = self.timeout;
        let stream = self.association.inner_stream();
        let sent = async {
            let command = request.encode();
            pdata::send(stream, pc, true, &command, max_pdu_length, timeout).await?;
            pdata::send(stream, pc, false, data, max_pdu_length, timeout).await
        };
        sent.await.map_err(|e| e.to_string())?;

        let mut bytes = Vec::new();
        loop {
            match self
                .association
                .receive()
                .await
                .map_err(|e| e.to_string())?
            {
                Pdu::PData { data } => {
                    for fragment in data {
                        if fragment.value_type != PDataValueType::Command {
                            continue;
                        }
                        bytes.extend_from_slice(&fragment.data);
                        if fragment.is_last {
                            let response = Command::parse(&bytes).map_err(|e| e.to_string())?;
                            if response.u16(tags::MESSAGE_ID_BEING_RESPONDED_TO) != message_id
                                || response.field() != Some(field::C_STORE_RQ | field::RESPONSE)
                            {
                                return Err("unexpected response".into());
                            }
                            return response
                                .status()
                                .ok_or_else(|| "response without status".into());
                        }
                    }
                }
                Pdu::AbortRQ { .. } => return Err("aborted by the destination".into()),
                pdu => return Err(format!("unexpected PDU {pdu:?}")),
            }
        }
    }

    async fn release(self) {
        if let Err(e) = self.association.release().await {
            tracing::debug!(error = %e, "C-MOVE: release failed");
        }
    }

    async fn abort(self) {
        let _ = self.association.abort().await;
    }
}

fn is_uncompressed_le(transfer_syntax: &str) -> bool {
    transfer_syntax == EXPLICIT_VR_LE || transfer_syntax == IMPLICIT_VR_LE
}

/// Presentation contexts to propose for `instances`: each stored (SOP class, transfer
/// syntax) on its own, so that the destination cannot pick another syntax for them, and
/// Explicit and Implicit VR Little Endian per SOP class as a fallback; within the limit of
/// 128 contexts, fallbacks first.
fn proposals(instances: &[StoredInstance]) -> Vec<(String, Vec<String>)> {
    let mut fallback: Vec<(String, Vec<String>)> = Vec::new();
    let mut stored: Vec<(String, Vec<String>)> = Vec::new();
    for instance in instances {
        if !fallback.iter().any(|(c, _)| *c == instance.sop_class_uid) {
            fallback.push((
                instance.sop_class_uid.clone(),
                vec![EXPLICIT_VR_LE.to_owned(), IMPLICIT_VR_LE.to_owned()],
            ));
        }
        if !stored
            .iter()
            .any(|(c, t)| *c == instance.sop_class_uid && t[0] == instance.transfer_syntax)
        {
            stored.push((
                instance.sop_class_uid.clone(),
                vec![instance.transfer_syntax.clone()],
            ));
        }
    }
    stored.truncate(MAX_CONTEXTS.saturating_sub(fallback.len()));
    stored.extend(fallback);
    stored.truncate(MAX_CONTEXTS);
    stored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(sop_class: &str, ts: &str) -> StoredInstance {
        StoredInstance {
            study_uid: "1".into(),
            series_uid: "1.1".into(),
            sop_uid: "1.1.1".into(),
            sop_class_uid: sop_class.into(),
            transfer_syntax: ts.into(),
            path: "x".into(),
            size: 0,
        }
    }

    #[test]
    fn proposes_stored_and_uncompressed_syntaxes() {
        let jpeg = "1.2.840.10008.1.2.4.50";
        let proposals = proposals(&[
            instance("1.2.3", jpeg),
            instance("1.2.3", jpeg),
            instance("1.2.3", EXPLICIT_VR_LE),
            instance("1.2.4", IMPLICIT_VR_LE),
        ]);
        assert_eq!(
            proposals,
            vec![
                ("1.2.3".to_owned(), vec![jpeg.to_owned()]),
                ("1.2.3".to_owned(), vec![EXPLICIT_VR_LE.to_owned()]),
                ("1.2.4".to_owned(), vec![IMPLICIT_VR_LE.to_owned()]),
                (
                    "1.2.3".to_owned(),
                    vec![EXPLICIT_VR_LE.to_owned(), IMPLICIT_VR_LE.to_owned()]
                ),
                (
                    "1.2.4".to_owned(),
                    vec![EXPLICIT_VR_LE.to_owned(), IMPLICIT_VR_LE.to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn selection_requires_the_key_of_the_level() {
        let identifier = InMemDicomObject::from_element_iter([
            DataElement::new(
                tags::STUDY_INSTANCE_UID,
                VR::UI,
                PrimitiveValue::from("1.2\\1.3"),
            ),
            DataElement::new(tags::PATIENT_ID, VR::LO, PrimitiveValue::from("P1")),
        ]);
        let study = selection(&identifier, QueryLevel::Study).unwrap();
        assert_eq!(study.studies, vec!["1.2", "1.3"]);
        assert_eq!(study.patient_id.as_deref(), Some("P1"));
        assert!(selection(&identifier, QueryLevel::Series).is_err());
        assert!(selection(&identifier, QueryLevel::Patient).is_ok());
    }
}
