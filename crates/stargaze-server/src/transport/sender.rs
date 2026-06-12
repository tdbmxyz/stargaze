//! Frame fragmentation and datagram sending for the server.
//!
//! Handles fragmenting [`EncodedPacket`] values into `QUIC` datagrams
//! and processing incoming control messages.

use stargaze_core::config::ServerConfig;
use stargaze_core::encode::EncodedPacket;
use stargaze_core::input::InputEvent;
use stargaze_core::transport::{
    ControlMessage, DatagramHeader, HEADER_SIZE_UPPER_BOUND, TransportError,
    deserialize_control_message, serialize_control_message, serialize_header,
};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Performs the session handshake with the client.
///
/// Reads `SessionRequest` from the control stream, validates it,
/// and sends back `SessionResponse`.
///
/// Returns (width, height, framerate, `bitrate_mbps`) of the confirmed session.
///
/// # Errors
///
/// Returns [`TransportError::SessionError`] if the handshake fails.
pub(crate) async fn handle_session_handshake(
    config: &ServerConfig,
    connection: &quinn::Connection,
    send_stream: &mut quinn::SendStream,
    recv_stream: &mut quinn::RecvStream,
) -> Result<(u32, u32, u32, u32), TransportError> {
    // Read length prefix.
    let mut len_buf = [0u8; 4];
    recv_stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::SessionError(format!("read request length: {e}")))?;
    let msg_len = u32::from_le_bytes(len_buf) as usize;

    if msg_len > 65536 {
        return Err(TransportError::SessionError(
            "session request too large".to_string(),
        ));
    }

    // Read message body.
    let mut body = vec![0u8; msg_len];
    recv_stream
        .read_exact(&mut body)
        .await
        .map_err(|e| TransportError::SessionError(format!("read request body: {e}")))?;

    let request = deserialize_control_message(&body)?;

    let (width, height, framerate, codec) = match request {
        ControlMessage::SessionRequest {
            width,
            height,
            framerate,
            codec,
        } => (width, height, framerate, codec),
        other => {
            return Err(TransportError::SessionError(format!(
                "expected SessionRequest, got {other:?}"
            )));
        }
    };

    info!(
        "Session request: {}x{} @ {}fps, {:?}",
        width, height, framerate, codec
    );

    // For MVP, use server's configured parameters.
    let max_datagram_size = connection.max_datagram_size().unwrap_or(0);
    let max_datagram_size_u16 = u16::try_from(max_datagram_size).unwrap_or(u16::MAX);

    info!(max_datagram_size, "Session handshake: datagram size limit");

    let response = ControlMessage::SessionResponse {
        width: config.resolution.width,
        height: config.resolution.height,
        framerate: config.framerate,
        bitrate_mbps: config.bitrate,
        codec: config.codec,
        max_datagram_size: max_datagram_size_u16,
        cursor_embedded: config.cursor.show_cursor,
        server_command: stargaze_core::config::sanitized_command_line(),
    };

    let response_bytes = serialize_control_message(&response)?;
    send_stream
        .write_all(&response_bytes)
        .await
        .map_err(|e| TransportError::SessionError(format!("send response: {e}")))?;

    Ok((
        config.resolution.width,
        config.resolution.height,
        config.framerate,
        config.bitrate,
    ))
}

/// Listens for control messages from the client (IDR requests, pings).
///
/// Runs until the stream is closed or an error occurs.
///
/// # Errors
///
/// Returns [`TransportError::ControlError`] on stream errors.
pub(crate) async fn handle_control_messages(
    recv_stream: &mut quinn::RecvStream,
    idr_tx: &watch::Sender<u64>,
    input_tx: &mpsc::Sender<InputEvent>,
) -> Result<(), TransportError> {
    loop {
        // Read length prefix.
        let mut len_buf = [0u8; 4];
        match recv_stream.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(_))) => {
                info!("Control stream: connection closed");
                return Ok(());
            }
            Err(quinn::ReadExactError::FinishedEarly(_)) => {
                info!("Control stream: client closed stream");
                return Ok(());
            }
            Err(e) => {
                return Err(TransportError::ControlError(format!("read length: {e}")));
            }
        }

        let msg_len = u32::from_le_bytes(len_buf) as usize;
        if msg_len > 65536 {
            return Err(TransportError::ControlError(
                "control message too large".to_string(),
            ));
        }

        let mut body = vec![0u8; msg_len];
        recv_stream
            .read_exact(&mut body)
            .await
            .map_err(|e| TransportError::ControlError(format!("read body: {e}")))?;

        let msg = deserialize_control_message(&body)?;

        match msg {
            ControlMessage::IdrRequest => {
                debug!("Received IDR request from client");
                idr_tx.send_modify(|v| *v += 1);
            }
            ControlMessage::Ping { timestamp_ms } => {
                debug!(timestamp_ms, "Received ping (pong not yet implemented)");
            }
            ControlMessage::Input(event) => {
                debug!("Received input event from client");
                if input_tx.try_send(event).is_err() {
                    debug!("Input channel full or closed, dropping event");
                }
            }
            other => {
                warn!("Unexpected control message: {other:?}");
            }
        }
    }
}

/// Sends encoded packets as fragmented `QUIC` datagrams.
///
/// Runs until the packet channel closes.
///
/// # Errors
///
/// Returns [`TransportError::SendError`] on datagram send failures.
pub(crate) async fn send_packets(
    connection: &quinn::Connection,
    packets: &mut mpsc::Receiver<EncodedPacket>,
    stream_type: u8,
) -> Result<(), TransportError> {
    use bytes::Bytes;

    let mut frame_index: u32 = 0;
    let mut logged_datagram_size = false;

    while let Some(pkt) = packets.recv().await {
        let Some(max_datagram_size) = connection.max_datagram_size() else {
            warn!(
                frame = frame_index,
                "Datagrams not supported by peer, skipping frame"
            );
            frame_index = frame_index.wrapping_add(1);
            continue;
        };

        if !logged_datagram_size {
            info!(max_datagram_size, "QUIC datagram size limit");
            logged_datagram_size = true;
        }

        let max_payload = max_datagram_size.saturating_sub(HEADER_SIZE_UPPER_BOUND);
        if max_payload == 0 {
            warn!(
                max_datagram_size,
                "Max datagram size too small for header, skipping frame"
            );
            frame_index = frame_index.wrapping_add(1);
            continue;
        }

        let fragment_count = pkt.data.len().div_ceil(max_payload);
        let fragment_count_u16 = u16::try_from(fragment_count).unwrap_or(u16::MAX);

        for i in 0..fragment_count {
            let start = i * max_payload;
            let end = ((i + 1) * max_payload).min(pkt.data.len());
            let payload = &pkt.data[start..end];

            let header = DatagramHeader {
                stream_type,
                frame_index,
                fragment_index: u16::try_from(i).unwrap_or(u16::MAX),
                fragment_count: fragment_count_u16,
                pts: pkt.pts,
                is_keyframe: pkt.is_keyframe,
                capture_us: pkt.capture_us,
                convert_us: pkt.convert_us,
                encode_us: pkt.encode_us,
            };

            let header_bytes = serialize_header(&header)
                .map_err(|e| TransportError::SendError(format!("serialize: {e}")))?;

            let mut datagram = Vec::with_capacity(header_bytes.len() + payload.len());
            datagram.extend_from_slice(&header_bytes);
            datagram.extend_from_slice(payload);

            debug_assert!(
                datagram.len() <= max_datagram_size,
                "datagram {} > max {}",
                datagram.len(),
                max_datagram_size
            );

            match connection.send_datagram(Bytes::from(datagram)) {
                Ok(()) => {}
                // The client went away — end this session so the server
                // can accept a new connection.
                Err(quinn::SendDatagramError::ConnectionLost(e)) => {
                    info!("Connection lost, sender exiting: {e}");
                    return Ok(());
                }
                Err(e) => {
                    debug!(
                        frame = frame_index,
                        fragment = i,
                        "Datagram send failed: {e}"
                    );
                }
            }
        }

        frame_index = frame_index.wrapping_add(1);
    }

    info!("Packet channel closed, transport sender exiting");
    Ok(())
}
