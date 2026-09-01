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

/// Minimum interval in milliseconds before an *unanswered* IDR request is
/// re-issued (no keyframe delivered yet). Deliberately longer than
/// [`IDR_RATE_LIMIT_MS`]: a keyframe is the largest frame on the wire and
/// can take well over 100 ms to encode and transmit on slow links, and
/// re-requesting while one is still in flight would produce redundant
/// keyframes that further congest the link.
pub const IDR_RETRY_MS: u64 = 500;

/// Conservative header size upper bound (bytes) for [`DatagramHeader`].
///
/// Postcard uses varint encoding, so the actual serialized size depends
/// on field values.  This constant is safe for any field combination and
/// avoids the need to serialize a sample header just to measure its
/// length.
pub const HEADER_SIZE_UPPER_BOUND: usize = 44;

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
    /// Host-side capture→encode latency in microseconds (0 = unknown).
    pub capture_us: u32,
    /// Host-side frame preparation (convert + upload) in microseconds.
    pub convert_us: u32,
    /// Host-side encode duration in microseconds (0 = unknown).
    pub encode_us: u32,
}

/// Messages exchanged over the reliable control stream.
///
/// Length-prefixed with 4-byte LE length before the `postcard`-serialized body.
/// New variants may be appended without breaking backward compatibility.
/// Trailing fields may be appended to an existing variant with care:
/// `postcard::from_bytes` ignores unread trailing bytes, so OLD peers
/// silently drop the new field — but NEW peers reading OLD bytes hit
/// EOF and need an explicit fallback (see
/// [`deserialize_session_request_compat`]).
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
        /// Requested bitrate in Mbps; 0 = use the server's configured
        /// bitrate. Appended in v1.3.0 — pre-1.3.0 servers ignore it.
        bitrate_mbps: u32,
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
        /// Server command line, sanitized of addresses and ports
        /// (for the client's session diagnostics).
        server_command: String,
        /// Number of audio channels the server encodes (1, 2, 6, or 8).
        /// Appended for surround support — pre-surround servers omit it and
        /// [`deserialize_session_response_compat`] defaults it to 2 (stereo).
        audio_channels: u16,
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

/// First bytes of a `QUIC` bidirectional stream carrying a tunneled USB
/// device (USB/IP over the session connection), followed by a
/// length-prefixed [`UsbTunnelHeader`].
pub const USB_STREAM_MAGIC: [u8; 4] = *b"SGUB";

/// Metadata for one tunneled USB device, sent by the client as the
/// first frame of a dedicated `QUIC` bidirectional stream (after
/// [`USB_STREAM_MAGIC`]). After this header, the stream carries the raw
/// kernel-to-kernel USB/IP byte flow (`usbip-host` stub on the client,
/// `vhci-hcd` on the server).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbTunnelHeader {
    /// USB vendor id of the exported device.
    pub vendor: u16,
    /// USB product id of the exported device.
    pub product: u16,
    /// USB/IP device id: `busnum << 16 | devnum` on the client.
    pub devid: u32,
    /// Kernel `usb_device_speed` value (2 = full, 3 = high, 5 = super).
    pub speed: u32,
    /// Human-readable device name, for logs only.
    pub name: String,
}

/// Serializes a USB tunnel stream preamble: magic, `u16` length, then
/// the postcard-encoded header.
///
/// # Errors
///
/// Returns [`TransportError::SerializationError`] if encoding fails or
/// the header exceeds a `u16` length.
pub fn serialize_usb_tunnel_header(header: &UsbTunnelHeader) -> Result<Vec<u8>, TransportError> {
    let body = postcard::to_allocvec(header)
        .map_err(|e| TransportError::SerializationError(e.to_string()))?;
    let len = u16::try_from(body.len())
        .map_err(|_| TransportError::SerializationError("usb header too large".to_string()))?;
    let mut bytes = Vec::with_capacity(USB_STREAM_MAGIC.len() + 2 + body.len());
    bytes.extend_from_slice(&USB_STREAM_MAGIC);
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

/// Deserializes the postcard body of a USB tunnel header (the bytes
/// after the magic and the `u16` length prefix).
///
/// # Errors
///
/// Returns [`TransportError::SerializationError`] on malformed input.
pub fn deserialize_usb_tunnel_header(body: &[u8]) -> Result<UsbTunnelHeader, TransportError> {
    postcard::from_bytes(body).map_err(|e| TransportError::SerializationError(e.to_string()))
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
    /// Host-side capture→encode latency in microseconds (0 = unknown).
    pub capture_us: u32,
    /// Host-side frame preparation (convert + upload) in microseconds.
    pub convert_us: u32,
    /// Host-side encode duration in microseconds (0 = unknown).
    pub encode_us: u32,
    /// When the frame finished reassembling on the client.
    pub received_at: std::time::Instant,
    /// True when this frame was delivered while video continuity was
    /// broken (a frame before it was lost and no keyframe has arrived
    /// yet). Decoding it would predict from missing references — ffmpeg
    /// fabricates gray placeholder frames — so the client drops tainted
    /// deltas and freezes on the last good picture instead.
    pub tainted: bool,
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

    /// An encode pipeline packet channel closed: the pipeline is dead
    /// and no session can be served until the process restarts.
    #[error("pipeline closed: {0}")]
    PipelineClosed(String),
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

/// The `SessionRequest` shape shipped before v1.3.0 (no bitrate).
/// Postcard enum tags are variant indices, so this parses the same
/// leading tag as [`ControlMessage::SessionRequest`].
#[derive(Deserialize)]
enum LegacyControlMessageV1 {
    SessionRequest {
        width: u32,
        height: u32,
        framerate: u32,
        codec: Codec,
    },
}

/// The `SessionResponse` shape shipped before surround audio (no
/// `audio_channels`). Variant order mirrors [`ControlMessage`] so the postcard
/// tag for `SessionResponse` (index 1) decodes into this variant; the leading
/// `SessionRequest` variant only exists to preserve that index.
#[derive(Serialize, Deserialize)]
enum LegacyControlMessageV2 {
    #[allow(dead_code)]
    SessionRequest {
        width: u32,
        height: u32,
        framerate: u32,
        codec: Codec,
        bitrate_mbps: u32,
    },
    SessionResponse {
        width: u32,
        height: u32,
        framerate: u32,
        bitrate_mbps: u32,
        codec: Codec,
        max_datagram_size: u16,
        cursor_embedded: bool,
        server_command: String,
    },
}

/// Deserializes a `SessionResponse`, accepting both the current shape and the
/// pre-surround one (which lacks `audio_channels`; it comes back as 2 = stereo,
/// the only layout those servers produce).
///
/// # Errors
///
/// Returns [`TransportError::SerializationError`] if the body is not a session
/// response in either shape.
pub fn deserialize_session_response_compat(body: &[u8]) -> Result<ControlMessage, TransportError> {
    match deserialize_control_message(body) {
        Ok(msg) => Ok(msg),
        Err(modern_err) => match postcard::from_bytes::<LegacyControlMessageV2>(body) {
            Ok(LegacyControlMessageV2::SessionResponse {
                width,
                height,
                framerate,
                bitrate_mbps,
                codec,
                max_datagram_size,
                cursor_embedded,
                server_command,
            }) => Ok(ControlMessage::SessionResponse {
                width,
                height,
                framerate,
                bitrate_mbps,
                codec,
                max_datagram_size,
                cursor_embedded,
                server_command,
                audio_channels: 2,
            }),
            _ => Err(modern_err),
        },
    }
}

/// Deserializes a `SessionRequest`, accepting both the current shape
/// and the pre-v1.3.0 one (which lacks `bitrate_mbps`; it comes back
/// as 0 = "server default").
///
/// # Errors
///
/// Returns [`TransportError::SerializationError`] if the body is not a
/// session request in either shape.
pub fn deserialize_session_request_compat(body: &[u8]) -> Result<ControlMessage, TransportError> {
    match deserialize_control_message(body) {
        Ok(msg) => Ok(msg),
        Err(modern_err) => match postcard::from_bytes::<LegacyControlMessageV1>(body) {
            Ok(LegacyControlMessageV1::SessionRequest {
                width,
                height,
                framerate,
                codec,
            }) => Ok(ControlMessage::SessionRequest {
                width,
                height,
                framerate,
                codec,
                bitrate_mbps: 0,
            }),
            Err(_) => Err(modern_err),
        },
    }
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
            capture_us: 2_100,
            convert_us: 1_500,
            encode_us: 3_400,
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
            capture_us: 0,
            convert_us: 0,
            encode_us: 0,
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
            bitrate_mbps: 25,
        };
        let bytes = serialize_control_message(&msg).unwrap();
        // First 4 bytes are length prefix.
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 4);
        let decoded = deserialize_control_message(&bytes[4..]).unwrap();
        assert_eq!(decoded, msg);
    }

    /// Old client → new server: a pre-v1.3.0 request (no bitrate field)
    /// must parse via the compat path with bitrate 0.
    #[test]
    fn session_request_compat_accepts_legacy_shape() {
        #[derive(Serialize)]
        enum OldControlMessage {
            SessionRequest {
                width: u32,
                height: u32,
                framerate: u32,
                codec: Codec,
            },
        }
        let old_bytes = postcard::to_allocvec(&OldControlMessage::SessionRequest {
            width: 1280,
            height: 800,
            framerate: 90,
            codec: Codec::H265,
        })
        .unwrap();

        let decoded = deserialize_session_request_compat(&old_bytes).unwrap();
        assert_eq!(
            decoded,
            ControlMessage::SessionRequest {
                width: 1280,
                height: 800,
                framerate: 90,
                codec: Codec::H265,
                bitrate_mbps: 0,
            }
        );
    }

    /// New client → old server: postcard ignores unread trailing bytes,
    /// so an old server parsing today's request with the legacy shape
    /// just drops the bitrate field. This test IS the old server.
    #[test]
    fn legacy_parser_tolerates_new_request_bytes() {
        let msg = ControlMessage::SessionRequest {
            width: 1920,
            height: 1080,
            framerate: 60,
            codec: Codec::Av1,
            bitrate_mbps: 42,
        };
        let body = postcard::to_allocvec(&msg).unwrap();
        let LegacyControlMessageV1::SessionRequest {
            width,
            height,
            framerate,
            codec,
        } = postcard::from_bytes::<LegacyControlMessageV1>(&body)
            .expect("old servers must still parse new requests");
        assert_eq!(
            (width, height, framerate, codec),
            (1920, 1080, 60, Codec::Av1)
        );
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
            server_command: "stargaze-server --bitrate 50".to_string(),
            audio_channels: 6,
        };
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn session_response_compat_reads_pre_surround_shape() {
        // A pre-surround server serializes a SessionResponse without the
        // trailing audio_channels field; a new client must default it to 2.
        let legacy = LegacyControlMessageV2::SessionResponse {
            width: 1920,
            height: 1080,
            framerate: 60,
            bitrate_mbps: 20,
            codec: Codec::H265,
            max_datagram_size: 1200,
            cursor_embedded: true,
            server_command: "stargaze-server".to_string(),
        };
        let body = postcard::to_allocvec(&legacy).unwrap();
        let decoded = deserialize_session_response_compat(&body).unwrap();
        assert_eq!(
            decoded,
            ControlMessage::SessionResponse {
                width: 1920,
                height: 1080,
                framerate: 60,
                bitrate_mbps: 20,
                codec: Codec::H265,
                max_datagram_size: 1200,
                cursor_embedded: true,
                server_command: "stargaze-server".to_string(),
                audio_channels: 2,
            }
        );
    }

    #[test]
    fn session_response_compat_reads_current_shape() {
        let msg = ControlMessage::SessionResponse {
            width: 3440,
            height: 1440,
            framerate: 100,
            bitrate_mbps: 30,
            codec: Codec::H265,
            max_datagram_size: 1200,
            cursor_embedded: false,
            server_command: String::new(),
            audio_channels: 8,
        };
        let body = postcard::to_allocvec(&msg).unwrap();
        assert_eq!(deserialize_session_response_compat(&body).unwrap(), msg);
    }

    /// New server → old client: postcard ignores the appended channel count,
    /// leaving the pre-surround response fields unchanged.
    #[test]
    fn legacy_parser_tolerates_new_response_bytes() {
        let msg = ControlMessage::SessionResponse {
            width: 2560,
            height: 1440,
            framerate: 120,
            bitrate_mbps: 50,
            codec: Codec::H265,
            max_datagram_size: 1200,
            cursor_embedded: true,
            server_command: "stargaze-server --bitrate 50".to_string(),
            audio_channels: 2,
        };
        let body = postcard::to_allocvec(&msg).unwrap();
        let LegacyControlMessageV2::SessionResponse {
            width,
            height,
            framerate,
            bitrate_mbps,
            codec,
            max_datagram_size,
            cursor_embedded,
            server_command,
        } = postcard::from_bytes::<LegacyControlMessageV2>(&body)
            .expect("old clients must still parse new responses")
        else {
            panic!("expected legacy session response");
        };
        assert_eq!(
            (
                width,
                height,
                framerate,
                bitrate_mbps,
                codec,
                max_datagram_size,
                cursor_embedded,
                server_command.as_str(),
            ),
            (
                2560,
                1440,
                120,
                50,
                Codec::H265,
                1200,
                true,
                "stargaze-server --bitrate 50",
            )
        );
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
            server_command: String::new(),
            audio_channels: 2,
        };
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn usb_tunnel_header_round_trip() {
        let header = UsbTunnelHeader {
            vendor: 0x28de,
            product: 0x1142,
            devid: (3 << 16) | 7,
            speed: 2,
            name: "Valve Software Steam Controller".to_string(),
        };
        let bytes = serialize_usb_tunnel_header(&header).unwrap();
        assert_eq!(bytes[..4], USB_STREAM_MAGIC);
        let len = u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as usize;
        assert_eq!(len, bytes.len() - 6);
        let decoded = deserialize_usb_tunnel_header(&bytes[6..]).unwrap();
        assert_eq!(decoded, header);
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
            capture_us: 0,
            convert_us: 0,
            encode_us: 0,
            received_at: std::time::Instant::now(),
            tainted: false,
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
            pad: 0,
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
            pad: 1,
            button: GamepadButton::South,
            pressed: true,
        });
        let bytes = serialize_control_message(&msg).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let decoded = deserialize_control_message(&bytes[4..4 + len]).unwrap();
        assert_eq!(decoded, msg);
    }
}
