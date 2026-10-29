//! P-DATA-TF PDUs written straight to the socket (PS3.8 §9.3.5).
//!
//! `dicom-ul`'s asynchronous P-DATA writer blocks the runtime thread when dropped, so
//! commands and data sets are framed here instead.

use std::time::Duration;

use tokio::io::{AsyncWrite, AsyncWriteExt};

/// PDU header (type, reserved, length) and PDV item header (length, context, control).
const HEADERS: usize = 6 + 6;

/// Write `data` as one command or data set message on presentation context `context`,
/// split into PDUs whose variable part is at most `max_pdu_length` bytes, each written
/// within `timeout`.
pub async fn send<W: AsyncWrite + Unpin>(
    stream: &mut W,
    context: u8,
    command: bool,
    data: &[u8],
    max_pdu_length: u32,
    timeout: Duration,
) -> std::io::Result<()> {
    let chunk = (max_pdu_length as usize)
        .saturating_sub(6)
        .clamp(1, 1 << 20);
    let mut buffer = Vec::with_capacity(HEADERS + chunk.min(data.len()));
    let mut offset = 0;
    loop {
        let end = (offset + chunk).min(data.len());
        let part = &data[offset..end];
        let last = end == data.len();
        let control = u8::from(command) | (u8::from(last) << 1);
        buffer.clear();
        buffer.extend([0x04, 0x00]);
        buffer.extend(((part.len() + 6) as u32).to_be_bytes());
        buffer.extend(((part.len() + 2) as u32).to_be_bytes());
        buffer.extend([context, control]);
        buffer.extend_from_slice(part);
        tokio::time::timeout(timeout, stream.write_all(&buffer))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
        if last {
            break;
        }
        offset = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use dicom_ul::Pdu;
    use dicom_ul::pdu::{PDataValueType, read_pdu};

    use super::*;

    #[tokio::test]
    async fn splits_messages_into_pdus() {
        let data: Vec<u8> = (0..=255).collect();
        let mut out = Vec::new();
        send(&mut out, 3, false, &data, 106, Duration::from_secs(1))
            .await
            .unwrap();
        let mut cursor = std::io::Cursor::new(&out[..]);
        let mut received = Vec::new();
        let mut pdus = 0;
        while let Some(Pdu::PData { data: values }) = read_pdu(&mut cursor, 1024, true).unwrap() {
            pdus += 1;
            for value in values {
                assert_eq!(value.presentation_context_id, 3);
                assert_eq!(value.value_type, PDataValueType::Data);
                assert_eq!(
                    value.is_last,
                    received.len() + value.data.len() == data.len()
                );
                received.extend(value.data);
            }
        }
        assert_eq!(received, data);
        assert_eq!(pdus, 3);

        let mut out = Vec::new();
        send(&mut out, 1, true, &[], 106, Duration::from_secs(1))
            .await
            .unwrap();
        let mut cursor = std::io::Cursor::new(&out[..]);
        let Some(Pdu::PData { data: values }) = read_pdu(&mut cursor, 1024, true).unwrap() else {
            panic!("expected a PDU");
        };
        assert!(values[0].is_last && values[0].value_type == PDataValueType::Command);
    }
}
