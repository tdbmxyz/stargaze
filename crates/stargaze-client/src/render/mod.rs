mod audio;
mod sdl;

use stargaze_core::decode::{DecodedFrame, DecoderConfig};
use stargaze_core::input::InputEvent;

/// # Errors
///
/// Returns an error if SDL2 initialization, window creation, or rendering fails.
pub fn start_renderer(
    sdl: &sdl2::Sdl,
    config: &DecoderConfig,
    decoded_rx: std::sync::mpsc::Receiver<DecodedFrame>,
    audio_pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    fullscreen: bool,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
) -> Result<(), anyhow::Error> {
    sdl::run_sdl_loop(sdl, config, decoded_rx, audio_pcm_rx, fullscreen, input_tx)
}
