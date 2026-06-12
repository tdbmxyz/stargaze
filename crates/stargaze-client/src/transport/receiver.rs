//! Datagram reassembly and control message handling for the client.
//!
//! Contains the [`FrameAssembler`] which collects datagram fragments
//! into complete frames, and the handshake/receive logic.

use std::collections::HashMap;
use std::time::Instant;

use stargaze_core::input::InputEvent;
use stargaze_core::transport::{
    ControlMessage, DatagramHeader, IDR_RATE_LIMIT_MS, MAX_PENDING_FRAMES, ReassembledFrame,
    STREAM_TYPE_AUDIO, STREAM_TYPE_VIDEO, TransportError, deserialize_control_message,
    deserialize_header, serialize_control_message,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::SessionRequest;

/// Session parameters confirmed by the server.
#[derive(Debug, Clone)]
pub struct SessionParams {
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
    /// Server command line, sanitized of addresses and ports.
    pub server_command: String,
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
            cursor_embedded: _,
            server_command,
        } => Ok(SessionParams {
            width,
            height,
            framerate,
            bitrate_mbps,
            max_datagram_size,
            server_command,
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
    /// Host-side capture→encode latency in microseconds.
    capture_us: u32,
    /// Host-side frame preparation (convert + upload) in microseconds.
    convert_us: u32,
    /// Host-side encode duration in microseconds.
    encode_us: u32,
}

/// Assembles datagram fragments into complete frames.
///
/// Keyed by `(stream_type, frame_index)` to prevent collisions between
/// audio and video streams that use independent frame counters.
pub struct FrameAssembler {
    /// In-progress frames, keyed by `(stream_type, frame_index)`.
    pending: HashMap<(u8, u32), PendingFrame>,
    /// Next `frame_index` expected per stream type for in-order delivery.
    next_frame: HashMap<u8, u32>,
    /// Maximum number of pending incomplete video frames before triggering `IDR`.
    max_pending: usize,
    /// Last time an `IDR` request was sent.
    last_idr_request: Option<Instant>,
}

impl FrameAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_frame: HashMap::new(),
            max_pending: MAX_PENDING_FRAMES,
            last_idr_request: None,
        }
    }

    /// Processes an incoming datagram fragment.
    ///
    /// Returns a list of completed frames, delivered in `frame_index`
    /// order (may be empty, or contain multiple frames if this fragment
    /// unblocked several already-complete frames).
    /// Also returns `true` in the second element if an `IDR` should be requested.
    pub fn process_datagram(
        &mut self,
        header: &DatagramHeader,
        payload: Vec<u8>,
    ) -> (Vec<ReassembledFrame>, bool) {
        let mut completed = Vec::new();
        let mut need_idr = false;

        // Start in-order tracking from the first frame seen on this stream
        // (the client may join mid-stream).
        let next = *self
            .next_frame
            .entry(header.stream_type)
            .or_insert(header.frame_index);

        // Late fragment for a frame already delivered or skipped — drop it
        // rather than re-creating a pending entry that can never complete.
        if header.frame_index < next {
            return (completed, false);
        }

        let key = (header.stream_type, header.frame_index);

        let pending = self.pending.entry(key).or_insert_with(|| PendingFrame {
            fragments: vec![None; usize::from(header.fragment_count)],
            received_count: 0,
            fragment_count: header.fragment_count,
            pts: header.pts,
            is_keyframe: header.is_keyframe,
            stream_type: header.stream_type,
            capture_us: header.capture_us,
            convert_us: header.convert_us,
            encode_us: header.encode_us,
        });

        let idx = usize::from(header.fragment_index);
        if idx < pending.fragments.len() && pending.fragments[idx].is_none() {
            pending.fragments[idx] = Some(payload);
            pending.received_count += 1;
        }

        let skipped = self.deliver_in_order(header.stream_type, &mut completed);

        // Request IDR if we skipped a gap (lost video frame) or too many
        // incomplete video frames are pending.
        if header.stream_type == STREAM_TYPE_VIDEO && skipped {
            need_idr = self.should_request_idr();
        }
        let video_pending = self
            .pending
            .keys()
            .filter(|(st, _)| *st == STREAM_TYPE_VIDEO)
            .count();
        if video_pending > self.max_pending {
            if !need_idr {
                need_idr = self.should_request_idr();
            }
            if need_idr {
                self.pending.retain(|(st, _), _| *st != STREAM_TYPE_VIDEO);
                self.next_frame.remove(&STREAM_TYPE_VIDEO);
            }
        }

        (completed, need_idr)
    }

    fn assemble_frame(&mut self, key: (u8, u32)) -> Option<ReassembledFrame> {
        let pending = self.pending.remove(&key)?;

        let mut data = Vec::new();
        for bytes in pending.fragments.into_iter().flatten() {
            data.extend_from_slice(&bytes);
        }

        Some(ReassembledFrame {
            data,
            pts: pending.pts,
            is_keyframe: pending.is_keyframe,
            stream_type: pending.stream_type,
            capture_us: pending.capture_us,
            convert_us: pending.convert_us,
            encode_us: pending.encode_us,
            received_at: Instant::now(),
        })
    }

    fn deliver_in_order(&mut self, stream_type: u8, completed: &mut Vec<ReassembledFrame>) -> bool {
        let mut next = *self.next_frame.entry(stream_type).or_insert(0);
        let mut skipped_gap = false;

        loop {
            let key = (stream_type, next);
            let is_complete = self
                .pending
                .get(&key)
                .is_some_and(|pf| pf.received_count == pf.fragment_count);
            if is_complete {
                if let Some(frame) = self.assemble_frame(key) {
                    completed.push(frame);
                }
                next = next.wrapping_add(1);
                continue;
            }

            // The next expected frame is incomplete. If the stream has
            // already moved at least two frames past it (a complete frame
            // with index >= next + 2 exists), treat it as lost and skip it
            // so the pipeline doesn't stall. This triggers an IDR request
            // in the caller. Requiring two frames of progress tolerates
            // simple datagram reordering without dropping frames that are
            // still in flight.
            let lost = self.pending.iter().any(|((st, idx), pf)| {
                *st == stream_type
                    && *idx >= next.saturating_add(2)
                    && pf.received_count == pf.fragment_count
            });
            if lost {
                // Drop the incomplete frame if it exists.
                self.pending.remove(&key);
                next = next.wrapping_add(1);
                skipped_gap = true;
                continue;
            }
            break;
        }
        self.next_frame.insert(stream_type, next);
        skipped_gap
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
/// Sends an `IdrRequest` on the control stream (best effort).
async fn send_idr_request(control_send: &mut quinn::SendStream) -> Result<(), TransportError> {
    let idr_msg = serialize_control_message(&ControlMessage::IdrRequest)?;
    if let Err(e) = control_send.write_all(&idr_msg).await {
        warn!("Failed to send IDR request: {e}");
    }
    Ok(())
}

pub(crate) async fn receive_loop(
    connection: quinn::Connection,
    mut control_send: quinn::SendStream,
    video_tx: mpsc::Sender<ReassembledFrame>,
    audio_tx: mpsc::Sender<ReassembledFrame>,
    mut input_rx: mpsc::Receiver<InputEvent>,
    mut decoder_idr_rx: mpsc::Receiver<()>,
    net_stats: &super::NetStats,
) -> Result<(), TransportError> {
    use std::sync::atomic::Ordering;
    let mut assembler = FrameAssembler::new();
    let mut total_frames: u64 = 0;
    let mut decoder_idr_open = true;

    loop {
        tokio::select! {
            // Keyframe requests from the decoder (after decode failures);
            // rate-limited like every other IDR request.
            idr = decoder_idr_rx.recv(), if decoder_idr_open => {
                match idr {
                    Some(()) if assembler.should_request_idr() => {
                        debug!("Requesting IDR keyframe (decoder recovery)");
                        send_idr_request(&mut control_send).await?;
                    }
                    Some(()) => {}
                    None => decoder_idr_open = false,
                }
            }

            datagram_result = connection.read_datagram() => {
                let datagram = match datagram_result {
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

                let (completed_frames, mut need_idr) =
                    assembler.process_datagram(&header, payload.to_vec());

                for frame in completed_frames {
                    total_frames += 1;
                    if frame.is_keyframe
                        || total_frames == 1
                        || (stargaze_core::logging::progress_logging() && total_frames % 300 == 1)
                    {
                        info!(
                            frame = total_frames,
                            pts = frame.pts,
                            size = frame.data.len(),
                            keyframe = frame.is_keyframe,
                            stream_type = frame.stream_type,
                            "Reassembled frame"
                        );
                    }

                    let send_result = match frame.stream_type {
                        STREAM_TYPE_VIDEO => {
                            net_stats
                                .video_bytes
                                .fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                            net_stats.video_frames.fetch_add(1, Ordering::Relaxed);

                            // Non-blocking send: if the decoder is behind,
                            // drop the frame rather than stalling datagram
                            // processing (which causes cascading loss).
                            match video_tx.try_send(frame) {
                                Ok(()) => Ok(()),
                                Err(mpsc::error::TrySendError::Full(f)) => {
                                    net_stats.video_dropped.fetch_add(1, Ordering::Relaxed);
                                    // Channel full — decoder is behind. Drop
                                    // the frame and request an IDR: the gap
                                    // would otherwise corrupt decoding until
                                    // the next periodic keyframe.
                                    debug!(
                                        pts = f.pts,
                                        "Dropping video frame (decoder backpressure)"
                                    );
                                    if !need_idr {
                                        need_idr = assembler.should_request_idr();
                                    }
                                    Ok(())
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    Err(mpsc::error::SendError(()))
                                }
                            }
                        }
                        STREAM_TYPE_AUDIO => {
                            audio_tx.send(frame).await.map_err(|_| mpsc::error::SendError(()))
                        }
                        other => {
                            warn!(stream_type = other, "Unknown stream type, dropping frame");
                            continue;
                        }
                    };

                    if send_result.is_err() {
                        info!("Frame receiver dropped, stopping transport");
                        return Ok(());
                    }
                }

                if need_idr {
                    debug!("Requesting IDR keyframe");
                    send_idr_request(&mut control_send).await?;
                }
            }

            input_event = input_rx.recv() => {
                let Some(event) = input_event else {
                    debug!("Input channel closed");
                    continue;
                };
                let msg = ControlMessage::Input(event);
                let bytes = serialize_control_message(&msg)?;
                if let Err(e) = control_send.write_all(&bytes).await {
                    warn!("Failed to send input event: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stargaze_core::transport::{STREAM_TYPE_AUDIO, STREAM_TYPE_VIDEO};

    fn make_header(
        stream_type: u8,
        frame_index: u32,
        fragment_index: u16,
        fragment_count: u16,
        pts: u64,
        is_keyframe: bool,
    ) -> DatagramHeader {
        DatagramHeader {
            stream_type,
            frame_index,
            fragment_index,
            fragment_count,
            pts,
            is_keyframe,
            capture_us: 0,
            convert_us: 0,
            encode_us: 0,
        }
    }

    fn video_header(
        frame_index: u32,
        fragment_index: u16,
        fragment_count: u16,
        pts: u64,
        is_keyframe: bool,
    ) -> DatagramHeader {
        make_header(
            STREAM_TYPE_VIDEO,
            frame_index,
            fragment_index,
            fragment_count,
            pts,
            is_keyframe,
        )
    }

    #[test]
    fn single_fragment_frame() {
        let mut assembler = FrameAssembler::new();
        let header = video_header(0, 0, 1, 100, true);
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

        let h0 = video_header(0, 0, 3, 0, false);
        let h1 = video_header(0, 1, 3, 0, false);
        let h2 = video_header(0, 2, 3, 0, false);

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

        let h2 = video_header(0, 2, 3, 42, true);
        let h0 = video_header(0, 0, 3, 42, true);
        let h1 = video_header(0, 1, 3, 42, true);

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

        let h0 = video_header(0, 0, 2, 0, false);
        let h1 = video_header(0, 1, 2, 0, false);

        let (frames, _) = assembler.process_datagram(&h0, vec![1, 2]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h0, vec![99, 99]);
        assert!(frames.is_empty());

        let (frames, _) = assembler.process_datagram(&h1, vec![3, 4]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn max_pending_triggers_idr() {
        let mut assembler = FrameAssembler::new();

        for i in 0..=MAX_PENDING_FRAMES as u32 {
            let h = video_header(i, 0, 2, u64::from(i), false);
            let (_, _need_idr) = assembler.process_datagram(&h, vec![0]);
        }

        let video_pending = assembler
            .pending
            .keys()
            .filter(|(st, _)| *st == STREAM_TYPE_VIDEO)
            .count();
        assert!(
            video_pending == 0 || video_pending <= MAX_PENDING_FRAMES,
            "Video pending should be cleared after IDR"
        );
    }

    #[test]
    fn idr_rate_limiting() {
        let mut assembler = FrameAssembler::new();

        assert!(assembler.should_request_idr());
        assert!(!assembler.should_request_idr());
    }

    #[test]
    fn multiple_frames_sequential() {
        let mut assembler = FrameAssembler::new();

        let h0 = video_header(0, 0, 1, 0, true);
        let (frames, _) = assembler.process_datagram(&h0, vec![10]);
        assert_eq!(frames.len(), 1);

        let h1 = video_header(1, 0, 1, 1, false);
        let (frames, _) = assembler.process_datagram(&h1, vec![20]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pts, 1);
    }

    #[test]
    fn mixed_streams_same_frame_index_no_collision() {
        let mut assembler = FrameAssembler::new();

        // Video frame 0 and audio frame 0 should not collide.
        let video_h = video_header(0, 0, 1, 100, true);
        let audio_h = make_header(STREAM_TYPE_AUDIO, 0, 0, 1, 200, false);

        let (frames, _) = assembler.process_datagram(&video_h, vec![0xAA]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].stream_type, STREAM_TYPE_VIDEO);
        assert_eq!(frames[0].data, vec![0xAA]);
        assert_eq!(frames[0].pts, 100);

        let (frames, _) = assembler.process_datagram(&audio_h, vec![0xBB]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].stream_type, STREAM_TYPE_AUDIO);
        assert_eq!(frames[0].data, vec![0xBB]);
        assert_eq!(frames[0].pts, 200);
    }

    #[test]
    fn per_stream_in_order_delivery() {
        let mut assembler = FrameAssembler::new();

        // Audio frame 1 is the first frame seen on this stream — in-order
        // tracking starts there and it is delivered immediately.
        let audio_1 = make_header(STREAM_TYPE_AUDIO, 1, 0, 1, 10, false);
        let (frames, _) = assembler.process_datagram(&audio_1, vec![0xBB]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, vec![0xBB]);

        // Audio frame 0 arrives late (before the stream start) — dropped.
        let audio_0 = make_header(STREAM_TYPE_AUDIO, 0, 0, 1, 5, false);
        let (frames, _) = assembler.process_datagram(&audio_0, vec![0xAA]);
        assert!(frames.is_empty());

        // Meanwhile, video stream is tracked independently.
        let video_0 = video_header(0, 0, 1, 50, true);
        let (frames, _) = assembler.process_datagram(&video_0, vec![0xCC]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].stream_type, STREAM_TYPE_VIDEO);
    }

    #[test]
    fn out_of_order_frames_delivered_in_order() {
        let mut assembler = FrameAssembler::new();

        // Frame 0 starts assembling (1 of 2 fragments).
        let f0_frag0 = video_header(0, 0, 2, 0, true);
        let (frames, _) = assembler.process_datagram(&f0_frag0, vec![1]);
        assert!(frames.is_empty());

        // Frame 1 completes while frame 0 is still pending — must NOT be
        // delivered ahead of frame 0.
        let f1 = video_header(1, 0, 1, 1, false);
        let (frames, need_idr) = assembler.process_datagram(&f1, vec![9]);
        assert!(frames.is_empty());
        assert!(!need_idr);

        // Frame 0 completes — both frames are delivered, in order.
        let f0_frag1 = video_header(0, 1, 2, 0, true);
        let (frames, _) = assembler.process_datagram(&f0_frag1, vec![2]);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, vec![1, 2]);
        assert_eq!(frames[1].data, vec![9]);
    }

    #[test]
    fn lost_frame_gap_skipped_and_idr_requested() {
        let mut assembler = FrameAssembler::new();

        // Frame 0 delivered normally.
        let f0 = video_header(0, 0, 1, 0, true);
        let (frames, _) = assembler.process_datagram(&f0, vec![1]);
        assert_eq!(frames.len(), 1);

        // Frame 1 is lost entirely; frame 2 arrives complete — only one
        // frame of progress, could still be simple reordering, so wait.
        let f2 = video_header(2, 0, 1, 2, false);
        let (frames, need_idr) = assembler.process_datagram(&f2, vec![3]);
        assert!(frames.is_empty());
        assert!(!need_idr);

        // Frame 3 completes too — frame 1 is now considered lost: the gap
        // is skipped, frames 2 and 3 are delivered, and an IDR is requested.
        let f3 = video_header(3, 0, 1, 3, false);
        let (frames, need_idr) = assembler.process_datagram(&f3, vec![4]);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].pts, 2);
        assert_eq!(frames[1].pts, 3);
        assert!(need_idr, "Skipping a lost video frame must request an IDR");

        // A late fragment of the skipped frame 1 is ignored.
        let f1_late = video_header(1, 0, 2, 1, false);
        let (frames, need_idr) = assembler.process_datagram(&f1_late, vec![7]);
        assert!(frames.is_empty());
        assert!(!need_idr);
    }

    #[test]
    fn audio_pending_does_not_trigger_idr() {
        let mut assembler = FrameAssembler::new();

        // Fill up many incomplete audio frames — should NOT trigger IDR.
        for i in 0..=MAX_PENDING_FRAMES as u32 + 5 {
            let h = make_header(STREAM_TYPE_AUDIO, i, 0, 2, u64::from(i), false);
            let (_, need_idr) = assembler.process_datagram(&h, vec![0]);
            assert!(!need_idr, "Audio frames should never trigger IDR");
        }
    }
}
