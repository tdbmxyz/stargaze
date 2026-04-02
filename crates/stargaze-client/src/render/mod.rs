//! Video rendering module — public API.
mod sdl;

use stargaze_core::decode::{DecodedFrame, DecoderConfig};

/// Starts the video renderer on the calling thread.
///
/// Takes over the calling thread to run the SDL2 event loop.
/// Returns when the window is closed or an error occurs.
///
/// # Errors
///
/// Returns an error if SDL2 initialization, window creation, or rendering fails.
pub fn start_renderer(
    config: &DecoderConfig,
    decoded_rx: std::sync::mpsc::Receiver<DecodedFrame>,
    fullscreen: bool,
) -> Result<(), anyhow::Error> {
    sdl::run_sdl_loop(config, decoded_rx, fullscreen)
}
