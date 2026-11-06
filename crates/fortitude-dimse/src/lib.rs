//! DIMSE services (PS3.7) as an SCP: Verification (C-ECHO), Storage (C-STORE), and the
//! Patient Root and Study Root Query/Retrieve models (C-FIND, C-MOVE, C-GET).
//!
//! Received instances go through the same ingest as STOW-RS, queries through the same
//! search as QIDO-RS. C-MOVE and C-GET send the stored files as they are (to the move
//! destination, or back on the requestor's association), unless the receiver only accepts
//! another transfer syntax.

pub mod association;
pub mod command;
mod echo;
mod find;
pub mod part10;
mod pdata;
mod retrieve;
pub mod scu;
mod sop_classes;
mod store;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dicom_dictionary_std::uids;
use fortitude_store::Store;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::association::{Acceptor, Connection, Incoming, Message};
use crate::command::{field, status};

pub use crate::part10::IMPLEMENTATION_CLASS_UID;

/// A remote application entity: a C-MOVE destination, and when
/// [`Config::allow_unknown_peers`] is off, one of the only peers allowed to associate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub ae_title: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub ae_title: String,
    /// Reject associations whose called AE title is not [`ae_title`](Self::ae_title).
    pub check_called_ae_title: bool,
    /// Accept associations from AE titles that are not in [`peers`](Self::peers).
    pub allow_unknown_peers: bool,
    pub peers: Vec<Peer>,
    /// Concurrent associations; more are rejected (transient, local limit exceeded).
    pub max_associations: usize,
    /// Largest PDU this node accepts.
    pub max_pdu_length: u32,
    /// Time allowed for each read or write on an association, and to connect to a C-MOVE
    /// destination.
    pub timeout: Duration,
    /// Maximum number of C-FIND matches (0 for no limit); beyond it, the query fails with
    /// status A700 after the first matches.
    pub max_find_results: usize,
    /// Largest data set accepted in a C-STORE request.
    pub max_instance_size: usize,
    /// Storage SOP classes accepted besides the standard ones (e.g. private classes).
    pub extra_sop_classes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ae_title: "FORTITUDE".to_owned(),
            check_called_ae_title: false,
            allow_unknown_peers: true,
            peers: Vec::new(),
            max_associations: 64,
            max_pdu_length: 256 * 1024,
            timeout: Duration::from_secs(30),
            max_find_results: 10_000,
            max_instance_size: 4096 << 20,
            extra_sop_classes: Vec::new(),
        }
    }
}

impl Config {
    fn peer(&self, ae_title: &str) -> Option<&Peer> {
        self.peers.iter().find(|p| p.ae_title == ae_title)
    }
}

/// Query/Retrieve SOP classes (PS3.4 Annex C).
pub(crate) const PATIENT_ROOT_FIND: &str = uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND;
pub(crate) const PATIENT_ROOT_MOVE: &str = uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
pub(crate) const STUDY_ROOT_FIND: &str = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_FIND;
pub(crate) const STUDY_ROOT_MOVE: &str = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_MOVE;
pub(crate) const PATIENT_ROOT_GET: &str = uids::PATIENT_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_GET;
pub(crate) const STUDY_ROOT_GET: &str = uids::STUDY_ROOT_QUERY_RETRIEVE_INFORMATION_MODEL_GET;

/// The DIMSE service: shared by all associations.
#[derive(Clone)]
pub struct Dimse {
    pub store: Arc<Store>,
    pub config: Arc<Config>,
}

/// Accept associations on `listener` until `shutdown` completes, then wait (up to the
/// configured timeout) for the open ones to end.
pub async fn serve(listener: TcpListener, dimse: Dimse, shutdown: impl Future<Output = ()>) {
    let config = dimse.config.clone();
    let acceptor = Arc::new(Acceptor::new(config.clone()));
    let slots = Arc::new(Semaphore::new(config.max_associations));
    tokio::pin!(shutdown);
    loop {
        let (socket, address) = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot accept DIMSE connection");
                    continue;
                }
            },
        };
        if let Err(e) = socket.set_nodelay(true) {
            tracing::warn!(error = %e, "cannot set TCP_NODELAY");
        }
        let slot = slots.clone().try_acquire_owned().ok();
        let (dimse, acceptor) = (dimse.clone(), acceptor.clone());
        tokio::spawn(async move {
            let connection = match acceptor.establish(socket, slot.is_none()).await {
                Ok(connection) => connection,
                Err(e) => {
                    tracing::info!(%address, error = %e, "association refused");
                    return;
                }
            };
            metrics::counter!("fortitude_dimse_associations_total").increment(1);
            tracing::debug!(%address, calling = connection.peer_ae_title(), "association accepted");
            run(&dimse, connection, address).await;
            drop(slot);
        });
    }
    let all = u32::try_from(config.max_associations).unwrap_or(u32::MAX);
    if tokio::time::timeout(config.timeout, slots.acquire_many(all))
        .await
        .is_err()
    {
        tracing::warn!("closing DIMSE associations still open");
    }
}

async fn run(dimse: &Dimse, mut connection: Connection, address: SocketAddr) {
    loop {
        let incoming = connection.receive(store::data_prefix).await;
        let message = match incoming {
            Ok(Incoming::Message(message)) => message,
            Ok(Incoming::Released) => {
                connection.close().await;
                return;
            }
            Ok(Incoming::Aborted) => return,
            Err(e) => {
                tracing::warn!(%address, error = %e, "association aborted");
                connection.abort().await;
                return;
            }
        };
        let started = Instant::now();
        let command_field = message.command.field().unwrap_or_default();
        let result = dispatch(dimse, &mut connection, message).await;
        let name = command_name(command_field);
        metrics::histogram!("fortitude_dimse_request_duration_seconds", "command" => name)
            .record(started.elapsed().as_secs_f64());
        match result {
            Ok(status) => {
                metrics::counter!("fortitude_dimse_requests_total", "command" => name, "status" => status_class(status))
                    .increment(1);
            }
            Err(e) => {
                tracing::warn!(%address, command = name, error = %e, "association aborted");
                connection.abort().await;
                return;
            }
        }
    }
}

/// Handle one request; returns the final status sent.
async fn dispatch(
    dimse: &Dimse,
    connection: &mut Connection,
    message: Message,
) -> Result<u16, association::Error> {
    let Some((abstract_syntax, _)) = connection.context(message.context) else {
        return Err(association::Error::Protocol(format!(
            "presentation context {} was not accepted",
            message.context
        )));
    };
    let abstract_syntax = abstract_syntax.to_owned();
    match message.command.field() {
        Some(field::C_ECHO_RQ) => echo::handle(connection, message).await,
        Some(field::C_STORE_RQ) => store::handle(dimse, connection, message).await,
        Some(field::C_FIND_RQ)
            if abstract_syntax == PATIENT_ROOT_FIND || abstract_syntax == STUDY_ROOT_FIND =>
        {
            find::handle(dimse, connection, message).await
        }
        Some(field::C_MOVE_RQ)
            if abstract_syntax == PATIENT_ROOT_MOVE || abstract_syntax == STUDY_ROOT_MOVE =>
        {
            retrieve::handle(dimse, connection, message).await
        }
        Some(field::C_GET_RQ)
            if abstract_syntax == PATIENT_ROOT_GET || abstract_syntax == STUDY_ROOT_GET =>
        {
            retrieve::handle(dimse, connection, message).await
        }
        // A cancel for a request that is already complete.
        Some(field::C_CANCEL_RQ) => Ok(status::SUCCESS),
        _ => {
            let response = message
                .command
                .response(status::UNRECOGNIZED_OPERATION, false);
            connection.send(message.context, &response, None).await?;
            Ok(status::UNRECOGNIZED_OPERATION)
        }
    }
}

fn command_name(field: u16) -> &'static str {
    match field {
        field::C_ECHO_RQ => "C-ECHO",
        field::C_STORE_RQ => "C-STORE",
        field::C_FIND_RQ => "C-FIND",
        field::C_MOVE_RQ => "C-MOVE",
        field::C_GET_RQ => "C-GET",
        field::C_CANCEL_RQ => "C-CANCEL",
        _ => "other",
    }
}

fn status_class(status: u16) -> &'static str {
    match status {
        status::SUCCESS => "success",
        status::CANCEL => "cancel",
        s if status::is_warning(s) => "warning",
        _ => "failure",
    }
}
