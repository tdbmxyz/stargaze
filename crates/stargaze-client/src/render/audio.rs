use anyhow::anyhow;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use tracing::info;

/// Desired SDL2 audio buffer size in samples.
///
/// 512 samples at 48 kHz ≈ 10.7 ms latency — matches Opus 10 ms frame size.
const AUDIO_BUFFER_SAMPLES: u16 = 512;

/// Queued bytes per millisecond of playback: 48 kHz stereo f32 PCM.
pub(super) const AUDIO_BYTES_PER_MS: u32 = 48 * 2 * 4;

/// Maximum decoded-audio backlog allowed in the SDL2 queue, in bytes (150 ms).
///
/// The queue plays out at exactly the rate PCM arrives, so it never
/// recovers from a backlog on its own: audio that piles up while the
/// video decoder and window initialize (~2 s), or that creeps in through
/// server/client clock drift, would lag behind the video forever.
/// When the backlog exceeds this cap the queue is cleared to resync
/// playback with the live edge.
pub(super) const MAX_QUEUED_AUDIO_BYTES: u32 = 150 * AUDIO_BYTES_PER_MS;

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
