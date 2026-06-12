mod audio;
mod gl;
mod input;
mod sdl;
mod stats;

use stargaze_core::decode::DecoderConfig;
use stargaze_core::input::InputEvent;

use crate::decode::VideoFrame;

/// Callback returning the current network round-trip time estimate,
/// used by the stats overlay.
pub type RttProbe = Box<dyn Fn() -> std::time::Duration + Send>;

/// Sanitized server/client command lines, recorded in the session report.
pub struct SessionCommands {
    /// The server's command line (received in the session handshake).
    pub server: String,
    /// This client's command line.
    pub client: String,
}

/// # Errors
///
/// Returns an error if SDL2 initialization, window creation, or rendering fails.
#[allow(clippy::too_many_arguments)]
pub fn start_renderer(
    sdl: &sdl2::Sdl,
    config: &DecoderConfig,
    decoded_rx: std::sync::mpsc::Receiver<VideoFrame>,
    audio_pcm_rx: std::sync::mpsc::Receiver<Vec<f32>>,
    fullscreen: bool,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
    rtt_probe: RttProbe,
    net_stats: std::sync::Arc<crate::transport::NetStats>,
    stats_file: Option<std::path::PathBuf>,
    commands: &SessionCommands,
    zero_copy: &std::sync::atomic::AtomicBool,
) -> Result<(), anyhow::Error> {
    sdl::run_sdl_loop(
        sdl,
        config,
        decoded_rx,
        audio_pcm_rx,
        fullscreen,
        input_tx,
        rtt_probe,
        &net_stats,
        stats_file.as_deref(),
        commands,
        zero_copy,
    )
}
