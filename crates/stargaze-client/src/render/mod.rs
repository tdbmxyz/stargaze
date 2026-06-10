mod audio;
mod input;
mod sdl;
mod stats;

use stargaze_core::decode::{DecodedFrame, DecoderConfig};
use stargaze_core::input::InputEvent;

/// Callback returning the current network round-trip time estimate,
/// used by the stats overlay.
pub type RttProbe = Box<dyn Fn() -> std::time::Duration + Send>;

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
    rtt_probe: RttProbe,
) -> Result<(), anyhow::Error> {
    sdl::run_sdl_loop(
        sdl,
        config,
        decoded_rx,
        audio_pcm_rx,
        fullscreen,
        input_tx,
        rtt_probe,
    )
}
