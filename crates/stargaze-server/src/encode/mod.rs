//! Video encoding module — public API.
//!
//! Provides `start_encoder()` which spawns a dedicated thread for
//! `FFmpeg` NVENC encoding and returns an `EncoderSession` handle
//! plus a channel receiver for encoded packets.

pub(crate) mod ffmpeg;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use stargaze_core::capture::Frame;
use stargaze_core::encode::{EncodeError, EncodedPacket, EncoderConfig};
use tokio::sync::mpsc;
use tracing::{error, info};

/// Channel capacity for encoded packet delivery.
///
/// 4 packets provides a small buffer without excessive latency.
/// If the consumer can't keep up, the encoder thread blocks on
/// `blocking_send()`, which backs up into the capture channel.
const PACKET_CHANNEL_CAPACITY: usize = 4;

/// Handle to a running encoder session.
///
/// Signals the encoder thread to shut down on drop. The caller must
/// keep this alive for the duration of encoding.
pub struct EncoderSession {
    /// Join handle for the dedicated encoder thread.
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Shared flag to signal the encoder thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl EncoderSession {
    /// Gracefully stops encoding: signals shutdown, waits for the
    /// encoder thread to flush remaining packets and exit.
    ///
    /// # Errors
    ///
    /// Returns `EncodeError::FfmpegError` if the encoder thread panicked.
    pub fn stop(mut self) -> Result<(), EncodeError> {
        self.signal_shutdown();
        self.join_thread()
    }

    /// Signals the encoder thread to shut down.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Joins the encoder thread, returning any error.
    fn join_thread(&mut self) -> Result<(), EncodeError> {
        if let Some(handle) = self.thread_handle.take() {
            handle
                .join()
                .map_err(|_| EncodeError::FfmpegError("encoder thread panicked".to_string()))?;
        }
        Ok(())
    }
}

impl Drop for EncoderSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        // Best-effort join — don't propagate errors from drop.
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts the video encoder.
///
/// Takes ownership of the frame receiver from capture and returns
/// an encoder session handle plus a channel receiver for encoded packets.
///
/// `FFmpeg` initialization (CUDA device, NVENC codec) happens on the
/// spawned thread. If initialization fails, the error is sent back
/// via a oneshot channel and returned from this function.
///
/// # Errors
///
/// Returns `EncodeError::InitError` if `FFmpeg`/NVENC initialization fails.
/// Returns `EncodeError::FfmpegError` if the encoder thread fails to spawn.
pub fn start_encoder(
    config: EncoderConfig,
    frames: mpsc::Receiver<Frame>,
) -> Result<(EncoderSession, mpsc::Receiver<EncodedPacket>), EncodeError> {
    let (packets_tx, packets_rx) = mpsc::channel(PACKET_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Use a oneshot channel to report initialization errors back to the caller.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), EncodeError>>();

    let thread_handle = thread::Builder::new()
        .name("stargaze-encoder".to_string())
        .spawn(move || {
            // Initialize the encoder on this thread (FFmpeg contexts are thread-local).
            let mut encoder = match ffmpeg::init_encoder(&config) {
                Ok(enc) => {
                    let _ = init_tx.send(Ok(()));
                    enc
                }
                Err(e) => {
                    error!("Encoder initialization failed: {e}");
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let mut frames = frames;

            // Run the encode loop until shutdown or channel close.
            if let Err(e) =
                ffmpeg::run_encode_loop(&mut encoder, &mut frames, &packets_tx, &shutdown_clone)
            {
                error!("Encoder loop failed: {e}");
            }

            info!("Encoder thread exiting");
        })
        .map_err(|e| EncodeError::FfmpegError(format!("failed to spawn encoder thread: {e}")))?;

    // Wait for initialization to complete.
    let init_result = init_rx.recv().map_err(|_| {
        EncodeError::InitError("encoder thread exited during initialization".to_string())
    })?;

    // If init failed, join the thread and propagate the error.
    init_result?;

    info!("Encoder started on dedicated thread");

    Ok((
        EncoderSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        packets_rx,
    ))
}
