//! `FFmpeg` H.265 decoder with VAAPI hardware acceleration.
//!
//! Attempts VAAPI hardware decoding first, falls back to multi-threaded
//! software decode if VAAPI is unavailable.  Hardware-decoded frames are
//! transferred from GPU to CPU (NV12/YUV420P) before being sent to the
//! renderer.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::config::Codec;
use stargaze_core::decode::{DecodeError, DecodedFrame, DecoderConfig, FrameStats};
use stargaze_core::transport::ReassembledFrame;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Opaque handle to initialized `FFmpeg` decoder state.
///
/// Owns the codec context, an optional hardware device context, and an
/// optional scaler to YUV420P.
pub(crate) struct FfmpegDecoder {
    /// Opened H.265 decoder (software or hardware-accelerated).
    decoder: ffmpeg_next::decoder::Video,
    /// Raw pointer to the VAAPI hardware device context (`AVBufferRef`).
    /// Null when using software decode.  Owned — freed via `av_buffer_unref`
    /// on drop.
    hw_device_ctx: *mut ffmpeg_sys_next::AVBufferRef,
    /// Whether the decoder is using hardware acceleration.
    hw_accel: bool,
    /// Lazily created scaler to YUV420P (for non-YUV420P output).
    scaler: Option<ffmpeg_next::software::scaling::Context>,
    /// Reusable software frame for `av_hwframe_transfer_data`.
    sw_frame: ffmpeg_next::frame::Video,
}

// Safety: FfmpegDecoder is only used on the dedicated decoder thread.
unsafe impl Send for FfmpegDecoder {}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.hw_device_ctx.is_null() {
                ffmpeg_sys_next::av_buffer_unref(&raw mut self.hw_device_ctx);
            }
        }
    }
}

/// Initializes the `FFmpeg` H.265 decoder.
///
/// Tries VAAPI hardware acceleration first.  If VAAPI is unavailable or
/// initialization fails, falls back to multi-threaded software decode.
///
/// # Errors
///
/// Returns `DecodeError::InitError` if both hardware and software decoders
/// fail to initialize.
/// Returns `DecodeError::UnsupportedCodec` if a non-H.265 codec is requested.
pub(crate) fn init_decoder(config: &DecoderConfig) -> Result<FfmpegDecoder, DecodeError> {
    ffmpeg_next::init().map_err(|e| DecodeError::InitError(format!("ffmpeg init: {e}")))?;

    match config.codec {
        Codec::H265 => {}
        Codec::Av1 => {
            return Err(DecodeError::UnsupportedCodec(
                "av1 — only H.265 is supported by this decoder".to_string(),
            ));
        }
    }

    // Try VAAPI first, then fall back to software.
    match init_vaapi_decoder(config) {
        Ok(dec) => {
            info!(
                width = config.width,
                height = config.height,
                "H.265 VAAPI hardware decoder initialized"
            );
            Ok(dec)
        }
        Err(e) => {
            warn!("VAAPI init failed ({e}), falling back to software decode");
            init_software_decoder(config)
        }
    }
}

/// `get_format` callback that selects the VAAPI hardware pixel format.
///
/// FFmpeg's default callback skips hardware formats, so without this the
/// decoder silently falls back to software decoding even when a hardware
/// device context is attached.
unsafe extern "C" fn select_vaapi_format(
    _ctx: *mut ffmpeg_sys_next::AVCodecContext,
    fmt_list: *const ffmpeg_sys_next::AVPixelFormat,
) -> ffmpeg_sys_next::AVPixelFormat {
    use ffmpeg_sys_next::AVPixelFormat;

    let mut fmt = fmt_list;
    unsafe {
        while *fmt != AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmt == AVPixelFormat::AV_PIX_FMT_VAAPI {
                return AVPixelFormat::AV_PIX_FMT_VAAPI;
            }
            fmt = fmt.add(1);
        }
    }

    // VAAPI is not offered for this stream (e.g. unsupported profile).
    // Fall back to the first software format, mirroring FFmpeg's default.
    warn!("VAAPI surface format not offered by decoder, decoding in software");
    let mut fmt = fmt_list;
    unsafe {
        while *fmt != AVPixelFormat::AV_PIX_FMT_NONE {
            let desc = ffmpeg_sys_next::av_pix_fmt_desc_get(*fmt);
            let hwaccel = u64::from(ffmpeg_sys_next::AV_PIX_FMT_FLAG_HWACCEL.unsigned_abs());
            if !desc.is_null() && ((*desc).flags & hwaccel) == 0 {
                return *fmt;
            }
            fmt = fmt.add(1);
        }
    }
    AVPixelFormat::AV_PIX_FMT_NONE
}

/// Returns true if the codec supports VAAPI decoding via a device context.
fn codec_supports_vaapi(codec: ffmpeg_next::Codec) -> bool {
    let mut index = 0;
    loop {
        let config = unsafe { ffmpeg_sys_next::avcodec_get_hw_config(codec.as_ptr(), index) };
        if config.is_null() {
            return false;
        }
        let config = unsafe { &*config };
        let via_device_ctx = (config.methods
            & ffmpeg_sys_next::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as libc::c_int)
            != 0;
        if via_device_ctx
            && config.device_type == ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
        {
            return true;
        }
        index += 1;
    }
}

/// Attempts to initialize a VAAPI-accelerated H.265 decoder.
fn init_vaapi_decoder(_config: &DecoderConfig) -> Result<FfmpegDecoder, DecodeError> {
    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC)
        .ok_or_else(|| DecodeError::InitError("hevc decoder not found".to_string()))?;

    if !codec_supports_vaapi(codec) {
        return Err(DecodeError::InitError(
            "hevc decoder does not support VAAPI via device context".to_string(),
        ));
    }

    // Create VAAPI hardware device context.
    let mut hw_device_ctx: *mut ffmpeg_sys_next::AVBufferRef = ptr::null_mut();
    let ret = unsafe {
        ffmpeg_sys_next::av_hwdevice_ctx_create(
            &raw mut hw_device_ctx,
            ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            ptr::null(),     // default device (/dev/dri/renderD128)
            ptr::null_mut(), // no options
            0,
        )
    };
    if ret < 0 {
        return Err(DecodeError::InitError(format!(
            "failed to create VAAPI device context (error {ret})"
        )));
    }
    debug!("Created VAAPI device context");

    let mut context = ffmpeg_next::codec::context::Context::new_with_codec(codec);

    // Attach the hardware device context so the decoder can allocate
    // hardware surfaces, and install the format callback that selects the
    // VAAPI surface format (without it FFmpeg defaults to software decode).
    unsafe {
        let raw_ctx = context.as_mut_ptr();
        (*raw_ctx).hw_device_ctx = ffmpeg_sys_next::av_buffer_ref(hw_device_ctx);
        if (*raw_ctx).hw_device_ctx.is_null() {
            ffmpeg_sys_next::av_buffer_unref(&raw mut hw_device_ctx);
            return Err(DecodeError::InitError(
                "av_buffer_ref failed for hw_device_ctx".to_string(),
            ));
        }
        (*raw_ctx).get_format = Some(select_vaapi_format);
        // Output frames as soon as they decode instead of waiting for the
        // stream's nominal reorder depth (we encode without B-frames).
        (*raw_ctx).flags |= ffmpeg_sys_next::AV_CODEC_FLAG_LOW_DELAY.cast_signed();
    }

    let decoder = context.decoder().video().map_err(|e| {
        unsafe { ffmpeg_sys_next::av_buffer_unref(&raw mut hw_device_ctx) };
        DecodeError::InitError(format!("failed to open VAAPI hevc decoder: {e}"))
    })?;

    Ok(FfmpegDecoder {
        decoder,
        hw_device_ctx,
        hw_accel: true,
        scaler: None,
        sw_frame: ffmpeg_next::frame::Video::empty(),
    })
}

/// Initializes a multi-threaded software H.265 decoder.
fn init_software_decoder(config: &DecoderConfig) -> Result<FfmpegDecoder, DecodeError> {
    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC)
        .ok_or_else(|| DecodeError::InitError("hevc decoder not found".to_string()))?;

    let mut context = ffmpeg_next::codec::context::Context::new_with_codec(codec);

    // Frame threading pipelines decoding across threads, but each thread
    // adds one frame of output delay — with auto-detection on a 16-core
    // machine that's ~250 ms at 60 fps. Cap the pipeline depth and set
    // LOW_DELAY so frames are emitted as soon as they're ready.
    context.set_threading(ffmpeg_next::codec::threading::Config {
        kind: ffmpeg_next::codec::threading::Type::Frame,
        count: 4,
    });
    unsafe {
        let raw_ctx = context.as_mut_ptr();
        (*raw_ctx).flags |= ffmpeg_sys_next::AV_CODEC_FLAG_LOW_DELAY.cast_signed();
    }

    let decoder = context
        .decoder()
        .video()
        .map_err(|e| DecodeError::InitError(format!("failed to open hevc decoder: {e}")))?;

    info!(
        width = config.width,
        height = config.height,
        threads = unsafe { (*decoder.as_ptr()).thread_count },
        "H.265 multi-threaded software decoder initialized"
    );

    Ok(FfmpegDecoder {
        decoder,
        hw_device_ctx: ptr::null_mut(),
        hw_accel: false,
        scaler: None,
        sw_frame: ffmpeg_next::frame::Video::empty(),
    })
}

/// Runs the decode loop: receives reassembled frames, decodes them, converts
/// to YUV420P planes, and sends decoded frames to the renderer.
///
/// Blocks until `shutdown` is signaled or the input channel closes.
/// Meant to run on a dedicated `std::thread`.
///
/// # Errors
///
/// Returns `DecodeError` on fatal errors. Non-fatal errors (corrupt packets)
/// are logged and skipped.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn run_decode_loop(
    decoder: &mut FfmpegDecoder,
    frames_rx: &mut mpsc::Receiver<ReassembledFrame>,
    decoded_tx: &std::sync::mpsc::Sender<DecodedFrame>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), DecodeError> {
    let mut decoded_frame = ffmpeg_next::frame::Video::empty();
    let mut packet_counter: u64 = 0;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let Some(frame) = frames_rx.blocking_recv() else {
            info!("Reassembled frame channel closed, flushing decoder");
            break;
        };

        // Decode every frame we receive: skipping a delta frame here would
        // break decoder references and corrupt output. If decoding falls
        // behind, the transport drops frames before they reach this channel
        // and requests an IDR, and the renderer keeps only the latest
        // decoded frame.

        if packet_counter < 5 {
            let preview_len = frame.data.len().min(64);
            info!(
                packet = packet_counter,
                size = frame.data.len(),
                is_keyframe = frame.is_keyframe,
                stream_type = frame.stream_type,
                first_bytes = ?&frame.data[..preview_len],
                "Decoder input dump"
            );
        }
        packet_counter += 1;

        let decode_start = std::time::Instant::now();
        let stats = FrameStats {
            capture_us: frame.capture_us,
            encode_us: frame.encode_us,
            queue_us: saturating_us(decode_start.duration_since(frame.received_at)),
            decode_us: 0, // filled in when the decoded frame pops out
            packet_bytes: u32::try_from(frame.data.len()).unwrap_or(u32::MAX),
        };

        let mut packet = ffmpeg_next::Packet::copy(&frame.data);
        packet.set_pts(Some(frame.pts.cast_signed()));

        if let Err(e) = decoder.decoder.send_packet(&packet) {
            warn!(pts = frame.pts, "Skipping corrupt packet: {e}");
            continue;
        }

        drain_decoded_frames(decoder, &mut decoded_frame, decoded_tx, stats, decode_start)?;
    }

    // Flush: send EOF and drain remaining frames.
    if let Err(e) = decoder.decoder.send_eof() {
        warn!("Failed to send EOF to decoder: {e}");
    } else {
        drain_decoded_frames(
            decoder,
            &mut decoded_frame,
            decoded_tx,
            FrameStats::default(),
            std::time::Instant::now(),
        )?;
    }

    info!("Decoder loop finished");
    Ok(())
}

/// Converts a duration to whole microseconds, saturating at `u32::MAX`.
fn saturating_us(d: std::time::Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

/// Drains all available decoded frames from the codec, converts to YUV420P
/// planes, and sends them to the renderer.
fn drain_decoded_frames(
    decoder: &mut FfmpegDecoder,
    decoded_frame: &mut ffmpeg_next::frame::Video,
    decoded_tx: &std::sync::mpsc::Sender<DecodedFrame>,
    stats: FrameStats,
    decode_start: std::time::Instant,
) -> Result<(), DecodeError> {
    loop {
        match decoder.decoder.receive_frame(decoded_frame) {
            Ok(()) => {}
            Err(ffmpeg_next::Error::Other {
                errno: libc::EAGAIN,
            }) => break,
            Err(e) => {
                warn!("receive_frame error: {e}");
                break;
            }
        }

        // If this is a hardware frame (VAAPI surface), transfer to CPU.
        let is_vaapi =
            decoder.hw_accel && decoded_frame.format() == ffmpeg_next::format::Pixel::VAAPI;

        if is_vaapi {
            let ret = unsafe {
                ffmpeg_sys_next::av_hwframe_transfer_data(
                    decoder.sw_frame.as_mut_ptr(),
                    decoded_frame.as_ptr(),
                    0,
                )
            };
            if ret < 0 {
                warn!("av_hwframe_transfer_data failed (error {ret}), skipping frame");
                continue;
            }
            decoder.sw_frame.set_pts(decoded_frame.pts());
        }

        // Pick the source frame: sw_frame after hw transfer, or decoded_frame.
        let (width, height, format) = if is_vaapi {
            (
                decoder.sw_frame.width(),
                decoder.sw_frame.height(),
                decoder.sw_frame.format(),
            )
        } else {
            (
                decoded_frame.width(),
                decoded_frame.height(),
                decoded_frame.format(),
            )
        };

        // Log decoded frame details for first few frames.
        static DIAG_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let diag_n = DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if diag_n < 5 {
            let src = if is_vaapi {
                &decoder.sw_frame
            } else {
                &*decoded_frame
            };
            info!(
                frame = diag_n,
                decoded_width = width,
                decoded_height = height,
                decoded_format = ?format,
                hw_accel = decoder.hw_accel,
                planes = src.planes(),
                stride_0 = plane_stride(src, 0),
                stride_1 = plane_stride(src, 1),
                stride_2 = plane_stride(src, 2),
                "Decoded frame diagnostics"
            );
        }

        // Convert to YUV420P planes for the renderer.
        // VAAPI typically transfers to NV12; software decode outputs YUV420P.
        let mut output = if format == ffmpeg_next::format::Pixel::NV12 {
            let src = if is_vaapi {
                &decoder.sw_frame
            } else {
                &*decoded_frame
            };
            extract_nv12_to_yuv420p(src, width, height)
        } else if format == ffmpeg_next::format::Pixel::YUV420P {
            let src = if is_vaapi {
                &decoder.sw_frame
            } else {
                &*decoded_frame
            };
            extract_yuv420p(src, width, height)
        } else {
            // Arbitrary format — use sws_scale to convert.  We must avoid
            // holding a borrow on decoder.sw_frame while mutably borrowing
            // decoder for the scaler, so always use decoded_frame here (the
            // scaler handles any format, and this path is never VAAPI).
            let yuv_frame = scale_to_yuv420p(decoder, decoded_frame, width, height)?;
            extract_yuv420p(&yuv_frame, width, height)
        };

        output.stats = FrameStats {
            decode_us: saturating_us(decode_start.elapsed()),
            ..stats
        };

        if decoded_tx.send(output).is_err() {
            return Ok(());
        }
    }

    Ok(())
}

/// Returns the stride of plane `index`, or 0 if the frame has fewer planes.
///
/// `ffmpeg_next::frame::Video::stride` panics with "out of bounds" when the
/// plane index is >= `planes()` (e.g. plane 2 of an NV12 frame, which only
/// has 2 planes).  Any stride query on a frame whose format isn't known in
/// advance must go through this.
fn plane_stride(frame: &ffmpeg_next::frame::Video, index: usize) -> usize {
    if index < frame.planes() {
        frame.stride(index)
    } else {
        0
    }
}

/// Extracts YUV420P planes from a frame, stripping stride padding.
fn extract_yuv420p(frame: &ffmpeg_next::frame::Video, width: u32, height: u32) -> DecodedFrame {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let ch = h / 2;

    let y_stride = frame.stride(0);
    let u_stride = frame.stride(1);
    let v_stride = frame.stride(2);
    let y_data = frame.data(0);
    let u_data = frame.data(1);
    let v_data = frame.data(2);

    let y_plane = copy_plane(y_data, y_stride, w, h);
    let u_plane = copy_plane(u_data, u_stride, cw, ch);
    let v_plane = copy_plane(v_data, v_stride, cw, ch);

    DecodedFrame {
        y_plane,
        u_plane,
        v_plane,
        width,
        height,
        pts: frame.pts().map_or(0, i64::cast_unsigned),
        stats: FrameStats::default(),
    }
}

/// Extracts YUV420P planes from an NV12 frame (interleaved UV → split U + V).
fn extract_nv12_to_yuv420p(
    frame: &ffmpeg_next::frame::Video,
    width: u32,
    height: u32,
) -> DecodedFrame {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let ch = h / 2;

    let y_stride = frame.stride(0);
    let uv_stride = frame.stride(1);
    let y_data = frame.data(0);
    let uv_data = frame.data(1);

    let y_plane = copy_plane(y_data, y_stride, w, h);

    // NV12: UV plane is interleaved (U0 V0 U1 V1 ...).  Split into separate
    // U and V planes.  Row-sliced with exact chunks so the inner loop has
    // no bounds checks and vectorizes — this runs per frame on the full
    // chroma plane, so the scalar version showed up in decode time.
    let mut u_plane = vec![0u8; cw * ch];
    let mut v_plane = vec![0u8; cw * ch];
    for row in 0..ch {
        let src = &uv_data[row * uv_stride..row * uv_stride + cw * 2];
        let dst_u = &mut u_plane[row * cw..(row + 1) * cw];
        let dst_v = &mut v_plane[row * cw..(row + 1) * cw];
        for ((pair, u), v) in src.chunks_exact(2).zip(dst_u).zip(dst_v) {
            *u = pair[0];
            *v = pair[1];
        }
    }

    DecodedFrame {
        y_plane,
        u_plane,
        v_plane,
        width,
        height,
        pts: frame.pts().map_or(0, i64::cast_unsigned),
        stats: FrameStats::default(),
    }
}

/// Copies a single plane, stripping stride padding.
/// Fast path when stride == width (no padding).
fn copy_plane(data: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    if stride == width {
        data[..width * height].to_vec()
    } else {
        let mut buf = Vec::with_capacity(width * height);
        for row in 0..height {
            let off = row * stride;
            buf.extend_from_slice(&data[off..off + width]);
        }
        buf
    }
}

/// Uses sws_scale to convert an arbitrary pixel format to YUV420P.
fn scale_to_yuv420p(
    decoder: &mut FfmpegDecoder,
    src: &ffmpeg_next::frame::Video,
    width: u32,
    height: u32,
) -> Result<ffmpeg_next::frame::Video, DecodeError> {
    let needs_new = decoder
        .scaler
        .as_ref()
        .is_none_or(|s| s.input().width != width || s.input().height != height);

    if needs_new {
        let scaler = ffmpeg_next::software::scaling::Context::get(
            src.format(),
            width,
            height,
            ffmpeg_next::format::Pixel::YUV420P,
            width,
            height,
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )
        .map_err(|e| DecodeError::FfmpegError(format!("failed to create scaler: {e}")))?;
        decoder.scaler = Some(scaler);
    }

    let scaler = decoder.scaler.as_mut().expect("scaler just created");
    let mut yuv =
        ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, width, height);
    scaler
        .run(src, &mut yuv)
        .map_err(|e| DecodeError::FfmpegError(format!("scaler run failed: {e}")))?;
    yuv.set_pts(src.pts());
    Ok(yuv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stargaze_core::config::Codec;

    #[test]
    fn decoder_init_h265_succeeds() {
        let config = DecoderConfig {
            width: 1920,
            height: 1080,
            codec: Codec::H265,
        };
        let result = init_decoder(&config);
        assert!(
            result.is_ok(),
            "H.265 decoder should initialize successfully"
        );
    }

    /// Regression test: VAAPI frames transferred to CPU are NV12, which has
    /// only 2 planes.  Querying `stride(2)` on such a frame panics inside
    /// ffmpeg-next ("out of bounds") and used to crash the decoder thread
    /// when the frame diagnostics logged `stride_2` unconditionally.
    #[test]
    fn plane_stride_does_not_panic_on_nv12_two_plane_frame() {
        ffmpeg_next::init().expect("ffmpeg init");
        let frame = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::NV12, 64, 64);
        assert_eq!(frame.planes(), 2, "NV12 must have exactly 2 planes");
        assert!(plane_stride(&frame, 0) >= 64);
        assert!(plane_stride(&frame, 1) >= 64);
        // frame.stride(2) would panic here; plane_stride must return 0.
        assert_eq!(plane_stride(&frame, 2), 0);
    }

    #[test]
    fn plane_stride_handles_empty_frame() {
        ffmpeg_next::init().expect("ffmpeg init");
        let frame = ffmpeg_next::frame::Video::empty();
        assert_eq!(plane_stride(&frame, 0), 0);
        assert_eq!(plane_stride(&frame, 1), 0);
        assert_eq!(plane_stride(&frame, 2), 0);
    }

    /// NV12 → YUV420P extraction must deinterleave the UV plane correctly
    /// and produce planes sized for the visible area (stride stripped).
    #[test]
    fn extract_nv12_deinterleaves_uv_and_strips_stride() {
        ffmpeg_next::init().expect("ffmpeg init");
        let (w, h) = (64u32, 36u32);
        let mut frame = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::NV12, w, h);
        frame.set_pts(Some(42));

        frame.data_mut(0).fill(0x11);
        // Interleave U=0xAA / V=0x55 in the UV plane.
        for chunk in frame.data_mut(1).chunks_exact_mut(2) {
            chunk[0] = 0xAA;
            chunk[1] = 0x55;
        }

        let out = extract_nv12_to_yuv420p(&frame, w, h);
        let (w, h) = (w as usize, h as usize);
        assert_eq!(out.y_plane.len(), w * h);
        assert_eq!(out.u_plane.len(), (w / 2) * (h / 2));
        assert_eq!(out.v_plane.len(), (w / 2) * (h / 2));
        assert!(out.y_plane.iter().all(|&b| b == 0x11));
        assert!(out.u_plane.iter().all(|&b| b == 0xAA));
        assert!(out.v_plane.iter().all(|&b| b == 0x55));
        assert_eq!(out.pts, 42);
    }

    #[test]
    fn extract_yuv420p_strips_stride() {
        ffmpeg_next::init().expect("ffmpeg init");
        let (w, h) = (64u32, 36u32);
        let mut frame = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, w, h);
        frame.data_mut(0).fill(1);
        frame.data_mut(1).fill(2);
        frame.data_mut(2).fill(3);

        let out = extract_yuv420p(&frame, w, h);
        let (w, h) = (w as usize, h as usize);
        assert_eq!(out.y_plane.len(), w * h);
        assert_eq!(out.u_plane.len(), (w / 2) * (h / 2));
        assert_eq!(out.v_plane.len(), (w / 2) * (h / 2));
        assert!(out.u_plane.iter().all(|&b| b == 2));
        assert!(out.v_plane.iter().all(|&b| b == 3));
    }

    #[test]
    fn decoder_init_rejects_av1() {
        let config = DecoderConfig {
            width: 1920,
            height: 1080,
            codec: Codec::Av1,
        };
        let result = init_decoder(&config);
        assert!(result.is_err(), "AV1 should be rejected");
        match result {
            Err(DecodeError::UnsupportedCodec(_)) => {}
            Err(e) => panic!("Expected UnsupportedCodec, got: {e:?}"),
            Ok(_) => panic!("Expected error for AV1 codec"),
        }
    }
}
