//! Associations accepted by Fortitude (PS3.8 §7.1, §9.3): negotiation, then DIMSE messages
//! reassembled from P-DATA fragments, responses sent back, and cancel requests noticed
//! between responses. PDUs are encoded and decoded by `dicom-ul`.
//!
//! Negotiation is done here rather than by `dicom-ul` for its transfer syntax choice: when a
//! storage presentation context offers both compressed and uncompressed syntaxes, the
//! compressed one (the data's own, almost always) is accepted, so that the sender does not
//! decompress it.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use dicom_dictionary_std::tags;
use dicom_encoding::TransferSyntaxIndex;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use dicom_ul::Pdu;
use dicom_ul::association::read_pdu_from_wire_async;
use dicom_ul::pdu::{
    AbortRQServiceProviderReason, AbortRQSource, AssociationAC, AssociationRJ, AssociationRJResult,
    AssociationRJServiceProviderPresentationReason, AssociationRJServiceUserReason,
    AssociationRJSource, PDataValue, PDataValueType, PresentationContextResult,
    PresentationContextResultReason, UserVariableItem, write_pdu,
};
use futures::FutureExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::command::{Command, CommandError, field};
use crate::part10::{IMPLEMENTATION_CLASS_UID, IMPLEMENTATION_VERSION_NAME};
use crate::{Config, pdata, sop_classes};

const APPLICATION_CONTEXT: &str = "1.2.840.10008.3.1.1.1";
const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";
/// Transfer syntaxes every implementation offers as a fallback.
const NATIVE: &[&str] = &[
    "1.2.840.10008.1.2.1",
    IMPLICIT_VR_LE,
    "1.2.840.10008.1.2.2",
    "1.2.840.10008.1.2.1.99",
];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Receive(#[from] dicom_ul::association::Error),
    #[error("cannot encode PDU: {0}")]
    Encode(#[from] dicom_ul::pdu::WriteError),
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out")]
    Timeout,
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("association rejected: {0}")]
    Rejected(&'static str),
    #[error("data set larger than {0} bytes")]
    TooLarge(usize),
}

/// A DIMSE message: a command and its data set.
#[derive(Debug)]
pub struct Message {
    pub context: u8,
    pub command: Command,
    pub data: Option<Vec<u8>>,
}

/// What came in on the association.
#[derive(Debug)]
pub enum Incoming {
    Message(Message),
    /// The peer released the association (it has been acknowledged).
    Released,
    Aborted,
}

/// Negotiates the associations requested to this node.
pub struct Acceptor {
    config: Arc<Config>,
    abstract_syntaxes: HashSet<String>,
    /// Transfer syntaxes accepted; all known ones when `None`.
    transfer_syntaxes: Option<Vec<String>>,
}

impl Acceptor {
    pub fn new(config: Arc<Config>) -> Self {
        let mut abstract_syntaxes: HashSet<String> = [
            dicom_dictionary_std::uids::VERIFICATION,
            crate::PATIENT_ROOT_FIND,
            crate::PATIENT_ROOT_MOVE,
            crate::STUDY_ROOT_FIND,
            crate::STUDY_ROOT_MOVE,
        ]
        .into_iter()
        .chain(sop_classes::STORAGE_SOP_CLASSES.iter().copied())
        .map(str::to_owned)
        .collect();
        abstract_syntaxes.extend(config.extra_sop_classes.iter().cloned());
        Self {
            config,
            abstract_syntaxes,
            transfer_syntaxes: None,
        }
    }

    /// Accept only these transfer syntaxes (a C-MOVE destination in tests).
    #[cfg(test)]
    pub fn with_transfer_syntaxes(mut self, transfer_syntaxes: &[&str]) -> Self {
        self.transfer_syntaxes = Some(transfer_syntaxes.iter().map(|t| t.to_string()).collect());
        self
    }

    /// Negotiate an association on `socket`. With `busy`, the request is rejected
    /// (transient, local limit exceeded).
    pub async fn establish(&self, socket: TcpStream, busy: bool) -> Result<Connection, Error> {
        let config = &self.config;
        let mut connection = Connection {
            socket,
            buffer: BytesMut::with_capacity(64 * 1024),
            contexts: Vec::new(),
            peer_ae_title: String::new(),
            peer_max_pdu_length: dicom_ul::pdu::DEFAULT_MAX_PDU,
            max_pdu_length: config.max_pdu_length,
            timeout: config.timeout,
            fragments: VecDeque::new(),
            deferred: VecDeque::new(),
            max_data_set: config.max_instance_size,
            released: false,
        };
        let Pdu::AssociationRQ(request) = connection.read_pdu().await? else {
            return Err(Error::Protocol("expected A-ASSOCIATE-RQ".into()));
        };
        let user_reject = |reason, text| {
            Some((
                AssociationRJResult::Permanent,
                AssociationRJSource::ServiceUser(reason),
                text,
            ))
        };
        let rejection = if busy {
            Some((
                AssociationRJResult::Transient,
                AssociationRJSource::ServiceProviderPresentation(
                    AssociationRJServiceProviderPresentationReason::LocalLimitExceeded,
                ),
                "too many open associations",
            ))
        } else if request.protocol_version & 1 == 0 {
            user_reject(
                AssociationRJServiceUserReason::NoReasonGiven,
                "unsupported protocol version",
            )
        } else if trim(&request.application_context_name) != APPLICATION_CONTEXT {
            user_reject(
                AssociationRJServiceUserReason::ApplicationContextNameNotSupported,
                "unsupported application context",
            )
        } else if config.check_called_ae_title && request.called_ae_title.trim() != config.ae_title
        {
            user_reject(
                AssociationRJServiceUserReason::CalledAETitleNotRecognized,
                "called AE title not recognized",
            )
        } else if !config.allow_unknown_peers
            && config.peer(request.calling_ae_title.trim()).is_none()
        {
            user_reject(
                AssociationRJServiceUserReason::CallingAETitleNotRecognized,
                "calling AE title not recognized",
            )
        } else {
            None
        };
        if let Some((result, source, reason)) = rejection {
            let reject = Pdu::AssociationRJ(AssociationRJ { result, source });
            connection.write_pdu(&reject).await?;
            return Err(Error::Rejected(reason));
        }

        for variable in &request.user_variables {
            if let UserVariableItem::MaxLength(length) = variable {
                connection.peer_max_pdu_length = match *length {
                    0 => u32::MAX,
                    length => length,
                };
            }
        }
        let mut results = Vec::with_capacity(request.presentation_contexts.len());
        for proposed in &request.presentation_contexts {
            let abstract_syntax = trim(&proposed.abstract_syntax);
            let transfer_syntax = if self.abstract_syntaxes.contains(abstract_syntax) {
                let candidates = proposed
                    .transfer_syntaxes
                    .iter()
                    .map(|t| trim(t))
                    .filter(|t| {
                        self.transfer_syntaxes
                            .as_ref()
                            .is_none_or(|allowed| allowed.iter().any(|a| a == t))
                    });
                choose_transfer_syntax(candidates, is_storage(abstract_syntax))
                    .ok_or(PresentationContextResultReason::TransferSyntaxesNotSupported)
            } else {
                Err(PresentationContextResultReason::AbstractSyntaxNotSupported)
            };
            tracing::trace!(
                id = proposed.id,
                abstract_syntax,
                proposed = ?proposed.transfer_syntaxes,
                accepted = ?transfer_syntax,
                "presentation context"
            );
            let (reason, transfer_syntax) = match transfer_syntax {
                Ok(ts) => {
                    connection.contexts.push(Accepted {
                        id: proposed.id,
                        abstract_syntax: abstract_syntax.to_owned(),
                        transfer_syntax: ts.to_owned(),
                    });
                    (PresentationContextResultReason::Acceptance, ts)
                }
                Err(reason) => (reason, IMPLICIT_VR_LE),
            };
            results.push(PresentationContextResult {
                id: proposed.id,
                reason,
                transfer_syntax: transfer_syntax.to_owned(),
            });
        }
        connection.peer_ae_title = request.calling_ae_title.trim().to_owned();
        let accept = Pdu::AssociationAC(AssociationAC {
            protocol_version: 1,
            calling_ae_title: request.calling_ae_title,
            called_ae_title: request.called_ae_title,
            application_context_name: APPLICATION_CONTEXT.to_owned(),
            presentation_contexts: results,
            user_variables: vec![
                UserVariableItem::MaxLength(config.max_pdu_length),
                UserVariableItem::ImplementationClassUID(IMPLEMENTATION_CLASS_UID.to_owned()),
                UserVariableItem::ImplementationVersionName(IMPLEMENTATION_VERSION_NAME.to_owned()),
            ],
        });
        connection.write_pdu(&accept).await?;
        Ok(connection)
    }
}

fn trim(uid: &str) -> &str {
    uid.trim_end_matches(['\0', ' '])
}

fn is_storage(abstract_syntax: &str) -> bool {
    !matches!(
        abstract_syntax,
        dicom_dictionary_std::uids::VERIFICATION
            | crate::PATIENT_ROOT_FIND
            | crate::PATIENT_ROOT_MOVE
            | crate::STUDY_ROOT_FIND
            | crate::STUDY_ROOT_MOVE
    )
}

/// The transfer syntax to accept among those proposed for one presentation context (in the
/// requestor's order of preference). Storage contexts prefer a non-native syntax: when a
/// sender offers JPEG and Explicit VR Little Endian together, its data is JPEG. Other
/// contexts prefer native syntaxes.
fn choose_transfer_syntax<'a>(
    proposed: impl Iterator<Item = &'a str>,
    storage: bool,
) -> Option<&'a str> {
    let known: Vec<&str> = proposed
        .filter(|ts| {
            TransferSyntaxRegistry
                .get(ts)
                .is_some_and(|t| !t.is_unsupported())
        })
        .collect();
    known
        .iter()
        .find(|ts| NATIVE.contains(ts) != storage)
        .or(known.first())
        .copied()
}

/// An accepted presentation context.
#[derive(Debug, Clone)]
struct Accepted {
    id: u8,
    abstract_syntax: String,
    transfer_syntax: String,
}

pub struct Connection {
    socket: TcpStream,
    /// Bytes received and not yet decoded into PDUs.
    buffer: BytesMut,
    contexts: Vec<Accepted>,
    peer_ae_title: String,
    peer_max_pdu_length: u32,
    max_pdu_length: u32,
    timeout: Duration,
    /// Fragments of the last P-DATA PDU not consumed yet.
    fragments: VecDeque<PDataValue>,
    /// PDUs received while looking for a cancel request, to be processed next.
    deferred: VecDeque<Pdu>,
    max_data_set: usize,
    released: bool,
}

impl Connection {
    pub fn peer_ae_title(&self) -> &str {
        &self.peer_ae_title
    }

    /// Abstract and transfer syntax of an accepted presentation context.
    pub fn context(&self, id: u8) -> Option<(&str, &str)> {
        self.contexts
            .iter()
            .find(|pc| pc.id == id)
            .map(|pc| (pc.abstract_syntax.as_str(), pc.transfer_syntax.as_str()))
    }

    /// Read one PDU; cancel safe (partial input stays in the buffer).
    async fn read_pdu(&mut self) -> Result<Pdu, Error> {
        let read = read_pdu_from_wire_async(
            &mut self.socket,
            &mut self.buffer,
            self.max_pdu_length,
            true,
        );
        Ok(tokio::time::timeout(self.timeout, read)
            .await
            .map_err(|_| Error::Timeout)??)
    }

    async fn write_pdu(&mut self, pdu: &Pdu) -> Result<(), Error> {
        let mut bytes = Vec::new();
        write_pdu(&mut bytes, pdu)?;
        tokio::time::timeout(self.timeout, self.socket.write_all(&bytes))
            .await
            .map_err(|_| Error::Timeout)??;
        Ok(())
    }

    /// The next message. `data_prefix` gives the bytes to put before the data set of a
    /// command, from the command, the transfer syntax of its presentation context and the
    /// peer AE title, so that a received instance can be stored without a copy.
    pub async fn receive(
        &mut self,
        data_prefix: impl FnOnce(&Command, &str, &str) -> Vec<u8>,
    ) -> Result<Incoming, Error> {
        let mut command = Vec::new();
        let mut context = None;
        loop {
            let Some(fragment) = self.next_fragment().await? else {
                return Ok(self.closed());
            };
            if fragment.value_type != PDataValueType::Command {
                return Err(Error::Protocol(
                    "data set received before its command".into(),
                ));
            }
            if *context.get_or_insert(fragment.presentation_context_id)
                != fragment.presentation_context_id
            {
                return Err(Error::Protocol(
                    "command spans presentation contexts".into(),
                ));
            }
            if command.is_empty() && fragment.is_last {
                command = fragment.data;
            } else {
                command.extend_from_slice(&fragment.data);
            }
            if fragment.is_last {
                break;
            }
        }
        let context = context.unwrap_or_default();
        let command = Command::parse(&command)?;
        if !command.has_data_set() {
            return Ok(Incoming::Message(Message {
                context,
                command,
                data: None,
            }));
        }
        let transfer_syntax = self.context(context).map_or("", |(_, ts)| ts);
        let mut data = data_prefix(&command, transfer_syntax, &self.peer_ae_title);
        let limit = data.len().saturating_add(self.max_data_set);
        loop {
            let Some(fragment) = self.next_fragment().await? else {
                return Ok(self.closed());
            };
            if fragment.value_type != PDataValueType::Data
                || fragment.presentation_context_id != context
            {
                return Err(Error::Protocol(
                    "expected the data set of the current command".into(),
                ));
            }
            if data.len() + fragment.data.len() > limit {
                return Err(Error::TooLarge(self.max_data_set));
            }
            data.extend_from_slice(&fragment.data);
            if fragment.is_last {
                break;
            }
        }
        Ok(Incoming::Message(Message {
            context,
            command,
            data: Some(data),
        }))
    }

    fn closed(&self) -> Incoming {
        if self.released {
            Incoming::Released
        } else {
            Incoming::Aborted
        }
    }

    async fn next_fragment(&mut self) -> Result<Option<PDataValue>, Error> {
        loop {
            if let Some(fragment) = self.fragments.pop_front() {
                return Ok(Some(fragment));
            }
            let pdu = match self.deferred.pop_front() {
                Some(pdu) => pdu,
                None => self.read_pdu().await?,
            };
            match pdu {
                Pdu::PData { data } => self.fragments.extend(data),
                Pdu::ReleaseRQ => {
                    self.write_pdu(&Pdu::ReleaseRP).await?;
                    self.released = true;
                    return Ok(None);
                }
                Pdu::AbortRQ { .. } => return Ok(None),
                pdu => {
                    return Err(Error::Protocol(format!("unexpected PDU {pdu:?}")));
                }
            }
        }
    }

    /// Send a command, then its data set when given.
    pub async fn send(
        &mut self,
        context: u8,
        command: &Command,
        data: Option<&[u8]>,
    ) -> Result<(), Error> {
        let (max, timeout) = (self.peer_max_pdu_length, self.timeout);
        let socket = &mut self.socket;
        pdata::send(socket, context, true, &command.encode(), max, timeout).await?;
        if let Some(data) = data {
            pdata::send(socket, context, false, data, max, timeout).await?;
        }
        Ok(())
    }

    /// Whether the peer asked to cancel request `message_id`, looking only at what has
    /// already arrived. A release or abort counts as a cancel; other PDUs are kept for
    /// [`receive`](Self::receive).
    pub fn cancelled(&mut self, message_id: u16) -> bool {
        while let Some(Ok(pdu)) = self.read_pdu().now_or_never() {
            match &pdu {
                Pdu::PData { data }
                    if data.len() == 1
                        && data[0].value_type == PDataValueType::Command
                        && data[0].is_last =>
                {
                    if let Ok(command) = Command::parse(&data[0].data)
                        && command.field() == Some(field::C_CANCEL_RQ)
                        && command.u16(tags::MESSAGE_ID_BEING_RESPONDED_TO) == Some(message_id)
                    {
                        return true;
                    }
                }
                Pdu::ReleaseRQ | Pdu::AbortRQ { .. } => {
                    self.deferred.push_back(pdu);
                    return true;
                }
                _ => {}
            }
            self.deferred.push_back(pdu);
        }
        false
    }

    /// Abort the association (provider initiated) and close the connection.
    pub async fn abort(mut self) {
        let abort = Pdu::AbortRQ {
            source: AbortRQSource::ServiceProvider(
                AbortRQServiceProviderReason::ReasonNotSpecified,
            ),
        };
        let _ = self.write_pdu(&abort).await;
        let _ = self.socket.shutdown().await;
    }

    pub async fn close(mut self) {
        let _ = self.socket.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_syntax_preferences() {
        let jpeg = "1.2.840.10008.1.2.4.50";
        let explicit = "1.2.840.10008.1.2.1";
        let offered = [explicit, IMPLICIT_VR_LE, jpeg];
        assert_eq!(
            choose_transfer_syntax(offered.into_iter(), true),
            Some(jpeg)
        );
        assert_eq!(
            choose_transfer_syntax(offered.into_iter(), false),
            Some(explicit)
        );
        assert_eq!(
            choose_transfer_syntax([IMPLICIT_VR_LE, explicit].into_iter(), true),
            Some(IMPLICIT_VR_LE)
        );
        assert_eq!(
            choose_transfer_syntax([jpeg].into_iter(), false),
            Some(jpeg)
        );
        assert_eq!(choose_transfer_syntax(["1.2.3.4"].into_iter(), true), None);
    }
}
