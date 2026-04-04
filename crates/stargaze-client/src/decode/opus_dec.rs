//! Opus audio decoder internals.
//!
//! Handles decoder initialization and the synchronous decode loop.
//! All `opus` crate interaction is confined to this module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::audio::{AudioDecoderConfig, AudioError};
use stargaze_core::transport::ReassembledFrame;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Number of PCM samples per channel per Opus frame at 48 kHz (10 ms).
const OPUS_FRAME_SAMPLES: usize = 480;

pub(crate) fn init_opus_decoder(config: &AudioDecoderConfig) -> Result<opus::Decoder, AudioError> {
    let channels = match config.channels {
        1 => opus::Channels::Mono,
        2 => opus::Channels::Stereo,
        n => {
            return Err(AudioError::DecoderInit(format!(
                "unsupported channel count {n}: Opus supports only 1 or 2 channels"
            )));
        }
    };

    let decoder = opus::Decoder::new(config.sample_rate, channels)
        .map_err(|e| AudioError::DecoderInit(format!("opus_decoder_create failed: {e}")))?;

    info!(
        sample_rate = config.sample_rate,
        channels = config.channels,
        "Opus decoder initialized"
    );

    Ok(decoder)
}

#[allow(clippy::unnecessary_wraps)]
pub(crate) fn run_opus_decode_loop(
    decoder: &mut opus::Decoder,
    frames_rx: &mut mpsc::Receiver<ReassembledFrame>,
    pcm_tx: &std::sync::mpsc::Sender<Vec<f32>>,
    channels: u16,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), AudioError> {
    let max_samples = OPUS_FRAME_SAMPLES * usize::from(channels);
    let mut output_buf = vec![0.0_f32; max_samples];
    let mut frame_counter: u64 = 0;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            debug!("Audio decoder shutdown signaled");
            break;
        }

        let Some(frame) = frames_rx.blocking_recv() else {
            info!("Audio frame channel closed, stopping decoder");
            break;
        };

        if shutdown.load(Ordering::Relaxed) {
            debug!("Audio decoder shutdown signaled after recv");
            break;
        }

        let samples_per_channel = match decoder.decode_float(&frame.data, &mut output_buf, false) {
            Ok(n) => n,
            Err(e) => {
                warn!(
                    frame = frame_counter,
                    "Opus decode error: {e}, skipping frame"
                );
                frame_counter += 1;
                continue;
            }
        };

        let total_samples = samples_per_channel * usize::from(channels);
        let pcm = output_buf[..total_samples].to_vec();

        if pcm_tx.send(pcm).is_err() {
            info!("Audio PCM receiver dropped, stopping decoder");
            break;
        }

        frame_counter += 1;
    }

    info!(total_frames = frame_counter, "Opus decoder loop finished");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stargaze_core::audio::AudioDecoderConfig;

    #[test]
    fn decoder_init_stereo_succeeds() {
        let config = AudioDecoderConfig {
            sample_rate: 48000,
            channels: 2,
        };
        assert!(init_opus_decoder(&config).is_ok());
    }

    #[test]
    fn decoder_init_mono_succeeds() {
        let config = AudioDecoderConfig {
            sample_rate: 48000,
            channels: 1,
        };
        assert!(init_opus_decoder(&config).is_ok());
    }

    #[test]
    fn decoder_init_rejects_invalid_channels() {
        let config = AudioDecoderConfig {
            sample_rate: 48000,
            channels: 3,
        };
        let result = init_opus_decoder(&config);
        assert!(result.is_err());
        match result {
            Err(AudioError::DecoderInit(msg)) => {
                assert!(msg.contains("unsupported channel count 3"));
            }
            other => panic!("Expected DecoderInit error, got: {other:?}"),
        }
    }

    #[test]
    fn opus_encode_decode_round_trip() {
        let encoder_config = stargaze_core::audio::AudioEncoderConfig {
            sample_rate: 48000,
            channels: 2,
            bitrate: 128_000,
            application: stargaze_core::audio::AudioApplication::Audio,
        };

        let mut encoder =
            opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        encoder
            .set_bitrate(opus::Bitrate::Bits(
                i32::try_from(encoder_config.bitrate).unwrap(),
            ))
            .unwrap();

        let silence = vec![0.0_f32; OPUS_FRAME_SAMPLES * 2];
        let mut encoded = vec![0u8; 4000];
        let encoded_len = encoder.encode_float(&silence, &mut encoded).unwrap();
        assert!(encoded_len > 0, "Encoded packet should not be empty");

        let decoder_config = AudioDecoderConfig {
            sample_rate: 48000,
            channels: 2,
        };
        let mut decoder = init_opus_decoder(&decoder_config).unwrap();
        let mut decoded = vec![0.0_f32; OPUS_FRAME_SAMPLES * 2];
        let samples_per_channel = decoder
            .decode_float(&encoded[..encoded_len], &mut decoded, false)
            .unwrap();

        assert_eq!(
            samples_per_channel, OPUS_FRAME_SAMPLES,
            "Expected {OPUS_FRAME_SAMPLES} samples per channel, got {samples_per_channel}"
        );
    }

    #[test]
    fn decode_loop_sends_pcm_to_channel() {
        let config = AudioDecoderConfig {
            sample_rate: 48000,
            channels: 2,
        };
        let mut decoder = init_opus_decoder(&config).unwrap();

        // Encode a test frame.
        let mut encoder =
            opus::Encoder::new(48000, opus::Channels::Stereo, opus::Application::Audio).unwrap();
        let silence = vec![0.0_f32; OPUS_FRAME_SAMPLES * 2];
        let mut encoded = vec![0u8; 4000];
        let encoded_len = encoder.encode_float(&silence, &mut encoded).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (frame_tx, mut frame_rx) = mpsc::channel::<ReassembledFrame>(4);
        let (pcm_tx, pcm_rx) = std::sync::mpsc::channel::<Vec<f32>>();
        let shutdown = Arc::new(AtomicBool::new(false));

        rt.block_on(async {
            frame_tx
                .send(ReassembledFrame {
                    stream_type: stargaze_core::transport::STREAM_TYPE_AUDIO,
                    pts: 0,
                    is_keyframe: false,
                    data: encoded[..encoded_len].to_vec(),
                })
                .await
                .unwrap();
        });

        // Drop sender so channel closes after one frame, causing the loop to exit.
        drop(frame_tx);

        let result = run_opus_decode_loop(&mut decoder, &mut frame_rx, &pcm_tx, 2, &shutdown);
        assert!(result.is_ok());

        let pcm = pcm_rx.try_recv().unwrap();
        assert_eq!(pcm.len(), OPUS_FRAME_SAMPLES * 2);
    }
}
