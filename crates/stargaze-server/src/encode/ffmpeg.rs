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
    base_extradata_len: usize,
    hdr_metadata: stargaze_core::hdr::HdrMetadata,
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
    /// On-GPU converter for CPU-memory (`MemFd`/mmap) frames. Lazily
    /// initialized on the first such frame; `None` until then or when
    /// NVRTC is unavailable.
    gpu_converter: Option<super::egl_cuda::GpuNv12Converter>,
    /// Consecutive GPU conversion failures for CPU-memory frames; same
    /// disable semantics as `gpu_path_failures`.
    gpu_convert_failures: u32,
}

/// Consecutive GPU-path failures after which the GPU NV12 path is disabled.
const MAX_GPU_PATH_FAILURES: u32 = 30;

// Safety: FfmpegEncoder is only used on the dedicated encoder thread.
// FFmpeg contexts are not thread-safe, but we never share them across threads.
unsafe impl Send for FfmpegEncoder {}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        // The EGL bridge and the CPU-frame converter hold CUDA resources
        // (registered GL image, kernel module, device/pinned buffers)
        // that belong to the context owned by `hw_device_ctx` — release
        // them before the context can be destroyed, with the context
        // current as the driver API requires.
        if self.egl_bridge.is_some() || self.gpu_converter.is_some() {
            unsafe {
                let res = cudarc::driver::sys::cuCtxPushCurrent_v2(self.cuda_ctx);
                drop(self.egl_bridge.take());
                drop(self.gpu_converter.take());
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
    if config.width == 0
        || config.height == 0
        || !config.width.is_multiple_of(2)
        || !config.height.is_multiple_of(2)
        || config.framerate == 0
    {
        return Err(EncodeError::InitError(
            "4:2:0 encode requires nonzero even dimensions and nonzero frame rate".into(),
        ));
    }
    if !config.color.is_supported_stream() {
        return Err(EncodeError::InitError("unsupported encoder color".into()));
    }
    let hdr = config.color.is_hdr10();
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
    //
    // cudarc panics (rather than returning Err) when libcuda.so cannot be
    // dynamically loaded.  Rust's default panic hook always prints a backtrace
    // to stderr before catch_unwind can intercept — temporarily replace it
    // with a no-op so the caller gets a clean error return instead of a scary
    // panic dump.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let cuda_init_result = std::panic::catch_unwind(cudarc::driver::result::init);
    std::panic::set_hook(prev_hook);
    match cuda_init_result {
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
        (*frames_ctx).sw_format = if hdr {
            ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_P010LE
        } else {
            ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_NV12
        };
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
        "Initialized CUDA hardware frames context ({}x{}, {})",
        config.width,
        config.height,
        if hdr { "P010" } else { "NV12" }
    );

    // Step 5: Create codec context and configure.
    let mut ctx = ffmpeg_next::codec::context::Context::new_with_codec(codec);

    // Attach hardware contexts before configuring the encoder.
    unsafe {
        let raw_ctx = ctx.as_mut_ptr();
        (*raw_ctx).hw_device_ctx = ffmpeg_sys_next::av_buffer_ref(hw_device_ctx);
        (*raw_ctx).hw_frames_ctx = ffmpeg_sys_next::av_buffer_ref(hw_frames_ctx);
    }

    let mut encoder = match ctx.encoder().video() {
        Ok(encoder) => encoder,
        Err(e) => {
            // `ctx` (and its attached ref copies) is dropped here, but the
            // originals allocated above are ours to free.
            unsafe {
                let mut hw_frames_ptr = hw_frames_ctx;
                ffmpeg_sys_next::av_buffer_unref(&raw mut hw_frames_ptr);
                ffmpeg_sys_next::av_buffer_unref(&raw mut hw_device_ctx);
            }
            return Err(EncodeError::InitError(format!(
                "failed to create video encoder context: {e}"
            )));
        }
    };

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
        (*raw_ctx).colorspace = if hdr {
            ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT2020_NCL
        } else {
            ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT709
        };
        (*raw_ctx).color_primaries = if hdr {
            ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT2020
        } else {
            ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT709
        };
        (*raw_ctx).color_trc = if hdr {
            ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
        } else {
            ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1
        };
    }

    // Step 6: Open encoder with NVENC-specific options.
    //
    // Preset and multipass come from config (defaults p1/disabled —
    // chosen for throughput; higher presets halve the sustainable
    // framerate at 1440p-class resolutions for a quality gain that LAN
    // bitrates make hard to notice). Spatial AQ distributes bits where
    // the eye notices (text edges, flat gradients) — without it the
    // picture has the typical "screen share" mosquito noise.
    let mut opts = ffmpeg_next::Dictionary::new();
    opts.set("preset", &config.tuning.preset);
    opts.set("profile", if hdr { "main10" } else { "main" });
    opts.set("tune", "ull");
    opts.set("rc", "cbr");
    opts.set("multipass", &config.tuning.multipass);
    opts.set("spatial-aq", "1");
    opts.set("aq-strength", "8");
    opts.set("delay", "0");
    opts.set("forced-idr", "1");
    opts.set("zerolatency", "1");

    let opened = match encoder.open_with(opts) {
        Ok(opened) => opened,
        Err(e) => {
            // `encoder` (and its attached ref copies) is dropped here, but the
            // originals allocated above are ours to free.
            unsafe {
                let mut hw_frames_ptr = hw_frames_ctx;
                ffmpeg_sys_next::av_buffer_unref(&raw mut hw_frames_ptr);
                ffmpeg_sys_next::av_buffer_unref(&raw mut hw_device_ctx);
            }
            return Err(EncodeError::InitError(format!(
                "failed to open hevc_nvenc encoder: {e}"
            )));
        }
    };

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
        color = ?config.color,
        pixel_format = if hdr { "P010" } else { "NV12" },
        profile = if hdr { "Main10" } else { "Main" },
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
        base_extradata_len: extradata.len(),
        extradata,
        hdr_metadata: stargaze_core::hdr::HdrMetadata::default(),
        sw_scaler: None,
        egl_bridge: None,
        gpu_path_failures: 0,
        gpu_converter: None,
        gpu_convert_failures: 0,
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
#[allow(
    clippy::unnecessary_wraps,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]
pub(crate) fn run_encode_loop(
    encoder: &mut FfmpegEncoder,
    config: &EncoderConfig,
    frames: &mut mpsc::Receiver<CapturedFrame>,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    shutdown: &Arc<AtomicBool>,
    mut idr_rx: watch::Receiver<u64>,
    mut bitrate_rx: watch::Receiver<u32>,
) -> Result<(), EncodeError> {
    let mut frame_counter: u64 = 0;
    let mut last_idr_value: u64 = 0;
    let mut current_bitrate = config.bitrate_mbps;
    let mut packet = ffmpeg_next::Packet::empty();
    let mut consecutive_panics: u32 = 0;

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
        if captured.color.stream() != Some(config.color) {
            return Err(EncodeError::FfmpegError(format!(
                "unsafe source color {:?}: no HDR tone/gamut mapping; stopping encoder",
                captured.color
            )));
        }
        let frame = captured.frame;

        // Session requested a different bitrate: NVENC has no runtime
        // rate-control reconfiguration through FFmpeg, so rebuild the
        // encoder context. Happens at most once per session handshake
        // (~100 ms hiccup); the fresh encoder starts with an IDR and
        // new extradata, so the client picks up seamlessly.
        let requested_bitrate = *bitrate_rx.borrow_and_update();
        if requested_bitrate != current_bitrate && requested_bitrate > 0 {
            info!(
                from = current_bitrate,
                to = requested_bitrate,
                "Rebuilding encoder for session-requested bitrate"
            );
            let new_config = EncoderConfig {
                bitrate_mbps: requested_bitrate,
                ..config.clone()
            };
            match init_encoder(&new_config) {
                Ok(new_encoder) => {
                    *encoder = new_encoder;
                    current_bitrate = requested_bitrate;
                }
                Err(e) => {
                    warn!("Encoder rebuild failed ({e}); keeping {current_bitrate} Mbps");
                    // Don't retry every frame.
                    current_bitrate = requested_bitrate;
                }
            }
        }

        // Check if an IDR keyframe was requested.
        let current_idr = *idr_rx.borrow_and_update();
        let metadata_changed = set_hdr_metadata(encoder, captured.hdr_metadata)?;
        let force_idr = frame_counter == 0 || current_idr != last_idr_value || metadata_changed;
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
        let capture_us = saturating_us(prep_start.saturating_duration_since(captured.captured_at));

        // Panic boundary. A bug in the conversion/upload/drain path (an
        // odd-dimension assert in `convert`, a scaler init `.expect`, a band
        // worker panicking under `thread::scope`) would otherwise unwind and
        // kill this dedicated thread, leaving the server up but permanently
        // unable to produce video with nothing surfaced beyond client
        // disconnects. Catch it, skip the frame, and force the next frame to
        // an IDR so the decoder resyncs. Sustained panics are treated as
        // fatal so the thread exits and a supervisor can restart it, rather
        // than spinning on a frame it can never encode.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            upload_and_encode(encoder, &frame, frame_counter, force_idr)?;
            // Frame preparation: pixel conversion + GPU upload + send_frame.
            let convert_us = saturating_us(prep_start.elapsed());
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
            Ok::<(), EncodeError>(())
        }));

        match outcome {
            Ok(Ok(())) => consecutive_panics = 0,
            Ok(Err(e)) => {
                if config.color.is_hdr10()
                    && matches!(
                        e,
                        EncodeError::InitError(_) | EncodeError::UnsupportedFormat(_)
                    )
                {
                    return Err(e);
                }
                warn!(frame = frame_counter, "Skipping frame: {e}");
                frame_counter += 1;
                continue;
            }
            Err(panic) => {
                consecutive_panics += 1;
                let msg = panic_message(panic.as_ref());
                error!(
                    frame = frame_counter,
                    consecutive_panics, "Encode path panicked, skipping frame: {msg}"
                );
                if consecutive_panics >= MAX_CONSECUTIVE_ENCODE_PANICS {
                    return Err(EncodeError::FfmpegError(format!(
                        "encode path panicked on {consecutive_panics} consecutive frames; \
                         last panic: {msg}"
                    )));
                }
                // Force the next frame to an IDR so the decoder resyncs after
                // the gap (differs last_idr_value from the unchanged watch).
                last_idr_value = last_idr_value.wrapping_sub(1);
                frame_counter += 1;
                continue;
            }
        }

        frame_counter += 1;
        if frame_counter == 1
            || (stargaze_core::logging::progress_logging() && frame_counter.is_multiple_of(300))
        {
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

fn set_hdr_metadata(
    encoder: &mut FfmpegEncoder,
    metadata: stargaze_core::hdr::HdrMetadata,
) -> Result<bool, EncodeError> {
    if !encoder.config.color.is_hdr10() && metadata != stargaze_core::hdr::HdrMetadata::default() {
        return Err(EncodeError::InitError(
            "HDR metadata attached to an SDR source".into(),
        ));
    }
    if metadata == encoder.hdr_metadata {
        return Ok(false);
    }
    let sei = metadata
        .hevc_sei()
        .map_err(|e| EncodeError::InitError(e.into()))?;
    encoder.extradata.truncate(encoder.base_extradata_len);
    encoder.extradata.extend(sei);
    encoder.hdr_metadata = metadata;
    info!(
        ?metadata,
        "HDR source metadata updated; repeat prefix SEI on every keyframe"
    );
    Ok(true)
}

/// Number of consecutive per-frame panics tolerated before the encode loop
/// gives up and returns an error (so the thread exits cleanly and a
/// supervisor can restart, rather than spinning on an unencodable frame).
const MAX_CONSECUTIVE_ENCODE_PANICS: u32 = 10;

/// Best-effort extraction of a human-readable message from a caught panic
/// payload (`catch_unwind`'s `Err`).
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// RAII guard for a CUDA context pushed as current on the calling thread.
///
/// Pushing on construction and popping on `Drop` keeps the thread's CUDA
/// context stack balanced across every exit path — a normal return, an
/// early `?`, or a panic unwinding through the encode path. The encode
/// loop's `catch_unwind` boundary would otherwise swallow a panic while
/// leaving the context pushed, imbalancing the stack for later frames.
struct CudaCtxGuard;

impl CudaCtxGuard {
    /// Pushes `ctx` as the current CUDA context.
    ///
    /// # Errors
    ///
    /// Returns `EncodeFrameError` if `cuCtxPushCurrent` fails; no guard is
    /// created in that case, so nothing is popped.
    fn push(ctx: cudarc::driver::sys::CUcontext, pts: u64) -> Result<Self, EncodeError> {
        let res = unsafe { cudarc::driver::sys::cuCtxPushCurrent_v2(ctx) };
        if res != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
            return Err(EncodeError::EncodeFrameError {
                frame: pts,
                reason: format!("cuCtxPushCurrent failed: {res:?}"),
            });
        }
        Ok(Self)
    }
}

impl Drop for CudaCtxGuard {
    fn drop(&mut self) {
        unsafe {
            let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
            cudarc::driver::sys::cuCtxPopCurrent_v2(&raw mut old);
        }
    }
}

/// Maps our capture `PixelFormat` to the corresponding `FFmpeg` pixel format.
fn capture_format_to_ffmpeg(fmt: PixelFormat) -> ffmpeg_next::format::Pixel {
    match fmt {
        PixelFormat::Bgra8 => ffmpeg_next::format::Pixel::BGRA,
        PixelFormat::Rgba8 => ffmpeg_next::format::Pixel::RGBA,
        PixelFormat::Nv12 => ffmpeg_next::format::Pixel::NV12,
        PixelFormat::Bgra10 => ffmpeg_next::format::Pixel::X2BGR10LE,
        PixelFormat::Rgba10 => ffmpeg_next::format::Pixel::X2RGB10LE,
        PixelFormat::Rgba16 => ffmpeg_next::format::Pixel::RGBA64LE,
    }
}

/// Bytes per pixel for a capture format (used for stride calculations).
fn bytes_per_pixel(fmt: PixelFormat) -> usize {
    match fmt {
        PixelFormat::Bgra8 | PixelFormat::Rgba8 | PixelFormat::Bgra10 | PixelFormat::Rgba10 => 4,
        PixelFormat::Rgba16 => 8,
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
                    drm_fourcc = format_args!("0x{:08x}", info.drm_fourcc),
                    modifier = format_args!("0x{:x}", info.modifier),
                    "Encoding DmaBuf frame"
                );
            }
            upload_dmabuf_and_encode(encoder, info, pts, force_idr)
        }
    }
}

/// Validate the packed source before GPU upload or CPU conversion.
fn validate_cpu_layout(
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
    pts: u64,
) -> Result<(), EncodeError> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || (stride as usize) < w * bytes_per_pixel(format) {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: "invalid source dimensions/stride".into(),
        });
    }
    let min_len = (h - 1) * stride as usize + w * bytes_per_pixel(format);
    if data.len() < min_len {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: "source buffer is shorter than its layout".into(),
        });
    }
    Ok(())
}

/// Converts CPU-accessible RGB to NV12 (SDR) or P010 (HDR), uploads to a
/// CUDA hardware frame, and sends it to the encoder. Used by both
/// `CpuMapped` and `DmaBuf` (mmap'd) paths.
///
/// HDR CPU fallback preserves precision; it never uses the SDR scaler.
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

    validate_cpu_layout(data, width, height, stride, format, pts)?;
    let hdr = encoder.config.color.is_hdr10();
    let direct = if hdr {
        super::hdr::pixel_bytes(format).map(|_| (0, 1, 2))
    } else {
        super::convert::channel_order(format)
    };

    // Fastest path: pinned upload + CUDA kernel straight into the
    // encoder's hardware frame (~3 ms at 3440x1440 vs ~14 ms for the
    // parallel CPU converter). Falls back to the CPU path per frame.
    if let Some(order) = direct
        && encoder.gpu_convert_failures < MAX_GPU_PATH_FAILURES
    {
        match gpu_convert_and_encode(
            encoder, data, width, height, stride, format, order, pts, force_idr,
        ) {
            Ok(()) => {
                encoder.gpu_convert_failures = 0;
                return Ok(());
            }
            Err(e) => {
                encoder.gpu_convert_failures += 1;
                if encoder.gpu_convert_failures <= 3 {
                    warn!(frame = pts, error = %e, "GPU conversion failed, using CPU fallback");
                } else if encoder.gpu_convert_failures == MAX_GPU_PATH_FAILURES {
                    error!(
                        error = %e,
                        "GPU conversion failed {MAX_GPU_PATH_FAILURES} frames in a row — \
                         disabling it; encoding will stay on the slower CPU conversion path"
                    );
                }
            }
        }
    }

    let mut nv12_frame = ffmpeg_next::frame::Video::new(
        if hdr {
            ffmpeg_next::format::Pixel::P010LE
        } else {
            ffmpeg_next::format::Pixel::NV12
        },
        width,
        height,
    );

    if hdr {
        let y_stride = nv12_frame.stride(0);
        let uv_stride = nv12_frame.stride(1);
        unsafe {
            let raw = &mut *nv12_frame.as_mut_ptr();
            let y = std::slice::from_raw_parts_mut(raw.data[0], y_stride * h);
            let uv = std::slice::from_raw_parts_mut(raw.data[1], uv_stride * h / 2);
            super::hdr::convert_to_p010(
                data,
                stride as usize,
                w,
                h,
                format,
                y,
                y_stride,
                uv,
                uv_stride,
            )
            .map_err(|e| EncodeError::EncodeFrameError {
                frame: pts,
                reason: e.into(),
            })?;
        }
    }
    // Fast path: direct parallel RGB→NV12 (BT.709, matching the encoder's
    // advertised colorspace). The sws path is single-threaded and was the
    // pipeline bottleneck at high resolutions.
    else if let Some(order) = direct {
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
    send_cpu_frame(encoder, &nv12_frame, pts, force_idr)
}

fn send_cpu_frame(
    encoder: &mut FfmpegEncoder,
    software: &ffmpeg_next::frame::Video,
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

    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_transfer_data(hw_frame.as_mut_ptr(), software.as_ptr(), 0)
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_transfer_data failed (error code {ret})"),
        });
    }

    send_hw_frame(encoder, &mut hw_frame, pts, force_idr)
}

/// GPU conversion for CPU-memory frames: pinned host staging → CUDA
/// kernel → NV12 hardware frame → NVENC, skipping the CPU converter and
/// the separate NV12 upload.
#[allow(clippy::too_many_arguments)]
fn gpu_convert_and_encode(
    encoder: &mut FfmpegEncoder,
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
    order: (usize, usize, usize),
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    use cudarc::driver::sys as cu;

    // Guard pops the CUDA context on every exit path, including a panic
    // unwinding through the converter/convert code below.
    let _ctx = CudaCtxGuard::push(encoder.cuda_ctx, pts)?;

    (|| {
        if encoder
            .gpu_converter
            .as_ref()
            .is_some_and(|c| c.input_format() != format)
        {
            encoder.gpu_converter = None;
        }
        if encoder.gpu_converter.is_none() {
            match super::egl_cuda::GpuNv12Converter::new_format(
                width,
                height,
                format,
                encoder.config.color.is_hdr10(),
            ) {
                Ok(conv) => encoder.gpu_converter = Some(conv),
                Err(e) => {
                    // Converter init failure (typically missing NVRTC) is
                    // permanent — don't retry it every frame.
                    encoder.gpu_convert_failures = MAX_GPU_PATH_FAILURES;
                    warn!("GPU converter unavailable ({e}), using CPU conversion");
                    return Err(e);
                }
            }
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

        unsafe {
            let raw = hw_frame.as_mut_ptr();
            let y_plane = (*raw).data[0] as cu::CUdeviceptr;
            let uv_plane = (*raw).data[1] as cu::CUdeviceptr;
            let y_pitch = usize::try_from((*raw).linesize[0]).unwrap_or(width as usize);
            let uv_pitch = usize::try_from((*raw).linesize[1]).unwrap_or(width as usize);
            encoder
                .gpu_converter
                .as_mut()
                .expect("converter initialized above")
                .convert_host_frame(
                    data,
                    stride as usize,
                    order,
                    y_plane,
                    y_pitch,
                    uv_plane,
                    uv_pitch,
                )?;
        }

        send_hw_frame(encoder, &mut hw_frame, pts, force_idr)
    })()
    // `_ctx` pops the CUDA context here, including on a panic unwind.
}

/// Stamps pts / forced-IDR on a hardware frame and submits it to NVENC.
fn send_hw_frame(
    encoder: &mut FfmpegEncoder,
    hw_frame: &mut ffmpeg_next::frame::Video,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    hw_frame.set_pts(Some(pts.cast_signed()));
    unsafe {
        let raw = hw_frame.as_mut_ptr();
        (*raw).color_range = ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG;
        let hdr = encoder.config.color.is_hdr10();
        (*raw).colorspace = if hdr {
            ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT2020_NCL
        } else {
            ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT709
        };
        (*raw).color_primaries = if hdr {
            ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT2020
        } else {
            ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT709
        };
        (*raw).color_trc = if hdr {
            ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
        } else {
            ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1
        };
    }
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

    let needs_scaler = encoder.sw_scaler.as_ref().is_none_or(|scaler| {
        let input = scaler.input();
        input.format != ffmpeg_fmt || input.width != width || input.height != height
    });
    if needs_scaler {
        encoder.sw_scaler = Some(sdr_capture_scaler(ffmpeg_fmt, width, height)?);
    }
    let scaler = encoder
        .sw_scaler
        .as_mut()
        .ok_or_else(|| EncodeError::FfmpegError("capture scaler unavailable".into()))?;

    scaler
        .run(&sw_frame, nv12_frame)
        .map_err(|e| EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("capture→NV12 scaling failed: {e}"),
        })
        .map(|_| ())
}

/// Full-range sRGB → limited-range 709 matrix, with no transfer conversion.
/// This is SDR precision reduction, NOT an HDR tone mapper.
fn sdr_capture_scaler(
    format: ffmpeg_next::format::Pixel,
    width: u32,
    height: u32,
) -> Result<ffmpeg_next::software::scaling::Context, EncodeError> {
    let mut scaler = ffmpeg_next::software::scaling::Context::get(
        format,
        width,
        height,
        ffmpeg_next::format::Pixel::NV12,
        width,
        height,
        ffmpeg_next::software::scaling::Flags::BILINEAR,
    )
    .map_err(|e| EncodeError::FfmpegError(format!("capture scaler: {e}")))?;
    let ret = unsafe {
        let coeffs = ffmpeg_sys_next::sws_getCoefficients(ffmpeg_sys_next::SWS_CS_ITU709);
        ffmpeg_sys_next::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            coeffs,
            1,
            coeffs,
            0,
            0,
            1 << 16,
            1 << 16,
        )
    };
    if ret < 0 {
        return Err(EncodeError::FfmpegError(format!(
            "capture scaler color configuration: {ret}"
        )));
    }
    Ok(scaler)
}

fn upload_dmabuf_and_encode(
    encoder: &mut FfmpegEncoder,
    info: &stargaze_core::capture::DmaBufInfo,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    // Guard pops the CUDA context on every exit path, including a panic
    // unwinding through the import/convert code below. Where a pop must
    // precede a nested call that re-pushes the context, it is dropped
    // explicitly (`drop(ctx)`).
    let ctx = CudaCtxGuard::push(encoder.cuda_ctx, pts)?;

    if encoder.egl_bridge.is_none() {
        // `?` here still pops the context: `ctx`'s guard drops on the early
        // return.
        let bridge = super::egl_cuda::EglCudaBridge::new_color(
            info.width,
            info.height,
            encoder.cuda_ctx,
            encoder.config.color.is_hdr10(),
        )?;
        encoder.egl_bridge = Some(bridge);
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
            // The mmap fallback runs without our context pushed (matching
            // the original hand-balanced pop at this point).
            drop(ctx);
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
                mmap_err
            });
        }
    };

    // The nested CPU encode re-pushes the context itself; drop our push
    // first so the same context isn't stacked twice.
    drop(ctx);

    // EGL→GL shader blit always outputs RGBA (glReadPixels with gl::RGBA),
    // regardless of the original DMA-BUF pixel format.
    let hdr = encoder.config.color.is_hdr10();
    let stride = info.width * if hdr { 8 } else { 4 };
    upload_cpu_data_and_encode(
        encoder,
        &cpu_buf,
        info.width,
        info.height,
        stride,
        if hdr {
            PixelFormat::Rgba16
        } else {
            PixelFormat::Rgba8
        },
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

    if info.modifier != 0 {
        return Err(EncodeError::UnsupportedFormat(
            "non-linear/implicit DMA-BUF requires EGL detiling; raw mmap would corrupt pixels"
                .into(),
        ));
    }
    let fd = info.fd.as_raw_fd();
    let stride = info.stride;
    let size = (stride as usize) * (info.height as usize);
    let offset = info.offset as usize;
    let mapped_size = offset
        .checked_add(size)
        .ok_or_else(|| EncodeError::UnsupportedFormat("DMA-BUF mapping size overflow".into()))?;

    let mapped = unsafe {
        libc::mmap(
            ptr::null_mut(),
            mapped_size,
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

    let pixels = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>().add(offset), size) };
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
        libc::munmap(mapped, mapped_size);
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

    #[test]
    fn ten_bit_sdr_scaler_uses_correct_channel_order_range_and_matrix() {
        ffmpeg_next::init().unwrap();
        for capture in [PixelFormat::Bgra10, PixelFormat::Rgba10] {
            let format = capture_format_to_ffmpeg(capture);
            let mut scaler = sdr_capture_scaler(format, 16, 16).unwrap();
            let mut src = ffmpeg_next::frame::Video::new(format, 16, 16);
            let mut dst = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::NV12, 16, 16);
            let mut check = |r: u32, g: u32, b: u32, expected: (u8, u8, u8)| {
                let packed = match capture {
                    PixelFormat::Bgra10 => r | (g << 10) | (b << 20),
                    PixelFormat::Rgba10 => b | (g << 10) | (r << 20),
                    _ => unreachable!(),
                };
                for pixel in src.data_mut(0).as_chunks_mut::<4>().0 {
                    pixel.copy_from_slice(&packed.to_le_bytes());
                }
                scaler.run(&src, &mut dst).unwrap();
                let actual = (dst.data(0)[0], dst.data(1)[0], dst.data(1)[1]);
                for (actual, expected) in [actual.0, actual.1, actual.2]
                    .into_iter()
                    .zip([expected.0, expected.1, expected.2])
                {
                    assert!(
                        (i16::from(actual) - i16::from(expected)).abs() <= 1,
                        "{capture:?}: {actual} != {expected}"
                    );
                }
            };
            check(0, 0, 0, (16, 128, 128));
            check(1023, 1023, 1023, (235, 128, 128));
            check(1023, 0, 0, (63, 102, 240));
            check(0, 1023, 0, (173, 42, 26));
            check(0, 0, 1023, (32, 240, 118));
            for gray in [1, 64, 256, 512, 768, 1023] {
                let y = u8::try_from(16 + (219 * gray + 511) / 1023).unwrap();
                check(gray, gray, gray, (y, 128, 128));
            }
        }
    }

    fn assert_hdr_cpu_gpu_agreement(encoder: &FfmpegEncoder, data: &[u8], format: PixelFormat) {
        use cudarc::driver::sys as cu;
        let (width, height) = (encoder.config.width, encoder.config.height);
        let (w, h) = (width as usize, height as usize);
        let bpp = super::super::hdr::pixel_bytes(format).unwrap();
        let _ctx = CudaCtxGuard::push(encoder.cuda_ctx, 0).unwrap();
        let mut converter =
            super::super::egl_cuda::GpuNv12Converter::new_format(width, height, format, true)
                .unwrap();
        let mut hw = ffmpeg_next::frame::Video::empty();
        assert_eq!(
            unsafe {
                ffmpeg_sys_next::av_hwframe_get_buffer(encoder.hw_frames_ctx, hw.as_mut_ptr(), 0)
            },
            0
        );
        unsafe {
            let raw = &*hw.as_ptr();
            converter
                .convert_host_frame(
                    data,
                    w * bpp,
                    (0, 1, 2),
                    raw.data[0] as cu::CUdeviceptr,
                    usize::try_from(raw.linesize[0]).unwrap(),
                    raw.data[1] as cu::CUdeviceptr,
                    usize::try_from(raw.linesize[1]).unwrap(),
                )
                .unwrap();
        }
        let mut gpu = ffmpeg_next::frame::Video::empty();
        assert_eq!(
            unsafe { ffmpeg_sys_next::av_hwframe_transfer_data(gpu.as_mut_ptr(), hw.as_ptr(), 0) },
            0
        );
        let mut cpu_y = vec![0; w * h * 2];
        let mut cpu_uv = vec![0; w * h];
        super::super::hdr::convert_to_p010(
            data,
            w * bpp,
            w,
            h,
            format,
            &mut cpu_y,
            w * 2,
            &mut cpu_uv,
            w * 2,
        )
        .unwrap();
        for (plane, expected, rows) in [(0, cpu_y, h), (1, cpu_uv, h / 2)] {
            for row in 0..rows {
                let actual =
                    &gpu.data(plane)[row * gpu.stride(plane)..row * gpu.stride(plane) + w * 2];
                let expected = &expected[row * w * 2..(row + 1) * w * 2];
                for (a, b) in actual
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .zip(expected.as_chunks::<2>().0)
                {
                    let a = u16::from_le_bytes(*a) >> 6;
                    let b = u16::from_le_bytes(*b) >> 6;
                    assert!(a.abs_diff(b) <= 1, "P010 GPU/CPU disagreement {a}/{b}");
                }
            }
        }
    }

    /// Compare colored packed RGB10 and RGBA16 inputs against CPU P010.
    /// `nix develop .#cuda -c cargo test -p stargaze-server hdr_gpu_input_formats -- --ignored`.
    #[test]
    #[ignore = "requires NVIDIA GPU, NVENC Main10 and NVRTC"]
    fn hdr_gpu_input_formats() {
        let (width, height) = (512_u32, 256_u32);
        let config = EncoderConfig {
            width,
            height,
            framerate: 145,
            bitrate_mbps: 80,
            color: stargaze_core::color::ColorDescription::HDR10,
            tuning: stargaze_core::config::EncoderTuning::default(),
        };
        let encoder = init_encoder(&config).unwrap();
        for format in [
            PixelFormat::Bgra10,
            PixelFormat::Rgba10,
            PixelFormat::Rgba16,
        ] {
            let mut data = Vec::new();
            for row in 0..height {
                for x in 0..width {
                    let (red, green, blue) = (x * 2, row * 4, (x ^ row) * 2);
                    if format == PixelFormat::Rgba16 {
                        for sample in [red, green, blue, 1023] {
                            data.extend(
                                u16::try_from((sample * 65535 + 511) / 1023)
                                    .unwrap()
                                    .to_le_bytes(),
                            );
                        }
                    } else {
                        let word = if format == PixelFormat::Bgra10 {
                            red | (green << 10) | (blue << 20)
                        } else {
                            blue | (green << 10) | (red << 20)
                        };
                        data.extend(word.to_le_bytes());
                    }
                }
            }
            assert_hdr_cpu_gpu_agreement(&encoder, &data, format);
        }
    }

    /// Real CUDA/NVENC Main10 round trip plus pre-encode CPU/GPU agreement.
    /// `nix develop .#cuda -c cargo test -p stargaze-server native_hdr10_round_trip -- --ignored --nocapture`.
    /// Optionally set `STARGAZE_TEST_DUMP_HEVC` to save its first keyframe.
    #[test]
    #[ignore = "requires NVIDIA GPU, NVENC Main10 and NVRTC"]
    fn native_hdr10_round_trip() {
        use stargaze_core::color::ColorDescription;
        let (width, height) = (2048_u32, 256_u32);
        let w = width as usize;
        let config = EncoderConfig {
            width,
            height,
            framerate: 145,
            bitrate_mbps: 80,
            color: ColorDescription::HDR10,
            tuning: stargaze_core::config::EncoderTuning::default(),
        };
        let mut encoder = init_encoder(&config).unwrap();
        let metadata = stargaze_core::hdr::HdrMetadata {
            mastering: Some(stargaze_core::hdr::MasteringDisplay {
                primaries: [[35400, 14600], [8500, 39850], [6550, 2300]],
                white_point: [15635, 16450],
                min_luminance: 1,
                max_luminance: 100_000_000,
            }),
            content_light: Some(stargaze_core::hdr::ContentLight {
                max_cll: 10000,
                max_fall: 10000,
            }),
        };
        set_hdr_metadata(&mut encoder, metadata).unwrap();
        let data: Vec<u8> = (0..height)
            .flat_map(|row| {
                (0..width).flat_map(move |x| {
                    let (red, green, blue) = if row < height * 3 / 4 {
                        let c = x / 2;
                        (c, c, c)
                    } else {
                        match x * 3 / width {
                            0 => (1023, 0, 0),
                            1 => (0, 1023, 0),
                            _ => (0, 0, 1023),
                        }
                    };
                    (red | (green << 10) | (blue << 20)).to_le_bytes()
                })
            })
            .collect();
        assert_hdr_cpu_gpu_agreement(&encoder, &data, PixelFormat::Bgra10);
        let frame = Frame::CpuMapped {
            data,
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Bgra10,
        };
        let (tx, mut rx) = mpsc::channel(32);
        let mut packet = ffmpeg_next::Packet::empty();
        upload_and_encode(&mut encoder, &frame, 0, true).unwrap();
        drain_packets(
            &mut encoder.encoder,
            &mut packet,
            &tx,
            0,
            &encoder.extradata,
            0,
            0,
            std::time::Instant::now(),
        );
        flush_encoder(&mut encoder.encoder, &mut packet, &tx, &encoder.extradata);
        let mut packets = Vec::new();
        while let Ok(packet) = rx.try_recv() {
            packets.push(packet);
        }
        assert!(!packets.is_empty());
        if let Ok(path) = std::env::var("STARGAZE_TEST_DUMP_HEVC") {
            std::fs::write(path, &packets[0].data).unwrap();
        }
        let decoded = decode_packets(&packets.into_iter().map(|p| p.data).collect::<Vec<_>>());
        assert_eq!(decoded.format(), ffmpeg_next::format::Pixel::YUV420P10LE);
        assert_eq!(decoded.color_space(), ffmpeg_next::color::Space::BT2020NCL);
        assert_eq!(
            decoded.color_primaries(),
            ffmpeg_next::color::Primaries::BT2020
        );
        assert_eq!(
            decoded.color_transfer_characteristic(),
            ffmpeg_next::color::TransferCharacteristic::SMPTE2084
        );
        assert_eq!(
            unsafe { stargaze_core::hdr::HdrMetadata::from_avframe(decoded.as_ptr()) },
            metadata
        );
        let row = &decoded.data(0)[..w * 2];
        let codes: Vec<u16> = row
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        assert!(codes[0].abs_diff(64) <= 4);
        assert!(codes[w - 1].abs_diff(940) <= 4);
        assert!(
            codes.windows(2).filter(|v| v[0] != v[1]).count() > 300,
            "Main10 precision was reduced to 8 bits"
        );
    }

    /// Checks every gray code on CUDA before lossy encoding, including
    /// full-range RGB black/white and limited-range NV12 output endpoints.
    /// `nix develop .#cuda -c cargo test -p stargaze-server cpu_gpu_sdr_gradient_agreement -- --ignored`.
    #[test]
    #[ignore = "requires NVIDIA GPU, NVENC, NVRTC"]
    fn cpu_gpu_sdr_gradient_agreement() {
        use cudarc::driver::sys as cu;
        let (width, height) = (512_u32, 256_u32);
        let config = EncoderConfig {
            width,
            height,
            framerate: 145,
            color: stargaze_core::color::ColorDescription::SDR_STREAM,
            bitrate_mbps: 40,
            tuning: stargaze_core::config::EncoderTuning::default(),
        };
        let encoder = init_encoder(&config).unwrap();
        let _ctx = CudaCtxGuard::push(encoder.cuda_ctx, 0).unwrap();
        let (w, h) = (width as usize, height as usize);
        let data: Vec<u8> = (0..height)
            .flat_map(|_| {
                (0..width).flat_map(|x| {
                    let gray = u8::try_from(x / 2).unwrap();
                    [gray, gray, gray, 255]
                })
            })
            .collect();
        let mut cpu_y = vec![0; w * h];
        let mut cpu_uv = vec![0; w * h / 2];
        super::super::convert::convert_to_nv12(
            &data,
            w * 4,
            w,
            h,
            (2, 1, 0),
            &mut cpu_y,
            w,
            &mut cpu_uv,
            w,
        );
        let mut hw = ffmpeg_next::frame::Video::empty();
        assert_eq!(
            unsafe {
                ffmpeg_sys_next::av_hwframe_get_buffer(encoder.hw_frames_ctx, hw.as_mut_ptr(), 0)
            },
            0
        );
        let mut converter = super::super::egl_cuda::GpuNv12Converter::new(width, height).unwrap();
        unsafe {
            let raw = &*hw.as_ptr();
            converter
                .convert_host_frame(
                    &data,
                    w * 4,
                    (2, 1, 0),
                    raw.data[0] as cu::CUdeviceptr,
                    usize::try_from(raw.linesize[0]).unwrap(),
                    raw.data[1] as cu::CUdeviceptr,
                    usize::try_from(raw.linesize[1]).unwrap(),
                )
                .unwrap();
        }
        let mut gpu = ffmpeg_next::frame::Video::empty();
        assert_eq!(
            unsafe { ffmpeg_sys_next::av_hwframe_transfer_data(gpu.as_mut_ptr(), hw.as_ptr(), 0) },
            0
        );
        for row in 0..h {
            assert_eq!(
                &cpu_y[row * w..(row + 1) * w],
                &gpu.data(0)[row * gpu.stride(0)..row * gpu.stride(0) + w]
            );
        }
        for row in 0..h / 2 {
            assert_eq!(
                &cpu_uv[row * w..(row + 1) * w],
                &gpu.data(1)[row * gpu.stride(1)..row * gpu.stride(1) + w]
            );
        }
    }

    #[test]
    fn swscale_and_direct_sdr_paths_agree() {
        ffmpeg_next::init().unwrap();
        let mut scaler = sdr_capture_scaler(ffmpeg_next::format::Pixel::BGRA, 16, 16).unwrap();
        let mut src = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::BGRA, 16, 16);
        let mut dst = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::NV12, 16, 16);
        for (r, g, b) in [
            (0, 0, 0),
            (255, 255, 255),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (45, 120, 200),
        ] {
            let data = [b, g, r, 255].repeat(16 * 16);
            for row in 0..16 {
                let stride = src.stride(0);
                src.data_mut(0)[row * stride..row * stride + 64]
                    .copy_from_slice(&data[row * 64..row * 64 + 64]);
            }
            scaler.run(&src, &mut dst).unwrap();
            let mut y = vec![0; 16 * 16];
            let mut uv = vec![0; 16 * 8];
            super::super::convert::convert_to_nv12(
                &data,
                64,
                16,
                16,
                (2, 1, 0),
                &mut y,
                16,
                &mut uv,
                16,
            );
            for (a, b) in [
                (y[0], dst.data(0)[0]),
                (uv[0], dst.data(1)[0]),
                (uv[1], dst.data(1)[1]),
            ] {
                assert!(
                    (i16::from(a) - i16::from(b)).abs() <= 1,
                    "direct {a}, swscale {b}"
                );
            }
        }
    }

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
    const GBM_BO_TRANSFER_READ: u32 = 1;

    /// A GBM-allocated linear buffer exported as a DMA-BUF, used to feed
    /// the encoder a real DMA-BUF without a compositor.
    struct GbmTestBuffer {
        lib: libloading::Library,
        device: *mut c_void,
        bo: *mut c_void,
        drm_fd: c_int,
        width: u32,
        height: u32,
        format: u32,
        pixel_format: PixelFormat,
    }

    impl GbmTestBuffer {
        fn new(width: u32, height: u32) -> Result<Self, String> {
            Self::new_format(width, height, GBM_FORMAT_ARGB8888, PixelFormat::Bgra8)
        }

        fn new_format(
            width: u32,
            height: u32,
            format: u32,
            pixel_format: PixelFormat,
        ) -> Result<Self, String> {
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
                    bo = unsafe { bo_create(device, width, height, format, flags) };
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
                    format,
                    pixel_format,
                });
            }
            Err("no DRM render node could allocate a GBM buffer".to_string())
        }

        fn write_packed(&self, data: &[u8]) {
            let map = unsafe { *self.lib.get::<GbmBoMapFn>(b"gbm_bo_map\0").unwrap() };
            let unmap = unsafe { *self.lib.get::<GbmBoUnmapFn>(b"gbm_bo_unmap\0").unwrap() };
            let (mut stride, mut handle) = (0, ptr::null_mut());
            let p = unsafe {
                map(
                    self.bo,
                    0,
                    0,
                    self.width,
                    self.height,
                    GBM_BO_TRANSFER_WRITE,
                    &raw mut stride,
                    &raw mut handle,
                )
            };
            assert!(!p.is_null(), "GBM HDR map");
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    p.cast::<u8>(),
                    stride as usize * self.height as usize,
                )
            };
            let row = self.width as usize * 4;
            for y in 0..self.height as usize {
                bytes[y * stride as usize..y * stride as usize + row]
                    .copy_from_slice(&data[y * row..(y + 1) * row]);
            }
            unsafe {
                unmap(self.bo, handle);
            }
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
                // Pattern encodes the absolute row in G (coarse) and R
                // (fine): row = G * 8 + R / 32. Column (mod 256) is in B.
                for y in 0..self.height as usize {
                    for x in 0..self.width as usize {
                        let o = y * stride as usize + x * 4;
                        buf[o] = (x % 256) as u8; // B
                        buf[o + 1] = (y / 8) as u8; // G
                        buf[o + 2] = ((y % 8) * 32) as u8; // R
                        buf[o + 3] = 255; // A
                    }
                }
                bo_unmap(self.bo, map_data);
            }
            // Read back a few rows to verify the write actually reached
            // the buffer object (NVIDIA's GBM map goes through a staging
            // blit that could fail silently).
            let mut stride = 0u32;
            let mut map_data: *mut c_void = ptr::null_mut();
            let ptr = unsafe {
                bo_map(
                    self.bo,
                    0,
                    0,
                    self.width,
                    self.height,
                    GBM_BO_TRANSFER_READ,
                    &raw mut stride,
                    &raw mut map_data,
                )
            };
            if ptr.is_null() {
                eprintln!("readback map failed");
                return;
            }
            unsafe {
                let buf = std::slice::from_raw_parts(
                    ptr.cast::<u8>(),
                    stride as usize * self.height as usize,
                );
                let last = self.height as usize - 1;
                for row in [0usize, last / 2, last] {
                    let o = row * stride as usize + 8 * 4; // x = 8
                    let expected = [8u8, (row / 8) as u8, ((row % 8) * 32) as u8, 255];
                    let got = &buf[o..o + 4];
                    if got != expected {
                        eprintln!("fill readback row {row}: got {got:?} expected {expected:?}");
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
                format: self.pixel_format,
                drm_fourcc: self.format,
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

    /// Real 10-bit DMA-BUF → RGBA16 EGL/CUDA staging → Main10, including the
    /// precision-preserving CPU fallback. No linear framebuffer truncation.
    /// `nix develop .#cuda -c cargo test -p stargaze-server hdr10_dma_buf_paths -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires NVIDIA GPU, NVENC, NVRTC and GBM 10-bit DMA-BUF support"]
    #[allow(clippy::too_many_lines)] // One hardware scenario compares both paths and decoded pixels.
    fn hdr10_dma_buf_paths() {
        let (width, height) = (2048_u32, 256_u32);
        let buffer = GbmTestBuffer::new_format(
            width,
            height,
            u32::from_le_bytes(*b"XB30"),
            PixelFormat::Bgra10,
        )
        .unwrap();
        let data: Vec<u8> = (0..height)
            .flat_map(|row| {
                (0..width).flat_map(move |x| {
                    let (red, green, blue) = if row < height * 3 / 4 {
                        let c = x / 2;
                        (c, c, c)
                    } else {
                        match x * 3 / width {
                            0 => (1023, 0, 0),
                            1 => (0, 1023, 0),
                            _ => (0, 0, 1023),
                        }
                    };
                    (red | (green << 10) | (blue << 20)).to_le_bytes()
                })
            })
            .collect();
        buffer.write_packed(&data);
        let mut reference: Option<Vec<u16>> = None;
        for gpu in [true, false] {
            let config = EncoderConfig {
                width,
                height,
                framerate: 145,
                bitrate_mbps: 80,
                color: stargaze_core::color::ColorDescription::HDR10,
                tuning: stargaze_core::config::EncoderTuning::default(),
            };
            let mut encoder = init_encoder(&config).unwrap();
            if !gpu {
                encoder.gpu_path_failures = MAX_GPU_PATH_FAILURES;
                encoder.gpu_convert_failures = MAX_GPU_PATH_FAILURES;
            }
            let frame = Frame::DmaBuf(buffer.export().unwrap());
            let (tx, mut rx) = mpsc::channel(32);
            let mut packet = ffmpeg_next::Packet::empty();
            upload_and_encode(&mut encoder, &frame, 0, true).unwrap();
            if gpu {
                assert!(
                    encoder
                        .egl_bridge
                        .as_ref()
                        .is_some_and(super::super::egl_cuda::EglCudaBridge::has_gpu_path),
                    "HDR GPU converter must be active"
                );
                assert_eq!(
                    encoder.gpu_path_failures, 0,
                    "HDR DMA-BUF GPU path must work"
                );
            }
            drain_packets(
                &mut encoder.encoder,
                &mut packet,
                &tx,
                0,
                &encoder.extradata,
                0,
                0,
                std::time::Instant::now(),
            );
            flush_encoder(&mut encoder.encoder, &mut packet, &tx, &encoder.extradata);
            let mut packets = Vec::new();
            while let Ok(p) = rx.try_recv() {
                packets.push(p.data);
            }
            let decoded = decode_packets(&packets);
            assert_eq!(decoded.format(), ffmpeg_next::format::Pixel::YUV420P10LE);
            let row = &decoded.data(0)[..width as usize * 2];
            let codes: Vec<u16> = row
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_le_bytes(*p))
                .collect();
            assert!(codes[0].abs_diff(64) <= 4, "HDR black {}", codes[0]);
            assert!(codes[width as usize - 1].abs_diff(940) <= 4);
            for x in [width / 4, width / 2, width * 3 / 4] {
                let expected = u16::try_from(64 + (876 * (x / 2) + 511) / 1023).unwrap();
                assert!(
                    codes[x as usize].abs_diff(expected) <= 4,
                    "PQ code values were transfer-converted"
                );
            }
            for (x, expected) in [(width / 6, 294_u16), (width / 2, 658), (width * 5 / 6, 116)] {
                let at = (height as usize - 1) * decoded.stride(0) + x as usize * 2;
                let value = u16::from_le_bytes([decoded.data(0)[at], decoded.data(0)[at + 1]]);
                assert!(
                    value.abs_diff(expected) <= 8,
                    "HDR DMA-BUF primary/order/orientation incorrect: {value}/{expected}"
                );
            }
            assert!(codes.windows(2).filter(|c| c[0] != c[1]).count() > 300);
            if let Some(ref_data) = &reference {
                for (&a, &b) in codes.iter().zip(ref_data) {
                    assert!(a.abs_diff(b) <= 4);
                }
            } else {
                reference = Some(codes);
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
            color: stargaze_core::color::ColorDescription::SDR_STREAM,
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

    /// Encodes 30 frames of `buffer` and writes the raw HEVC bitstream to
    /// `path`. When `use_gpu` is false the GPU NV12 path is disabled so
    /// the frames take the CPU conversion fallback. Returns the per-frame
    /// `upload_dmabuf_and_encode` durations and whether the GPU path
    /// stayed active for every frame.
    fn encode_frames_to_file(
        use_gpu: bool,
        buffer: &GbmTestBuffer,
        path: &str,
    ) -> (Vec<std::time::Duration>, bool) {
        use std::io::Write;

        let config = EncoderConfig {
            width: buffer.width,
            height: buffer.height,
            color: stargaze_core::color::ColorDescription::SDR_STREAM,
            framerate: 175,
            bitrate_mbps: 60,
            tuning: stargaze_core::config::EncoderTuning::default(),
        };
        let mut encoder = init_encoder(&config).expect("NVENC encoder init");
        if !use_gpu {
            encoder.gpu_path_failures = MAX_GPU_PATH_FAILURES;
        }

        let (packets_tx, mut packets_rx) = mpsc::channel::<EncodedPacket>(1024);
        let mut packet = ffmpeg_next::Packet::empty();
        let mut timings = Vec::new();

        for pts in 0..30u64 {
            let info = buffer.export().expect("DMA-BUF export");
            let start = std::time::Instant::now();
            upload_dmabuf_and_encode(&mut encoder, &info, pts, pts == 0)
                .expect("DMA-BUF upload + encode");
            timings.push(start.elapsed());
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
        flush_encoder(
            &mut encoder.encoder,
            &mut packet,
            &packets_tx,
            &encoder.extradata,
        );

        let mut file = std::fs::File::create(path).expect("create bitstream file");
        while let Ok(pkt) = packets_rx.try_recv() {
            file.write_all(&pkt.data).expect("write bitstream");
        }

        let gpu_active = use_gpu && encoder.gpu_path_failures == 0;
        (timings, gpu_active)
    }

    /// Diagnostic: encode the same full-resolution DMA-BUF through the
    /// GPU NV12 path and the CPU fallback path, dump both bitstreams to
    /// /tmp for visual comparison, and print per-frame timings.
    ///
    /// Decode the outputs with e.g.:
    /// `ffmpeg -y -i /tmp/stargaze_gpu_path.h265 -frames:v 1 /tmp/gpu.png`
    #[test]
    #[ignore = "requires NVIDIA GPU, NVENC, NVRTC, and a DRM render node"]
    fn gpu_vs_cpu_path_visual_diagnostic() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let buffer = GbmTestBuffer::new(3440, 1440).expect("GBM buffer allocation");
        buffer.fill();

        let (gpu_timings, gpu_active) =
            encode_frames_to_file(true, &buffer, "/tmp/stargaze_gpu_path.h265");
        assert!(gpu_active, "GPU path must stay active for every frame");
        let (cpu_timings, _) = encode_frames_to_file(false, &buffer, "/tmp/stargaze_cpu_path.h265");

        let report = |name: &str, timings: &[std::time::Duration]| {
            // Skip the first frame (bridge/encoder warmup).
            let us: Vec<u128> = timings
                .iter()
                .skip(1)
                .map(std::time::Duration::as_micros)
                .collect();
            let avg = us.iter().sum::<u128>() / us.len() as u128;
            let min = us.iter().min().copied().unwrap_or(0);
            let max = us.iter().max().copied().unwrap_or(0);
            eprintln!("{name}: avg {avg} us, min {min} us, max {max} us (29 frames)");
        };
        report("GPU path", &gpu_timings);
        report("CPU path", &cpu_timings);
    }

    /// Diagnostic: bypass the encoder and inspect the raw output of the
    /// EGL import + blit (via the GPU→CPU readback). Prints which source
    /// row each output row actually contains.
    #[test]
    #[ignore = "requires NVIDIA GPU, NVENC, NVRTC, and a DRM render node"]
    fn blit_row_mapping_diagnostic() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let (width, height) = (3440u32, 1440u32);
        let buffer = GbmTestBuffer::new(width, height).expect("GBM buffer allocation");
        buffer.fill();

        let config = EncoderConfig {
            width,
            height,
            framerate: 175,
            bitrate_mbps: 60,
            color: stargaze_core::color::ColorDescription::SDR_STREAM,
            tuning: stargaze_core::config::EncoderTuning::default(),
        };
        let mut encoder = init_encoder(&config).expect("NVENC encoder init");

        // One encode call to lazily initialize the EGL bridge.
        let info = buffer.export().expect("DMA-BUF export");
        upload_dmabuf_and_encode(&mut encoder, &info, 0, true).expect("encode");

        unsafe {
            let res = cudarc::driver::sys::cuCtxPushCurrent_v2(encoder.cuda_ctx);
            assert_eq!(res, cudarc::driver::sys::CUresult::CUDA_SUCCESS);
        }
        let bridge = encoder.egl_bridge.as_ref().expect("bridge");
        let info = buffer.export().expect("DMA-BUF export");
        let rgba = bridge.import_dmabuf_to_cpu(&info).expect("blit readback");
        unsafe {
            let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
            cudarc::driver::sys::cuCtxPopCurrent_v2(&raw mut old);
        }

        report_row_mapping(&rgba, width, height, false);
    }

    /// Prints which source row each output row contains, given a
    /// tightly-packed readback of a buffer filled by
    /// [`GbmTestBuffer::fill`] (pattern: row = G*8 + R/32). `bgra` selects
    /// the channel order of the readback (raw DMA-BUF bytes are BGRA, the
    /// GL blit outputs RGBA).
    #[allow(clippy::many_single_char_names)] // pixel/coordinate shorthand
    fn report_row_mapping(rgba: &[u8], width: u32, height: u32, bgra: bool) {
        let (r_off, b_off) = if bgra { (2, 0) } else { (0, 2) };
        let w = width as usize;
        let mut first_black = None;
        for y in 0..height as usize {
            let o = (y * w + 8) * 4;
            let (r, g, b, a) = (rgba[o + r_off], rgba[o + 1], rgba[o + b_off], rgba[o + 3]);
            if (r, g, b, a) == (0, 0, 0, 0) && first_black.is_none() {
                first_black = Some(y);
            }
            if y % 64 == 0 || y == 455 || y == 456 || y == 1439 {
                let src_row = usize::from(g) * 8 + usize::from(r) / 32;
                eprintln!("out row {y}: rgba=({r},{g},{b},{a}) -> source row {src_row}");
            }
        }
        eprintln!("first all-zero output row: {first_black:?}");
    }

    /// Builds a tightly-specified BGRA test frame: four solid quadrants
    /// (red, green, blue, white) with `stride` bytes per row.
    fn bgra_quadrant_frame(width: u32, height: u32, stride: usize) -> Vec<u8> {
        let (w, h) = (width as usize, height as usize);
        let mut data = vec![0u8; (h - 1) * stride + w * 4];
        for y in 0..h {
            for x in 0..w {
                let bgra: [u8; 4] = match (x < w / 2, y < h / 2) {
                    (true, true) => [0, 0, 255, 255],       // red
                    (false, true) => [0, 255, 0, 255],      // green
                    (true, false) => [255, 0, 0, 255],      // blue
                    (false, false) => [255, 255, 255, 255], // white
                };
                data[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&bgra);
            }
        }
        data
    }

    /// Encodes `frames` repetitions of a CPU-memory BGRA frame and
    /// returns the encoded packets, whether the GPU converter handled
    /// every frame, and the per-frame upload+encode durations.
    fn encode_cpu_frames(
        use_gpu: bool,
        data: &[u8],
        width: u32,
        height: u32,
        stride: u32,
        frames: u64,
    ) -> (Vec<Vec<u8>>, bool, Vec<std::time::Duration>) {
        // Tuning overridable for throughput experiments, e.g.
        // STARGAZE_TEST_PRESET=p1 STARGAZE_TEST_MULTIPASS=disabled.
        let tuning = stargaze_core::config::EncoderTuning {
            preset: std::env::var("STARGAZE_TEST_PRESET").unwrap_or_else(|_| "p4".to_string()),
            multipass: std::env::var("STARGAZE_TEST_MULTIPASS")
                .unwrap_or_else(|_| "qres".to_string()),
        };
        let config = EncoderConfig {
            width,
            height,
            framerate: 175,
            bitrate_mbps: 60,
            color: stargaze_core::color::ColorDescription::SDR_STREAM,
            tuning,
        };
        let mut encoder = init_encoder(&config).expect("NVENC encoder init");
        if !use_gpu {
            encoder.gpu_convert_failures = MAX_GPU_PATH_FAILURES;
        }

        let (packets_tx, mut packets_rx) = mpsc::channel::<EncodedPacket>(1024);
        let mut packet = ffmpeg_next::Packet::empty();
        let mut timings = Vec::new();
        for pts in 0..frames {
            let start = std::time::Instant::now();
            upload_cpu_data_and_encode(
                &mut encoder,
                data,
                width,
                height,
                stride,
                PixelFormat::Bgra8,
                pts,
                pts == 0,
            )
            .expect("upload + encode");
            timings.push(start.elapsed());
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
        flush_encoder(
            &mut encoder.encoder,
            &mut packet,
            &packets_tx,
            &encoder.extradata,
        );

        let mut packets = Vec::new();
        while let Ok(pkt) = packets_rx.try_recv() {
            packets.push(pkt.data);
        }
        let gpu_active =
            use_gpu && encoder.gpu_convert_failures == 0 && encoder.gpu_converter.is_some();
        (packets, gpu_active, timings)
    }

    /// Decodes HEVC packets with the software decoder and returns the
    /// last decoded frame (YUV420P).
    fn decode_packets(packets: &[Vec<u8>]) -> ffmpeg_next::frame::Video {
        let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC).expect("HEVC decoder");
        let ctx = ffmpeg_next::codec::Context::new_with_codec(codec);
        let mut decoder = ctx.decoder().video().expect("video decoder");
        // avcodec_receive_frame unrefs the destination on every call, so
        // the last *successful* frame must be kept aside — the loop always
        // ends with a failing call that would wipe it.
        let mut frame = ffmpeg_next::frame::Video::empty();
        let mut last = None;
        for data in packets {
            let pkt = ffmpeg_next::Packet::copy(data);
            if decoder.send_packet(&pkt).is_ok() {
                while decoder.receive_frame(&mut frame).is_ok() {
                    last = Some(frame.clone());
                }
            }
        }
        let _ = decoder.send_eof();
        while decoder.receive_frame(&mut frame).is_ok() {
            last = Some(frame.clone());
        }
        last.expect("decoder should produce at least one frame")
    }

    /// Samples decoded Y/U/V at a pixel position (YUV420P layout).
    fn sample_yuv(frame: &ffmpeg_next::frame::Video, x: usize, y: usize) -> (u8, u8, u8) {
        let luma = frame.data(0)[y * frame.stride(0) + x];
        let cb = frame.data(1)[(y / 2) * frame.stride(1) + x / 2];
        let cr = frame.data(2)[(y / 2) * frame.stride(2) + x / 2];
        (luma, cb, cr)
    }

    /// Round-trip correctness for CPU-memory (`MemFd`) frames through
    /// both the GPU converter and the CPU fallback: known BGRA quadrants
    /// must come back as the expected BT.709 limited-range YUV.
    ///
    /// This is the regression test for scrambled/discolored encodes: any
    /// channel-order, stride, pitch, or layout bug in either conversion
    /// path shifts the sampled values far beyond the tolerance.
    #[test]
    #[ignore = "requires NVIDIA GPU, NVENC, NVRTC"]
    fn cpu_frame_gpu_and_cpu_conversions_round_trip_correct_colors() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .try_init();

        let (width, height) = (3440u32, 1440u32);
        // Padded stride exercises the row-repacking path.
        let stride = width as usize * 4 + 256;
        let data = bgra_quadrant_frame(width, height, stride);

        // BT.709 limited-range encodings of the quadrant colors
        // (matching both converters' integer math).
        let expected = [
            ("red", (63u8, 102u8, 240u8)),
            ("green", (172, 41, 26)),
            ("blue", (31, 240, 118)),
            ("white", (235, 128, 128)),
        ];
        let (w, h) = (width as usize, height as usize);
        let centers = [
            (w / 4, h / 4),
            (3 * w / 4, h / 4),
            (w / 4, 3 * h / 4),
            (3 * w / 4, 3 * h / 4),
        ];

        for use_gpu in [true, false] {
            let name = if use_gpu { "GPU" } else { "CPU" };
            let (packets, gpu_active, timings) = encode_cpu_frames(
                use_gpu,
                &data,
                width,
                height,
                u32::try_from(stride).expect("stride fits u32"),
                10,
            );
            if use_gpu {
                assert!(gpu_active, "GPU converter must handle every frame");
            }
            let us: Vec<u128> = timings
                .iter()
                .skip(1)
                .map(std::time::Duration::as_micros)
                .collect();
            eprintln!(
                "{name} conversion: avg {} us/frame (frames 1..10)",
                us.iter().sum::<u128>() / us.len() as u128
            );

            let frame = decode_packets(&packets);
            assert_eq!(frame.format(), ffmpeg_next::format::Pixel::YUV420P);
            assert_eq!(frame.color_range(), ffmpeg_next::color::Range::MPEG);
            assert_eq!(frame.color_space(), ffmpeg_next::color::Space::BT709);
            assert_eq!(
                frame.color_primaries(),
                ffmpeg_next::color::Primaries::BT709
            );
            assert_eq!(
                frame.color_transfer_characteristic(),
                ffmpeg_next::color::TransferCharacteristic::IEC61966_2_1
            );
            for ((color, (ey, eu, ev)), (cx, cy)) in expected.iter().zip(centers) {
                let (luma, cb, cr) = sample_yuv(&frame, cx, cy);
                let ok = (i16::from(luma) - i16::from(*ey)).abs() <= 12
                    && (i16::from(cb) - i16::from(*eu)).abs() <= 12
                    && (i16::from(cr) - i16::from(*ev)).abs() <= 12;
                assert!(
                    ok,
                    "{name} path, {color} quadrant: got YUV ({luma},{cb},{cr}), \
                     expected ~({ey},{eu},{ev})"
                );
            }
        }
    }
}
