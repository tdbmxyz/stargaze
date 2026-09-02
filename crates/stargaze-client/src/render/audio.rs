use anyhow::anyhow;
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use tracing::info;

/// Desired SDL2 audio buffer size in samples.
///
/// 512 samples at 48 kHz ≈ 10.7 ms latency — matches Opus 10 ms frame size.
const AUDIO_BUFFER_SAMPLES: u16 = 512;

/// Queued bytes per millisecond of playback for `channels` of 48 kHz f32 PCM.
pub(super) fn audio_bytes_per_ms(channels: u16) -> u32 {
    48 * u32::from(channels) * 4
}

/// Maximum decoded-audio backlog allowed in the SDL2 queue, in bytes (150 ms).
///
/// The queue plays out at exactly the rate PCM arrives, so it never
/// recovers from a backlog on its own: audio that piles up while the
/// video decoder and window initialize (~2 s), or that creeps in through
/// server/client clock drift, would lag behind the video forever.
/// When the backlog exceeds this cap the queue is cleared to resync
/// playback with the live edge.
pub(super) fn max_queued_audio_bytes(channels: u16) -> u32 {
    150 * audio_bytes_per_ms(channels)
}

/// Creates and starts an SDL2 audio playback queue for `channels`.
///
/// `open_queue` passes `allowed_changes = 0` to `SDL_OpenAudioDevice`, so SDL
/// always accepts the requested layout and converts internally when the
/// physical device differs — queuing 5.1/7.1 PCM works (downmixed by SDL) even
/// on stereo-only client hardware.
///
/// # Errors
///
/// Returns an error if the SDL2 audio subsystem or device fails to open.
pub(super) fn create_audio_queue(
    sdl: &sdl2::Sdl,
    channels: u16,
) -> Result<AudioQueue<f32>, anyhow::Error> {
    let audio_subsystem = sdl
        .audio()
        .map_err(|e| anyhow!("SDL2 audio subsystem init failed: {e}"))?;

    let sdl_channels = u8::try_from(channels)
        .map_err(|_| anyhow!("unsupported audio channel count {channels}"))?;
    let desired_spec = AudioSpecDesired {
        freq: Some(48_000),
        channels: Some(sdl_channels),
        samples: Some(AUDIO_BUFFER_SAMPLES),
    };

    let queue = audio_subsystem
        .open_queue::<f32, _>(None, &desired_spec)
        .map_err(|e| anyhow!("SDL2 audio queue open failed: {e}"))?;

    queue.resume();

    info!(
        freq = 48_000,
        channels = sdl_channels,
        buffer_samples = AUDIO_BUFFER_SAMPLES,
        "SDL2 audio queue started"
    );

    Ok(queue)
}
