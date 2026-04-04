pub(crate) mod uinput;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use stargaze_core::input::InputEvent;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::input::uinput::InputError;

pub struct InputSession {
    thread_handle: Option<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl InputSession {
    /// # Errors
    ///
    /// Returns `InputError` if the injection thread panicked.
    pub fn stop(mut self) -> Result<(), InputError> {
        self.signal_shutdown();
        self.join_thread()
    }

    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn join_thread(&mut self) -> Result<(), InputError> {
        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| InputError::ThreadPanic)?;
        }
        Ok(())
    }
}

impl Drop for InputSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// # Errors
///
/// Returns `InputError` if the injection thread cannot be spawned or
/// virtual device initialization fails.
pub fn start_input_injection(
    input_rx: mpsc::Receiver<InputEvent>,
) -> Result<InputSession, InputError> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), InputError>>();

    let thread_handle = thread::Builder::new()
        .name("stargaze-input".to_string())
        .spawn(move || {
            match uinput::create_virtual_devices() {
                Ok(devices) => {
                    let _ = init_tx.send(Ok(()));
                    if let Err(e) = uinput::run_injection_loop(devices, input_rx, &shutdown_clone) {
                        error!("Input injection loop failed: {e}");
                    }
                }
                Err(e) => {
                    error!("Failed to create virtual devices: {e}");
                    let _ = init_tx.send(Err(e));
                }
            }
            info!("Input injection thread exiting");
        })
        .map_err(|e| InputError::SpawnFailed(e.to_string()))?;

    let init_result = init_rx.recv().map_err(|_| {
        InputError::SpawnFailed("input thread exited during initialization".to_string())
    })?;
    init_result?;

    info!("Input injection started on dedicated thread");

    Ok(InputSession {
        thread_handle: Some(thread_handle),
        shutdown,
    })
}
