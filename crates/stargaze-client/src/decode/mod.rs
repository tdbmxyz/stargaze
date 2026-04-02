//! Video decoding module — public API.
//!
//! Provides `start_decoder()` which spawns a dedicated thread for
//! `FFmpeg` H.265 software decoding and returns a `DecoderSession` handle
//! plus a channel receiver for decoded frames.

pub(crate) mod ffmpeg;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use stargaze_core::decode::{DecodeError, DecodedFrame, DecoderConfig};
use stargaze_core::transport::ReassembledFrame;
use tokio::sync::mpsc;
use tracing::{error, info};

/// Handle to a running decoder session.
///
/// Signals the decoder thread to shut down on drop. The caller must
/// keep this alive for the duration of decoding.
pub struct DecoderSession {
    /// Join handle for the dedicated decoder thread.
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Shared flag to signal the decoder thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl DecoderSession {
    /// Gracefully stops decoding: signals shutdown, waits for the
    /// decoder thread to drain remaining frames and exit.
    ///
    /// # Errors
    ///
    /// Returns `DecodeError::FfmpegError` if the decoder thread panicked.
    pub fn stop(mut self) -> Result<(), DecodeError> {
        self.signal_shutdown();
        self.join_thread()
    }

    /// Signals the decoder thread to shut down.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Joins the decoder thread, returning any error.
    fn join_thread(&mut self) -> Result<(), DecodeError> {
        if let Some(handle) = self.thread_handle.take() {
            handle
                .join()
                .map_err(|_| DecodeError::FfmpegError("decoder thread panicked".to_string()))?;
        }
        Ok(())
    }
}

impl Drop for DecoderSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        // Best-effort join — don't propagate errors from drop.
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts the video decoder.
///
/// Takes ownership of the reassembled frame receiver from transport and returns
/// a `DecoderSession` handle plus a channel receiver for decoded frames ready
/// for rendering.
///
/// The decoder→renderer channel uses `std::sync::mpsc` (not tokio) because
/// the renderer runs synchronously.
///
/// `FFmpeg` initialization happens on the spawned thread. If initialization fails,
/// the error is sent back via a oneshot channel and returned from this function.
///
/// # Errors
///
/// Returns `DecodeError::InitError` if FFmpeg/HEVC initialization fails.
/// Returns `DecodeError::FfmpegError` if the decoder thread fails to spawn.
pub fn start_decoder(
    config: DecoderConfig,
    frames_rx: mpsc::Receiver<ReassembledFrame>,
) -> Result<(DecoderSession, std::sync::mpsc::Receiver<DecodedFrame>), DecodeError> {
    let (decoded_tx, decoded_rx) = std::sync::mpsc::channel::<DecodedFrame>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Use a sync channel to report initialization errors back to the caller.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), DecodeError>>();

    let thread_handle = thread::Builder::new()
        .name("stargaze-decoder".to_string())
        .spawn(move || {
            // Initialize the decoder on this thread (FFmpeg contexts are thread-local).
            let mut decoder = match ffmpeg::init_decoder(&config) {
                Ok(dec) => {
                    let _ = init_tx.send(Ok(()));
                    dec
                }
                Err(e) => {
                    error!("Decoder initialization failed: {e}");
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let mut frames_rx = frames_rx;

            // Run the decode loop until shutdown or channel close.
            if let Err(e) = ffmpeg::run_decode_loop(
                &mut decoder,
                &mut frames_rx,
                &decoded_tx,
                &shutdown_clone,
            ) {
                error!("Decoder loop failed: {e}");
            }

            info!("Decoder thread exiting");
        })
        .map_err(|e| DecodeError::FfmpegError(format!("failed to spawn decoder thread: {e}")))?;

    // Wait for initialization to complete.
    let init_result = init_rx.recv().map_err(|_| {
        DecodeError::InitError("decoder thread exited during initialization".to_string())
    })?;

    // If init failed, join the thread and propagate the error.
    init_result?;

    info!("Decoder started on dedicated thread");

    Ok((
        DecoderSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        decoded_rx,
    ))
}
