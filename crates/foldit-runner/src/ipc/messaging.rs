//! Length-prefixed message protocol with optional message type framing
//!
//! Two protocols are supported:
//! - **Simple protocol**: `[4-byte length][payload]` - used for basic
//!   request/response
//! - **Typed protocol**: `[1-byte type][4-byte length][payload]` - used for
//!   streaming

use std::io::{Read, Write};

use anyhow::{Context, Result};

/// Message types for the typed protocol (used for streaming)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    /// Request message (orchestrator → worker)
    Request = 0x00,
    /// Final response message (worker → orchestrator)
    Response = 0x01,
    /// Intermediate streaming update (worker → orchestrator)
    StreamUpdate = 0x02,
}

impl TryFrom<u8> for MessageType {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0x00 => Ok(MessageType::Request),
            0x01 => Ok(MessageType::Response),
            0x02 => Ok(MessageType::StreamUpdate),
            _ => anyhow::bail!("invalid message type: 0x{value:02x}"),
        }
    }
}

// Simple Protocol (existing, for backward compatibility)

/// Send a length-prefixed message over a stream.
///
/// Protocol: `[4-byte length (little-endian)][message bytes]`
///
/// # Errors
///
/// Returns an error if the stream write or flush fails.
pub fn send_message(stream: &mut impl Write, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len())
        .context("message exceeds 4GB wire-protocol limit")?;
    stream
        .write_all(&len.to_le_bytes())
        .context("failed to write message length")?;
    stream
        .write_all(bytes)
        .context("failed to write message data")?;
    stream.flush().context("failed to flush stream")?;
    Ok(())
}

/// Receive a length-prefixed message from a stream.
///
/// Protocol: `[4-byte length (little-endian)][message bytes]`
///
/// # Errors
///
/// Returns an error if the length prefix or the payload can't be read.
pub fn receive_message(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut len_bytes = [0u8; 4];
    stream
        .read_exact(&mut len_bytes)
        .context("failed to read message length")?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    let mut buf = vec![0u8; len];
    stream
        .read_exact(&mut buf)
        .context("failed to read message data")?;
    Ok(buf)
}

// Typed Protocol (for streaming support)

/// Send a typed message over a stream.
///
/// Protocol: `[1-byte type][4-byte length (little-endian)][message bytes]`
///
/// # Errors
///
/// Returns an error if the stream write or flush fails.
pub fn send_typed_message(
    stream: &mut impl Write,
    msg_type: MessageType,
    bytes: &[u8],
) -> Result<()> {
    // Write message type
    stream
        .write_all(&[msg_type as u8])
        .context("failed to write message type")?;

    // Write length and payload
    let len = u32::try_from(bytes.len())
        .context("message exceeds 4GB wire-protocol limit")?;
    stream
        .write_all(&len.to_le_bytes())
        .context("failed to write message length")?;
    stream
        .write_all(bytes)
        .context("failed to write message data")?;
    stream.flush().context("failed to flush stream")?;
    Ok(())
}

/// Receive a typed message from a stream.
///
/// Protocol: `[1-byte type][4-byte length (little-endian)][message bytes]`.
/// Returns the message type and payload bytes.
///
/// # Errors
///
/// Returns an error if the read fails or the type byte is unrecognized.
pub fn receive_typed_message(
    stream: &mut impl Read,
) -> Result<(MessageType, Vec<u8>)> {
    // Read message type
    let mut type_byte = [0u8; 1];
    stream
        .read_exact(&mut type_byte)
        .context("failed to read message type")?;
    let msg_type = MessageType::try_from(type_byte[0])?;

    // Read length
    let mut len_bytes = [0u8; 4];
    stream
        .read_exact(&mut len_bytes)
        .context("failed to read message length")?;
    let len = u32::from_le_bytes(len_bytes) as usize;

    // Read payload
    let mut buf = vec![0u8; len];
    stream
        .read_exact(&mut buf)
        .context("failed to read message data")?;

    Ok((msg_type, buf))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    // Simple protocol tests

    #[test]
    fn test_send_receive_message_roundtrip() {
        let original = b"test message data".to_vec();
        let mut buffer = Cursor::new(Vec::new());

        send_message(&mut buffer, &original).unwrap();
        buffer.set_position(0);

        let received = receive_message(&mut buffer).unwrap();
        assert_eq!(received, original);
    }

    #[test]
    fn test_receive_message_invalid_length() {
        let mut buffer = Cursor::new(vec![0xFF, 0xFF, 0xFF, 0xFF]); // Length = u32::MAX
        assert!(receive_message(&mut buffer).is_err());
    }

    // Typed protocol tests

    #[test]
    fn test_typed_message_roundtrip_request() {
        let original = b"request payload".to_vec();
        let mut buffer = Cursor::new(Vec::new());

        send_typed_message(&mut buffer, MessageType::Request, &original)
            .unwrap();
        buffer.set_position(0);

        let (msg_type, received) = receive_typed_message(&mut buffer).unwrap();
        assert_eq!(msg_type, MessageType::Request);
        assert_eq!(received, original);
    }

    #[test]
    fn test_typed_message_roundtrip_response() {
        let original = b"response payload".to_vec();
        let mut buffer = Cursor::new(Vec::new());

        send_typed_message(&mut buffer, MessageType::Response, &original)
            .unwrap();
        buffer.set_position(0);

        let (msg_type, received) = receive_typed_message(&mut buffer).unwrap();
        assert_eq!(msg_type, MessageType::Response);
        assert_eq!(received, original);
    }

    #[test]
    fn test_typed_message_roundtrip_stream_update() {
        let original = b"stream update payload".to_vec();
        let mut buffer = Cursor::new(Vec::new());

        send_typed_message(&mut buffer, MessageType::StreamUpdate, &original)
            .unwrap();
        buffer.set_position(0);

        let (msg_type, received) = receive_typed_message(&mut buffer).unwrap();
        assert_eq!(msg_type, MessageType::StreamUpdate);
        assert_eq!(received, original);
    }

    #[test]
    fn test_typed_message_invalid_type() {
        // Invalid message type byte (0xFF)
        let mut buffer = Cursor::new(vec![0xFF, 0x00, 0x00, 0x00, 0x00]);
        assert!(receive_typed_message(&mut buffer).is_err());
    }

    #[test]
    fn test_typed_message_multiple_messages() {
        let mut buffer = Cursor::new(Vec::new());

        // Send multiple messages
        send_typed_message(&mut buffer, MessageType::Request, b"first")
            .unwrap();
        send_typed_message(&mut buffer, MessageType::StreamUpdate, b"update1")
            .unwrap();
        send_typed_message(&mut buffer, MessageType::StreamUpdate, b"update2")
            .unwrap();
        send_typed_message(&mut buffer, MessageType::Response, b"final")
            .unwrap();

        buffer.set_position(0);

        // Receive them in order
        let (t1, d1) = receive_typed_message(&mut buffer).unwrap();
        assert_eq!(t1, MessageType::Request);
        assert_eq!(d1, b"first");

        let (t2, d2) = receive_typed_message(&mut buffer).unwrap();
        assert_eq!(t2, MessageType::StreamUpdate);
        assert_eq!(d2, b"update1");

        let (t3, d3) = receive_typed_message(&mut buffer).unwrap();
        assert_eq!(t3, MessageType::StreamUpdate);
        assert_eq!(d3, b"update2");

        let (t4, d4) = receive_typed_message(&mut buffer).unwrap();
        assert_eq!(t4, MessageType::Response);
        assert_eq!(d4, b"final");
    }

    #[test]
    fn test_message_type_conversion() {
        assert_eq!(MessageType::try_from(0x00).unwrap(), MessageType::Request);
        assert_eq!(MessageType::try_from(0x01).unwrap(), MessageType::Response);
        assert_eq!(
            MessageType::try_from(0x02).unwrap(),
            MessageType::StreamUpdate
        );
        assert!(MessageType::try_from(0x03).is_err());
        assert!(MessageType::try_from(0xFF).is_err());
    }
}
