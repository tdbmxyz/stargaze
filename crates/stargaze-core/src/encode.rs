use thiserror::Error;

/// An encoded video packet (one or more NAL units from a single frame).
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// H.265 NAL units with Annex B start codes (0x00000001 prefix).
    pub data: Vec<u8>,
    /// Presentation timestamp (frame number).
    pub pts: u64,
    /// True for IDR frames.
    pub is_keyframe: bool,
    /// Microseconds from capture to the start of encoding (host side).
    /// Zero when unknown (e.g. audio packets).
    pub capture_us: u32,
    /// Microseconds spent encoding (host side). Zero when unknown.
    pub encode_us: u32,
}

/// Configuration for the video encoder.
///
/// Constructed from `ServerConfig` fields but kept separate so
/// the encoder doesn't depend on the full config system.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Target framerate.
    pub framerate: u32,
    /// Target bitrate in Mbps.
    pub bitrate_mbps: u32,
}

/// Errors from the video encoding subsystem.
#[derive(Error, Debug)]
pub enum EncodeError {
    /// An `FFmpeg` operation failed.
    #[error("FFmpeg error: {0}")]
    FfmpegError(String),

    /// Encoder initialization failed (NVENC unavailable, CUDA device error, etc.).
    #[error("Encoder initialization failed: {0}")]
    InitError(String),

    /// Encoding a specific frame failed.
    #[error("Encoding failed for frame {frame}: {reason}")]
    EncodeFrameError {
        /// Frame number that failed.
        frame: u64,
        /// Description of the failure.
        reason: String,
    },

    /// The captured frame has an unsupported pixel format.
    #[error("Unsupported pixel format: {0}")]
    UnsupportedFormat(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_error_display_ffmpeg() {
        let err = EncodeError::FfmpegError("codec not found".to_string());
        assert_eq!(err.to_string(), "FFmpeg error: codec not found");
    }

    #[test]
    fn encode_error_display_init() {
        let err = EncodeError::InitError("CUDA device creation failed".to_string());
        assert_eq!(
            err.to_string(),
            "Encoder initialization failed: CUDA device creation failed"
        );
    }

    #[test]
    fn encode_error_display_frame() {
        let err = EncodeError::EncodeFrameError {
            frame: 42,
            reason: "upload failed".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Encoding failed for frame 42: upload failed"
        );
    }

    #[test]
    fn encode_error_display_unsupported_format() {
        let err = EncodeError::UnsupportedFormat("YUV444P".to_string());
        assert_eq!(err.to_string(), "Unsupported pixel format: YUV444P");
    }

    #[test]
    fn encoded_packet_construction() {
        let pkt = EncodedPacket {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x40, 0x01],
            pts: 0,
            is_keyframe: true,
            capture_us: 0,
            encode_us: 0,
        };
        assert_eq!(pkt.data.len(), 6);
        assert_eq!(pkt.pts, 0);
        assert!(pkt.is_keyframe);
    }

    #[test]
    fn encoder_config_construction() {
        let cfg = EncoderConfig {
            width: 1920,
            height: 1080,
            framerate: 60,
            bitrate_mbps: 20,
        };
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert_eq!(cfg.framerate, 60);
        assert_eq!(cfg.bitrate_mbps, 20);
    }
}
