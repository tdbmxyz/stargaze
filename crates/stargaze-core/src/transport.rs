//! Shared transport types for network communication.
//!
//! Defines packet headers, control messages, and error types used by
//! both the server sender and client receiver.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Codec;
use crate::input::InputEvent;

/// Stream type tag for video datagrams.
pub const STREAM_TYPE_VIDEO: u8 = 0;

/// Stream type tag for audio datagrams.
pub const STREAM_TYPE_AUDIO: u8 = 1;

/// Maximum number of incomplete frames the assembler will buffer
/// before requesting an IDR.
pub const MAX_PENDING_FRAMES: usize = 8;

/// Minimum interval between IDR requests in milliseconds.
pub const IDR_RATE_LIMIT_MS: u64 = 100;

/// Conservative header size upper bound (bytes) for [`DatagramHeader`].
///
/// Postcard uses varint encoding, so the actual serialized size depends
/// on field values.  This constant is safe for any field combination and
/// avoids the need to serialize a sample header just to measure its
/// length.
pub const HEADER_SIZE_UPPER_BOUND: usize = 20;

/// Initial QUIC MTU for LAN streaming (1500 Ethernet − 20 IP − 8 UDP − 20 headroom).
pub const STREAMING_INITIAL_MTU: u16 = 1452;

/// Outgoing datagram send buffer size (4 MiB).
///
/// A single high-bitrate keyframe can be hundreds of KB; all its
/// fragments queue in this buffer before being written to the wire.
pub const DATAGRAM_SEND_BUFFER_SIZE: usize = 4 * 1024 * 1024;

/// Header prepended to each `QUIC` datagram.
///
/// Serialized with `postcard` (compact binary format). The remaining
/// bytes after the header are the fragment payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatagramHeader {
    /// Stream type: 0 = video, 1 = audio.
    pub stream_type: u8,
    /// Monotonically increasing frame index (per stream type).
    pub frame_index: u32,
    /// 0-based index of this fragment within the frame.
    pub fragment_index: u16,
    /// Total number of fragments in this frame.
    pub fragment_count: u16,
    /// Presentation timestamp (frame number from encoder).
    pub pts: u64,
    /// True for IDR/keyframes (video only, always false for audio).
    pub is_keyframe: bool,
}

/// Messages exchanged over the reliable control stream.
///
/// Length-prefixed with 4-byte LE length before the `postcard`-serialized body.
/// New variants may be appended without breaking backward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Client -> Server: request a streaming session.
    SessionRequest {
        /// Requested video width in pixels.
        width: u32,
        /// Requested video height in pixels.
        height: u32,
        /// Requested framerate.
        framerate: u32,
        /// Requested video codec.
        codec: Codec,
    },
    /// Server -> Client: confirm session parameters.
    SessionResponse {
        /// Confirmed video width in pixels.
        width: u32,
        /// Confirmed video height in pixels.
        height: u32,
        /// Confirmed framerate.
        framerate: u32,
        /// Bitrate in Mbps.
        bitrate_mbps: u32,
        /// Confirmed video codec.
        codec: Codec,
        /// Maximum datagram payload size for the connection.
        max_datagram_size: u16,
        /// Whether the cursor is embedded in video frames.
        cursor_embedded: bool,
    },
    /// Client -> Server: request an IDR keyframe (after packet loss).
    IdrRequest,
    /// Bidirectional: keepalive with timestamp.
    Ping {
        /// Millisecond timestamp from sender.
        timestamp_ms: u64,
    },
    /// Bidirectional: keepalive response.
    Pong {
        /// Echoed timestamp from the original `Ping`.
        timestamp_ms: u64,
    },
    /// Client -> Server: input event from keyboard, mouse, or gamepad.
    Input(InputEvent),
}

/// A fully reassembled frame ready for decoding.
#[derive(Debug, Clone)]
pub struct ReassembledFrame {
    /// Concatenated payload data from all fragments.
    pub data: Vec<u8>,
    /// Presentation timestamp.
    pub pts: u64,
    /// Whether this is a keyframe.
    pub is_keyframe: bool,
    /// Stream type (video or audio).
    pub stream_type: u8,
}

/// Errors from the transport subsystem.
#[derive(Error, Debug)]
pub enum TransportError {
    /// `QUIC` connection failed.
    #[error("connection error: {0}")]
    ConnectionError(String),

    /// Failed to send a datagram.
    #[error("send error: {0}")]
    SendError(String),

    /// Failed to read or write a control message.
    #[error("control channel error: {0}")]
    ControlError(String),

    /// Session handshake failed or was rejected.
    #[error("session error: {0}")]
    SessionError(String),

    /// TLS certificate generation or loading failed.
    #[error("TLS error: {0}")]
    TlsError(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    SerializationError(String),
}

/// Serializes a [`DatagramHeader`] to bytes using `postcard`.
///
/// # Errors
///
/// Returns [`TransportError::SerializationError`] if serialization fails.
pub fn serialize_header(header: &DatagramHeader) -> Result<Vec<u8>, TransportError> {
    postcard::to_allocvec(header)
        .map_err(|e| TransportError::SerializationError(format!("header serialize: {e}")))
}

/// Deserializes a [`DatagramHeader`] from bytes, returning the header
/// and the remaining bytes (the payload).
///
/// # Errors
///
/// Returns [`TransportError::SerializationError`] if deserialization fails.
pub fn deserialize_header(buf: &[u8]) -> Result<(DatagramHeader, &[u8]), TransportError> {
    postcard::take_from_bytes(buf)
        .map_err(|e| TransportError::SerializationError(format!("header deserialize: {e}")))
}

/// Serializes a [`ControlMessage`] to a length-prefixed byte buffer.
///
/// Format: `[4 bytes LE: body length][postcard-serialized body]`
///
/// # Errors
///
/// Returns [`TransportError::SerializationError`] if serialization fails.
pub fn serialize_control_message(msg: &ControlMessage) -> Result<Vec<u8>, TransportError> {
    let body = postcard::to_allocvec(msg)
        .map_err(|e| TransportError::SerializationError(format!("control serialize: {e}")))?;
    let len = u32::try_from(body.len())
        .map_err(|_| TransportError::SerializationError("control message too large".to_string()))?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

/// Deserializes a [`ControlMessage`] from a `postcard`-serialized body
/// (without the length prefix — the caller reads the length first).
///
/// # Errors
///
/// Returns [`TransportError::SerializationError`] if deserialization fails.
pub fn deserialize_control_message(body: &[u8]) -> Result<ControlMessage, TransportError> {
    postcard::from_bytes(body)
        .map_err(|e| TransportError::SerializationError(format!("control deserialize: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datagram_header_round_trip() {
        let header = DatagramHeader {
            stream_type: STREAM_TYPE_VIDEO,
            frame_index: 42,
            fragment_index: 3,
            fragment_count: 10,
            pts: 12345,
            is_keyframe: true,
        };
        let bytes = serialize_header(&header).unwrap();
        let (decoded, remainder) = deserialize_header(&bytes).unwrap();
        assert_eq!(decoded, header);
        assert!(remainder.is_empty());
    }

    #[test]
    fn datagram_header_with_payload() {
        let header = DatagramHeader {
            stream_type: STREAM_TYPE_AUDIO,
            frame_index: 0,
            fragment_index: 0,
            fragment_count: 1,
            pts: 0,
            is_keyframe: false,
        };
        let header_bytes = serialize_header(&header).unwrap();
        let payload = b"audio data here";
        let mut datagram = header_bytes.clone();
        datagram.extend_from_slice(payload);
        let (decoded, remainder) = deserialize_header(&datagram).unwrap();
        assert_eq!(decoded, header);
        assert_eq!(remainder, payload);
    }

    #[test]
    fn control_message_session_request_round_trip() {
        let msg = ControlMessage::SessionRequest {
            width: 1920,
            height: 1080,
            framerate: 60,
            codec: Codec::H265,
        };
        let bytes = serialize_control_message(&msg).unwrap();
        // First 4 bytes are length prefix.
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4);
        let decoded = deserialize_control_message(&bytes[4..]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_session_response_round_trip() {
        let msg = ControlMessage::SessionResponse {
            width: 2560,
            height: 1440,
            framerate: 120,
            bitrate_mbps: 50,
            codec: Codec::Av1,
            max_datagram_size: 1200,
            cursor_embedded: true,
        };
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_idr_request_round_trip() {
        let msg = ControlMessage::IdrRequest;
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_session_response_cursor_hidden_round_trip() {
        let msg = ControlMessage::SessionResponse {
            width: 1920,
            height: 1080,
            framerate: 60,
            bitrate_mbps: 20,
            codec: Codec::H265,
            max_datagram_size: 1200,
            cursor_embedded: false,
        };
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_ping_pong_round_trip() {
        for msg in [
            ControlMessage::Ping {
                timestamp_ms: 1_000_000,
            },
            ControlMessage::Pong {
                timestamp_ms: 1_000_000,
            },
        ] {
            let bytes = serialize_control_message(&msg).unwrap();
            let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn transport_error_display() {
        let err = TransportError::ConnectionError("timeout".to_string());
        assert_eq!(err.to_string(), "connection error: timeout");

        let err = TransportError::TlsError("bad cert".to_string());
        assert_eq!(err.to_string(), "TLS error: bad cert");

        let err = TransportError::SerializationError("eof".to_string());
        assert_eq!(err.to_string(), "serialization error: eof");
    }

    #[test]
    fn reassembled_frame_construction() {
        let frame = ReassembledFrame {
            data: vec![1, 2, 3],
            pts: 100,
            is_keyframe: false,
            stream_type: STREAM_TYPE_VIDEO,
        };
        assert_eq!(frame.data.len(), 3);
        assert_eq!(frame.pts, 100);
        assert!(!frame.is_keyframe);
        assert_eq!(frame.stream_type, STREAM_TYPE_VIDEO);
    }

    #[test]
    fn stream_type_constants() {
        assert_eq!(STREAM_TYPE_VIDEO, 0);
        assert_eq!(STREAM_TYPE_AUDIO, 1);
        assert_ne!(STREAM_TYPE_VIDEO, STREAM_TYPE_AUDIO);
    }

    #[test]
    fn control_message_input_keyboard_round_trip() {
        use crate::input::InputEvent;
        let msg = ControlMessage::Input(InputEvent::Keyboard {
            scancode: 4,
            pressed: true,
        });
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_input_mouse_move_round_trip() {
        use crate::input::InputEvent;
        let msg = ControlMessage::Input(InputEvent::MouseMove { dx: -10, dy: 5 });
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_input_mouse_button_round_trip() {
        use crate::input::{InputEvent, MouseButton};
        let msg = ControlMessage::Input(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        });
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_input_mouse_wheel_round_trip() {
        use crate::input::InputEvent;
        let msg = ControlMessage::Input(InputEvent::MouseWheel { dx: 0, dy: 3 });
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_input_gamepad_axis_round_trip() {
        use crate::input::{GamepadAxis, InputEvent};
        let msg = ControlMessage::Input(InputEvent::GamepadAxis {
            axis: GamepadAxis::LeftX,
            value: -16000,
        });
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn control_message_input_gamepad_button_round_trip() {
        use crate::input::{GamepadButton, InputEvent};
        let msg = ControlMessage::Input(InputEvent::GamepadButton {
            button: GamepadButton::South,
            pressed: true,
        });
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }
}
