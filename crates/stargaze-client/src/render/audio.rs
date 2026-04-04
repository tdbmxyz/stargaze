use anyhow::anyhow;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use tracing::info;

/// Desired SDL2 audio buffer size in samples.
///
/// 512 samples at 48 kHz ≈ 10.7 ms latency — matches Opus 10 ms frame size.
const AUDIO_BUFFER_SAMPLES: u16 = 512;

/// Creates and starts an SDL2 audio playback queue.
///
/// # Errors
///
/// Returns an error if the SDL2 audio subsystem or device fails to open.
pub(super) fn create_audio_queue(sdl: &sdl2::Sdl) -> Result<AudioQueue<f32>, anyhow::Error> {
    let audio_subsystem = sdl
        .audio()
        .map_err(|e| anyhow!("SDL2 audio subsystem init failed: {e}"))?;

    let desired_spec = AudioSpecDesired {
        freq: Some(48_000),
        channels: Some(2),
        samples: Some(AUDIO_BUFFER_SAMPLES),
    };

    let queue = audio_subsystem
        .open_queue::<f32, _>(None, &desired_spec)
        .map_err(|e| anyhow!("SDL2 audio queue open failed: {e}"))?;

    queue.resume();

    info!(
        freq = 48_000,
        channels = 2,
        buffer_samples = AUDIO_BUFFER_SAMPLES,
        "SDL2 audio queue started"
    );

    Ok(queue)
}
