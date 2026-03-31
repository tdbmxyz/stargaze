use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use stargaze_core::capture::{CaptureError, Frame};
use tokio::sync::mpsc;

use super::CaptureConfig;

/// Runs the `PipeWire` capture stream on the current thread (blocking).
///
/// This function blocks until the shutdown signal is set or an error occurs.
pub fn run_capture_stream(
    _pw_fd: OwnedFd,
    _pw_node_id: u32,
    _config: CaptureConfig,
    _tx: mpsc::Sender<Frame>,
    _shutdown: Arc<AtomicBool>,
) -> Result<(), CaptureError> {
    todo!("PipeWire capture stream — implemented in Task 5")
}
