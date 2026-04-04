//! Video and audio rendering module — public API.
mod audio;
mod sdl;

use stargaze_core::decode::{DecodedFrame, DecoderConfig};

/// Starts the video and audio renderer on the calling thread.
///
/// # Errors
///
/// Returns an error if SDL2 initialization, window creation, or rendering fails.
pub fn start_renderer(
    sdl: &sdl2::Sdl,
    config: &DecoderConfig,
    decoded_rx: std::sync::mpsc::Receiver<DecodedFrame>,
    audio_pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    fullscreen: bool,
) -> Result<(), anyhow::Error> {
    sdl::run_sdl_loop(sdl, config, decoded_rx, audio_pcm_rx, fullscreen)
}
