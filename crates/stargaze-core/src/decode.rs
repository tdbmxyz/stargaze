//! Shared types for video decoding.
//!
//! Defines decoded frame data, decoder configuration, and error types
//! used by the client decoder and renderer.

use crate::config::Codec;
use thiserror::Error;

/// A decoded video frame ready for rendering.
///
/// Contains raw pixel data in YUV420P (planar) format with three
/// separate planes: Y (luma), U (chroma-blue), V (chroma-red).
///
/// - Y plane:  `width * height` bytes
/// - U plane:  `(width / 2) * (height / 2)` bytes
/// - V plane:  `(width / 2) * (height / 2)` bytes
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Y (luma) plane data.
    pub y_plane: Vec<u8>,
    /// U (chroma-blue) plane data.
    pub u_plane: Vec<u8>,
    /// V (chroma-red) plane data.
    pub v_plane: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Presentation timestamp (matches the encoded frame's PTS).
    pub pts: u64,
    /// Per-frame pipeline timing, for the client stats overlay.
    pub stats: FrameStats,
}

/// Per-frame pipeline timing measurements, accumulated as a frame moves
/// from capture (server) to decode (client). All values in microseconds;
/// zero means "not measured".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// Host: capture → start of encoding.
    pub capture_us: u32,
    /// Host: frame preparation (pixel conversion + GPU upload).
    pub convert_us: u32,
    /// Host: encode duration.
    pub encode_us: u32,
    /// Client: frame fully received → decode started.
    pub queue_us: u32,
    /// Client: decode duration.
    pub decode_us: u32,
    /// Size of the encoded frame in bytes (for bitrate display).
    pub packet_bytes: u32,
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
        let y_size = (width * height) as usize;
        let chroma_size = ((width / 2) * (height / 2)) as usize;
        let frame = DecodedFrame {
            y_plane: vec![128; y_size],
            u_plane: vec![128; chroma_size],
            v_plane: vec![128; chroma_size],
            width,
            height,
            pts: 0,
            stats: FrameStats::default(),
        };
        assert_eq!(frame.y_plane.len(), y_size);
        assert_eq!(frame.u_plane.len(), chroma_size);
        assert_eq!(frame.v_plane.len(), chroma_size);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.pts, 0);
    }

    #[test]
    fn decoded_frame_yuv420p_plane_sizes() {
        let width: u32 = 640;
        let height: u32 = 480;
        let y_size = (width * height) as usize;
        let chroma_size = ((width / 2) * (height / 2)) as usize;

        let frame = DecodedFrame {
            y_plane: vec![0; y_size],
            u_plane: vec![0; chroma_size],
            v_plane: vec![0; chroma_size],
            width,
            height,
            pts: 100,
            stats: FrameStats::default(),
        };

        assert_eq!(y_size, 307_200);
        assert_eq!(chroma_size, 76_800);
        assert_eq!(
            frame.y_plane.len() + frame.u_plane.len() + frame.v_plane.len(),
            y_size + 2 * chroma_size
        );
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
