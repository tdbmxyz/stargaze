//! Direct, per-frame color-described Wayland capture. The portal remains the
//! default SDR backend; selecting this backend never relabels portal pixels.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use stargaze_core::capture::{CaptureError, CapturedFrame, DmaBufInfo, Frame, PixelFormat};
use stargaze_core::color::ColorDescription;
use stargaze_core::config::Resolution;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

use super::{CaptureConfig, CaptureFormat, CaptureSession, FRAME_CHANNEL_CAPACITY};

#[repr(C)]
#[derive(Default)]
struct NativeFrame {
    fd: i32,
    data: *const u8,
    width: u32,
    height: u32,
    stride: u32,
    offset: u32,
    modifier: u64,
    hdr: u32,
    fourcc: u32,
}

unsafe extern "C" {
    fn sg_capture_open(
        output: *const c_char,
        cursor: c_int,
        dma: c_int,
        allow_hdr: c_int,
        error: *mut c_char,
        error_size: usize,
    ) -> *mut c_void;
    fn sg_capture_close(capture: *mut c_void);
    fn sg_capture_format(
        capture: *mut c_void,
        width: *mut u32,
        height: *mut u32,
        hdr: *mut u32,
    ) -> c_int;
    fn sg_capture_next(capture: *mut c_void, timeout_ms: c_int, frame: *mut NativeFrame) -> c_int;
    fn sg_capture_error(capture: *mut c_void) -> *const c_char;
}

// Constructed and used only inside the capture thread. Deliberately not Send or
// Sync: no Wayland/GBM operation runs on the encoder thread.
struct NativeCapture(NonNull<c_void>);

impl Drop for NativeCapture {
    fn drop(&mut self) {
        // SAFETY: the unique capture-thread owner destroys the live C context.
        unsafe { sg_capture_close(self.0.as_ptr()) };
    }
}

impl NativeCapture {
    fn open(config: &CaptureConfig) -> Result<Self, CaptureError> {
        let output = CString::new(config.wayland_output.as_deref().unwrap_or_default())
            .map_err(|_| CaptureError::NegotiationError("Wayland output contains NUL".into()))?;
        let mut error = [0 as c_char; 256];
        // SAFETY: NUL-terminated output/error storage remains valid for the call;
        // the C bridge copies the output name and owns every returned resource.
        let pointer = unsafe {
            sg_capture_open(
                output.as_ptr(),
                i32::from(config.show_cursor),
                i32::from(!config.wayland_shm),
                i32::from(!config.wayland_sdr),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        NonNull::new(pointer).map(Self).ok_or_else(|| {
            // SAFETY: sg_capture_open always terminates its nonempty error buffer.
            let message = unsafe { CStr::from_ptr(error.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            CaptureError::NegotiationError(message)
        })
    }

    fn format(&self) -> Result<CaptureFormat, CaptureError> {
        let (mut width, mut height, mut hdr) = (0, 0, 0);
        // SAFETY: live context and writable scalar output pointers.
        let valid = unsafe {
            sg_capture_format(
                self.0.as_ptr(),
                &raw mut width,
                &raw mut height,
                &raw mut hdr,
            )
        };
        if valid == 0 || width == 0 || height == 0 {
            return Err(self.error());
        }
        Ok(CaptureFormat {
            resolution: Resolution { width, height },
            color: source_color(hdr != 0),
        })
    }

    fn error(&self) -> CaptureError {
        // SAFETY: the bridge owns a persistent, NUL-terminated error buffer.
        let message = unsafe { CStr::from_ptr(sg_capture_error(self.0.as_ptr())) }
            .to_string_lossy()
            .into_owned();
        CaptureError::NegotiationError(message)
    }

    fn next(
        &self,
        format: CaptureFormat,
        metadata: stargaze_core::hdr::HdrMetadata,
    ) -> Result<Option<CapturedFrame>, CaptureError> {
        let mut native = NativeFrame::default();
        // SAFETY: single-threaded context; the writable frame is repr(C).
        let result = unsafe { sg_capture_next(self.0.as_ptr(), 20, &raw mut native) };
        if result < 0 {
            return Err(self.error());
        }
        if result == 0 {
            return Ok(None);
        }
        // Take the FD immediately, including on validation/error paths.
        let fd = if native.fd >= 0 {
            // SAFETY: the bridge transfers this independent exported FD exactly once.
            Some(unsafe { OwnedFd::from_raw_fd(native.fd) })
        } else {
            None
        };
        if native.width != format.resolution.width
            || native.height != format.resolution.height
            || source_color(native.hdr != 0) != format.color
            || native.fourcc != 0x3033_4258
        {
            return Err(CaptureError::NegotiationError(
                "Wayland frame changed its immutable capture contract".into(),
            ));
        }
        let frame = if let Some(fd) = fd {
            Frame::DmaBuf(DmaBufInfo {
                fd,
                width: native.width,
                height: native.height,
                format: PixelFormat::Bgra10,
                drm_fourcc: native.fourcc,
                modifier: native.modifier,
                stride: native.stride,
                offset: native.offset,
            })
        } else {
            let length = usize::try_from(u64::from(native.stride) * u64::from(native.height))
                .map_err(|_| CaptureError::NegotiationError("Wayland SHM size overflow".into()))?;
            if native.data.is_null()
                || length > 256 * 1024 * 1024
                || native.stride < native.width.saturating_mul(4)
            {
                return Err(CaptureError::NegotiationError(
                    "Invalid Wayland RGB10 SHM frame".into(),
                ));
            }
            // SAFETY: C guarantees an initialized mapping of stride*height bytes,
            // borrowed until the next next/close call. Copy before either occurs.
            let data = unsafe { std::slice::from_raw_parts(native.data, length) }.to_vec();
            Frame::CpuMapped {
                data,
                width: native.width,
                height: native.height,
                stride: native.stride,
                format: PixelFormat::Bgra10,
            }
        };
        Ok(Some(CapturedFrame {
            frame,
            captured_at: Instant::now(),
            color: format.color,
            hdr_metadata: metadata,
        }))
    }
}

fn source_color(hdr: bool) -> ColorDescription {
    if hdr {
        ColorDescription::HDR10_RGB
    } else {
        ColorDescription {
            bit_depth: 10,
            ..ColorDescription::SRGB
        }
    }
}

pub(super) fn start(
    config: CaptureConfig,
) -> Result<
    (
        CaptureSession,
        mpsc::Receiver<CapturedFrame>,
        oneshot::Receiver<CaptureFormat>,
    ),
    CaptureError,
> {
    let output = config.wayland_output.as_deref().unwrap_or_default();
    if output.is_empty() || config.wayland_framerate > 240 {
        return Err(CaptureError::NegotiationError(
            "Direct Wayland capture requires a named output and frame rate <=240".into(),
        ));
    }
    let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
    let (format_tx, format_rx) = oneshot::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);
    let thread_handle = std::thread::Builder::new()
        .name("stargaze-wayland".into())
        .spawn(move || {
            if let Err(error) = run(&config, &tx, format_tx, &thread_shutdown) {
                error!(%error, "Direct Wayland capture stopped");
            }
        })
        .map_err(|error| {
            CaptureError::NegotiationError(format!("Cannot spawn Wayland capture thread: {error}"))
        })?;
    Ok((
        CaptureSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        rx,
        format_rx,
    ))
}

fn run(
    config: &CaptureConfig,
    tx: &mpsc::Sender<CapturedFrame>,
    format_tx: oneshot::Sender<CaptureFormat>,
    shutdown: &AtomicBool,
) -> Result<(), CaptureError> {
    let capture = NativeCapture::open(config)?;
    let format = capture.format()?;
    info!(output = ?config.wayland_output, backend = if config.wayland_shm { "SHM" } else { "DMA-BUF" }, ?format.color, width = format.resolution.width, height = format.resolution.height, "Explicit Wayland RGB10 frame contract acknowledged");
    if format_tx.send(format).is_err() {
        return Ok(());
    }
    let rate = if config.wayland_framerate == 0 {
        60
    } else {
        config.wayland_framerate
    };
    let period = Duration::from_secs_f64(1.0 / f64::from(rate));
    let mut deadline = Instant::now();
    let mut request_started = None;
    let mut first = true;
    while !shutdown.load(Ordering::Relaxed) {
        while Instant::now() < deadline && !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(5)),
            );
        }
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let requested_at = *request_started.get_or_insert_with(Instant::now);
        let Some(frame) = capture.next(format, config.hdr_metadata)? else {
            continue;
        };
        if first {
            info!(frame = ?frame.frame, color = ?frame.color, "First direct Wayland capture frame (owned independent storage)");
            first = false;
        }
        match tx.try_send(frame) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
        request_started = None;
        // Pace request starts, not completion plus a period. Rendering time
        // must not unnecessarily lower a 60-fps request to 30-fps delivery.
        deadline = requested_at + period;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_configuration_fails_before_connecting() {
        for config in [
            CaptureConfig::default(),
            CaptureConfig {
                wayland_output: Some(String::new()),
                ..CaptureConfig::default()
            },
            CaptureConfig {
                wayland_output: Some("Main".into()),
                wayland_framerate: 241,
                ..CaptureConfig::default()
            },
        ] {
            assert!(start(config).is_err());
        }
    }

    #[tokio::test]
    async fn conflicting_sources_fail_before_connecting() {
        for (node_id, hdr_test_pattern) in [(Some(1), None), (None, Some(60))] {
            let config = CaptureConfig {
                wayland_output: Some("Main".into()),
                node_id,
                hdr_test_pattern,
                ..CaptureConfig::default()
            };
            assert!(super::super::start_capture(config).await.is_err());
        }
    }

    #[test]
    fn ten_bit_srgb_does_not_select_hdr() {
        assert_eq!(
            source_color(false).stream(),
            Some(ColorDescription::SDR_STREAM)
        );
        assert_eq!(source_color(true).stream(), Some(ColorDescription::HDR10));
    }

    /// Isolated native RGB10 fixture: validates both source backends and Main10
    /// color selection. Run with explicit lab `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`.
    #[tokio::test]
    #[ignore = "requires isolated patched Hyprland and native color-managed-source"]
    async fn wayland_native_rgb10_desktop() {
        for shm in [true, false] {
            let config = CaptureConfig {
                wayland_output: Some("Main".into()),
                wayland_shm: shm,
                show_cursor: false,
                ..CaptureConfig::default()
            };
            let (session, mut frames, format) = start(config).unwrap();
            let format = tokio::time::timeout(Duration::from_secs(10), format)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(format.color, ColorDescription::HDR10_RGB);
            let frame = receive_frame(&mut frames).await;
            assert_eq!(frame.color.stream(), Some(ColorDescription::HDR10));
            if let Frame::CpuMapped {
                data,
                width,
                stride,
                format,
                ..
            } = &frame.frame
            {
                assert!(*width >= 1024);
                assert_eq!(*format, PixelFormat::Bgra10);
                for (row, shift, mask) in [
                    (32, 0, 0x3fff_ffff),
                    (96, 0, 1023),
                    (160, 10, 1023),
                    (224, 20, 1023),
                ] {
                    for code in 0..1024u32 {
                        let offset = row * *stride as usize + code as usize * 4;
                        let pixel =
                            u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                        let expected = if row == 32 {
                            code | (code << 10) | (code << 20)
                        } else {
                            code
                        };
                        assert_eq!((pixel >> shift) & mask, expected);
                    }
                }
            } else {
                assert!(!shm);
            }
            // Retain the first frame across new allocations and compositor-wrapper
            // destruction. The NVENC test below imports such retained DMA-BUFs.
            for _ in 0..3 {
                let _ = receive_frame(&mut frames).await;
            }
            session.stop().unwrap();
            drop(frame);
        }
    }

    async fn receive_frame(frames: &mut mpsc::Receiver<CapturedFrame>) -> CapturedFrame {
        tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .unwrap()
            .unwrap()
    }

    /// Run against the isolated native fixture through the CUDA dev shell.
    /// Encodes retained real desktop buffers after closing their Wayland owner.
    #[tokio::test]
    #[ignore = "requires isolated patched Hyprland, native fixture, NVIDIA NVENC and NVRTC"]
    async fn wayland_desktop_main10() {
        let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
        for shm in [true, false] {
            let config = CaptureConfig {
                wayland_output: Some("Main".into()),
                wayland_shm: shm,
                ..CaptureConfig::default()
            };
            let (capture, mut frames, format) = start(config).unwrap();
            let format = tokio::time::timeout(Duration::from_secs(10), format)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(format.color, ColorDescription::HDR10_RGB);
            let (tx, rx) = mpsc::channel(8);
            for _ in 0..3 {
                tx.send(receive_frame(&mut frames).await).await.unwrap();
            }
            capture.stop().unwrap();
            drop(tx);
            let config = stargaze_core::encode::EncoderConfig {
                width: format.resolution.width,
                height: format.resolution.height,
                framerate: 60,
                bitrate_mbps: 80,
                color: ColorDescription::HDR10,
                tuning: stargaze_core::config::EncoderTuning::default(),
            };
            let (encoder, mut packets, _idr, _bitrate) =
                crate::encode::start_encoder(config, rx).unwrap();
            let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC).unwrap();
            let mut decoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
                .decoder()
                .video()
                .unwrap();
            let mut picture_count = 0;
            for _ in 0..3 {
                let packet = tokio::time::timeout(Duration::from_secs(10), packets.recv())
                    .await
                    .unwrap()
                    .unwrap();
                decoder
                    .send_packet(&ffmpeg_next::Packet::copy(&packet.data))
                    .unwrap();
                picture_count += drain_native_pictures(&mut decoder);
            }
            decoder.send_eof().unwrap();
            picture_count += drain_native_pictures(&mut decoder);
            assert_eq!(picture_count, 3);
            encoder.stop().unwrap();
        }
    }

    fn drain_native_pictures(decoder: &mut ffmpeg_next::decoder::Video) -> usize {
        let mut picture = ffmpeg_next::frame::Video::empty();
        let mut picture_count = 0;
        while decoder.receive_frame(&mut picture).is_ok() {
            assert_eq!(picture.format(), ffmpeg_next::format::Pixel::YUV420P10LE);
            assert_eq!(
                picture.color_primaries(),
                ffmpeg_next::color::Primaries::BT2020
            );
            assert_eq!(
                picture.color_transfer_characteristic(),
                ffmpeg_next::color::TransferCharacteristic::SMPTE2084
            );
            assert_eq!(picture.color_space(), ffmpeg_next::color::Space::BT2020NCL);
            let row = &picture.data(0)[32 * picture.stride(0)..][..1024 * 2];
            let codes: Vec<_> = row
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_le_bytes(*p))
                .collect();
            assert!(codes[0].abs_diff(64) <= 4);
            assert!(codes[1023].abs_diff(940) <= 4);
            assert!(
                codes.windows(2).filter(|v| v[0] != v[1]).count() > 300,
                "Main10 precision reduced to eight bits"
            );
            picture_count += 1;
        }
        picture_count
    }

    /// Explicit SDR policy on an HDR output must acknowledge and deliver sRGB.
    #[tokio::test]
    #[ignore = "requires isolated patched Hyprland and native fixture"]
    async fn wayland_desktop_sdr_policy() {
        let config = CaptureConfig {
            wayland_output: Some("Main".into()),
            wayland_shm: true,
            wayland_sdr: true,
            ..CaptureConfig::default()
        };
        let (capture, mut frames, format) = start(config).unwrap();
        let format = tokio::time::timeout(Duration::from_secs(10), format)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(format.color, source_color(false));
        assert_eq!(
            receive_frame(&mut frames).await.color.stream(),
            Some(ColorDescription::SDR_STREAM)
        );
        // A full frame channel must never prevent shutdown.
        tokio::time::sleep(Duration::from_millis(100)).await;
        capture.stop().unwrap();
    }

    /// Point at an isolated compositor WITHOUT the experimental extension.
    #[tokio::test]
    #[ignore = "requires isolated unpatched Wayland compositor"]
    async fn wayland_unsupported_compositor() {
        let config = CaptureConfig {
            wayland_output: Some("Main".into()),
            ..CaptureConfig::default()
        };
        let (capture, mut frames, format) = start(config).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), format)
                .await
                .unwrap()
                .is_err()
        );
        assert!(frames.recv().await.is_none());
        capture.stop().unwrap();
    }
}
