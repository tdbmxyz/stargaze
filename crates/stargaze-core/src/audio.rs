use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Raw audio frame from capture — interleaved f32 PCM samples.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Interleaved f32 PCM samples in SPA/WAV channel order
    /// (L0, R0, L1, R1, ... for stereo).
    pub data: Vec<f32>,
    /// Sample rate in Hz (expected: 48000).
    pub sample_rate: u32,
    /// Number of channels (1, 2, 6, or 8).
    pub channels: u16,
    /// Presentation timestamp (monotonic frame counter).
    pub pts: u64,
}

/// The Opus multistream layout for a given channel count.
///
/// Opus encodes a multichannel signal as a set of independent Opus streams,
/// some coupled (stereo) and some uncoupled (mono), plus a `mapping` table
/// that assigns each interleaved input/output channel to one encoded channel
/// slot. Stargaze controls both ends of the wire, so it uses a custom mapping
/// keyed to the SPA/WAV channel order (see [`opus_channel_layout`]) rather than
/// Opus channel mapping family 1. Only the channel count travels over the wire;
/// both peers derive the identical layout from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusChannelLayout {
    /// Total number of Opus streams (coupled + uncoupled).
    pub streams: u8,
    /// Number of those streams that are coupled (stereo).
    pub coupled_streams: u8,
    /// Per-channel mapping table (`mapping.len() == channels`): the encoded
    /// channel index that carries interleaved channel `i`.
    pub mapping: Vec<u8>,
}

/// Returns the Opus multistream layout for a supported channel count.
///
/// Supported counts are 1 (mono), 2 (stereo), 6 (5.1), and 8 (7.1). The
/// mappings couple the natural front/rear/side L-R pairs into stereo Opus
/// streams and carry front-center and LFE as mono streams, in the SPA/WAV
/// interleave order documented in `docs/surround-audio.md`.
///
/// # Errors
///
/// Returns [`AudioError::UnsupportedChannels`] for any other channel count.
pub fn opus_channel_layout(channels: u16) -> Result<OpusChannelLayout, AudioError> {
    // mapping[input_channel] = encoded_channel_index. Coupled streams occupy
    // two consecutive encoded slots each (in stream order), uncoupled streams
    // one slot each after the coupled ones.
    let (streams, coupled_streams, mapping): (u8, u8, &[u8]) = match channels {
        // MONO -> one mono stream.
        1 => (1, 0, &[0]),
        // FL FR -> one coupled stereo stream.
        2 => (1, 1, &[0, 1]),
        // FL FR FC LFE RL RR: couple (FL,FR) and (RL,RR); FC, LFE mono.
        6 => (4, 2, &[0, 1, 4, 5, 2, 3]),
        // FL FR FC LFE RL RR SL SR: couple (FL,FR),(RL,RR),(SL,SR); FC, LFE mono.
        8 => (5, 3, &[0, 1, 6, 7, 2, 3, 4, 5]),
        n => return Err(AudioError::UnsupportedChannels(n)),
    };
    Ok(OpusChannelLayout {
        streams,
        coupled_streams,
        mapping: mapping.to_vec(),
    })
}

/// Configuration for audio capture.
#[derive(Debug, Clone)]
pub struct AudioCaptureConfig {
    /// Target sample rate in Hz (e.g. 48000).
    pub sample_rate: u32,
    /// Number of channels (1, 2, 6, or 8).
    pub channels: u16,
}

/// Opus application mode controlling encoder tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioApplication {
    /// General audio (music, game sounds, mixed content).
    Audio,
    /// Voice-optimized (speech-heavy content).
    Voip,
    /// Ultra-low latency (sacrifices quality for speed).
    LowDelay,
}

/// Configuration for the Opus audio decoder.
#[derive(Debug, Clone)]
pub struct AudioDecoderConfig {
    /// Sample rate in Hz (must be 48000 for Opus).
    pub sample_rate: u32,
    /// Number of channels (1, 2, 6, or 8 — see [`opus_channel_layout`]).
    pub channels: u16,
}

/// Configuration for the Opus audio encoder.
#[derive(Debug, Clone)]
pub struct AudioEncoderConfig {
    /// Sample rate in Hz (must be 48000 for Opus).
    pub sample_rate: u32,
    /// Number of channels (1, 2, 6, or 8 — see [`opus_channel_layout`]).
    pub channels: u16,
    /// Target bitrate in bits per second (e.g. 128000).
    pub bitrate: u32,
    /// Opus application mode.
    pub application: AudioApplication,
}

/// Errors from the audio subsystem (capture and encoding).
#[derive(Error, Debug)]
pub enum AudioError {
    #[error("audio capture initialization failed: {0}")]
    CaptureInit(String),

    #[error("audio capture stream error: {0}")]
    CaptureStream(String),

    #[error("audio encoder initialization failed: {0}")]
    EncoderInit(String),

    #[error("audio encoding failed: {0}")]
    EncodeFailed(String),

    #[error("audio decoder initialization failed: {0}")]
    DecoderInit(String),

    #[error("audio decoding failed: {0}")]
    DecodeFailed(String),

    #[error("audio channel closed: {0}")]
    ChannelClosed(String),

    #[error("unsupported channel count {0}: only 1, 2, 6, or 8 are supported")]
    UnsupportedChannels(u16),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frame_construction() {
        let frame = AudioFrame {
            data: vec![0.0_f32; 960],
            sample_rate: 48000,
            channels: 2,
            pts: 42,
        };
        assert_eq!(frame.data.len(), 960);
        assert_eq!(frame.sample_rate, 48000);
        assert_eq!(frame.channels, 2);
        assert_eq!(frame.pts, 42);
    }

    #[test]
    fn audio_capture_config_construction() {
        let cfg = AudioCaptureConfig {
            sample_rate: 48000,
            channels: 2,
        };
        assert_eq!(cfg.sample_rate, 48000);
        assert_eq!(cfg.channels, 2);
    }

    #[test]
    fn audio_encoder_config_construction() {
        let cfg = AudioEncoderConfig {
            sample_rate: 48000,
            channels: 2,
            bitrate: 128_000,
            application: AudioApplication::Audio,
        };
        assert_eq!(cfg.sample_rate, 48000);
        assert_eq!(cfg.channels, 2);
        assert_eq!(cfg.bitrate, 128_000);
        assert_eq!(cfg.application, AudioApplication::Audio);
    }

    #[test]
    fn audio_error_display() {
        let err = AudioError::CaptureInit("PipeWire not available".to_string());
        assert_eq!(
            err.to_string(),
            "audio capture initialization failed: PipeWire not available"
        );

        let err = AudioError::CaptureStream("buffer overrun".to_string());
        assert_eq!(
            err.to_string(),
            "audio capture stream error: buffer overrun"
        );

        let err = AudioError::EncoderInit("invalid sample rate".to_string());
        assert_eq!(
            err.to_string(),
            "audio encoder initialization failed: invalid sample rate"
        );

        let err = AudioError::EncodeFailed("frame too short".to_string());
        assert_eq!(err.to_string(), "audio encoding failed: frame too short");

        let err = AudioError::ChannelClosed("receiver dropped".to_string());
        assert_eq!(err.to_string(), "audio channel closed: receiver dropped");
    }

    #[test]
    fn audio_application_variants() {
        assert_ne!(AudioApplication::Audio, AudioApplication::Voip);
        assert_ne!(AudioApplication::Voip, AudioApplication::LowDelay);
        assert_ne!(AudioApplication::Audio, AudioApplication::LowDelay);
    }

    #[test]
    fn audio_decoder_config_construction() {
        let cfg = AudioDecoderConfig {
            sample_rate: 48000,
            channels: 2,
        };
        assert_eq!(cfg.sample_rate, 48000);
        assert_eq!(cfg.channels, 2);
    }

    #[test]
    fn opus_layout_supported_counts() {
        // Encoded-channel-slot count implied by the layout must equal the
        // channel count: coupled streams contribute 2 slots, uncoupled 1.
        for &ch in &[1u16, 2, 6, 8] {
            let layout = opus_channel_layout(ch).unwrap();
            assert_eq!(layout.mapping.len(), usize::from(ch));
            let slots = usize::from(layout.coupled_streams) * 2
                + usize::from(layout.streams - layout.coupled_streams);
            assert_eq!(slots, usize::from(ch), "slot count for {ch} channels");
            // The mapping must be a permutation of 0..slots (bijective).
            let mut seen = layout.mapping.clone();
            seen.sort_unstable();
            let expected: Vec<u8> = (0..u8::try_from(slots).unwrap()).collect();
            assert_eq!(seen, expected, "mapping for {ch} channels is a permutation");
        }
    }

    #[test]
    fn opus_layout_specific_tables() {
        assert_eq!(opus_channel_layout(2).unwrap().mapping, vec![0, 1]);
        assert_eq!(
            opus_channel_layout(6).unwrap().mapping,
            vec![0, 1, 4, 5, 2, 3]
        );
        assert_eq!(
            opus_channel_layout(8).unwrap().mapping,
            vec![0, 1, 6, 7, 2, 3, 4, 5]
        );
        assert_eq!(opus_channel_layout(6).unwrap().streams, 4);
        assert_eq!(opus_channel_layout(6).unwrap().coupled_streams, 2);
    }

    #[test]
    fn opus_layout_rejects_unsupported() {
        for &ch in &[0u16, 3, 4, 5, 7, 9] {
            assert!(matches!(
                opus_channel_layout(ch),
                Err(AudioError::UnsupportedChannels(_))
            ));
        }
    }

    #[test]
    fn audio_error_decoder_variants_display() {
        let err = AudioError::DecoderInit("opus init failed".to_string());
        assert_eq!(
            err.to_string(),
            "audio decoder initialization failed: opus init failed"
        );

        let err = AudioError::DecodeFailed("corrupt packet".to_string());
        assert_eq!(err.to_string(), "audio decoding failed: corrupt packet");
    }
}
