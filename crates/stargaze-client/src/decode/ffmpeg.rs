//! `FFmpeg` H.265 software decoder internals.
//!
//! Handles codec initialization and the synchronous decode loop.
//! All `FFmpeg` interaction is confined to this module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::config::Codec;
use stargaze_core::decode::{DecodeError, DecodedFrame, DecoderConfig};
use stargaze_core::transport::ReassembledFrame;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Opaque handle to initialized `FFmpeg` decoder state.
///
/// Owns the codec context and an optional scaler to YUV420P.
pub(crate) struct FfmpegDecoder {
    /// Opened H.265 software decoder (owns the `AVCodecContext`).
    decoder: ffmpeg_next::decoder::Video,
    /// Lazily created scaler to YUV420P.
    /// Created on first decoded frame when the output format is known.
    scaler: Option<ffmpeg_next::software::scaling::Context>,
}

/// Initializes the `FFmpeg` H.265 software decoder.
///
/// # Errors
///
/// Returns `DecodeError::InitError` if `FFmpeg` initialization fails or
/// the HEVC codec cannot be found/opened.
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

    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC).ok_or_else(|| {
        DecodeError::InitError(
            "hevc decoder not found — is FFmpeg compiled with H.265 support?".to_string(),
        )
    })?;

    let context = ffmpeg_next::codec::context::Context::new_with_codec(codec);
    let decoder = context
        .decoder()
        .video()
        .map_err(|e| DecodeError::InitError(format!("failed to open hevc decoder: {e}")))?;

    info!(
        width = config.width,
        height = config.height,
        "H.265 software decoder initialized"
    );

    Ok(FfmpegDecoder {
        decoder,
        scaler: None,
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

/// Drains all available decoded frames from the codec and converts them to YUV420P.
///
/// Returns `Ok(())` normally, or `Ok(())` if the receiver was dropped (clean shutdown).
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

        let width = decoded_frame.width();
        let height = decoded_frame.height();

        // Log decoded frame details for first few frames to diagnose banding.
        static DIAG_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let diag_n = DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if diag_n < 5 {
            info!(
                frame = diag_n,
                decoded_width = width,
                decoded_height = height,
                decoded_format = ?decoded_frame.format(),
                decoded_stride_0 = decoded_frame.stride(0),
                decoded_stride_1 = decoded_frame.stride(1),
                decoded_stride_2 = decoded_frame.stride(2),
                "Decoded frame diagnostics (pre-scaler)"
            );
        }

        // Create or recreate the scaler if dimensions/format changed or on first use.
        let needs_new_scaler = decoder
            .scaler
            .as_ref()
            .is_none_or(|s| s.input().width != width || s.input().height != height);

        if needs_new_scaler {
            let scaler = ffmpeg_next::software::scaling::Context::get(
                decoded_frame.format(),
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

        let scaler = decoder
            .scaler
            .as_mut()
            .expect("scaler was just created above");

        let mut yuv_frame =
            ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, width, height);

        scaler
            .run(decoded_frame, &mut yuv_frame)
            .map_err(|e| DecodeError::FfmpegError(format!("scaler run failed: {e}")))?;

        let width_usize = width as usize;
        let height_usize = height as usize;
        let chroma_width = width_usize / 2;
        let chroma_height = height_usize / 2;

        let y_stride = yuv_frame.stride(0);
        let u_stride = yuv_frame.stride(1);
        let v_stride = yuv_frame.stride(2);
        let y_data = yuv_frame.data(0);
        let u_data = yuv_frame.data(1);
        let v_data = yuv_frame.data(2);

        if diag_n < 5 {
            info!(
                frame = diag_n,
                yuv_width = width,
                yuv_height = height,
                y_stride,
                u_stride,
                v_stride,
                y_data_len = y_data.len(),
                u_data_len = u_data.len(),
                v_data_len = v_data.len(),
                width_usize,
                chroma_width,
                chroma_height,
                y_plane_expected = width_usize * height_usize,
                u_plane_expected = chroma_width * chroma_height,
                "Post-scaler YUV420P diagnostics"
            );
        }
        let mut y_plane = Vec::with_capacity(width_usize * height_usize);
        for row in 0..height_usize {
            let src = row * y_stride;
            y_plane.extend_from_slice(&y_data[src..src + width_usize]);
        }

        let mut u_plane = Vec::with_capacity(chroma_width * chroma_height);
        for row in 0..chroma_height {
            let src = row * u_stride;
            u_plane.extend_from_slice(&u_data[src..src + chroma_width]);
        }

        let mut v_plane = Vec::with_capacity(chroma_width * chroma_height);
        for row in 0..chroma_height {
            let src = row * v_stride;
            v_plane.extend_from_slice(&v_data[src..src + chroma_width]);
        }

        let pts = decoded_frame.pts().map_or(0, i64::cast_unsigned);

        let output_frame = DecodedFrame {
            y_plane,
            u_plane,
            v_plane,
            width,
            height,
            pts,
        };

        if decoded_tx.send(output_frame).is_err() {
            return Ok(());
        }
    }

    Ok(())
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
