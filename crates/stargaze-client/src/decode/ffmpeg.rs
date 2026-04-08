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
use stargaze_core::decode::{DecodeError, DecodedFrame, DecoderConfig};
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

/// Attempts to initialize a VAAPI-accelerated H.265 decoder.
fn init_vaapi_decoder(_config: &DecoderConfig) -> Result<FfmpegDecoder, DecodeError> {
    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC)
        .ok_or_else(|| DecodeError::InitError("hevc decoder not found".to_string()))?;

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
    // hardware surfaces.
    unsafe {
        let raw_ctx = context.as_mut_ptr();
        (*raw_ctx).hw_device_ctx = ffmpeg_sys_next::av_buffer_ref(hw_device_ctx);
        if (*raw_ctx).hw_device_ctx.is_null() {
            ffmpeg_sys_next::av_buffer_unref(&raw mut hw_device_ctx);
            return Err(DecodeError::InitError(
                "av_buffer_ref failed for hw_device_ctx".to_string(),
            ));
        }
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

    // Enable multi-threaded decoding (auto-detect core count).
    context.set_threading(ffmpeg_next::codec::threading::Config {
        kind: ffmpeg_next::codec::threading::Type::Frame,
        count: 0,
    });

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

        let Some(mut frame) = frames_rx.blocking_recv() else {
            info!("Reassembled frame channel closed, flushing decoder");
            break;
        };

        // Drain any queued frames, keeping only the latest.  Always prefer
        // a keyframe — it resets the decoder and avoids reference corruption.
        while let Ok(newer) = frames_rx.try_recv() {
            if newer.is_keyframe || !frame.is_keyframe {
                frame = newer;
            }
        }

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

        let mut packet = ffmpeg_next::Packet::copy(&frame.data);
        packet.set_pts(Some(frame.pts.cast_signed()));

        if let Err(e) = decoder.decoder.send_packet(&packet) {
            warn!(pts = frame.pts, "Skipping corrupt packet: {e}");
            continue;
        }

        drain_decoded_frames(decoder, &mut decoded_frame, decoded_tx)?;
    }

    // Flush: send EOF and drain remaining frames.
    if let Err(e) = decoder.decoder.send_eof() {
        warn!("Failed to send EOF to decoder: {e}");
    } else {
        drain_decoded_frames(decoder, &mut decoded_frame, decoded_tx)?;
    }

    info!("Decoder loop finished");
    Ok(())
}

/// Drains all available decoded frames from the codec, converts to YUV420P
/// planes, and sends them to the renderer.
fn drain_decoded_frames(
    decoder: &mut FfmpegDecoder,
    decoded_frame: &mut ffmpeg_next::frame::Video,
    decoded_tx: &std::sync::mpsc::Sender<DecodedFrame>,
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
                stride_0 = src.stride(0),
                stride_1 = src.stride(1),
                stride_2 = src.stride(2),
                "Decoded frame diagnostics"
            );
        }

        // Convert to YUV420P planes for the renderer.
        // VAAPI typically transfers to NV12; software decode outputs YUV420P.
        let output = if format == ffmpeg_next::format::Pixel::NV12 {
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

        if decoded_tx.send(output).is_err() {
            return Ok(());
        }
    }

    Ok(())
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
    // U and V planes.
    let mut u_plane = Vec::with_capacity(cw * ch);
    let mut v_plane = Vec::with_capacity(cw * ch);
    for row in 0..ch {
        let src_off = row * uv_stride;
        for col in 0..cw {
            u_plane.push(uv_data[src_off + col * 2]);
            v_plane.push(uv_data[src_off + col * 2 + 1]);
        }
    }

    DecodedFrame {
        y_plane,
        u_plane,
        v_plane,
        width,
        height,
        pts: frame.pts().map_or(0, i64::cast_unsigned),
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
