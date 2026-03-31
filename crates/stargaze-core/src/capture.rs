use std::fmt;
use std::os::unix::io::OwnedFd;

use thiserror::Error;

/// Pixel format of a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// BGRA 8-bit per channel (`DRM_FORMAT_XRGB8888` read as BGRA).
    Bgra8,
    /// RGBA 8-bit per channel (`DRM_FORMAT_XBGR8888` read as RGBA).
    Rgba8,
    /// NV12 semi-planar YUV (possible direct from some sources).
    Nv12,
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bgra8 => write!(f, "BGRA8"),
            Self::Rgba8 => write!(f, "RGBA8"),
            Self::Nv12 => write!(f, "NV12"),
        }
    }
}

/// Metadata for a DMA-BUF frame (zero-copy GPU buffer).
///
/// The `fd` is a duped file descriptor owned by this struct.
/// It will be closed when this struct is dropped.
pub struct DmaBufInfo {
    /// DMA-BUF file descriptor (duped, caller-owned).
    pub fd: OwnedFd,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Pixel format.
    pub format: PixelFormat,
    /// DRM format modifier (tiling, compression).
    pub modifier: u64,
    /// Bytes per row.
    pub stride: u32,
    /// Offset into the buffer in bytes.
    pub offset: u32,
}

impl fmt::Debug for DmaBufInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmaBufInfo")
            .field("fd", &self.fd)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("modifier", &format_args!("0x{:x}", self.modifier))
            .field("stride", &self.stride)
            .field("offset", &self.offset)
            .finish()
    }
}

/// A captured video frame — either zero-copy GPU buffer or CPU-mapped data.
#[derive(Debug)]
pub enum Frame {
    /// Zero-copy DMA-BUF frame. The fd is owned and closed on drop.
    DmaBuf(DmaBufInfo),
    /// CPU-mapped frame with owned pixel data.
    CpuMapped {
        /// Owned pixel data (copied from the `PipeWire` buffer).
        data: Vec<u8>,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
        /// Bytes per row.
        stride: u32,
        /// Pixel format.
        format: PixelFormat,
    },
}

/// Errors from the video capture subsystem.
#[derive(Error, Debug)]
pub enum CaptureError {
    /// The xdg-desktop-portal session failed.
    #[error("Portal session failed: {0}")]
    PortalError(String),

    /// A `PipeWire` connection or stream error occurred.
    #[error("PipeWire error: {0}")]
    PipeWireError(String),

    /// Could not negotiate a supported buffer format.
    #[error("Buffer format negotiation failed: {0}")]
    NegotiationError(String),

    /// The capture stream ended unexpectedly.
    #[error("Capture stream ended unexpectedly")]
    StreamEnded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_error_display_portal() {
        let err = CaptureError::PortalError("dbus connection refused".to_string());
        assert_eq!(
            err.to_string(),
            "Portal session failed: dbus connection refused"
        );
    }

    #[test]
    fn test_capture_error_display_pipewire() {
        let err = CaptureError::PipeWireError("failed to connect".to_string());
        assert_eq!(err.to_string(), "PipeWire error: failed to connect");
    }

    #[test]
    fn test_capture_error_display_negotiation() {
        let err = CaptureError::NegotiationError("no supported format".to_string());
        assert_eq!(
            err.to_string(),
            "Buffer format negotiation failed: no supported format"
        );
    }

    #[test]
    fn test_capture_error_display_stream_ended() {
        let err = CaptureError::StreamEnded;
        assert_eq!(err.to_string(), "Capture stream ended unexpectedly");
    }

    #[test]
    fn test_pixel_format_display() {
        assert_eq!(PixelFormat::Bgra8.to_string(), "BGRA8");
        assert_eq!(PixelFormat::Rgba8.to_string(), "RGBA8");
        assert_eq!(PixelFormat::Nv12.to_string(), "NV12");
    }

    #[test]
    fn test_frame_cpu_mapped_construction() {
        let data = vec![0u8; 1920 * 1080 * 4];
        let frame = Frame::CpuMapped {
            data,
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            format: PixelFormat::Bgra8,
        };

        match &frame {
            Frame::CpuMapped {
                width,
                height,
                stride,
                format,
                data,
            } => {
                assert_eq!(*width, 1920);
                assert_eq!(*height, 1080);
                assert_eq!(*stride, 1920 * 4);
                assert_eq!(*format, PixelFormat::Bgra8);
                assert_eq!(data.len(), 1920 * 1080 * 4);
            }
            Frame::DmaBuf(_) => panic!("expected CpuMapped variant"),
        }
    }

    #[test]
    fn test_frame_dmabuf_construction_with_memfd() {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::io::FromRawFd;

        // Create a synthetic fd using memfd_create (no real DMA-BUF needed)
        let name = std::ffi::CString::new("test-dmabuf").unwrap();
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(raw_fd >= 0, "memfd_create failed");

        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // Verify the fd is valid before wrapping
        let raw = fd.as_raw_fd();
        assert!(raw >= 0);

        let frame = Frame::DmaBuf(DmaBufInfo {
            fd,
            width: 1920,
            height: 1080,
            format: PixelFormat::Bgra8,
            modifier: 0,
            stride: 1920 * 4,
            offset: 0,
        });

        match &frame {
            Frame::DmaBuf(info) => {
                assert_eq!(info.width, 1920);
                assert_eq!(info.height, 1080);
                assert_eq!(info.format, PixelFormat::Bgra8);
                assert_eq!(info.modifier, 0);
                assert_eq!(info.stride, 1920 * 4);
                assert_eq!(info.offset, 0);
            }
            Frame::CpuMapped { .. } => panic!("expected DmaBuf variant"),
        }
        // fd is closed when frame is dropped
    }
}
