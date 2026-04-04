//! Opus audio encoder internals.
//!
//! Handles encoder initialization and the synchronous encode loop.
//! All `opus` crate interaction is confined to this module.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::audio::{AudioApplication, AudioEncoderConfig, AudioError, AudioFrame};
use stargaze_core::encode::EncodedPacket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Number of PCM samples per channel per Opus frame at 48 kHz (10 ms).
const OPUS_FRAME_SAMPLES: usize = 480;

/// Maximum encoded Opus packet size in bytes.
const OPUS_MAX_PACKET_SIZE: usize = 4000;

/// Initializes the Opus encoder from the given configuration.
///
/// # Errors
///
/// Returns [`AudioError::EncoderInit`] if the encoder cannot be created or
/// configured (unsupported channel count, invalid sample rate, etc.).
pub(crate) fn init_opus_encoder(config: &AudioEncoderConfig) -> Result<opus::Encoder, AudioError> {
    let channels = match config.channels {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        n => {
            return Err(AudioError::EncoderInit(format!(
                "unsupported channel count {n}: Opus supports only 1 or 2 channels"
            )));
        }
    };

    let application = match config.application {
        AudioApplication::Audio => opus::Application::Audio,
        AudioApplication::Voip => opus::Application::Voip,
        AudioApplication::LowDelay => opus::Application::LowDelay,
    };

    let mut encoder = opus::Encoder::new(config.sample_rate, channels, application)
        .map_err(|e| AudioError::EncoderInit(format!("opus_encoder_create failed: {e}")))?;

    encoder
        .set_bitrate(opus::Bitrate::Bits(
            i32::try_from(config.bitrate).unwrap_or(i32::MAX),
        ))
        .map_err(|e| AudioError::EncoderInit(format!("set_bitrate failed: {e}")))?;

    encoder
        .set_complexity(5)
        .map_err(|e| AudioError::EncoderInit(format!("set_complexity failed: {e}")))?;

    encoder
        .set_vbr(true)
        .map_err(|e| AudioError::EncoderInit(format!("set_vbr failed: {e}")))?;

    info!(
        sample_rate = config.sample_rate,
        channels = config.channels,
        bitrate = config.bitrate,
        "Opus encoder initialized"
    );

    Ok(encoder)
}

/// Runs the Opus encode loop: receives [`AudioFrame`]s, encodes them, sends [`EncodedPacket`]s.
///
/// `PipeWire` delivers audio buffers of arbitrary size, but Opus requires
/// exactly `OPUS_FRAME_SAMPLES` samples per channel per encode call.  This
/// loop accumulates incoming PCM data in a ring buffer and drains it in
/// correctly-sized chunks so no audio is dropped.
///
/// Blocks until `shutdown` is signaled or the input channel closes.
/// Meant to run on a dedicated [`std::thread`].
///
/// # Errors
///
/// Returns [`AudioError::EncodeFailed`] if a fatal encode error occurs.
/// Non-fatal per-frame errors are logged and skipped.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn run_opus_encode_loop(
    encoder: &mut opus::Encoder,
    frames: &mut mpsc::Receiver<AudioFrame>,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), AudioError> {
    let mut output_buf = vec![0u8; OPUS_MAX_PACKET_SIZE];
    let mut frame_counter: u64 = 0;
    let mut sample_buf: VecDeque<f32> = VecDeque::new();
    let mut samples_consumed: u64 = 0;
    let mut channels: u16 = 0;

    loop {
        // Check shutdown flag before blocking.
        if shutdown.load(Ordering::Relaxed) {
            debug!("Audio encoder shutdown signaled");
            break;
        }

        // Blocking receive from the audio capture channel.
        let Some(frame) = frames.blocking_recv() else {
            info!("Audio frame channel closed, stopping encoder");
            break;
        };

        // Re-check shutdown after waking from blocking_recv.
        if shutdown.load(Ordering::Relaxed) {
            debug!("Audio encoder shutdown signaled after recv");
            break;
        }

        if channels == 0 {
            channels = frame.channels;
        }

        sample_buf.extend(&frame.data);

        let chunk_size = OPUS_FRAME_SAMPLES * usize::from(channels);

        while sample_buf.len() >= chunk_size {
            let chunk: Vec<f32> = sample_buf.drain(..chunk_size).collect();

            let pts = samples_consumed / u64::from(channels);

            let len = match encoder.encode_float(&chunk, &mut output_buf) {
                Ok(n) => n,
                Err(e) => {
                    warn!(
                        frame = frame_counter,
                        "Opus encode error: {e}, skipping frame"
                    );
                    frame_counter += 1;
                    samples_consumed += chunk_size as u64;
                    continue;
                }
            };

            let packet = EncodedPacket {
                data: output_buf[..len].to_vec(),
                pts,
                is_keyframe: false,
            };

            if packets_tx.blocking_send(packet).is_err() {
                debug!("Audio packet receiver dropped, stopping encoder");
                return Ok(());
            }

            frame_counter += 1;
            samples_consumed += chunk_size as u64;
        }
    }

    info!(total_frames = frame_counter, "Opus encoder loop finished");
    Ok(())
}
