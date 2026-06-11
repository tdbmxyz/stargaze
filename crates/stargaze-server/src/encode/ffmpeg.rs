//! `FFmpeg` NVENC encoder internals.
//!
//! Handles CUDA hardware context setup, codec configuration,
//! and the synchronous encode loop. All `FFmpeg` interaction is
//! confined to this module.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::capture::{CapturedFrame, Frame, PixelFormat};
use stargaze_core::encode::{EncodeError, EncodedPacket, EncoderConfig};
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

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
    /// CUDA context extracted from `FFmpeg`'s `AVCUDADeviceContext`.
    /// Must be pushed as current before calling CUDA driver API functions
    /// (e.g. `cuImportExternalMemory`) from the encoder thread.
    cuda_ctx: cudarc::driver::sys::CUcontext,
    /// Encoder configuration (width, height, framerate, bitrate).
    #[allow(dead_code)]
    config: EncoderConfig,
    /// H.265 parameter sets (VPS/SPS/PPS) extracted from the encoder's
    /// `extradata` after initialization. Prepended to every keyframe so
    /// the decoder can start decoding from any keyframe without prior state.
    extradata: Vec<u8>,
    /// Software scaler (capture format → NV12). Lazily created on first frame
    /// so `av_hwframe_transfer_data` receives NV12 matching the hw context's
    /// `sw_format`.
    sw_scaler: Option<ffmpeg_next::software::scaling::Context>,
    /// EGL-GL-CUDA bridge for DMA-BUF import. Lazily initialized on first
    /// DMA-BUF frame (requires CUDA context to be active).
    egl_bridge: Option<super::egl_cuda::EglCudaBridge>,
    /// Consecutive GPU NV12 path failures. The path is disabled (with an
    /// error log) once this hits `MAX_GPU_PATH_FAILURES` so a persistent
    /// failure can't silently pin every frame to the slow CPU fallback.
    gpu_path_failures: u32,
}

/// Consecutive GPU-path failures after which the GPU NV12 path is disabled.
const MAX_GPU_PATH_FAILURES: u32 = 30;

// Safety: FfmpegEncoder is only used on the dedicated encoder thread.
// FFmpeg contexts are not thread-safe, but we never share them across threads.
unsafe impl Send for FfmpegEncoder {}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        // The EGL bridge holds CUDA resources (registered GL image, kernel
        // module, device buffer) that belong to the context owned by
        // `hw_device_ctx` — release them before the context can be
        // destroyed, with the context current as the driver API requires.
        if let Some(bridge) = self.egl_bridge.take() {
            unsafe {
                let res = cudarc::driver::sys::cuCtxPushCurrent_v2(self.cuda_ctx);
                drop(bridge);
                if res == cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                    let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
                    cudarc::driver::sys::cuCtxPopCurrent_v2(&raw mut old);
                }
            }
        }
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

    // Step 2: Pre-validate CUDA availability via cudarc before FFmpeg tries
    // to create a CUDA device context.  FFmpeg's av_hwdevice_ctx_create can
    // segfault if the CUDA driver is missing or broken rather than returning
    // an error code.  Calling cuInit(0) first surfaces the problem cleanly.
    match std::panic::catch_unwind(cudarc::driver::result::init) {
        Ok(Ok(())) => debug!("CUDA pre-check passed (cuInit succeeded)"),
        Ok(Err(e)) => {
            return Err(EncodeError::InitError(format!(
                "CUDA driver error during cuInit: {e:?} — is the NVIDIA driver loaded?"
            )));
        }
        Err(_) => {
            return Err(EncodeError::InitError(
                "CUDA driver library (libcuda.so) not found — \
                 is the NVIDIA driver installed and LD_LIBRARY_PATH set?"
                    .to_string(),
            ));
        }
    }

    // Step 3: Create CUDA hardware device context.
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

    // Step 4: Allocate and configure hardware frames context.
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

    // Step 5: Create codec context and configure.
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
    // Infinite GOP: no periodic keyframes. Periodic IDRs cause a visible
    // quality "pulse" every GOP and waste bitrate; the client requests an
    // IDR explicitly on packet loss (forced-idr below), which is all that's
    // needed — the same strategy Sunshine/Moonlight use.
    encoder.set_gop(i32::MAX.cast_unsigned());

    // Constrain the rate controller to a one-frame VBV buffer so every
    // frame (including on-demand IDRs) fits the per-frame bitrate budget.
    // Large keyframes otherwise burst over the link and arrive late.
    unsafe {
        let raw_ctx = encoder.as_mut_ptr();
        let per_frame_bits = (config.bitrate_mbps * 1_000_000 / config.framerate).cast_signed();
        (*raw_ctx).rc_buffer_size = per_frame_bits;
        (*raw_ctx).rc_max_rate = i64::from(config.bitrate_mbps) * 1_000_000;
    }

    // Set color space parameters.
    unsafe {
        let raw_ctx = encoder.as_mut_ptr();
        (*raw_ctx).color_range = ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG;
        (*raw_ctx).colorspace = ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT709;
        (*raw_ctx).color_primaries = ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT709;
        (*raw_ctx).color_trc = ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    }

    // Step 6: Open encoder with NVENC-specific options.
    //
    // Preset and multipass come from config (defaults p4/qres): the
    // quality difference over p1 is dramatic at streaming bitrates.
    // Spatial AQ distributes bits where the eye notices (text edges,
    // flat gradients) — without it the picture has the typical
    // "screen share" mosquito noise.
    let mut opts = ffmpeg_next::Dictionary::new();
    opts.set("preset", &config.tuning.preset);
    opts.set("tune", "ull");
    opts.set("rc", "cbr");
    opts.set("multipass", &config.tuning.multipass);
    opts.set("spatial-aq", "1");
    opts.set("aq-strength", "8");
    opts.set("delay", "0");
    opts.set("forced-idr", "1");
    opts.set("zerolatency", "1");

    let opened = encoder
        .open_with(opts)
        .map_err(|e| EncodeError::InitError(format!("failed to open hevc_nvenc encoder: {e}")))?;

    // Extract VPS/SPS/PPS from encoder extradata (NVENC stores parameter sets
    // here rather than inline in the bitstream).
    let extradata = unsafe {
        let ctx = opened.as_ptr();
        let ptr = (*ctx).extradata;
        let size = (*ctx).extradata_size;
        if !ptr.is_null() && size > 0 {
            let slice = std::slice::from_raw_parts(ptr, size.cast_unsigned() as usize);
            info!(
                size,
                first_bytes = ?&slice[..slice.len().min(32)],
                "Extracted encoder extradata (VPS/SPS/PPS)"
            );
            slice.to_vec()
        } else {
            warn!("No extradata from encoder — parameter sets should be inline in bitstream");
            Vec::new()
        }
    };

    // Extract CUcontext from FFmpeg's AVCUDADeviceContext.
    // Layout: AVBufferRef.data → AVHWDeviceContext.hwctx → AVCUDADeviceContext.cuda_ctx
    // AVCUDADeviceContext's first field is CUcontext (a pointer).
    let cuda_ctx: cudarc::driver::sys::CUcontext = unsafe {
        #[allow(clippy::cast_ptr_alignment)]
        let dev_ctx = (*hw_device_ctx)
            .data
            .cast::<ffmpeg_sys_next::AVHWDeviceContext>();
        let hwctx = (*dev_ctx).hwctx;
        *hwctx.cast::<cudarc::driver::sys::CUcontext>()
    };
    debug!("Extracted CUDA context from FFmpeg hw device: {cuda_ctx:?}");

    info!(
        "NVENC encoder initialized: {}x{} @ {}fps, {} Mbps, H.265 (preset {}, multipass {})",
        config.width,
        config.height,
        config.framerate,
        config.bitrate_mbps,
        config.tuning.preset,
        config.tuning.multipass
    );

    Ok(FfmpegEncoder {
        encoder: opened,
        hw_device_ctx,
        hw_frames_ctx,
        cuda_ctx,
        config: config.clone(),
        extradata,
        sw_scaler: None,
        egl_bridge: None,
        gpu_path_failures: 0,
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
    frames: &mut mpsc::Receiver<CapturedFrame>,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    shutdown: &Arc<AtomicBool>,
    mut idr_rx: watch::Receiver<u64>,
) -> Result<(), EncodeError> {
    let mut frame_counter: u64 = 0;
    let mut last_idr_value: u64 = 0;
    let mut packet = ffmpeg_next::Packet::empty();

    info!("Encoder loop started, waiting for frames from capture pipeline");

    loop {
        // Check shutdown flag.
        if shutdown.load(Ordering::Relaxed) {
            debug!("Shutdown signaled, flushing encoder");
            break;
        }

        // Blocking receive from capture channel.
        let Some(captured) = frames.blocking_recv() else {
            info!("Capture channel closed, flushing encoder");
            break;
        };
        let frame = captured.frame;

        // Check if an IDR keyframe was requested.
        let current_idr = *idr_rx.borrow_and_update();
        let force_idr = frame_counter == 0 || current_idr != last_idr_value;
        if force_idr {
            last_idr_value = current_idr;
            debug!(
                frame = frame_counter,
                "Forcing IDR keyframe (requested by client)"
            );
        }

        // Upload frame to GPU and encode.
        if frame_counter == 0 {
            info!("First frame received from capture pipeline, uploading to encoder");
        }
        let prep_start = std::time::Instant::now();
        let capture_us = saturating_us(prep_start - captured.captured_at);
        match upload_and_encode(encoder, &frame, frame_counter, force_idr) {
            Ok(()) => {}
            Err(e) => {
                warn!(frame = frame_counter, "Skipping frame: {e}");
                frame_counter += 1;
                continue;
            }
        }
        // Frame preparation: pixel conversion + GPU upload + send_frame.
        let convert_us = saturating_us(prep_start.elapsed());

        // Receive encoded packets.
        drain_packets(
            &mut encoder.encoder,
            &mut packet,
            packets_tx,
            frame_counter,
            &encoder.extradata,
            capture_us,
            convert_us,
            std::time::Instant::now(),
        );

        frame_counter += 1;
        if frame_counter == 1 || frame_counter.is_multiple_of(300) {
            info!(frame = frame_counter, "Encode progress");
        }
    }

    // Flush: send null frame to drain the encoder.
    flush_encoder(
        &mut encoder.encoder,
        &mut packet,
        packets_tx,
        &encoder.extradata,
    );

    info!(total_frames = frame_counter, "Encoder loop finished");
    Ok(())
}

/// Maps our capture `PixelFormat` to the corresponding `FFmpeg` pixel format.
fn capture_format_to_ffmpeg(fmt: PixelFormat) -> ffmpeg_next::format::Pixel {
    match fmt {
        PixelFormat::Bgra8 => ffmpeg_next::format::Pixel::BGRA,
        PixelFormat::Rgba8 => ffmpeg_next::format::Pixel::RGBA,
        PixelFormat::Nv12 => ffmpeg_next::format::Pixel::NV12,
        PixelFormat::Bgra10 | PixelFormat::Rgba10 => ffmpeg_next::format::Pixel::X2BGR10LE,
    }
}

/// Bytes per pixel for a capture format (used for stride calculations).
fn bytes_per_pixel(fmt: PixelFormat) -> usize {
    match fmt {
        PixelFormat::Bgra8 | PixelFormat::Rgba8 | PixelFormat::Bgra10 | PixelFormat::Rgba10 => 4,
        PixelFormat::Nv12 => 1, // NV12 is planar; stride = width for Y plane
    }
}

fn upload_and_encode(
    encoder: &mut FfmpegEncoder,
    frame: &Frame,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    match frame {
        Frame::CpuMapped {
            data,
            width,
            height,
            stride,
            format,
        } => {
            if pts < 3 {
                info!(
                    frame = pts,
                    ?format,
                    width,
                    height,
                    stride,
                    data_len = data.len(),
                    "Encoding CpuMapped frame"
                );
            }
            upload_cpu_data_and_encode(
                encoder,
                data.as_slice(),
                *width,
                *height,
                *stride,
                *format,
                pts,
                force_idr,
            )
        }
        Frame::DmaBuf(info) => {
            if pts < 3 {
                info!(
                    frame = pts,
                    ?info.format,
                    width = info.width,
                    height = info.height,
                    stride = info.stride,
                    modifier = format_args!("0x{:x}", info.modifier),
                    "Encoding DmaBuf frame"
                );
            }
            upload_dmabuf_and_encode(encoder, info, pts, force_idr)
        }
    }
}

/// Converts CPU-accessible pixel data to NV12, uploads to a CUDA hardware
/// frame, and sends it to the encoder. Used by both `CpuMapped` and
/// `DmaBuf` (mmap'd) paths.
///
/// 8-bit RGB formats take the multithreaded direct converter; other
/// formats (NV12 input, 10-bit) fall back to `sws_scale`.
#[allow(clippy::too_many_arguments)]
fn upload_cpu_data_and_encode(
    encoder: &mut FfmpegEncoder,
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    let w = width as usize;
    let h = height as usize;
    let mut nv12_frame =
        ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::NV12, width, height);

    // Fast path: direct parallel RGB→NV12 (BT.709, matching the encoder's
    // advertised colorspace). The sws path is single-threaded and was the
    // pipeline bottleneck at high resolutions.
    let min_len = (h - 1) * stride as usize + w * bytes_per_pixel(format);
    let direct = super::convert::channel_order(format).filter(|_| data.len() >= min_len);

    if let Some(order) = direct {
        let y_stride = nv12_frame.stride(0);
        let uv_stride = nv12_frame.stride(1);
        // Both planes are borrowed mutably at once; the raw slices are
        // disjoint (separate plane allocations within the frame buffer).
        unsafe {
            let raw = nv12_frame.as_mut_ptr();
            let y_plane = std::slice::from_raw_parts_mut((*raw).data[0], y_stride * h);
            let uv_plane = std::slice::from_raw_parts_mut((*raw).data[1], uv_stride * (h / 2));
            super::convert::convert_to_nv12(
                data,
                stride as usize,
                w,
                h,
                order,
                y_plane,
                y_stride,
                uv_plane,
                uv_stride,
            );
        }
    } else {
        upload_via_sws(
            encoder,
            data,
            width,
            height,
            stride,
            format,
            pts,
            &mut nv12_frame,
        )?;
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
        ffmpeg_sys_next::av_hwframe_transfer_data(hw_frame.as_mut_ptr(), nv12_frame.as_ptr(), 0)
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_transfer_data failed (error code {ret})"),
        });
    }

    send_hw_frame(encoder, &mut hw_frame, pts, force_idr)
}

/// Stamps pts / forced-IDR on a hardware frame and submits it to NVENC.
fn send_hw_frame(
    encoder: &mut FfmpegEncoder,
    hw_frame: &mut ffmpeg_next::frame::Video,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    hw_frame.set_pts(Some(pts.cast_signed()));
    if force_idr {
        unsafe {
            (*hw_frame.as_mut_ptr()).pict_type = ffmpeg_sys_next::AVPictureType::AV_PICTURE_TYPE_I;
        }
    }
    encoder
        .encoder
        .send_frame(hw_frame)
        .map_err(|e| EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("avcodec_send_frame failed: {e}"),
        })
}

/// Legacy conversion path via `sws_scale` for formats the direct
/// converter doesn't handle (NV12 input, 10-bit RGB).
#[allow(clippy::too_many_arguments)]
fn upload_via_sws(
    encoder: &mut FfmpegEncoder,
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
    pts: u64,
    nv12_frame: &mut ffmpeg_next::frame::Video,
) -> Result<(), EncodeError> {
    let ffmpeg_fmt = capture_format_to_ffmpeg(format);
    let bpp = bytes_per_pixel(format);

    let mut sw_frame = ffmpeg_next::frame::Video::new(ffmpeg_fmt, width, height);
    let dst_stride = sw_frame.stride(0);
    let dst_data = sw_frame.data_mut(0);
    for y in 0..height as usize {
        let src_offset = y * stride as usize;
        let dst_offset = y * dst_stride;
        let copy_len = (width as usize * bpp).min(stride as usize).min(dst_stride);
        if src_offset + copy_len <= data.len() && dst_offset + copy_len <= dst_data.len() {
            dst_data[dst_offset..dst_offset + copy_len]
                .copy_from_slice(&data[src_offset..src_offset + copy_len]);
        }
    }

    let scaler = encoder.sw_scaler.get_or_insert_with(|| {
        ffmpeg_next::software::scaling::Context::get(
            ffmpeg_fmt,
            width,
            height,
            ffmpeg_next::format::Pixel::NV12,
            width,
            height,
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )
        .expect("failed to create capture→NV12 scaler")
    });

    scaler
        .run(&sw_frame, nv12_frame)
        .map_err(|e| EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("capture→NV12 scaling failed: {e}"),
        })
        .map(|_| ())
}

fn upload_dmabuf_and_encode(
    encoder: &mut FfmpegEncoder,
    info: &stargaze_core::capture::DmaBufInfo,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    use cudarc::driver::sys as cu;

    unsafe {
        let res = cu::cuCtxPushCurrent_v2(encoder.cuda_ctx);
        if res != cu::CUresult::CUDA_SUCCESS {
            return Err(EncodeError::EncodeFrameError {
                frame: pts,
                reason: format!("cuCtxPushCurrent failed: {res:?}"),
            });
        }
    }

    if encoder.egl_bridge.is_none() {
        match super::egl_cuda::EglCudaBridge::new(info.width, info.height, encoder.cuda_ctx) {
            Ok(bridge) => encoder.egl_bridge = Some(bridge),
            Err(e) => {
                unsafe {
                    let mut old: cu::CUcontext = ptr::null_mut();
                    cu::cuCtxPopCurrent_v2(&raw mut old);
                }
                return Err(e);
            }
        }
    }

    // Fully-GPU path: EGL → GL → CUDA kernel → NV12 hw frame, no CPU
    // round trip. Falls through to the CPU path on per-frame errors.
    if encoder.gpu_path_failures < MAX_GPU_PATH_FAILURES
        && encoder
            .egl_bridge
            .as_ref()
            .is_some_and(super::egl_cuda::EglCudaBridge::has_gpu_path)
    {
        match gpu_import_and_encode(encoder, info, pts, force_idr) {
            Ok(()) => {
                encoder.gpu_path_failures = 0;
                unsafe {
                    let mut old: cu::CUcontext = ptr::null_mut();
                    cu::cuCtxPopCurrent_v2(&raw mut old);
                }
                return Ok(());
            }
            Err(e) => {
                encoder.gpu_path_failures += 1;
                if encoder.gpu_path_failures <= 3 {
                    warn!(frame = pts, error = %e, "GPU NV12 path failed, using CPU fallback");
                } else if encoder.gpu_path_failures == MAX_GPU_PATH_FAILURES {
                    error!(
                        error = %e,
                        "GPU NV12 path failed {MAX_GPU_PATH_FAILURES} frames in a row — \
                         disabling it; encoding will stay on the slow CPU conversion path"
                    );
                }
            }
        }
    }

    let bridge = encoder.egl_bridge.as_ref().unwrap();

    let cpu_buf = match bridge.import_dmabuf_to_cpu(info) {
        Ok(buf) => buf,
        Err(e) => {
            unsafe {
                let mut old: cu::CUcontext = ptr::null_mut();
                cu::cuCtxPopCurrent_v2(&raw mut old);
            }
            if pts < 3 {
                warn!(
                    frame = pts,
                    error = %e,
                    "EGL bridge failed, attempting mmap fallback"
                );
            }
            return try_mmap_dmabuf_fallback(encoder, info, pts, force_idr).map_err(|mmap_err| {
                if pts < 3 {
                    warn!(frame = pts, error = %mmap_err, "mmap fallback also failed");
                }
                e
            });
        }
    };

    unsafe {
        let mut old: cu::CUcontext = ptr::null_mut();
        cu::cuCtxPopCurrent_v2(&raw mut old);
    }

    // EGL→GL shader blit always outputs RGBA (glReadPixels with gl::RGBA),
    // regardless of the original DMA-BUF pixel format.
    let stride = info.width * 4;
    upload_cpu_data_and_encode(
        encoder,
        &cpu_buf,
        info.width,
        info.height,
        stride,
        PixelFormat::Rgba8,
        pts,
        force_idr,
    )
}

/// GPU-only encode: allocate the NV12 hardware frame and let the EGL
/// bridge import + convert the DMA-BUF directly into it.
///
/// The CUDA context must already be pushed on this thread.
fn gpu_import_and_encode(
    encoder: &mut FfmpegEncoder,
    info: &stargaze_core::capture::DmaBufInfo,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
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

    encoder
        .egl_bridge
        .as_ref()
        .expect("bridge checked by caller")
        .import_dmabuf_to_hw_frame(info, &mut hw_frame)?;

    send_hw_frame(encoder, &mut hw_frame, pts, force_idr)
}

fn try_mmap_dmabuf_fallback(
    encoder: &mut FfmpegEncoder,
    info: &stargaze_core::capture::DmaBufInfo,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    use std::os::unix::io::AsRawFd;

    let fd = info.fd.as_raw_fd();
    let stride = info.stride;
    let size = (stride as usize) * (info.height as usize);

    let mapped = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };

    if mapped == libc::MAP_FAILED {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!(
                "mmap DMA-BUF fd {} failed: {}",
                fd,
                std::io::Error::last_os_error()
            ),
        });
    }

    let pixels = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), size) };
    let result = upload_cpu_data_and_encode(
        encoder,
        pixels,
        info.width,
        info.height,
        stride,
        info.format,
        pts,
        force_idr,
    );

    unsafe {
        libc::munmap(mapped, size);
    }

    result
}

/// Converts a duration to whole microseconds, saturating at `u32::MAX`.
fn saturating_us(d: std::time::Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

/// Drains all available packets from the encoder after sending a frame.
///
/// For keyframes, prepends `extradata` (VPS/SPS/PPS) so the decoder can
/// start decoding from any keyframe without prior state.
#[allow(clippy::similar_names, clippy::too_many_arguments)]
fn drain_packets(
    enc: &mut ffmpeg_next::encoder::video::Encoder,
    packet: &mut ffmpeg_next::Packet,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    frame_counter: u64,
    extradata: &[u8],
    capture_us: u32,
    convert_us: u32,
    encode_start: std::time::Instant,
) {
    loop {
        match enc.receive_packet(packet) {
            Ok(()) => {
                let raw_data = packet.data().map_or_else(Vec::new, <[u8]>::to_vec);
                let is_keyframe = packet.is_key();

                let data = if is_keyframe && !extradata.is_empty() {
                    let mut buf = Vec::with_capacity(extradata.len() + raw_data.len());
                    buf.extend_from_slice(extradata);
                    buf.extend_from_slice(&raw_data);
                    buf
                } else {
                    raw_data
                };

                let pkt = EncodedPacket {
                    data,
                    pts: packet.pts().unwrap_or(0).cast_unsigned(),
                    is_keyframe,
                    capture_us,
                    convert_us,
                    encode_us: saturating_us(encode_start.elapsed()),
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
    extradata: &[u8],
) {
    debug!("Flushing encoder (sending EOF)");

    if let Err(e) = enc.send_eof() {
        warn!("Failed to send EOF to encoder: {e}");
        return;
    }

    while let Ok(()) = enc.receive_packet(packet) {
        let raw_data = packet.data().map_or_else(Vec::new, <[u8]>::to_vec);
        let is_keyframe = packet.is_key();

        let data = if is_keyframe && !extradata.is_empty() {
            let mut buf = Vec::with_capacity(extradata.len() + raw_data.len());
            buf.extend_from_slice(extradata);
            buf.extend_from_slice(&raw_data);
            buf
        } else {
            raw_data
        };

        let pkt = EncodedPacket {
            data,
            pts: packet.pts().unwrap_or(0).cast_unsigned(),
            is_keyframe,
            capture_us: 0,
            convert_us: 0,
            encode_us: 0,
        };

        if packets_tx.blocking_send(pkt).is_err() {
            break;
        }
    }

    debug!("Encoder flushed");
}

#[cfg(test)]
mod tests {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::raw::{c_int, c_void};

    use super::*;

    // Minimal libgbm bindings for allocating a real DMA-BUF in tests.
    type GbmCreateDeviceFn = unsafe extern "C" fn(c_int) -> *mut c_void;
    type GbmDeviceDestroyFn = unsafe extern "C" fn(*mut c_void);
    type GbmBoCreateFn = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void;
    type GbmBoDestroyFn = unsafe extern "C" fn(*mut c_void);
    type GbmBoGetFdFn = unsafe extern "C" fn(*mut c_void) -> c_int;
    type GbmBoGetStrideFn = unsafe extern "C" fn(*mut c_void) -> u32;
    type GbmBoGetModifierFn = unsafe extern "C" fn(*mut c_void) -> u64;
    #[allow(clippy::type_complexity)]
    type GbmBoMapFn = unsafe extern "C" fn(
        *mut c_void,
        u32,
        u32,
        u32,
        u32,
        u32,
        *mut u32,
        *mut *mut c_void,
    ) -> *mut c_void;
    type GbmBoUnmapFn = unsafe extern "C" fn(*mut c_void, *mut c_void);

    const GBM_FORMAT_ARGB8888: u32 = 0x3432_5241; // fourcc 'AR24'
    const GBM_BO_USE_SCANOUT: u32 = 1;
    const GBM_BO_USE_RENDERING: u32 = 4;
    const GBM_BO_USE_LINEAR: u32 = 16;
    const GBM_BO_TRANSFER_WRITE: u32 = 2;

    /// A GBM-allocated linear buffer exported as a DMA-BUF, used to feed
    /// the encoder a real DMA-BUF without a compositor.
    struct GbmTestBuffer {
        lib: libloading::Library,
        device: *mut c_void,
        bo: *mut c_void,
        drm_fd: c_int,
        width: u32,
        height: u32,
    }

    impl GbmTestBuffer {
        fn new(width: u32, height: u32) -> Result<Self, String> {
            let lib = unsafe { libloading::Library::new("libgbm.so.1") }
                .map_err(|e| format!("libgbm.so.1 not available: {e}"))?;
            let create_device: GbmCreateDeviceFn =
                unsafe { *lib.get(b"gbm_create_device\0").map_err(|e| e.to_string())? };
            let bo_create: GbmBoCreateFn =
                unsafe { *lib.get(b"gbm_bo_create\0").map_err(|e| e.to_string())? };
            let device_destroy: GbmDeviceDestroyFn = unsafe {
                *lib.get(b"gbm_device_destroy\0")
                    .map_err(|e| e.to_string())?
            };

            // Find a render node whose GBM backend can allocate the buffer.
            for i in 128..136 {
                let path = format!("/dev/dri/renderD{i}\0");
                let drm_fd =
                    unsafe { libc::open(path.as_ptr().cast(), libc::O_RDWR | libc::O_CLOEXEC) };
                if drm_fd < 0 {
                    eprintln!(
                        "renderD{i}: open failed: {}",
                        std::io::Error::last_os_error()
                    );
                    continue;
                }
                let device = unsafe { create_device(drm_fd) };
                if device.is_null() {
                    eprintln!("renderD{i}: gbm_create_device failed");
                    unsafe { libc::close(drm_fd) };
                    continue;
                }
                // Flag support differs per GBM backend (the NVIDIA one
                // rejects some combinations with EINVAL) — try a few.
                let flag_choices = [
                    GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR,
                    GBM_BO_USE_LINEAR,
                    GBM_BO_USE_RENDERING,
                    GBM_BO_USE_SCANOUT,
                ];
                let mut bo = ptr::null_mut();
                for flags in flag_choices {
                    bo = unsafe { bo_create(device, width, height, GBM_FORMAT_ARGB8888, flags) };
                    if !bo.is_null() {
                        break;
                    }
                    eprintln!(
                        "renderD{i}: gbm_bo_create(flags={flags:#x}) failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
                if bo.is_null() {
                    unsafe {
                        device_destroy(device);
                        libc::close(drm_fd);
                    }
                    continue;
                }
                return Ok(Self {
                    lib,
                    device,
                    bo,
                    drm_fd,
                    width,
                    height,
                });
            }
            Err("no DRM render node could allocate a GBM buffer".to_string())
        }

        /// Fills the buffer with a gradient. Best-effort: some GBM backends
        /// don't support CPU mapping; the encode path doesn't care about
        /// the pixel contents.
        #[allow(clippy::cast_possible_truncation)] // x % 256 always fits u8
        fn fill(&self) {
            let Ok(bo_map) = (unsafe { self.lib.get::<GbmBoMapFn>(b"gbm_bo_map\0") }) else {
                return;
            };
            let Ok(bo_unmap) = (unsafe { self.lib.get::<GbmBoUnmapFn>(b"gbm_bo_unmap\0") }) else {
                return;
            };
            let mut stride = 0u32;
            let mut map_data: *mut c_void = ptr::null_mut();
            let ptr = unsafe {
                bo_map(
                    self.bo,
                    0,
                    0,
                    self.width,
                    self.height,
                    GBM_BO_TRANSFER_WRITE,
                    &raw mut stride,
                    &raw mut map_data,
                )
            };
            if ptr.is_null() {
                return;
            }
            unsafe {
                let buf = std::slice::from_raw_parts_mut(
                    ptr.cast::<u8>(),
                    stride as usize * self.height as usize,
                );
                for y in 0..self.height as usize {
                    for x in 0..self.width as usize {
                        let o = y * stride as usize + x * 4;
                        buf[o] = (x % 256) as u8; // B
                        buf[o + 1] = (y % 256) as u8; // G
                        buf[o + 2] = ((x + y) % 256) as u8; // R
                        buf[o + 3] = 255; // A
                    }
                }
                bo_unmap(self.bo, map_data);
            }
        }

        /// Exports the buffer as a `DmaBufInfo` (fresh fd each call).
        fn export(&self) -> Result<stargaze_core::capture::DmaBufInfo, String> {
            let bo_get_fd: GbmBoGetFdFn = unsafe {
                *self
                    .lib
                    .get(b"gbm_bo_get_fd\0")
                    .map_err(|e| e.to_string())?
            };
            let bo_get_stride: GbmBoGetStrideFn = unsafe {
                *self
                    .lib
                    .get(b"gbm_bo_get_stride\0")
                    .map_err(|e| e.to_string())?
            };
            let bo_get_modifier: GbmBoGetModifierFn = unsafe {
                *self
                    .lib
                    .get(b"gbm_bo_get_modifier\0")
                    .map_err(|e| e.to_string())?
            };
            let fd = unsafe { bo_get_fd(self.bo) };
            if fd < 0 {
                return Err("gbm_bo_get_fd failed".to_string());
            }
            Ok(stargaze_core::capture::DmaBufInfo {
                fd: unsafe { OwnedFd::from_raw_fd(fd) },
                width: self.width,
                height: self.height,
                format: PixelFormat::Bgra8,
                modifier: unsafe { bo_get_modifier(self.bo) },
                stride: unsafe { bo_get_stride(self.bo) },
                offset: 0,
            })
        }
    }

    impl Drop for GbmTestBuffer {
        fn drop(&mut self) {
            unsafe {
                if let Ok(bo_destroy) = self.lib.get::<GbmBoDestroyFn>(b"gbm_bo_destroy\0") {
                    bo_destroy(self.bo);
                }
                if let Ok(device_destroy) =
                    self.lib.get::<GbmDeviceDestroyFn>(b"gbm_device_destroy\0")
                {
                    device_destroy(self.device);
                }
                libc::close(self.drm_fd);
            }
        }
    }

    /// End-to-end check of the fully-GPU DMA-BUF encode path: a real
    /// DMA-BUF (GBM, linear ARGB8888) must go through EGL import, the
    /// CUDA NV12 kernel, and NVENC without ever hitting the CPU fallback.
    ///
    /// This is the regression test for the silent CPU-fallback bug where
    /// a missing/incompatible libnvrtc capped the pipeline at ~70 fps.
    #[test]
    #[ignore = "requires NVIDIA GPU, NVENC, NVRTC, and a DRM render node"]
    fn gpu_dmabuf_path_encodes_without_cpu_fallback() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();

        let width = 1280u32;
        let height = 720u32;

        let buffer = GbmTestBuffer::new(width, height).expect("GBM buffer allocation");
        buffer.fill();

        let config = EncoderConfig {
            width,
            height,
            framerate: 60,
            bitrate_mbps: 5,
            tuning: stargaze_core::config::EncoderTuning::default(),
        };
        let mut encoder = init_encoder(&config).expect("NVENC encoder init");

        let (packets_tx, mut packets_rx) = mpsc::channel::<EncodedPacket>(64);
        let mut packet = ffmpeg_next::Packet::empty();

        for pts in 0..5u64 {
            let info = buffer.export().expect("DMA-BUF export");
            upload_dmabuf_and_encode(&mut encoder, &info, pts, pts == 0)
                .expect("DMA-BUF upload + encode");
            drain_packets(
                &mut encoder.encoder,
                &mut packet,
                &packets_tx,
                pts,
                &encoder.extradata,
                0,
                0,
                std::time::Instant::now(),
            );
        }

        let bridge = encoder.egl_bridge.as_ref().expect("EGL bridge initialized");
        assert!(
            bridge.has_gpu_path(),
            "GPU NV12 converter must initialize (is libnvrtc available and \
             matching cudarc's CUDA version feature?)"
        );
        assert_eq!(
            encoder.gpu_path_failures, 0,
            "every frame must take the GPU path, not the CPU fallback"
        );

        let mut count = 0u32;
        let mut got_keyframe = false;
        while let Ok(pkt) = packets_rx.try_recv() {
            assert!(!pkt.data.is_empty());
            got_keyframe |= pkt.is_keyframe;
            count += 1;
        }
        assert!(count > 0, "encoder should have produced packets");
        assert!(got_keyframe, "first frame should be an IDR keyframe");
    }
}
