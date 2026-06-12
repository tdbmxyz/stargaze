//! Video and audio decoding module — public API.
//!
//! Provides [`start_decoder()`] for `FFmpeg` H.265 video decoding and
//! [`start_audio_decoder()`] for Opus audio decoding. Both return a
//! session handle that can be used to stop the decoder thread.

pub(crate) mod ffmpeg;
pub(crate) mod opus_dec;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use stargaze_core::audio::{AudioDecoderConfig, AudioError};
use stargaze_core::decode::{DecodeError, DecodedFrame, DecoderConfig, FrameStats};
use stargaze_core::transport::ReassembledFrame;
use tokio::sync::mpsc;
use tracing::{error, info};

pub use ffmpeg::DmaBufFrame;

/// A decoded frame flowing from the decoder to the renderer.
pub enum VideoFrame {
    /// Pixel planes in CPU memory (software decode, or hardware decode
    /// when the zero-copy path is unavailable).
    Cpu(DecodedFrame),
    /// A GPU-resident frame exported as DRM PRIME dma-bufs, rendered
    /// without ever touching CPU memory.
    DmaBuf(DmaBufFrame),
}

impl VideoFrame {
    /// Per-frame pipeline timing, for the client stats overlay.
    #[must_use]
    pub fn stats(&self) -> FrameStats {
        match self {
            Self::Cpu(f) => f.stats,
            Self::DmaBuf(f) => f.stats,
        }
    }
}

/// Handle to a running decoder session (decode thread + extraction thread).
///
/// Signals the decoder thread to shut down on drop. The caller must
/// keep this alive for the duration of decoding.
pub struct DecoderSession {
    /// Join handle for the dedicated decoder thread.
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Join handle for the plane-extraction thread.
    extract_handle: Option<thread::JoinHandle<()>>,
    /// Shared flag to signal the decoder thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl DecoderSession {
    /// Gracefully stops decoding: signals shutdown, waits for both
    /// threads to drain remaining frames and exit.
    ///
    /// # Errors
    ///
    /// Returns `DecodeError::FfmpegError` if a decoder thread panicked.
    pub fn stop(mut self) -> Result<(), DecodeError> {
        self.signal_shutdown();
        self.join_threads()
    }

    /// Signals the decoder thread to shut down.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Joins both threads, returning any error. The decode thread exits
    /// first (on shutdown), which closes the raw-frame channel and ends
    /// the extraction thread.
    fn join_threads(&mut self) -> Result<(), DecodeError> {
        for handle in [self.thread_handle.take(), self.extract_handle.take()]
            .into_iter()
            .flatten()
        {
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
        for handle in [self.thread_handle.take(), self.extract_handle.take()]
            .into_iter()
            .flatten()
        {
            let _ = handle.join();
        }
    }
}

/// Starts the video decoder.
///
/// Returns the session handle, the decoded-frame receiver, and the shared
/// zero-copy flag: initially `true` when hardware decoding is active, and
/// cleared by either side (decoder or renderer) when dma-buf delivery
/// fails — after which the decoder ships CPU frames instead.
///
/// # Errors
///
/// Returns `DecodeError::InitError` if FFmpeg/HEVC initialization fails.
/// Returns `DecodeError::FfmpegError` if the decoder thread fails to spawn.
pub fn start_decoder(
    config: DecoderConfig,
    frames_rx: mpsc::Receiver<ReassembledFrame>,
    idr_tx: mpsc::Sender<()>,
) -> Result<
    (
        DecoderSession,
        std::sync::mpsc::Receiver<VideoFrame>,
        Arc<AtomicBool>,
    ),
    DecodeError,
> {
    let (decoded_tx, decoded_rx) = std::sync::mpsc::channel::<VideoFrame>();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let zero_copy = Arc::new(AtomicBool::new(false));
    let zero_copy_clone = Arc::clone(&zero_copy);

    // Decode → extraction channel. Capacity 1 bounds the pipeline to one
    // frame in flight: extraction of frame N overlaps decode of N+1
    // without adding queueing latency.
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<ffmpeg::DecodeOutput>(1);

    // Use a sync channel to report initialization errors back to the caller.
    // Ok carries whether hardware (VAAPI) decoding is active.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<bool, DecodeError>>();

    let thread_handle = thread::Builder::new()
        .name("stargaze-decoder".to_string())
        .spawn(move || {
            // Initialize the decoder on this thread (FFmpeg contexts are thread-local).
            let mut decoder = match ffmpeg::init_decoder(&config) {
                Ok(dec) => {
                    let _ = init_tx.send(Ok(dec.hw_accel));
                    dec
                }
                Err(e) => {
                    error!("Decoder initialization failed: {e}");
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let mut frames_rx = frames_rx;

            // Run the decode loop until shutdown or channel close,
            // re-creating the decoder if zero-copy exports poisoned it.
            loop {
                match ffmpeg::run_decode_loop(
                    &mut decoder,
                    &mut frames_rx,
                    &raw_tx,
                    &shutdown_clone,
                    &zero_copy_clone,
                    &idr_tx,
                ) {
                    Ok(ffmpeg::LoopExit::Finished) => break,
                    Ok(ffmpeg::LoopExit::Reinit) => match ffmpeg::init_decoder(&config) {
                        Ok(dec) => {
                            info!("Video decoder re-created after zero-copy poisoning");
                            decoder = dec;
                        }
                        Err(e) => {
                            error!("Decoder re-initialization failed: {e}");
                            break;
                        }
                    },
                    Err(e) => {
                        error!("Decoder loop failed: {e}");
                        break;
                    }
                }
            }

            info!("Decoder thread exiting");
        })
        .map_err(|e| DecodeError::FfmpegError(format!("failed to spawn decoder thread: {e}")))?;

    let extract_handle = thread::Builder::new()
        .name("stargaze-extract".to_string())
        .spawn(move || {
            ffmpeg::run_extract_loop(&raw_rx, &decoded_tx);
            info!("Extraction thread exiting");
        })
        .map_err(|e| DecodeError::FfmpegError(format!("failed to spawn extract thread: {e}")))?;

    // Wait for initialization to complete.
    let init_result = init_rx.recv().map_err(|_| {
        DecodeError::InitError("decoder thread exited during initialization".to_string())
    })?;

    // If init failed, join the thread and propagate the error.
    let hw_accel = init_result?;
    zero_copy.store(hw_accel && zero_copy_allowed(), Ordering::Relaxed);

    info!(hw_accel, "Decoder started on dedicated thread");

    Ok((
        DecoderSession {
            thread_handle: Some(thread_handle),
            extract_handle: Some(extract_handle),
            shutdown,
        },
        decoded_rx,
        zero_copy,
    ))
}

/// Whether dma-buf zero-copy delivery may be enabled for this machine.
///
/// `STARGAZE_NO_ZERO_COPY=1` disables it (diagnostic escape hatch) and
/// `STARGAZE_FORCE_ZERO_COPY=1` enables it even on blocklisted drivers.
///
/// nvidia-vaapi-driver is blocklisted: exporting its surfaces breaks
/// decoding (every subsequent `vaBeginPicture` fails with
/// `MAX_NUM_EXCEEDED`) and the exported planes read as zeros (verified
/// on driver 595.71.05).
fn zero_copy_allowed() -> bool {
    if std::env::var_os("STARGAZE_NO_ZERO_COPY").is_some_and(|v| v == "1") {
        info!("Zero-copy rendering disabled by STARGAZE_NO_ZERO_COPY");
        return false;
    }
    let nvidia = std::fs::read_link("/sys/class/drm/renderD128/device/driver")
        .ok()
        .and_then(|p| p.file_name().map(|n| n == "nvidia"))
        .unwrap_or(false);
    if nvidia && std::env::var_os("STARGAZE_FORCE_ZERO_COPY").is_none_or(|v| v != "1") {
        info!(
            "Zero-copy rendering disabled: nvidia-vaapi-driver dma-buf export \
             is broken (set STARGAZE_FORCE_ZERO_COPY=1 to try anyway)"
        );
        return false;
    }
    true
}

/// Handle to a running audio decoder session.
pub struct AudioDecoderSession {
    thread_handle: Option<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl AudioDecoderSession {
    /// Gracefully stops decoding and waits for the thread to exit.
    ///
    /// # Errors
    ///
    /// Returns `AudioError::DecoderInit` if the audio decoder thread panicked.
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
                AudioError::DecoderInit("audio decoder thread panicked".to_string())
            })?;
        }
        Ok(())
    }
}

impl Drop for AudioDecoderSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts the Opus audio decoder.
///
/// Spawns a dedicated thread that reads [`ReassembledFrame`]s from `frames_rx`,
/// decodes them with Opus, and sends decoded PCM samples to the returned receiver.
///
/// # Errors
///
/// Returns [`AudioError::DecoderInit`] if Opus initialization fails or the
/// thread cannot be spawned.
pub fn start_audio_decoder(
    config: AudioDecoderConfig,
    frames_rx: mpsc::Receiver<ReassembledFrame>,
) -> Result<(AudioDecoderSession, std::sync::mpsc::Receiver<Vec<f32>>), AudioError> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);
    let channels = config.channels;

    let (pcm_tx, pcm_rx) = std::sync::mpsc::channel::<Vec<f32>>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), AudioError>>();

    let thread_handle = thread::Builder::new()
        .name("stargaze-audio-decoder".to_string())
        .spawn(move || {
            let mut decoder = match opus_dec::init_opus_decoder(&config) {
                Ok(dec) => {
                    let _ = init_tx.send(Ok(()));
                    dec
                }
                Err(e) => {
                    error!("Audio decoder initialization failed: {e}");
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let mut frames_rx = frames_rx;

            if let Err(e) = opus_dec::run_opus_decode_loop(
                &mut decoder,
                &mut frames_rx,
                &pcm_tx,
                channels,
                &shutdown_clone,
            ) {
                error!("Audio decoder loop failed: {e}");
            }

            info!("Audio decoder thread exiting");
        })
        .map_err(|e| {
            AudioError::DecoderInit(format!("failed to spawn audio decoder thread: {e}"))
        })?;

    let init_result = init_rx.recv().map_err(|_| {
        AudioError::DecoderInit("audio decoder thread exited during initialization".to_string())
    })?;

    init_result?;

    info!("Audio decoder started on dedicated thread");

    Ok((
        AudioDecoderSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        pcm_rx,
    ))
}
