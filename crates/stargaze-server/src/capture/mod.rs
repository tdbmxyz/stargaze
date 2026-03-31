pub mod pipewire;
pub mod portal;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use stargaze_core::capture::{CaptureError, Frame};
use tokio::sync::mpsc;
use tracing::{error, info};

/// Channel capacity for frame delivery (provides backpressure).
const FRAME_CHANNEL_CAPACITY: usize = 2;

/// Configuration for the capture session.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Desired capture width in pixels.
    pub width: u32,
    /// Desired capture height in pixels.
    pub height: u32,
    /// Desired capture framerate.
    pub framerate: u32,
}

/// Handle to a running capture session.
///
/// Signals the `PipeWire` thread to shut down on drop.
/// The caller must keep this alive for the duration of capture.
pub struct CaptureSession {
    /// Join handle for the dedicated `PipeWire` thread.
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Shared flag to signal the `PipeWire` thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl CaptureSession {
    /// Gracefully stops the capture session and waits for the `PipeWire` thread to exit.
    ///
    /// # Errors
    ///
    /// Returns `CaptureError::PipeWireError` if the `PipeWire` thread panicked.
    pub fn stop(mut self) -> Result<(), CaptureError> {
        self.signal_shutdown();
        self.join_thread()
    }

    /// Signals the `PipeWire` thread to shut down.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Joins the `PipeWire` thread, returning any error.
    fn join_thread(&mut self) -> Result<(), CaptureError> {
        if let Some(handle) = self.thread_handle.take() {
            handle
                .join()
                .map_err(|_| CaptureError::PipeWireError("PipeWire thread panicked".to_string()))?;
        }
        Ok(())
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        // Best-effort join — don't propagate errors from drop.
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts screen capture via xdg-desktop-portal and `PipeWire`.
///
/// Performs the portal handshake asynchronously (D-Bus), then spawns a
/// dedicated thread for the `PipeWire` main loop. Returns a session handle
/// and a channel receiver that yields captured frames.
///
/// # Errors
///
/// Returns `CaptureError::PortalError` if the portal session fails.
/// Returns `CaptureError::PipeWireError` if the `PipeWire` connection fails.
pub async fn start_capture(
    config: CaptureConfig,
) -> Result<(CaptureSession, mpsc::Receiver<Frame>), CaptureError> {
    // Step 1: Portal handshake (async, runs on tokio).
    let (pw_fd, pw_node_id) = portal::create_screencast_session().await?;

    info!(
        node_id = pw_node_id,
        "Portal session established, starting PipeWire capture"
    );

    // Step 2: Create the frame channel.
    let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Step 3: Spawn the dedicated PipeWire thread.
    let thread_handle = thread::Builder::new()
        .name("stargaze-pipewire".to_string())
        .spawn(move || {
            if let Err(e) =
                pipewire::run_capture_stream(pw_fd, pw_node_id, config, tx, shutdown_clone)
            {
                error!("PipeWire capture stream failed: {e}");
            }
        })
        .map_err(|e| CaptureError::PipeWireError(format!("failed to spawn thread: {e}")))?;

    Ok((
        CaptureSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        rx,
    ))
}
