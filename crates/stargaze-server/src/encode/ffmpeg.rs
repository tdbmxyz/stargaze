//! `FFmpeg` NVENC encoder internals.
//!
//! Handles CUDA hardware context setup, codec configuration,
//! and the synchronous encode loop. All `FFmpeg` interaction is
//! confined to this module.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::capture::Frame;
use stargaze_core::encode::{EncodeError, EncodedPacket, EncoderConfig};
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, trace, warn};

/// Opaque handle to initialized `FFmpeg` encoder state.
///
/// Owns the codec context, hardware device context, and hardware frames context.
/// All fields are used through the `FFmpeg` safe wrappers except for the raw
/// hardware context pointers which require `ffmpeg-sys-next` FFI.
pub(crate) struct FfmpegEncoder {
    /// Opened H.265 NVENC encoder (owns the `AVCodecContext`).
    encoder: ffmpeg_next::encoder::video::Encoder,
    /// Raw pointer to the CUDA hardware device context (`AVBufferRef`).
    /// Owned — must be freed via `av_buffer_unref` on drop.
    hw_device_ctx: *mut ffmpeg_sys_next::AVBufferRef,
    /// Raw pointer to the hardware frames context (`AVBufferRef`).
    /// Owned — must be freed via `av_buffer_unref` on drop.
    hw_frames_ctx: *mut ffmpeg_sys_next::AVBufferRef,
    /// Encoder configuration (width, height, framerate, bitrate).
    #[allow(dead_code)]
    config: EncoderConfig,
}

// Safety: FfmpegEncoder is only used on the dedicated encoder thread.
// FFmpeg contexts are not thread-safe, but we never share them across threads.
unsafe impl Send for FfmpegEncoder {}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.hw_frames_ctx.is_null() {
                ffmpeg_sys_next::av_buffer_unref(&raw mut self.hw_frames_ctx);
            }
            if !self.hw_device_ctx.is_null() {
                ffmpeg_sys_next::av_buffer_unref(&raw mut self.hw_device_ctx);
            }
        }
    }
}

/// Initializes the `FFmpeg` NVENC encoder with CUDA hardware acceleration.
///
/// Sets up:
/// 1. CUDA device context (`av_hwdevice_ctx_create`)
/// 2. Hardware frames context (`av_hwframe_ctx_alloc` + `av_hwframe_ctx_init`)
/// 3. H.265 NVENC codec context with ultra-low-latency settings
///
/// # Errors
///
/// Returns `EncodeError::InitError` if any `FFmpeg` initialization step fails
/// (NVENC not available, CUDA device not found, etc.).
#[allow(clippy::too_many_lines)]
pub(crate) fn init_encoder(config: &EncoderConfig) -> Result<FfmpegEncoder, EncodeError> {
    // Initialize FFmpeg (safe to call multiple times).
    ffmpeg_next::init().map_err(|e| EncodeError::InitError(format!("ffmpeg init: {e}")))?;

    // Step 1: Find the hevc_nvenc encoder.
    let codec = ffmpeg_next::encoder::find_by_name("hevc_nvenc").ok_or_else(|| {
        EncodeError::InitError(
            "hevc_nvenc encoder not found — is FFmpeg compiled with NVENC support?".to_string(),
        )
    })?;

    info!("Found hevc_nvenc encoder: {}", codec.name());

    // Step 2: Create CUDA hardware device context.
    let mut hw_device_ctx: *mut ffmpeg_sys_next::AVBufferRef = ptr::null_mut();
    let ret = unsafe {
        ffmpeg_sys_next::av_hwdevice_ctx_create(
            &raw mut hw_device_ctx,
            ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            ptr::null(),     // default CUDA device
            ptr::null_mut(), // no options
            0,               // no flags
        )
    };
    if ret < 0 {
        return Err(EncodeError::InitError(format!(
            "failed to create CUDA device context (error code {ret}) — is an NVIDIA GPU available?"
        )));
    }
    debug!("Created CUDA device context");

    // Step 3: Allocate and configure hardware frames context.
    let hw_frames_ctx = unsafe { ffmpeg_sys_next::av_hwframe_ctx_alloc(hw_device_ctx) };
    if hw_frames_ctx.is_null() {
        unsafe { ffmpeg_sys_next::av_buffer_unref(&raw mut hw_device_ctx) };
        return Err(EncodeError::InitError(
            "failed to allocate hardware frames context".to_string(),
        ));
    }

    unsafe {
        #[allow(clippy::cast_ptr_alignment)]
        let frames_ctx = (*hw_frames_ctx)
            .data
            .cast::<ffmpeg_sys_next::AVHWFramesContext>();
        (*frames_ctx).format = ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*frames_ctx).sw_format = ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_NV12;
        (*frames_ctx).width = config.width.cast_signed();
        (*frames_ctx).height = config.height.cast_signed();
        (*frames_ctx).initial_pool_size = 0; // on-demand allocation
    }

    let ret = unsafe { ffmpeg_sys_next::av_hwframe_ctx_init(hw_frames_ctx) };
    if ret < 0 {
        unsafe {
            let mut hw_frames_ptr = hw_frames_ctx;
            ffmpeg_sys_next::av_buffer_unref(&raw mut hw_frames_ptr);
            ffmpeg_sys_next::av_buffer_unref(&raw mut hw_device_ctx);
        };
        return Err(EncodeError::InitError(format!(
            "failed to initialize hardware frames context (error code {ret})"
        )));
    }
    debug!(
        "Initialized CUDA hardware frames context ({}x{}, NV12)",
        config.width, config.height
    );

    // Step 4: Create codec context and configure.
    let mut ctx = ffmpeg_next::codec::context::Context::new_with_codec(codec);

    // Attach hardware contexts before configuring the encoder.
    unsafe {
        let raw_ctx = ctx.as_mut_ptr();
        (*raw_ctx).hw_device_ctx = ffmpeg_sys_next::av_buffer_ref(hw_device_ctx);
        (*raw_ctx).hw_frames_ctx = ffmpeg_sys_next::av_buffer_ref(hw_frames_ctx);
    }

    let mut encoder = ctx.encoder().video().map_err(|e| {
        EncodeError::InitError(format!("failed to create video encoder context: {e}"))
    })?;

    // Configure codec context.
    encoder.set_width(config.width);
    encoder.set_height(config.height);
    encoder.set_format(ffmpeg_next::format::Pixel::CUDA);
    encoder.set_time_base(ffmpeg_next::Rational(1, config.framerate.cast_signed()));
    encoder.set_frame_rate(Some(ffmpeg_next::Rational(
        config.framerate.cast_signed(),
        1,
    )));
    encoder.set_bit_rate(config.bitrate_mbps as usize * 1_000_000);
    encoder.set_max_b_frames(0);
    encoder.set_gop(config.framerate * 2); // keyframe every ~2 seconds

    // Set color space parameters.
    unsafe {
        let raw_ctx = encoder.as_mut_ptr();
        (*raw_ctx).color_range = ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG;
        (*raw_ctx).colorspace = ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT709;
        (*raw_ctx).color_primaries = ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT709;
        (*raw_ctx).color_trc = ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    }

    // Step 5: Open encoder with NVENC-specific options.
    let mut opts = ffmpeg_next::Dictionary::new();
    opts.set("preset", "p1");
    opts.set("tune", "ull");
    opts.set("rc", "cbr");
    opts.set("delay", "0");
    opts.set("forced-idr", "1");
    opts.set("zerolatency", "1");

    let opened = encoder
        .open_with(opts)
        .map_err(|e| EncodeError::InitError(format!("failed to open hevc_nvenc encoder: {e}")))?;

    info!(
        "NVENC encoder initialized: {}x{} @ {}fps, {} Mbps, H.265",
        config.width, config.height, config.framerate, config.bitrate_mbps
    );

    Ok(FfmpegEncoder {
        encoder: opened,
        hw_device_ctx,
        hw_frames_ctx,
        config: config.clone(),
    })
}

/// Runs the encode loop: receives frames, uploads to GPU, encodes, sends packets.
///
/// This function blocks until `shutdown` is signaled or the input channel closes.
/// It is meant to run on a dedicated `std::thread`.
///
/// The `idr_rx` watch channel is checked before each frame. When its value
/// changes (incremented by the transport layer), the next frame is forced
/// to be an IDR keyframe by setting `AV_PICTURE_TYPE_I`.
///
/// # Errors
///
/// Returns `EncodeError` if a fatal encoding error occurs. Non-fatal errors
/// (e.g., a single frame upload failure) are logged and skipped.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn run_encode_loop(
    encoder: &mut FfmpegEncoder,
    frames: &mut mpsc::Receiver<Frame>,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    shutdown: &Arc<AtomicBool>,
    mut idr_rx: watch::Receiver<u64>,
) -> Result<(), EncodeError> {
    let mut frame_counter: u64 = 0;
    let mut last_idr_value: u64 = 0;
    let mut packet = ffmpeg_next::Packet::empty();

    loop {
        // Check shutdown flag.
        if shutdown.load(Ordering::Relaxed) {
            debug!("Shutdown signaled, flushing encoder");
            break;
        }

        // Blocking receive from capture channel.
        let Some(frame) = frames.blocking_recv() else {
            info!("Capture channel closed, flushing encoder");
            break;
        };

        // Check if an IDR keyframe was requested.
        let current_idr = *idr_rx.borrow_and_update();
        let force_idr = current_idr != last_idr_value;
        if force_idr {
            last_idr_value = current_idr;
            debug!(
                frame = frame_counter,
                "Forcing IDR keyframe (requested by client)"
            );
        }

        // Upload frame to GPU and encode.
        match upload_and_encode(encoder, &frame, frame_counter, force_idr) {
            Ok(()) => {}
            Err(e) => {
                warn!(frame = frame_counter, "Skipping frame: {e}");
                frame_counter += 1;
                continue;
            }
        }

        // Receive encoded packets.
        drain_packets(&mut encoder.encoder, &mut packet, packets_tx, frame_counter);

        frame_counter += 1;
        if frame_counter.is_multiple_of(300) {
            trace!(frame = frame_counter, "Encode progress");
        }
    }

    // Flush: send null frame to drain the encoder.
    flush_encoder(&mut encoder.encoder, &mut packet, packets_tx);

    info!(total_frames = frame_counter, "Encoder loop finished");
    Ok(())
}

/// Uploads a captured frame to a GPU hardware frame and sends it to the encoder.
fn upload_and_encode(
    encoder: &mut FfmpegEncoder,
    frame: &Frame,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    let (data, width, height, stride) = match frame {
        Frame::CpuMapped {
            data,
            width,
            height,
            stride,
            ..
        } => (data.as_slice(), *width, *height, *stride),
        Frame::DmaBuf(info) => {
            return upload_dmabuf_and_encode(encoder, info, pts, force_idr);
        }
    };

    // Create software frame with BGRA pixel data.
    let mut sw_frame =
        ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::BGRA, width, height);

    // Copy pixel data into the software frame line by line.
    let dst_stride = sw_frame.stride(0);
    let dst_data = sw_frame.data_mut(0);
    for y in 0..height as usize {
        let src_offset = y * stride as usize;
        let dst_offset = y * dst_stride;
        let copy_len = (width as usize * 4).min(stride as usize).min(dst_stride);
        if src_offset + copy_len <= data.len() && dst_offset + copy_len <= dst_data.len() {
            dst_data[dst_offset..dst_offset + copy_len]
                .copy_from_slice(&data[src_offset..src_offset + copy_len]);
        }
    }

    // Allocate a hardware frame from the pool.
    let mut hw_frame = ffmpeg_next::frame::Video::empty();
    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_get_buffer(encoder.hw_frames_ctx, hw_frame.as_mut_ptr(), 0)
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_get_buffer failed (error code {ret})"),
        });
    }

    // Upload SW frame -> HW frame (handles BGRA->NV12 conversion).
    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_transfer_data(hw_frame.as_mut_ptr(), sw_frame.as_ptr(), 0)
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_transfer_data failed (error code {ret})"),
        });
    }

    // Set PTS and send to encoder.
    hw_frame.set_pts(Some(pts.cast_signed()));
    if force_idr {
        unsafe {
            (*hw_frame.as_mut_ptr()).pict_type = ffmpeg_sys_next::AVPictureType::AV_PICTURE_TYPE_I;
        }
    }
    encoder
        .encoder
        .send_frame(&hw_frame)
        .map_err(|e| EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("avcodec_send_frame failed: {e}"),
        })?;

    Ok(())
}

/// Uploads a `DMA-BUF` frame to the encoder by `mmap`-ing the fd.
fn upload_dmabuf_and_encode(
    encoder: &mut FfmpegEncoder,
    info: &stargaze_core::capture::DmaBufInfo,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    use std::os::unix::io::AsRawFd;

    let size = (info.stride * info.height) as usize;
    if size == 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: "DMA-BUF has zero size".to_string(),
        });
    }

    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            info.fd.as_raw_fd(),
            i64::from(info.offset),
        )
    };

    if ptr == libc::MAP_FAILED {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: "mmap of DMA-BUF fd failed".to_string(),
        });
    }

    let data = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) };

    let mut sw_frame =
        ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::BGRA, info.width, info.height);

    let dst_stride = sw_frame.stride(0);
    let dst_data = sw_frame.data_mut(0);
    for y in 0..info.height as usize {
        let src_offset = y * info.stride as usize;
        let dst_offset = y * dst_stride;
        let copy_len = (info.width as usize * 4)
            .min(info.stride as usize)
            .min(dst_stride);
        if src_offset + copy_len <= data.len() && dst_offset + copy_len <= dst_data.len() {
            dst_data[dst_offset..dst_offset + copy_len]
                .copy_from_slice(&data[src_offset..src_offset + copy_len]);
        }
    }

    unsafe {
        libc::munmap(ptr, size);
    }

    let mut hw_frame = ffmpeg_next::frame::Video::empty();
    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_get_buffer(encoder.hw_frames_ctx, hw_frame.as_mut_ptr(), 0)
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_get_buffer failed (error code {ret})"),
        });
    }

    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_transfer_data(hw_frame.as_mut_ptr(), sw_frame.as_ptr(), 0)
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_transfer_data failed (error code {ret})"),
        });
    }

    hw_frame.set_pts(Some(pts.cast_signed()));
    if force_idr {
        unsafe {
            (*hw_frame.as_mut_ptr()).pict_type = ffmpeg_sys_next::AVPictureType::AV_PICTURE_TYPE_I;
        }
    }
    encoder
        .encoder
        .send_frame(&hw_frame)
        .map_err(|e| EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("avcodec_send_frame failed: {e}"),
        })?;

    Ok(())
}

/// Drains all available packets from the encoder after sending a frame.
#[allow(clippy::similar_names)]
fn drain_packets(
    enc: &mut ffmpeg_next::encoder::video::Encoder,
    packet: &mut ffmpeg_next::Packet,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    frame_counter: u64,
) {
    loop {
        match enc.receive_packet(packet) {
            Ok(()) => {
                let pkt = EncodedPacket {
                    data: packet.data().map_or_else(Vec::new, <[u8]>::to_vec),
                    pts: packet.pts().unwrap_or(0).cast_unsigned(),
                    is_keyframe: packet.is_key(),
                };

                if pkt.is_keyframe {
                    debug!(pts = pkt.pts, size = pkt.data.len(), "Keyframe encoded");
                }

                if packets_tx.blocking_send(pkt).is_err() {
                    warn!("Packet receiver dropped, stopping encoder");
                    return;
                }
            }
            Err(ffmpeg_next::Error::Other {
                errno: libc::EAGAIN,
            }) => {
                break;
            }
            Err(e) => {
                error!(frame = frame_counter, "receive_packet error: {e}");
                break;
            }
        }
    }
}

/// Flushes the encoder by sending a null frame and draining remaining packets.
#[allow(clippy::similar_names)]
fn flush_encoder(
    enc: &mut ffmpeg_next::encoder::video::Encoder,
    packet: &mut ffmpeg_next::Packet,
    packets_tx: &mpsc::Sender<EncodedPacket>,
) {
    debug!("Flushing encoder (sending EOF)");

    if let Err(e) = enc.send_eof() {
        warn!("Failed to send EOF to encoder: {e}");
        return;
    }

    while let Ok(()) = enc.receive_packet(packet) {
        let pkt = EncodedPacket {
            data: packet.data().map_or_else(Vec::new, <[u8]>::to_vec),
            pts: packet.pts().unwrap_or(0).cast_unsigned(),
            is_keyframe: packet.is_key(),
        };

        if packets_tx.blocking_send(pkt).is_err() {
            break;
        }
    }

    debug!("Encoder flushed");
}
