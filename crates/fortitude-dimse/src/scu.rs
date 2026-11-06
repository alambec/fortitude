//! A minimal DIMSE service user over `dicom-ul`: one request at a time, responses collected
//! up to the final one, C-STORE sub-operations of a C-GET answered on the way. Used by the
//! tests and the benchmark tool.

use std::time::Duration;

use dicom_dictionary_std::tags;
use dicom_ul::Pdu;
use dicom_ul::association::Association;
use dicom_ul::association::client::{AsyncClientAssociation, ClientAssociationOptions};
use dicom_ul::pdu::{PresentationContextResultReason, UserVariableItem};
use tokio::net::{TcpStream, ToSocketAddrs};

use crate::association::Error;
use crate::command::{Command, field, status};
use crate::pdata;

/// A response: its command and data set (identifier), as received.
#[derive(Debug)]
pub struct Response {
    pub command: Command,
    pub data: Option<Vec<u8>>,
}

pub struct Scu {
    association: AsyncClientAssociation<TcpStream>,
    message_id: u16,
    timeout: Duration,
}

impl Scu {
    /// Request an association proposing `contexts` (abstract syntax, transfer syntaxes).
    pub async fn connect(
        address: impl ToSocketAddrs,
        calling_ae_title: &str,
        called_ae_title: &str,
        contexts: &[(String, Vec<String>)],
        timeout: Duration,
    ) -> Result<Self, Error> {
        Self::connect_with_scp_role(
            address,
            calling_ae_title,
            called_ae_title,
            contexts,
            &[],
            timeout,
        )
        .await
    }

    /// [`connect`](Self::connect), proposing the SCP role as well for the `scp_role` SOP
    /// classes (storage classes, to receive the instances of a C-GET).
    pub async fn connect_with_scp_role(
        address: impl ToSocketAddrs,
        calling_ae_title: &str,
        called_ae_title: &str,
        contexts: &[(String, Vec<String>)],
        scp_role: &[String],
        timeout: Duration,
    ) -> Result<Self, Error> {
        let mut options = ClientAssociationOptions::new()
            .calling_ae_title(calling_ae_title.to_owned())
            .called_ae_title(called_ae_title.to_owned())
            .max_pdu_length(256 * 1024)
            .read_timeout(timeout)
            .write_timeout(timeout);
        for (abstract_syntax, transfer_syntaxes) in contexts {
            options = options
                .with_presentation_context(abstract_syntax.clone(), transfer_syntaxes.clone());
        }
        for sop_class in scp_role {
            options = options.with_role_selection(sop_class.clone(), true, true);
        }
        let mut association = options.establish_async(address).await?;
        association.inner_stream().set_nodelay(true)?;
        Ok(Self {
            association,
            message_id: 0,
            timeout,
        })
    }

    /// An accepted presentation context for `abstract_syntax` (with `transfer_syntax` when
    /// given): its ID and transfer syntax.
    pub fn context(
        &self,
        abstract_syntax: &str,
        transfer_syntax: Option<&str>,
    ) -> Option<(u8, &str)> {
        self.association
            .presentation_contexts()
            .iter()
            .find(|pc| {
                pc.reason == PresentationContextResultReason::Acceptance
                    && pc.abstract_syntax == abstract_syntax
                    && transfer_syntax.is_none_or(|ts| pc.transfer_syntax == ts)
            })
            .map(|pc| (pc.id, pc.transfer_syntax.as_str()))
    }

    /// Whether the acceptor let this node take the SCP role for `sop_class`.
    pub fn scp_role(&self, sop_class: &str) -> bool {
        self.association.user_variables().iter().any(|v| {
            matches!(v, UserVariableItem::ScuScpRoleSelectionSubItem(c, roles)
                if c.trim_end_matches('\0') == sop_class && roles.scp)
        })
    }

    /// Send a request on presentation context `context` (its Message ID is set here) and
    /// collect the responses, up to the final one. C-STORE requests received meanwhile are
    /// refused.
    pub async fn request(
        &mut self,
        context: u8,
        command: Command,
        data: Option<&[u8]>,
    ) -> Result<Vec<Response>, Error> {
        self.request_with(context, command, data, |_, _, _| status::OUT_OF_RESOURCES)
            .await
    }

    /// [`request`](Self::request), answering each C-STORE request received meanwhile (the
    /// sub-operations of a C-GET) with the status returned by `on_store`, given the request,
    /// the transfer syntax of its presentation context and its data set.
    pub async fn request_with(
        &mut self,
        context: u8,
        command: Command,
        data: Option<&[u8]>,
        mut on_store: impl FnMut(&Command, &str, Vec<u8>) -> u16,
    ) -> Result<Vec<Response>, Error> {
        self.message_id = self.message_id.wrapping_add(1);
        let command = command
            .with_u16(tags::MESSAGE_ID, self.message_id)
            .with_data_set(data.is_some());
        let max_pdu_length = self.association.peer_max_pdu_length();
        let timeout = self.timeout;
        let stream = self.association.inner_stream();
        pdata::send(
            stream,
            context,
            true,
            &command.encode(),
            max_pdu_length,
            timeout,
        )
        .await?;
        if let Some(data) = data {
            pdata::send(stream, context, false, data, max_pdu_length, timeout).await?;
        }
        let mut responses = Vec::new();
        loop {
            let (pc, response) = self.receive().await?;
            if response.command.field() == Some(field::C_STORE_RQ) {
                let transfer_syntax = self
                    .association
                    .presentation_contexts()
                    .iter()
                    .find(|c| c.id == pc)
                    .map(|c| c.transfer_syntax.clone())
                    .unwrap_or_default();
                let status = on_store(
                    &response.command,
                    &transfer_syntax,
                    response.data.unwrap_or_default(),
                );
                let reply = response.command.response(status, false).encode();
                let stream = self.association.inner_stream();
                pdata::send(stream, pc, true, &reply, max_pdu_length, timeout).await?;
                continue;
            }
            let pending = matches!(response.command.status(), Some(status::PENDING | 0xFF01));
            responses.push(response);
            if !pending {
                return Ok(responses);
            }
        }
    }

    /// The next message and its presentation context.
    async fn receive(&mut self) -> Result<(u8, Response), Error> {
        let mut command: Option<Command> = None;
        let mut bytes = Vec::new();
        loop {
            let data = match self.association.receive().await? {
                Pdu::PData { data } => data,
                pdu => return Err(Error::Protocol(format!("unexpected PDU {pdu:?}"))),
            };
            for fragment in data {
                let pc = fragment.presentation_context_id;
                bytes.extend_from_slice(&fragment.data);
                if !fragment.is_last {
                    continue;
                }
                match command.take() {
                    None => {
                        let parsed = Command::parse(&bytes)?;
                        bytes.clear();
                        if !parsed.has_data_set() {
                            let response = Response {
                                command: parsed,
                                data: None,
                            };
                            return Ok((pc, response));
                        }
                        command = Some(parsed);
                    }
                    Some(command) => {
                        let response = Response {
                            command,
                            data: Some(bytes),
                        };
                        return Ok((pc, response));
                    }
                }
            }
        }
    }

    pub async fn release(self) -> Result<(), Error> {
        Ok(self.association.release().await?)
    }
}
