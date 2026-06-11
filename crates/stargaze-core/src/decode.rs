//! Shared types for video decoding.
//!
//! Defines decoded frame data, decoder configuration, and error types
//! used by the client decoder and renderer.

use crate::config::Codec;
use thiserror::Error;

/// Pixel data of a decoded 4:2:0 frame, in the layout the decoder
/// produced it — the renderer uploads either layout directly, so no
/// repacking happens on the decode path.
#[derive(Debug, Clone)]
pub enum FramePixels {
    /// Planar YUV 4:2:0 (I420): separate Y, U, V planes.
    /// Typical software-decode output.
    I420 {
        /// Y (luma) plane, `width * height` bytes.
        y: Vec<u8>,
        /// U (chroma-blue) plane, `(width/2) * (height/2)` bytes.
        u: Vec<u8>,
        /// V (chroma-red) plane, `(width/2) * (height/2)` bytes.
        v: Vec<u8>,
    },
    /// Semi-planar NV12: Y plane plus interleaved UV plane.
    /// Typical hardware-decode (VAAPI) output — kept as-is to avoid a
    /// per-frame deinterleave on the client.
    Nv12 {
        /// Y (luma) plane, `width * height` bytes.
        y: Vec<u8>,
        /// Interleaved UV plane (U0 V0 U1 V1 ...), `width * height / 2` bytes.
        uv: Vec<u8>,
    },
}

/// A decoded video frame ready for rendering.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// Pixel planes in the decoder's native 4:2:0 layout.
    pub pixels: FramePixels,
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
            pixels: FramePixels::I420 {
                y: vec![128; y_size],
                u: vec![128; chroma_size],
                v: vec![128; chroma_size],
            },
            width,
            height,
            pts: 0,
            stats: FrameStats::default(),
        };
        match &frame.pixels {
            FramePixels::I420 { y, u, v } => {
                assert_eq!(y.len(), y_size);
                assert_eq!(u.len(), chroma_size);
                assert_eq!(v.len(), chroma_size);
            }
            FramePixels::Nv12 { .. } => panic!("expected I420"),
        }
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

        let frame = DecodedFrame {
            pixels: FramePixels::Nv12 {
                y: vec![0; y_size],
                uv: vec![0; uv_size],
            },
            width,
            height,
            pts: 100,
            stats: FrameStats::default(),
        };

        match &frame.pixels {
            FramePixels::Nv12 { y, uv } => {
                assert_eq!(y.len(), 307_200);
                assert_eq!(uv.len(), 153_600);
            }
            FramePixels::I420 { .. } => panic!("expected NV12"),
        }
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
