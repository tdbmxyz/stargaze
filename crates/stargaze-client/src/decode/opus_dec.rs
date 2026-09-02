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

pub(crate) fn init_opus_decoder(
    config: &AudioDecoderConfig,
) -> Result<opus::MSDecoder, AudioError> {
    let layout = stargaze_core::audio::opus_channel_layout(config.channels)?;

    let decoder = opus::MSDecoder::new(
        config.sample_rate,
        layout.streams,
        layout.coupled_streams,
        &layout.mapping,
    )
    .map_err(|e| AudioError::DecoderInit(format!("opus_multistream_decoder_create failed: {e}")))?;

    info!(
        sample_rate = config.sample_rate,
        channels = config.channels,
        streams = layout.streams,
        "Opus multistream decoder initialized"
    );

    Ok(decoder)
}

#[allow(clippy::unnecessary_wraps)]
pub(crate) fn run_opus_decode_loop(
    decoder: &mut opus::MSDecoder,
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
        assert!(matches!(
            init_opus_decoder(&config),
            Err(AudioError::UnsupportedChannels(3))
        ));
    }

    /// Builds a multistream Opus encoder mirroring the server for `channels`.
    fn ms_encoder(channels: u16) -> opus::MSEncoder {
        let layout = stargaze_core::audio::opus_channel_layout(channels).unwrap();
        opus::MSEncoder::new(
            48000,
            layout.streams,
            layout.coupled_streams,
            &layout.mapping,
            opus::Application::Audio,
        )
        .unwrap()
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn opus_encode_decode_round_trip() {
        for &channels in &[2u16, 6, 8] {
            let mut encoder = ms_encoder(channels);
            let silence = vec![0.0_f32; OPUS_FRAME_SAMPLES * usize::from(channels)];
            let encoded = encoder.encode_vec_float(&silence, 8192).unwrap();
            assert!(!encoded.is_empty(), "Encoded packet should not be empty");

            let decoder_config = AudioDecoderConfig {
                sample_rate: 48000,
                channels,
            };
            let mut decoder = init_opus_decoder(&decoder_config).unwrap();
            let mut decoded = vec![0.0_f32; OPUS_FRAME_SAMPLES * usize::from(channels)];
            let samples_per_channel = decoder.decode_float(&encoded, &mut decoded, false).unwrap();

            assert_eq!(
                samples_per_channel, OPUS_FRAME_SAMPLES,
                "{channels}ch: expected {OPUS_FRAME_SAMPLES} samples/channel, got {samples_per_channel}"
            );
        }
    }

    /// The default one-coupled-stream packet must remain a regular stereo
    /// Opus packet so pre-surround clients can decode new stereo servers.
    #[test]
    fn multistream_stereo_packet_is_legacy_decoder_compatible() {
        let mut encoder = ms_encoder(2);
        let silence = vec![0.0_f32; OPUS_FRAME_SAMPLES * 2];
        let packet = encoder.encode_vec_float(&silence, 4000).unwrap();

        let mut legacy_decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut decoded = vec![0.0_f32; OPUS_FRAME_SAMPLES * 2];
        let samples_per_channel = legacy_decoder
            .decode_float(&packet, &mut decoded, false)
            .unwrap();

        assert_eq!(samples_per_channel, OPUS_FRAME_SAMPLES);
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn decode_loop_sends_pcm_to_channel() {
        let config = AudioDecoderConfig {
            sample_rate: 48000,
            channels: 2,
        };
        let mut decoder = init_opus_decoder(&config).unwrap();

        // Encode a test frame.
        let mut encoder = ms_encoder(2);
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
                    capture_us: 0,
                    convert_us: 0,
                    encode_us: 0,
                    received_at: std::time::Instant::now(),
                    tainted: false,
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
