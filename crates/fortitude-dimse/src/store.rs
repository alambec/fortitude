//! Storage service (PS3.4 Annex B): C-STORE.
//!
//! The data set is received behind a file meta group built from the command, so the stored
//! file is the received bytes; ingest and duplicate handling are those of STOW-RS.

use dicom_dictionary_std::tags;
use fortitude_store::Stored;

use crate::association::{Connection, Error, Message};
use crate::command::{Command, field, status};
use crate::part10::{self, Meta};

/// Initial capacity of a received instance; the buffer grows as needed.
const INITIAL_CAPACITY: usize = 256 * 1024;

/// Bytes put before the data set of a message: a file meta group for C-STORE requests.
pub(crate) fn data_prefix(
    command: &Command,
    transfer_syntax: &str,
    peer_ae_title: &str,
) -> Vec<u8> {
    if command.field() != Some(field::C_STORE_RQ) {
        return Vec::new();
    }
    let sop_class_uid = command
        .string(tags::AFFECTED_SOP_CLASS_UID)
        .unwrap_or_default();
    let sop_instance_uid = command
        .string(tags::AFFECTED_SOP_INSTANCE_UID)
        .unwrap_or_default();
    part10::header(
        &Meta {
            sop_class_uid: &sop_class_uid,
            sop_instance_uid: &sop_instance_uid,
            transfer_syntax,
            source_ae_title: peer_ae_title,
        },
        INITIAL_CAPACITY,
    )
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
    let (status, comment) = match data {
        None => (status::UNABLE_TO_PROCESS, Some("no data set".to_owned())),
        Some(file) => {
            let result = dimse.store.store(file.into(), None).await;
            let expected = command.string(tags::AFFECTED_SOP_INSTANCE_UID);
            if result.sop_uid.is_some() && result.sop_uid != expected {
                tracing::warn!(
                    command = expected.as_deref().unwrap_or_default(),
                    data_set = result.sop_uid.as_deref().unwrap_or_default(),
                    "C-STORE: SOP Instance UID of the command and of the data set differ"
                );
            }
            match result.outcome {
                Ok(stored) => {
                    let outcome = match stored {
                        Stored::New => "stored",
                        Stored::Duplicate => "duplicate",
                    };
                    metrics::counter!("fortitude_dimse_instances_total", "outcome" => outcome)
                        .increment(1);
                    (status::SUCCESS, None)
                }
                Err(failure) => {
                    metrics::counter!("fortitude_dimse_instances_total", "outcome" => "failed")
                        .increment(1);
                    tracing::info!(
                        sop_uid = result.sop_uid.as_deref().unwrap_or_default(),
                        error = %failure,
                        "C-STORE failed"
                    );
                    // STOW-RS failure reasons are also C-STORE statuses (PS3.4 Table B.2-1).
                    (failure.failure_reason(), Some(failure.to_string()))
                }
            }
        }
    };
    let mut response = command.response(status, false);
    if let Some(comment) = comment {
        response = response.with_text(tags::ERROR_COMMENT, truncate(&comment, 64));
    }
    connection.send(context, &response, None).await?;
    Ok(status)
}

/// At most `max` bytes of `text`, cut on a character boundary.
pub(crate) fn truncate(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
