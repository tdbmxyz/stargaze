//! Datagram reassembly and control message handling for the client.
//!
//! Contains the [`FrameAssembler`] which collects datagram fragments
//! into complete frames, and the handshake/receive logic.

use std::collections::HashMap;
use std::time::Instant;

use stargaze_core::transport::{
    ControlMessage, DatagramHeader, IDR_RATE_LIMIT_MS, MAX_PENDING_FRAMES, ReassembledFrame,
    TransportError, deserialize_control_message, deserialize_header, serialize_control_message,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::SessionRequest;

/// Session parameters confirmed by the server.
#[derive(Debug, Clone)]
pub(crate) struct SessionParams {
    /// Confirmed video width in pixels.
    pub width: u32,
    /// Confirmed video height in pixels.
    pub height: u32,
    /// Confirmed framerate.
    pub framerate: u32,
    /// Bitrate in Mbps.
    pub bitrate_mbps: u32,
    /// Maximum datagram payload size for the connection.
    pub max_datagram_size: u16,
}

/// Performs the session handshake with the server.
///
/// Sends `SessionRequest` and reads `SessionResponse`.
///
/// # Errors
///
/// Returns `TransportError::SessionError` if the handshake fails.
pub(crate) async fn perform_handshake(
    request: &SessionRequest,
    send_stream: &mut quinn::SendStream,
    recv_stream: &mut quinn::RecvStream,
) -> Result<SessionParams, TransportError> {
    // Send session request.
    let req_msg = ControlMessage::SessionRequest {
        width: request.width,
        height: request.height,
        framerate: request.framerate,
        codec: request.codec,
    };
    let req_bytes = serialize_control_message(&req_msg)?;
    send_stream
        .write_all(&req_bytes)
        .await
        .map_err(|e| TransportError::SessionError(format!("send request: {e}")))?;

    // Read session response.
    let mut len_buf = [0u8; 4];
    recv_stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::SessionError(format!("read response length: {e}")))?;
    let msg_len = u32::from_le_bytes(len_buf) as usize;

    if msg_len > 65536 {
        return Err(TransportError::SessionError(
            "session response too large".to_string(),
        ));
    }

    let mut body = vec![0u8; msg_len];
    recv_stream
        .read_exact(&mut body)
        .await
        .map_err(|e| TransportError::SessionError(format!("read response body: {e}")))?;

    let response = deserialize_control_message(&body)?;

    match response {
        ControlMessage::SessionResponse {
            width,
            height,
            framerate,
            bitrate_mbps,
            codec: _,
            max_datagram_size,
        } => Ok(SessionParams {
            width,
            height,
            framerate,
            bitrate_mbps,
            max_datagram_size,
        }),
        other => Err(TransportError::SessionError(format!(
            "expected SessionResponse, got {other:?}"
        ))),
    }
}

/// A pending frame being assembled from fragments.
struct PendingFrame {
    /// Fragment slots (`None` = not yet received).
    fragments: Vec<Option<Vec<u8>>>,
    /// Number of fragments received so far.
    received_count: u16,
    /// Total fragments expected.
    fragment_count: u16,
    /// Presentation timestamp.
    pts: u64,
    /// Whether this is a keyframe.
    is_keyframe: bool,
    /// Stream type.
    stream_type: u8,
}

/// Assembles datagram fragments into complete frames.
pub struct FrameAssembler {
    /// In-progress frames, keyed by `frame_index`.
    pending: HashMap<u32, PendingFrame>,
    /// Next `frame_index` expected for in-order delivery.
    next_frame: u32,
    /// Maximum number of pending incomplete frames before triggering `IDR`.
    max_pending: usize,
    /// Last time an `IDR` request was sent.
    last_idr_request: Option<Instant>,
}

impl FrameAssembler {
    /// Creates a new `FrameAssembler`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_frame: 0,
            max_pending: MAX_PENDING_FRAMES,
            last_idr_request: None,
        }
    }

    /// Processes an incoming datagram fragment.
    ///
    /// Returns a list of completed frames (may be empty or contain
    /// multiple frames if out-of-order fragments completed several frames).
    /// Also returns `true` in the second element if an `IDR` should be requested.
    pub fn process_datagram(
        &mut self,
        header: &DatagramHeader,
        payload: Vec<u8>,
    ) -> (Vec<ReassembledFrame>, bool) {
        let mut completed = Vec::new();
        let mut need_idr = false;

        // Insert fragment.
        let pending = self
            .pending
            .entry(header.frame_index)
            .or_insert_with(|| PendingFrame {
                fragments: vec![None; usize::from(header.fragment_count)],
                received_count: 0,
                fragment_count: header.fragment_count,
                pts: header.pts,
                is_keyframe: header.is_keyframe,
                stream_type: header.stream_type,
            });

        let idx = usize::from(header.fragment_index);
        if idx < pending.fragments.len() && pending.fragments[idx].is_none() {
            pending.fragments[idx] = Some(payload);
            pending.received_count += 1;
        }

        // Check if this frame is now complete.
        if pending.received_count == pending.fragment_count
            && let Some(frame) = self.assemble_frame(header.frame_index)
        {
            completed.push(frame);
        }

        // Deliver any consecutive completed frames starting from next_frame.
        self.deliver_in_order(&mut completed);

        // Check if we need an IDR (too many pending frames = likely loss).
        if self.pending.len() > self.max_pending {
            need_idr = self.should_request_idr();
            if need_idr {
                self.pending.clear();
            }
        }

        (completed, need_idr)
    }

    /// Assembles a complete frame from its fragments and removes it from pending.
    fn assemble_frame(&mut self, frame_index: u32) -> Option<ReassembledFrame> {
        let pending = self.pending.remove(&frame_index)?;

        let mut data = Vec::new();
        for bytes in pending.fragments.into_iter().flatten() {
            data.extend_from_slice(&bytes);
        }

        Some(ReassembledFrame {
            data,
            pts: pending.pts,
            is_keyframe: pending.is_keyframe,
            stream_type: pending.stream_type,
        })
    }

    /// Delivers frames in order starting from `next_frame`.
    fn deliver_in_order(&mut self, completed: &mut Vec<ReassembledFrame>) {
        loop {
            if self.pending.contains_key(&self.next_frame) {
                let pending = &self.pending[&self.next_frame];
                if pending.received_count == pending.fragment_count {
                    if let Some(frame) = self.assemble_frame(self.next_frame) {
                        completed.push(frame);
                    }
                    self.next_frame = self.next_frame.wrapping_add(1);
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// Checks if we should send an `IDR` request based on rate limiting.
    pub fn should_request_idr(&mut self) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_idr_request
            && now.duration_since(last).as_millis() < u128::from(IDR_RATE_LIMIT_MS)
        {
            return false;
        }
        self.last_idr_request = Some(now);
        true
    }
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

/// Main receive loop: reads datagrams from the connection and
/// assembles them into frames.
///
/// # Errors
///
/// Returns `TransportError` on fatal errors.
pub(crate) async fn receive_loop(
    connection: quinn::Connection,
    mut control_send: quinn::SendStream,
    frames_tx: mpsc::Sender<ReassembledFrame>,
) -> Result<(), TransportError> {
    let mut assembler = FrameAssembler::new();
    let mut total_frames: u64 = 0;

    loop {
        let datagram = match connection.read_datagram().await {
            Ok(bytes) => bytes,
            Err(quinn::ConnectionError::ApplicationClosed(_)) => {
                info!("Server closed connection");
                return Ok(());
            }
            Err(quinn::ConnectionError::LocallyClosed) => {
                info!("Connection closed locally");
                return Ok(());
            }
            Err(e) => {
                return Err(TransportError::ConnectionError(format!(
                    "read datagram: {e}"
                )));
            }
        };

        let (header, payload) = match deserialize_header(&datagram) {
            Ok(result) => result,
            Err(e) => {
                warn!("Failed to deserialize datagram header: {e}");
                continue;
            }
        };

        let (completed_frames, need_idr) = assembler.process_datagram(&header, payload.to_vec());

        for frame in completed_frames {
            total_frames += 1;
            if frame.is_keyframe || total_frames % 300 == 1 {
                info!(
                    frame = total_frames,
                    pts = frame.pts,
                    size = frame.data.len(),
                    keyframe = frame.is_keyframe,
                    "Reassembled frame"
                );
            }
            if frames_tx.send(frame).await.is_err() {
                info!("Frame receiver dropped, stopping transport");
                return Ok(());
            }
        }

        if need_idr {
            debug!("Requesting IDR keyframe");
            let idr_msg = serialize_control_message(&ControlMessage::IdrRequest)?;
            if let Err(e) = control_send.write_all(&idr_msg).await {
                warn!("Failed to send IDR request: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stargaze_core::transport::STREAM_TYPE_VIDEO;

    /// Helper: creates a `DatagramHeader` for testing.
    fn make_header(
        frame_index: u32,
        fragment_index: u16,
        fragment_count: u16,
        pts: u64,
        is_keyframe: bool,
    ) -> DatagramHeader {
        DatagramHeader {
            stream_type: STREAM_TYPE_VIDEO,
            frame_index,
            fragment_index,
            fragment_count,
            pts,
            is_keyframe,
        }
    }

    #[test]
    fn single_fragment_frame() {
        let mut assembler = FrameAssembler::new();
        let header = make_header(0, 0, 1, 100, true);
        let payload = vec![1, 2, 3, 4, 5];

        let (frames, need_idr) = assembler.process_datagram(&header, payload.clone());

        assert!(!need_idr);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, payload);
        assert_eq!(frames[0].pts, 100);
        assert!(frames[0].is_keyframe);
        assert_eq!(frames[0].stream_type, STREAM_TYPE_VIDEO);
    }

    #[test]
    fn multi_fragment_in_order() {
        let mut assembler = FrameAssembler::new();

        // 3 fragments for frame 0.
        let h0 = make_header(0, 0, 3, 0, false);
        let h1 = make_header(0, 1, 3, 0, false);
        let h2 = make_header(0, 2, 3, 0, false);

        let (frames, _) = assembler.process_datagram(&h0, vec![1, 2]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h1, vec![3, 4]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h2, vec![5, 6]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn multi_fragment_out_of_order() {
        let mut assembler = FrameAssembler::new();

        // Send fragments in reverse order.
        let h2 = make_header(0, 2, 3, 42, true);
        let h0 = make_header(0, 0, 3, 42, true);
        let h1 = make_header(0, 1, 3, 42, true);

        let (frames, _) = assembler.process_datagram(&h2, vec![5, 6]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h0, vec![1, 2]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h1, vec![3, 4]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(frames[0].pts, 42);
        assert!(frames[0].is_keyframe);
    }

    #[test]
    fn duplicate_fragment_ignored() {
        let mut assembler = FrameAssembler::new();

        let h0 = make_header(0, 0, 2, 0, false);
        let h1 = make_header(0, 1, 2, 0, false);

        // Send fragment 0 twice.
        let (frames, _) = assembler.process_datagram(&h0, vec![1, 2]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h0, vec![99, 99]);
        assert!(frames.is_empty()); // Duplicate ignored.

        let (frames, _) = assembler.process_datagram(&h1, vec![3, 4]);
        assert_eq!(frames.len(), 1);
        // Original data preserved, not the duplicate.
        assert_eq!(frames[0].data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn max_pending_triggers_idr() {
        let mut assembler = FrameAssembler::new();

        // Fill up max_pending + 1 incomplete frames.
        for i in 0..=MAX_PENDING_FRAMES as u32 {
            let h = make_header(i, 0, 2, u64::from(i), false);
            let (_, _need_idr) = assembler.process_datagram(&h, vec![0]);
        }

        // After exceeding max_pending, the assembler should request IDR
        // and clear pending frames.
        assert!(assembler.pending.is_empty() || assembler.pending.len() <= MAX_PENDING_FRAMES);
    }

    #[test]
    fn idr_rate_limiting() {
        let mut assembler = FrameAssembler::new();

        // First IDR request should succeed.
        assert!(assembler.should_request_idr());

        // Immediate second request should be rate-limited.
        assert!(!assembler.should_request_idr());
    }

    #[test]
    fn multiple_frames_sequential() {
        let mut assembler = FrameAssembler::new();

        // Frame 0: single fragment.
        let h0 = make_header(0, 0, 1, 0, true);
        let (frames, _) = assembler.process_datagram(&h0, vec![10]);
        assert_eq!(frames.len(), 1);

        // Frame 1: single fragment.
        let h1 = make_header(1, 0, 1, 1, false);
        let (frames, _) = assembler.process_datagram(&h1, vec![20]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pts, 1);
    }
}
