pub(crate) mod pipewire_audio;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use stargaze_core::audio::{AudioCaptureConfig, AudioError, AudioFrame};
use tokio::sync::mpsc;
use tracing::{error, info};

/// Channel capacity for audio frame delivery (provides backpressure).
const AUDIO_FRAME_CHANNEL_CAPACITY: usize = 8;

/// Handle to a running audio capture session.
///
/// Signals the `PipeWire` thread to shut down on drop.
/// The caller must keep this alive for the duration of audio capture.
pub struct AudioCaptureSession {
    /// Join handle for the dedicated `PipeWire` audio thread.
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Shared flag to signal the `PipeWire` thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl AudioCaptureSession {
    /// Gracefully stops the audio capture session and waits for the `PipeWire` thread to exit.
    ///
    /// # Errors
    ///
    /// Returns `AudioError::CaptureStream` if the `PipeWire` thread panicked.
    pub fn stop(mut self) -> Result<(), AudioError> {
        self.signal_shutdown();
        self.join_thread()
    }

    /// Signals the `PipeWire` audio thread to shut down.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Joins the `PipeWire` audio thread, returning any error.
    fn join_thread(&mut self) -> Result<(), AudioError> {
        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| {
                AudioError::CaptureStream("PipeWire audio thread panicked".to_string())
            })?;
        }
        Ok(())
    }
}

impl Drop for AudioCaptureSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        // Best-effort join — don't propagate errors from drop.
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts audio capture via `PipeWire` (sink monitor).
///
/// Spawns a dedicated thread for the `PipeWire` main loop. Returns a session
/// handle and a channel receiver that yields captured audio frames.
///
/// # Errors
///
/// Returns `AudioError::CaptureInit` if the `PipeWire` connection or stream setup fails.
/// Returns `AudioError::CaptureStream` if the dedicated thread cannot be spawned.
pub fn start_audio_capture(
    config: AudioCaptureConfig,
) -> Result<(AudioCaptureSession, mpsc::Receiver<AudioFrame>), AudioError> {
    // Step 1: Create the audio frame channel.
    let (frames_tx, frames_rx) = mpsc::channel(AUDIO_FRAME_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Step 2: Create a one-shot channel to receive init errors from the thread.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), AudioError>>();

    // Step 3: Spawn the dedicated PipeWire audio thread.
    let thread_handle = thread::Builder::new()
        .name("stargaze-audio-capture".to_string())
        .spawn(move || {
            if let Err(e) =
                pipewire_audio::run_audio_capture(&config, frames_tx, shutdown_clone, init_tx)
            {
                error!("PipeWire audio capture failed: {e}");
            }
        })
        .map_err(|e| AudioError::CaptureStream(format!("failed to spawn audio thread: {e}")))?;

    // Step 4: Wait for the init result from the thread.
    match init_rx.recv() {
        Ok(Ok(())) => {
            info!("PipeWire audio capture initialized");
        }
        Ok(Err(e)) => {
            // Thread reported an init error — join it and propagate.
            let _ = thread_handle.join();
            return Err(e);
        }
        Err(_) => {
            // Channel closed without sending — thread may have panicked.
            let _ = thread_handle.join();
            return Err(AudioError::CaptureInit(
                "audio capture thread failed to initialize".to_string(),
            ));
        }
    }

    Ok((
        AudioCaptureSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        frames_rx,
    ))
}
