//! Video and audio encoding module — public API.
//!
//! Provides [`start_encoder()`] for `FFmpeg` NVENC video encoding and
//! [`start_audio_encoder()`] for Opus audio encoding. Both return a
//! session handle plus a channel receiver for encoded packets.

pub(crate) mod ffmpeg;
pub(crate) mod opus_enc;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use stargaze_core::audio::{AudioEncoderConfig, AudioError, AudioFrame};
use stargaze_core::capture::Frame;
use stargaze_core::encode::{EncodeError, EncodedPacket, EncoderConfig};
use tokio::sync::{mpsc, watch};
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
/// Takes ownership of the frame receiver from capture and returns a
/// 3-tuple: an encoder session handle, a channel receiver for encoded
/// packets, and a `watch::Sender<u64>` for IDR keyframe requests.
///
/// To request an IDR keyframe (e.g., after the client detects packet
/// loss), increment the value sent through the `watch::Sender`. The
/// encoder checks the watch channel before each frame and forces a
/// keyframe whenever the value changes.
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
) -> Result<
    (
        EncoderSession,
        mpsc::Receiver<EncodedPacket>,
        watch::Sender<u64>,
    ),
    EncodeError,
> {
    let (packets_tx, packets_rx) = mpsc::channel(PACKET_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let (idr_tx, idr_rx) = watch::channel(0u64);

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
            if let Err(e) = ffmpeg::run_encode_loop(
                &mut encoder,
                &mut frames,
                &packets_tx,
                &shutdown_clone,
                idr_rx,
            ) {
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
        idr_tx,
    ))
}

/// Handle to a running audio encoder session.
///
/// Signals the audio encoder thread to shut down on drop.
pub struct AudioEncoderSession {
    /// Join handle for the dedicated audio encoder thread.
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Shared flag to signal the audio encoder thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl AudioEncoderSession {
    /// Gracefully stops encoding: signals shutdown and waits for the thread to exit.
    ///
    /// # Errors
    ///
    /// Returns `AudioError::EncoderInit` if the audio encoder thread panicked.
    pub fn stop(mut self) -> Result<(), AudioError> {
        self.signal_shutdown();
        self.join_thread()
    }

    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn join_thread(&mut self) -> Result<(), AudioError> {
        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| {
                AudioError::EncoderInit("audio encoder thread panicked".to_string())
            })?;
        }
        Ok(())
    }
}

impl Drop for AudioEncoderSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts the Opus audio encoder.
///
/// Spawns a dedicated thread that reads [`AudioFrame`]s from `frames`,
/// encodes them with Opus, and sends [`EncodedPacket`]s to the returned receiver.
///
/// Initialization happens on the spawned thread. If it fails, the error is
/// propagated back to the caller via a oneshot channel.
///
/// # Errors
///
/// Returns [`AudioError::EncoderInit`] if Opus initialization fails or the
/// thread cannot be spawned.
pub fn start_audio_encoder(
    config: AudioEncoderConfig,
    frames: mpsc::Receiver<AudioFrame>,
) -> Result<(AudioEncoderSession, mpsc::Receiver<EncodedPacket>), AudioError> {
    let (packets_tx, packets_rx) = mpsc::channel(PACKET_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), AudioError>>();

    let thread_handle = thread::Builder::new()
        .name("stargaze-audio-encoder".to_string())
        .spawn(move || {
            let mut encoder = match opus_enc::init_opus_encoder(&config) {
                Ok(enc) => {
                    let _ = init_tx.send(Ok(()));
                    enc
                }
                Err(e) => {
                    error!("Audio encoder initialization failed: {e}");
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let mut frames = frames;

            if let Err(e) = opus_enc::run_opus_encode_loop(
                &mut encoder,
                &mut frames,
                &packets_tx,
                &shutdown_clone,
            ) {
                error!("Audio encoder loop failed: {e}");
            }

            info!("Audio encoder thread exiting");
        })
        .map_err(|e| {
            AudioError::EncoderInit(format!("failed to spawn audio encoder thread: {e}"))
        })?;

    let init_result = init_rx.recv().map_err(|_| {
        AudioError::EncoderInit("audio encoder thread exited during initialization".to_string())
    })?;

    init_result?;

    info!("Audio encoder started on dedicated thread");

    Ok((
        AudioEncoderSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        packets_rx,
    ))
}
