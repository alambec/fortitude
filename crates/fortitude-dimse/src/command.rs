//! DIMSE command sets (PS3.7 §6.3, §E): group 0000 elements, always encoded in Implicit VR
//! Little Endian, with Command Group Length first.
//!
//! Command sets are small and their attributes few, so they are encoded and decoded here by
//! hand rather than through a generic data set.

use dicom_core::Tag;
use dicom_dictionary_std::tags;

/// Command Field (0000,0100) values.
pub mod field {
    pub const C_STORE_RQ: u16 = 0x0001;
    pub const C_GET_RQ: u16 = 0x0010;
    pub const C_FIND_RQ: u16 = 0x0020;
    pub const C_MOVE_RQ: u16 = 0x0021;
    pub const C_ECHO_RQ: u16 = 0x0030;
    pub const C_CANCEL_RQ: u16 = 0x0FFF;
    /// Set on the command field of every response.
    pub const RESPONSE: u16 = 0x8000;
}

/// Status (0000,0900) values used by Fortitude (PS3.7 Annex C, PS3.4 Annex B and C).
pub mod status {
    pub const SUCCESS: u16 = 0x0000;
    pub const PENDING: u16 = 0xFF00;
    pub const CANCEL: u16 = 0xFE00;
    /// Sub-operations complete, one or more failures or warnings.
    pub const SUBOPERATIONS_WARNING: u16 = 0xB000;
    pub const UNRECOGNIZED_OPERATION: u16 = 0x0211;
    pub const OUT_OF_RESOURCES: u16 = 0xA700;
    /// Out of resources: unable to perform sub-operations.
    pub const SUBOPERATIONS_REFUSED: u16 = 0xA702;
    pub const MOVE_DESTINATION_UNKNOWN: u16 = 0xA801;
    pub const IDENTIFIER_MISMATCH: u16 = 0xA900;
    pub const UNABLE_TO_PROCESS: u16 = 0xC000;

    /// Warning statuses of a C-STORE response (PS3.4 Table B.2-1).
    pub fn is_warning(status: u16) -> bool {
        status == 0x0001 || status & 0xF000 == 0xB000
    }
}

/// Command Data Set Type (0000,0800) when no data set follows.
const NO_DATA_SET: u16 = 0x0101;
/// Command Data Set Type used when a data set follows (any other value is valid).
const DATA_SET: u16 = 0x0001;

#[derive(Debug, thiserror::Error)]
#[error("malformed command set: {0}")]
pub struct CommandError(&'static str);

/// A command set: its elements in tag order, values as encoded (with padding).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Command {
    elements: Vec<(Tag, Vec<u8>)>,
}

impl Command {
    pub fn parse(bytes: &[u8]) -> Result<Self, CommandError> {
        let mut elements = Vec::new();
        let mut pos = 0;
        while pos < bytes.len() {
            let header = bytes
                .get(pos..pos + 8)
                .ok_or(CommandError("truncated header"))?;
            let tag = Tag(
                u16::from_le_bytes([header[0], header[1]]),
                u16::from_le_bytes([header[2], header[3]]),
            );
            let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
            pos += 8;
            let value = bytes
                .get(pos..pos.saturating_add(length))
                .ok_or(CommandError("truncated value"))?;
            pos += length;
            if tag.group() != 0x0000 {
                return Err(CommandError("element outside group 0000"));
            }
            if tag != tags::COMMAND_GROUP_LENGTH {
                elements.push((tag, value.to_vec()));
            }
        }
        Ok(Self { elements })
    }

    /// Encoded command set, Command Group Length included.
    pub fn encode(&self) -> Vec<u8> {
        let mut elements: Vec<&(Tag, Vec<u8>)> = self.elements.iter().collect();
        elements.sort_by_key(|(tag, _)| *tag);
        let length: usize = elements.iter().map(|(_, v)| 8 + v.len()).sum();
        let mut out = Vec::with_capacity(12 + length);
        push_element(
            &mut out,
            tags::COMMAND_GROUP_LENGTH,
            &(length as u32).to_le_bytes(),
        );
        for (tag, value) in elements {
            push_element(&mut out, *tag, value);
        }
        out
    }

    fn raw(&self, tag: Tag) -> Option<&[u8]> {
        self.elements
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_slice())
    }

    pub fn u16(&self, tag: Tag) -> Option<u16> {
        match self.raw(tag)? {
            [a, b, ..] => Some(u16::from_le_bytes([*a, *b])),
            _ => None,
        }
    }

    /// A text or UID value without its padding.
    pub fn string(&self, tag: Tag) -> Option<String> {
        let value = std::str::from_utf8(self.raw(tag)?).ok()?;
        let value = value.trim_matches(['\0', ' ']);
        (!value.is_empty()).then(|| value.to_owned())
    }

    pub fn field(&self) -> Option<u16> {
        self.u16(tags::COMMAND_FIELD)
    }

    pub fn message_id(&self) -> Option<u16> {
        self.u16(tags::MESSAGE_ID)
    }

    pub fn status(&self) -> Option<u16> {
        self.u16(tags::STATUS)
    }

    pub fn has_data_set(&self) -> bool {
        self.u16(tags::COMMAND_DATA_SET_TYPE)
            .is_some_and(|t| t != NO_DATA_SET)
    }

    fn set(&mut self, tag: Tag, value: Vec<u8>) {
        match self.elements.iter_mut().find(|(t, _)| *t == tag) {
            Some((_, v)) => *v = value,
            None => self.elements.push((tag, value)),
        }
    }

    pub fn with_u16(mut self, tag: Tag, value: u16) -> Self {
        self.set(tag, value.to_le_bytes().to_vec());
        self
    }

    /// A UID, padded with a null byte to an even length.
    pub fn with_uid(mut self, tag: Tag, value: &str) -> Self {
        let mut bytes = value.as_bytes().to_vec();
        if bytes.len() % 2 == 1 {
            bytes.push(0);
        }
        self.set(tag, bytes);
        self
    }

    /// Text (AE, LO), padded with a space to an even length.
    pub fn with_text(mut self, tag: Tag, value: &str) -> Self {
        let mut bytes = value.as_bytes().to_vec();
        if bytes.len() % 2 == 1 {
            bytes.push(b' ');
        }
        self.set(tag, bytes);
        self
    }

    pub fn with_data_set(self, present: bool) -> Self {
        let kind = if present { DATA_SET } else { NO_DATA_SET };
        self.with_u16(tags::COMMAND_DATA_SET_TYPE, kind)
    }

    /// The response to this request: same SOP class (and instance, when given), message ID
    /// being responded to, and `status`.
    pub fn response(&self, status: u16, data_set: bool) -> Self {
        let mut response = Command::default()
            .with_u16(
                tags::COMMAND_FIELD,
                self.field().unwrap_or_default() | field::RESPONSE,
            )
            .with_u16(
                tags::MESSAGE_ID_BEING_RESPONDED_TO,
                self.message_id().unwrap_or_default(),
            )
            .with_u16(tags::STATUS, status)
            .with_data_set(data_set);
        for tag in [
            tags::AFFECTED_SOP_CLASS_UID,
            tags::AFFECTED_SOP_INSTANCE_UID,
        ] {
            if let Some(value) = self.raw(tag) {
                response.set(tag, value.to_vec());
            }
        }
        response
    }
}

fn push_element(out: &mut Vec<u8>, tag: Tag, value: &[u8]) {
    out.extend(tag.group().to_le_bytes());
    out.extend(tag.element().to_le_bytes());
    out.extend((value.len() as u32).to_le_bytes());
    out.extend(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_response() {
        let request = Command::default()
            .with_u16(tags::COMMAND_FIELD, field::C_STORE_RQ)
            .with_u16(tags::MESSAGE_ID, 7)
            .with_uid(tags::AFFECTED_SOP_CLASS_UID, "1.2.840.10008.5.1.4.1.1.2")
            .with_uid(tags::AFFECTED_SOP_INSTANCE_UID, "1.2.3")
            .with_text(tags::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE, "SCU")
            .with_data_set(true);
        let encoded = request.encode();
        // Command Group Length counts every following byte.
        assert_eq!(
            u32::from_le_bytes(encoded[8..12].try_into().unwrap()) as usize,
            encoded.len() - 12
        );
        let parsed = Command::parse(&encoded).unwrap();
        assert_eq!(parsed.encode(), encoded);
        assert_eq!(parsed.field(), Some(field::C_STORE_RQ));
        assert_eq!(
            parsed.string(tags::AFFECTED_SOP_INSTANCE_UID).as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            parsed
                .string(tags::MOVE_ORIGINATOR_APPLICATION_ENTITY_TITLE)
                .as_deref(),
            Some("SCU")
        );
        assert!(parsed.has_data_set());

        let response = parsed.response(status::SUCCESS, false);
        assert_eq!(response.field(), Some(0x8001));
        assert_eq!(response.u16(tags::MESSAGE_ID_BEING_RESPONDED_TO), Some(7));
        assert_eq!(
            response.string(tags::AFFECTED_SOP_INSTANCE_UID).as_deref(),
            Some("1.2.3")
        );
        assert!(!response.has_data_set());
    }

    #[test]
    fn rejects_truncated_input() {
        let encoded = Command::default()
            .with_u16(tags::COMMAND_FIELD, field::C_ECHO_RQ)
            .encode();
        assert!(Command::parse(&encoded[..encoded.len() - 1]).is_err());
    }
}
