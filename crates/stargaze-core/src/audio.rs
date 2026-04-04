use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Raw audio frame from capture — interleaved f32 PCM samples.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Interleaved f32 PCM samples (L0, R0, L1, R1, ...).
    pub data: Vec<f32>,
    /// Sample rate in Hz (expected: 48000).
    pub sample_rate: u32,
    /// Number of channels (expected: 2 for stereo).
    pub channels: u16,
    /// Presentation timestamp (monotonic frame counter).
    pub pts: u64,
}

/// Configuration for audio capture.
#[derive(Debug, Clone)]
pub struct AudioCaptureConfig {
    /// Target sample rate in Hz (e.g. 48000).
    pub sample_rate: u32,
    /// Number of channels (2 for stereo).
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
    /// Number of channels (1 or 2).
    pub channels: u16,
}

/// Configuration for the Opus audio encoder.
#[derive(Debug, Clone)]
pub struct AudioEncoderConfig {
    /// Sample rate in Hz (must be 48000 for Opus).
    pub sample_rate: u32,
    /// Number of channels (1 or 2).
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
