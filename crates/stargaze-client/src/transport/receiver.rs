//! Datagram reassembly and control message handling for the client.
//!
//! Contains the [`FrameAssembler`] which collects datagram fragments
//! into complete frames, and the handshake/receive logic.

use std::collections::HashMap;
use std::time::Instant;

use stargaze_core::input::InputEvent;
use stargaze_core::transport::{
    ControlMessage, DatagramHeader, IDR_RATE_LIMIT_MS, IDR_RETRY_MS, MAX_PENDING_FRAMES,
    ReassembledFrame, STREAM_TYPE_AUDIO, STREAM_TYPE_VIDEO, TransportError, deserialize_header,
    deserialize_session_response_compat, serialize_control_message,
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
    /// Confirmed video codec — what the server actually encodes with
    /// (it may ignore the requested codec); the decoder must use this.
    pub codec: stargaze_core::config::Codec,
    /// Maximum datagram payload size for the connection.
    pub max_datagram_size: u16,
    /// Server command line, sanitized of addresses and ports.
    pub server_command: String,
    /// Number of audio channels the server encodes (1, 2, 6, or 8).
    pub audio_channels: u16,
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
        bitrate_mbps: request.bitrate_mbps,
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

    // Compat parse: pre-surround servers send the response without an
    // audio_channels field (defaults to 2 = stereo).
    let response = deserialize_session_response_compat(&body)?;

    match response {
        ControlMessage::SessionResponse {
            width,
            height,
            framerate,
            bitrate_mbps,
            codec,
            max_datagram_size,
            cursor_embedded: _,
            server_command,
            audio_channels,
        } => Ok(SessionParams {
            width,
            height,
            framerate,
            bitrate_mbps,
            codec,
            max_datagram_size,
            server_command,
            audio_channels,
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
    /// True from the moment video continuity breaks (a frame was lost or
    /// dropped) until a keyframe is delivered. While set, IDR requests
    /// are re-issued every [`IDR_RETRY_MS`]: a single request can be
    /// lost, or the keyframe it produces can itself be lost, and with an
    /// infinite GOP nothing else would ever clean the corruption up.
    awaiting_keyframe: bool,
    /// A disruption happened and its IDR request has not been issued yet
    /// (deferred by the rate limiter). Unlike `awaiting_keyframe` this
    /// clears as soon as a request goes out: fresh disruptions request at
    /// the [`IDR_RATE_LIMIT_MS`] floor, unanswered ones retry at the
    /// slower [`IDR_RETRY_MS`] cadence.
    request_pending: bool,
}

impl FrameAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            next_frame: HashMap::new(),
            max_pending: MAX_PENDING_FRAMES,
            last_idr_request: None,
            awaiting_keyframe: false,
            request_pending: false,
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

        // Reject malformed headers off the untrusted network before they
        // touch any state: a zero `fragment_count` would create an
        // instantly-"complete" empty frame (and, if keyframe-flagged,
        // falsely clear recovery state), and an out-of-range
        // `fragment_index` can never index into the fragment buffer.
        if header.fragment_count == 0 || header.fragment_index >= header.fragment_count {
            return (completed, false);
        }

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

        self.deliver_in_order(header.stream_type, next, &mut completed);

        // Too many incomplete video frames pending: reassembly is
        // hopelessly behind.
        let video_pending = self
            .pending
            .keys()
            .filter(|(st, _)| *st == STREAM_TYPE_VIDEO)
            .count();
        if video_pending > self.max_pending {
            self.note_disruption();
        }

        // Fresh disruptions request at the rate-limit floor (deferred,
        // never dropped, if inside the window); an unanswered awaiting
        // state retries at the slower cadence so a keyframe still in
        // flight is not duplicated.
        if self.request_pending {
            if self.should_request_idr() {
                self.request_pending = false;
                need_idr = true;
            }
        } else if self.awaiting_keyframe && self.retry_due() {
            need_idr = self.should_request_idr();
        }

        // Reset video tracking to the freshest frames — but only once the
        // paired IDR request actually goes out: wiping earlier would
        // destroy buffered frames (possibly a recovery keyframe stuck
        // behind a gap) with no replacement on the way yet.
        if need_idr && video_pending > self.max_pending {
            let resume = self
                .pending
                .keys()
                .filter(|(st, _)| *st == STREAM_TYPE_VIDEO)
                .map(|(_, idx)| *idx)
                .max()
                .map(|idx| idx.wrapping_add(1));
            self.pending.retain(|(st, _), _| *st != STREAM_TYPE_VIDEO);
            if let Some(resume) = resume {
                // Keep the in-order cursor monotonic instead of removing
                // it: re-seeding from the next datagram would let an
                // unordered straggler of a wiped frame anchor tracking
                // backwards and stall delivery.
                self.next_frame.insert(STREAM_TYPE_VIDEO, resume);
            }
        }

        (completed, need_idr)
    }

    /// Records a video disruption (lost frame, decoder backpressure drop,
    /// decode failure, pending overflow): keeps IDR requests firing until
    /// the next keyframe is delivered.
    pub fn note_disruption(&mut self) {
        self.awaiting_keyframe = true;
        self.request_pending = true;
    }

    /// Records an external disruption and immediately decides whether to
    /// send the IDR request now (rate-limited). If deferred, the pending
    /// request is issued from the datagram path as soon as the limiter
    /// allows.
    pub fn request_idr_now(&mut self) -> bool {
        self.note_disruption();
        if self.should_request_idr() {
            self.request_pending = false;
            return true;
        }
        false
    }

    /// True once enough time has passed to re-issue an unanswered request.
    fn retry_due(&self) -> bool {
        self.last_idr_request
            .is_none_or(|last| last.elapsed().as_millis() >= u128::from(IDR_RETRY_MS))
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
            // Delivered mid-recovery: decoding this delta would predict
            // from missing references (ffmpeg substitutes gray frames).
            tainted: self.awaiting_keyframe && !pending.is_keyframe,
        })
    }

    /// Delivers complete frames in order starting from `next` (the
    /// caller's already-seeded cursor for this stream).
    fn deliver_in_order(
        &mut self,
        stream_type: u8,
        mut next: u32,
        completed: &mut Vec<ReassembledFrame>,
    ) {
        loop {
            let key = (stream_type, next);
            let is_complete = self
                .pending
                .get(&key)
                .is_some_and(|pf| pf.received_count == pf.fragment_count);
            if is_complete {
                if let Some(frame) = self.assemble_frame(key) {
                    if stream_type == STREAM_TYPE_VIDEO && frame.is_keyframe {
                        // A keyframe fully resets the decoder (extradata is
                        // prepended server-side) — recovery is complete and
                        // any queued request is moot.
                        self.awaiting_keyframe = false;
                        self.request_pending = false;
                    }
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
                if stream_type == STREAM_TYPE_VIDEO {
                    // Downstream consequence: the decoder is now missing a
                    // reference frame and will log `ffmpeg` warnings such
                    // as "Could not find ref with POC N" (with visible
                    // artifacts) until the requested IDR keyframe arrives.
                    warn!(
                        frame_index = next,
                        "Video frame lost on the network; skipping it and \
                         requesting an IDR (expect decoder reference warnings \
                         until the keyframe arrives)"
                    );
                    self.awaiting_keyframe = true;
                    self.request_pending = true;
                }
                next = next.wrapping_add(1);
                continue;
            }
            break;
        }
        self.next_frame.insert(stream_type, next);
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
                    Some(()) => {
                        // Sticky: if this request (or its keyframe) is lost,
                        // the assembler re-requests until a keyframe lands.
                        // A message racing with an already-delivered keyframe
                        // costs one redundant keyframe — acceptable, since
                        // NOT requesting risks persistent corruption.
                        if assembler.request_idr_now() {
                            debug!("Requesting IDR keyframe (decoder recovery)");
                            send_idr_request(&mut control_send).await?;
                        }
                    }
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
                        debug!(
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

                            // A delta delivered while recovery is pending
                            // can only decode into gray-smeared garbage
                            // (its references were lost). Freeze on the
                            // last good frame instead of displaying it;
                            // the pending IDR restarts decoding cleanly.
                            if frame.tainted {
                                net_stats.video_dropped.fetch_add(1, Ordering::Relaxed);
                                debug!(
                                    pts = frame.pts,
                                    "Dropping tainted video frame (awaiting keyframe)"
                                );
                                continue;
                            }

                            // Non-blocking send: if the decoder is behind,
                            // drop the frame rather than stalling datagram
                            // processing (which causes cascading loss).
                            match video_tx.try_send(frame) {
                                Ok(()) => Ok(()),
                                Err(mpsc::error::TrySendError::Full(f)) => {
                                    net_stats.video_dropped.fetch_add(1, Ordering::Relaxed);
                                    // Channel full — decoder is behind. Drop
                                    // the frame and request an IDR: the gap
                                    // would otherwise corrupt decoding
                                    // forever (infinite GOP).
                                    debug!(
                                        pts = f.pts,
                                        "Dropping video frame (decoder backpressure)"
                                    );
                                    if !need_idr {
                                        need_idr = assembler.request_idr_now();
                                    } else {
                                        assembler.note_disruption();
                                    }
                                    Ok(())
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    Err(mpsc::error::SendError(()))
                                }
                            }
                        }
                        STREAM_TYPE_AUDIO => {
                            // Non-blocking, like the video path above:
                            // awaiting on a full audio channel would
                            // backpressure this datagram select loop and
                            // stall *video* reads, causing cascading
                            // unreliable-datagram loss. A dropped Opus frame
                            // is a brief glitch the decoder recovers from on
                            // the next packet — no IDR needed.
                            match audio_tx.try_send(frame) {
                                // Sent, or dropped because the decoder is
                                // behind — both are fine for audio.
                                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    Err(mpsc::error::SendError(()))
                                }
                            }
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
    fn malformed_zero_fragment_count_rejected() {
        // A zero fragment_count off the network must not create an
        // instantly-"complete" empty frame, nor (when keyframe-flagged)
        // clear recovery state.
        let mut assembler = FrameAssembler::new();
        let header = video_header(0, 0, 0, 100, true);

        let (frames, need_idr) = assembler.process_datagram(&header, vec![1, 2, 3]);

        assert!(frames.is_empty());
        assert!(!need_idr);
    }

    #[test]
    fn malformed_datagram_does_not_anchor_delivery() {
        // Malformed headers must be rejected *before* they seed in-order
        // tracking. Without the guard, an out-of-range fragment_index (or a
        // zero fragment_count) with a high frame_index would seed
        // `next_frame` to that index, causing a later legitimate frame 0 to
        // be dropped as "late".
        let mut assembler = FrameAssembler::new();

        // High frame_index, fragment_index >= fragment_count.
        let bad = video_header(10_000, 3, 3, 100, false);
        let (frames, _) = assembler.process_datagram(&bad, vec![9]);
        assert!(frames.is_empty());

        // High frame_index, zero fragment_count.
        let bad2 = video_header(10_001, 0, 0, 100, true);
        let (frames, _) = assembler.process_datagram(&bad2, vec![1, 2, 3]);
        assert!(frames.is_empty());

        // A legitimate frame 0 must still be delivered — proving neither
        // malformed datagram anchored delivery at their high indices.
        let good = video_header(0, 0, 1, 200, true);
        let (frames, need_idr) = assembler.process_datagram(&good, vec![1, 2, 3]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, vec![1, 2, 3]);
        assert!(!need_idr);
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

    /// Backdates the last IDR request by `ms` milliseconds.
    fn backdate_last_request(assembler: &mut FrameAssembler, ms: u64) {
        assembler.last_idr_request = Some(Instant::now() - std::time::Duration::from_millis(ms));
    }

    fn video_pending_count(assembler: &FrameAssembler) -> usize {
        assembler
            .pending
            .keys()
            .filter(|(st, _)| *st == STREAM_TYPE_VIDEO)
            .count()
    }

    #[test]
    fn idr_rerequested_until_keyframe_arrives() {
        let mut assembler = FrameAssembler::new();

        // Frame 0 delivered normally.
        let (frames, _) = assembler.process_datagram(&video_header(0, 0, 1, 0, true), vec![1]);
        assert_eq!(frames.len(), 1);

        // Frame 1 lost; frames 2 and 3 arrive → gap skipped, IDR requested.
        assembler.process_datagram(&video_header(2, 0, 1, 2, false), vec![2]);
        let (_, need_idr) = assembler.process_datagram(&video_header(3, 0, 1, 3, false), vec![3]);
        assert!(need_idr);

        // Within the retry window: no duplicate request while the
        // keyframe may still be in flight.
        let (_, need_idr) = assembler.process_datagram(&video_header(4, 0, 1, 4, false), vec![4]);
        assert!(!need_idr);
        backdate_last_request(&mut assembler, IDR_RATE_LIMIT_MS * 2);
        let (_, need_idr) = assembler.process_datagram(&video_header(5, 0, 1, 5, false), vec![5]);
        assert!(!need_idr, "retries are slower than the rate-limit floor");

        // Retry window elapses and still no keyframe (the request or the
        // IDR it produced was lost): the request must be re-issued.
        backdate_last_request(&mut assembler, IDR_RETRY_MS * 2);
        let (_, need_idr) = assembler.process_datagram(&video_header(6, 0, 1, 6, false), vec![6]);
        assert!(
            need_idr,
            "IDR must be re-requested until a keyframe arrives"
        );

        // Keyframe delivered: recovery complete, requests stop.
        let (frames, need_idr) =
            assembler.process_datagram(&video_header(7, 0, 1, 7, true), vec![7]);
        assert_eq!(frames.len(), 1);
        assert!(!need_idr);
        backdate_last_request(&mut assembler, IDR_RETRY_MS * 2);
        let (_, need_idr) = assembler.process_datagram(&video_header(8, 0, 1, 8, false), vec![8]);
        assert!(!need_idr, "recovery is over once a keyframe is delivered");
    }

    #[test]
    fn fresh_disruption_request_is_deferred_not_dropped() {
        let mut assembler = FrameAssembler::new();

        // Frame 0 delivered; an IDR request just went out.
        assembler.process_datagram(&video_header(0, 0, 1, 0, true), vec![1]);
        assembler.last_idr_request = Some(Instant::now());

        // A fresh disruption inside the rate-limit window: the request is
        // deferred, not dropped.
        assembler.note_disruption();
        let (_, need_idr) = assembler.process_datagram(&video_header(1, 0, 1, 1, false), vec![2]);
        assert!(!need_idr);

        // Once the rate-limit floor passes (well before the retry
        // cadence), the deferred request goes out.
        backdate_last_request(&mut assembler, IDR_RATE_LIMIT_MS * 2);
        let (_, need_idr) = assembler.process_datagram(&video_header(2, 0, 1, 2, false), vec![3]);
        assert!(
            need_idr,
            "a deferred fresh request fires at the rate-limit floor"
        );
    }

    #[test]
    fn note_disruption_triggers_idr_rerequests() {
        let mut assembler = FrameAssembler::new();

        // Frame 0 delivered, then a frame is dropped outside the assembler
        // (decoder backpressure).
        assembler.process_datagram(&video_header(0, 0, 1, 0, true), vec![1]);
        assembler.note_disruption();

        backdate_last_request(&mut assembler, IDR_RATE_LIMIT_MS * 2);
        let (_, need_idr) = assembler.process_datagram(&video_header(1, 0, 1, 1, false), vec![2]);
        assert!(need_idr);

        // Cleared by the next keyframe.
        let (_, need_idr) = assembler.process_datagram(&video_header(2, 0, 1, 2, true), vec![3]);
        assert!(!need_idr);
        backdate_last_request(&mut assembler, IDR_RETRY_MS * 2);
        let (_, need_idr) = assembler.process_datagram(&video_header(3, 0, 1, 3, false), vec![4]);
        assert!(!need_idr);
    }

    #[test]
    fn overflow_wipe_waits_for_granted_request_and_resumes_forward() {
        let mut assembler = FrameAssembler::new();
        // An IDR request just went out: the limiter suppresses new ones.
        assembler.last_idr_request = Some(Instant::now());

        // Fill past max_pending with incomplete frames (1 of 2 fragments).
        for i in 0..=MAX_PENDING_FRAMES as u32 {
            let h = video_header(i, 0, 2, u64::from(i), false);
            let (_, need_idr) = assembler.process_datagram(&h, vec![0]);
            assert!(!need_idr, "request suppressed inside the rate window");
        }
        assert!(
            video_pending_count(&assembler) > MAX_PENDING_FRAMES,
            "buffered frames must be retained until the request can go out"
        );

        // Window expires: the request fires and video tracking resets
        // past the freshest wiped frame.
        backdate_last_request(&mut assembler, IDR_RATE_LIMIT_MS * 2);
        let newest = MAX_PENDING_FRAMES as u32 + 1;
        let (_, need_idr) =
            assembler.process_datagram(&video_header(newest, 0, 2, 99, false), vec![0]);
        assert!(need_idr);
        assert_eq!(video_pending_count(&assembler), 0);
        assert_eq!(assembler.next_frame[&STREAM_TYPE_VIDEO], newest + 1);

        // A straggler fragment of a wiped frame must not re-anchor the
        // cursor backwards.
        let (frames, _) = assembler.process_datagram(&video_header(3, 1, 2, 3, false), vec![1]);
        assert!(frames.is_empty());
        assert_eq!(assembler.next_frame[&STREAM_TYPE_VIDEO], newest + 1);
    }

    #[test]
    fn frames_after_gap_are_tainted_until_keyframe() {
        let mut assembler = FrameAssembler::new();

        // Keyframe 0 delivered clean.
        let (frames, _) = assembler.process_datagram(&video_header(0, 0, 1, 0, true), vec![1]);
        assert!(!frames[0].tainted);

        // Frame 1 lost; frames 2 and 3 arrive → gap skipped, both
        // delivered tainted (their references are gone).
        assembler.process_datagram(&video_header(2, 0, 1, 2, false), vec![2]);
        let (frames, _) = assembler.process_datagram(&video_header(3, 0, 1, 3, false), vec![3]);
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.tainted));

        // Still tainted until a keyframe arrives.
        let (frames, _) = assembler.process_datagram(&video_header(4, 0, 1, 4, false), vec![4]);
        assert!(frames[0].tainted);

        // The recovery keyframe itself is clean, and so is what follows.
        let (frames, _) = assembler.process_datagram(&video_header(5, 0, 1, 5, true), vec![5]);
        assert!(!frames[0].tainted);
        let (frames, _) = assembler.process_datagram(&video_header(6, 0, 1, 6, false), vec![6]);
        assert!(!frames[0].tainted);
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
