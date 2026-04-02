//! Shared types for video decoding.
//!
//! Defines decoded frame data, decoder configuration, and error types
//! used by the client decoder and renderer.

use crate::config::Codec;
use thiserror::Error;

/// A decoded video frame ready for rendering.
///
/// Contains raw pixel data in NV12 format: a Y (luma) plane followed
/// by an interleaved UV (chroma) plane at half vertical resolution.
///
/// Total data size: `width * height * 3 / 2` bytes.
/// - Y plane:  `data[0 .. width * height]`
/// - UV plane: `data[width * height .. width * height * 3 / 2]`
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// NV12 pixel data (Y plane followed by interleaved UV plane).
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Presentation timestamp (matches the encoded frame's PTS).
    pub pts: u64,
}

/// Configuration for the video decoder.
///
/// Constructed from session parameters received during the transport
/// handshake — describes the expected stream properties.
#[derive(Debug, Clone)]
pub struct DecoderConfig {
    /// Expected frame width in pixels.
    pub width: u32,
    /// Expected frame height in pixels.
    pub height: u32,
    /// Codec to decode.
    pub codec: Codec,
}

/// Errors from the video decoding subsystem.
#[derive(Error, Debug)]
pub enum DecodeError {
    /// An `FFmpeg` operation failed.
    #[error("FFmpeg error: {0}")]
    FfmpegError(String),

    /// Decoder initialization failed (codec unavailable, etc.).
    #[error("Decoder initialization failed: {0}")]
    InitError(String),

    /// Decoding a specific frame failed.
    #[error("Decoding failed for frame at PTS {pts}: {reason}")]
    DecodeFrameError {
        /// PTS of the frame that failed.
        pts: u64,
        /// Description of the failure.
        reason: String,
    },

    /// The requested codec is not supported.
    #[error("Unsupported codec: {0}")]
    UnsupportedCodec(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_error_display_ffmpeg() {
        let err = DecodeError::FfmpegError("packet rejected".to_string());
        assert_eq!(err.to_string(), "FFmpeg error: packet rejected");
    }

    #[test]
    fn decode_error_display_init() {
        let err = DecodeError::InitError("hevc decoder not found".to_string());
        assert_eq!(
            err.to_string(),
            "Decoder initialization failed: hevc decoder not found"
        );
    }

    #[test]
    fn decode_error_display_frame() {
        let err = DecodeError::DecodeFrameError {
            pts: 42,
            reason: "corrupt NAL unit".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Decoding failed for frame at PTS 42: corrupt NAL unit"
        );
    }

    #[test]
    fn decode_error_display_unsupported_codec() {
        let err = DecodeError::UnsupportedCodec("VP9".to_string());
        assert_eq!(err.to_string(), "Unsupported codec: VP9");
    }

    #[test]
    fn decoded_frame_construction() {
        let width: u32 = 1920;
        let height: u32 = 1080;
        let nv12_size = (width * height * 3 / 2) as usize;
        let frame = DecodedFrame {
            data: vec![128; nv12_size],
            width,
            height,
            pts: 0,
        };
        assert_eq!(frame.data.len(), nv12_size);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.pts, 0);
    }

    #[test]
    fn decoded_frame_nv12_plane_sizes() {
        let width: u32 = 640;
        let height: u32 = 480;
        let y_size = (width * height) as usize;
        let uv_size = (width * height / 2) as usize;
        let total = y_size + uv_size;

        let frame = DecodedFrame {
            data: vec![0; total],
            width,
            height,
            pts: 100,
        };

        // Y plane: first width*height bytes.
        assert_eq!(y_size, 307_200);
        // UV plane: next width*height/2 bytes.
        assert_eq!(uv_size, 153_600);
        assert_eq!(frame.data.len(), y_size + uv_size);
    }

    #[test]
    fn decoder_config_construction() {
        let cfg = DecoderConfig {
            width: 1920,
            height: 1080,
            codec: Codec::H265,
        };
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert!(matches!(cfg.codec, Codec::H265));
    }
}
