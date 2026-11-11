//! `FFmpeg` H.265 decoder with VAAPI hardware acceleration.
//!
//! Attempts VAAPI hardware decoding first, falls back to multi-threaded
//! software decode if VAAPI is unavailable.  Hardware-decoded frames are
//! exported as DRM PRIME dma-bufs for zero-copy rendering when possible,
//! otherwise transferred from GPU to CPU (NV12/YUV420P) before being sent
//! to the renderer.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::config::Codec;
use stargaze_core::decode::{DecodeError, DecodedFrame, DecoderConfig, FrameStats};
use stargaze_core::transport::ReassembledFrame;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::VideoFrame;

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
    pub(crate) hw_accel: bool,
    /// Decode-failure score: +2 per `receive_frame` error, -1 per decoded
    /// frame. A healthy stream with sporadic errors decays back to zero; a
    /// stream failing every picture climbs even when each failure is
    /// interleaved with a delivered frame (nvidia-vaapi-driver's export
    /// breakage looks exactly like that).
    error_score: u32,
    /// Session contract, re-applied after decoder recreation.
    color: stargaze_core::color::ColorDescription,
    color_logged: bool,
}

/// Decode-failure score at which the zero-copy path is assumed to be
/// poisoning the decoder and is permanently disabled (~0.5 s of
/// every-frame failures at 85 fps).
const MAX_ZERO_COPY_ERROR_SCORE: u32 = 40;

/// A decoded frame in CPU memory, shipped from the decode thread to the
/// extraction thread so plane extraction overlaps the next decode.
pub(crate) struct RawDecodedFrame {
    /// CPU-side frame (NV12 after VAAPI transfer, YUV420P from software
    /// decode). Buffers are refcounted `AVBuffer`s.
    pub(crate) frame: ffmpeg_next::frame::Video,
    /// Pipeline timings accumulated so far (decode_us already filled).
    pub(crate) stats: FrameStats,
}

// Safety: the frame owns CPU-side refcounted buffers and is accessed by
// one thread at a time (moved through the channel).
unsafe impl Send for RawDecodedFrame {}

/// A decoded frame still resident in GPU memory, exported as DRM PRIME
/// dma-buf file descriptors (the zero-copy render path).
///
/// Holds a mapped `DRM_PRIME` `AVFrame` whose buffer references keep the
/// underlying VAAPI surface alive; dropping it closes the fds and releases
/// the surface back to the decoder's pool.
pub struct DmaBufFrame {
    /// The `AV_PIX_FMT_DRM_PRIME` frame owning the descriptor and fds.
    frame: ffmpeg_next::frame::Video,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Presentation timestamp.
    pub pts: u64,
    /// Per-frame pipeline timing, for the client stats overlay.
    pub stats: FrameStats,
}

// Safety: the frame owns refcounted buffers and fds and is accessed by one
// thread at a time (moved through the channel to the renderer).
unsafe impl Send for DmaBufFrame {}

impl DmaBufFrame {
    /// Actual HDR static metadata retained on the exported frame.
    #[must_use]
    pub fn hdr_metadata(&self) -> stargaze_core::hdr::HdrMetadata {
        unsafe { stargaze_core::hdr::HdrMetadata::from_avframe(self.frame.as_ptr()) }
    }
    /// The DRM PRIME descriptor (objects, layers, planes) of this frame.
    ///
    /// Valid for as long as `self` is alive.
    #[must_use]
    // data[0] is allocated by ffmpeg as an AVDRMFrameDescriptor, so it is
    // properly aligned despite the u8 pointer type.
    #[allow(clippy::cast_ptr_alignment)]
    pub fn descriptor(&self) -> &ffmpeg_sys_next::AVDRMFrameDescriptor {
        // A DRM_PRIME frame stores the descriptor pointer in data[0].
        unsafe {
            &*(*self.frame.as_ptr()).data[0]
                .cast_const()
                .cast::<ffmpeg_sys_next::AVDRMFrameDescriptor>()
        }
    }
}

/// Output of the decode thread, shipped to the extraction thread.
pub(crate) enum DecodeOutput {
    /// A CPU frame that still needs plane extraction.
    Raw(RawDecodedFrame),
    /// A frame already in its final form, passed through unchanged.
    Ready(VideoFrame),
}

/// Why [`run_decode_loop`] returned.
pub(crate) enum LoopExit {
    /// Shutdown was signaled or the input channel closed.
    Finished,
    /// The decoder is persistently failing and must be re-created
    /// (zero-copy exports can poison nvidia-vaapi-driver's decode state
    /// beyond recovery).
    Reinit,
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

    if !config.color.is_supported_stream() {
        return Err(DecodeError::InitError(format!(
            "unsupported negotiated color {:?}; no HDR presentation/tone mapping",
            config.color
        )));
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
fn init_vaapi_decoder(config: &DecoderConfig) -> Result<FfmpegDecoder, DecodeError> {
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
        // The zero-copy render path keeps surfaces alive outside the
        // decoder (in the render channel and on screen), so the surface
        // pool needs headroom beyond the codec's own reference needs.
        // Kept small: nvidia-vaapi-driver fails vaBeginPicture with
        // MAX_NUM_EXCEEDED when the pool exceeds what NVDEC can map.
        (*raw_ctx).extra_hw_frames = std::env::var("STARGAZE_EXTRA_HW_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
    }

    let decoder = context.decoder().video().map_err(|e| {
        unsafe { ffmpeg_sys_next::av_buffer_unref(&raw mut hw_device_ctx) };
        DecodeError::InitError(format!("failed to open VAAPI hevc decoder: {e}"))
    })?;

    Ok(FfmpegDecoder {
        decoder,
        hw_device_ctx,
        hw_accel: true,
        error_score: 0,
        color: config.color,
        color_logged: false,
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
        error_score: 0,
        color: config.color,
        color_logged: false,
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
    raw_tx: &std::sync::mpsc::SyncSender<DecodeOutput>,
    shutdown: &Arc<AtomicBool>,
    zero_copy: &AtomicBool,
    idr_tx: &mpsc::Sender<()>,
) -> Result<LoopExit, DecodeError> {
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
            convert_us: frame.convert_us,
            encode_us: frame.encode_us,
            queue_us: saturating_us(decode_start.duration_since(frame.received_at)),
            decode_us: 0, // filled in when the decoded frame pops out
            packet_bytes: u32::try_from(frame.data.len()).unwrap_or(u32::MAX),
        };

        // Keyframes only ever arrive at session start or as loss
        // recovery (infinite GOP), and each carries its parameter sets
        // (the server prepends extradata). Flushing first clears any
        // stale DPB state — without it, a recovery IDR can be rejected
        // with "Duplicate POC in a sequence: 0" when a gray placeholder
        // or the original POC-0 picture still occupies the buffer, and
        // the corruption then persists past the keyframe.
        if frame.is_keyframe {
            decoder.decoder.flush();
        }

        let mut packet = ffmpeg_next::Packet::copy(&frame.data);
        packet.set_pts(Some(frame.pts.cast_signed()));

        if let Err(e) = decoder.decoder.send_packet(&packet) {
            warn!(pts = frame.pts, "Skipping corrupt packet: {e}");
            // A rejected packet breaks decoder references; ask for a
            // keyframe (rate-limited by the transport) to recover.
            let _ = idr_tx.try_send(());
            continue;
        }

        drain_decoded_frames(decoder, raw_tx, stats, decode_start, zero_copy, idr_tx)?;

        // A decoder that errors on every packet while zero-copy is active
        // is being poisoned by the surface exports (seen with
        // nvidia-vaapi-driver): turn the exports off for good and have
        // the decoder re-created — the driver's decode state doesn't
        // recover even after the exports stop.
        if decoder.error_score >= MAX_ZERO_COPY_ERROR_SCORE
            && zero_copy.swap(false, Ordering::Relaxed)
        {
            warn!(
                "Persistent decode failures with zero-copy active, \
                 switching to CPU frames"
            );
            let _ = idr_tx.try_send(());
            return Ok(LoopExit::Reinit);
        }
    }

    // Flush: send EOF and drain remaining frames.
    if let Err(e) = decoder.decoder.send_eof() {
        warn!("Failed to send EOF to decoder: {e}");
    } else {
        drain_decoded_frames(
            decoder,
            raw_tx,
            FrameStats::default(),
            std::time::Instant::now(),
            zero_copy,
            idr_tx,
        )?;
    }

    info!("Decoder loop finished");
    Ok(LoopExit::Finished)
}

/// Runs the plane-extraction loop on its own thread: receives raw CPU
/// frames from the decode thread, extracts pixel planes in the frame's
/// native layout, and sends [`DecodedFrame`]s to the renderer.
///
/// Running extraction separately overlaps the (expensive) VAAPI GPU→CPU
/// transfer of the next frame with the plane copies of the current one.
pub(crate) fn run_extract_loop(
    raw_rx: &std::sync::mpsc::Receiver<DecodeOutput>,
    decoded_tx: &std::sync::mpsc::Sender<VideoFrame>,
) {
    while let Ok(output) = raw_rx.recv() {
        let raw = match output {
            DecodeOutput::Raw(raw) => raw,
            // Zero-copy frames need no extraction; forward in order.
            DecodeOutput::Ready(frame) => {
                if decoded_tx.send(frame).is_err() {
                    info!("Decoded frame receiver dropped, stopping extraction");
                    return;
                }
                continue;
            }
        };
        let frame = &raw.frame;
        let (width, height, format) = (frame.width(), frame.height(), frame.format());

        let mut output = if matches!(
            format,
            ffmpeg_next::format::Pixel::P010LE | ffmpeg_next::format::Pixel::YUV420P10LE
        ) {
            extract_p010(frame, width, height)
        } else if format == ffmpeg_next::format::Pixel::NV12 {
            extract_nv12(frame, width, height)
        } else if format == ffmpeg_next::format::Pixel::YUV420P {
            extract_yuv420p(frame, width, height)
        } else {
            // Never truncate P010/10-bit or relabel PQ through swscale.
            warn!(
                ?format,
                "Unsupported decoded precision/layout; stopping extraction"
            );
            return;
        };

        output.stats = raw.stats;

        if decoded_tx.send(VideoFrame::Cpu(output)).is_err() {
            info!("Decoded frame receiver dropped, stopping extraction");
            return;
        }
    }
    info!("Extraction loop finished");
}

/// Converts a duration to whole microseconds, saturating at `u32::MAX`.
fn saturating_us(d: std::time::Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

/// Maps a VAAPI hardware frame to a `DRM_PRIME` frame (dma-buf export).
///
/// The returned frame references the source surface, keeping it alive
/// until the renderer is done with it.
fn map_to_drm_prime(src: &ffmpeg_next::frame::Video) -> Result<ffmpeg_next::frame::Video, String> {
    let mut mapped = ffmpeg_next::frame::Video::empty();
    unsafe {
        (*mapped.as_mut_ptr()).format = ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        let ret = ffmpeg_sys_next::av_hwframe_map(
            mapped.as_mut_ptr(),
            src.as_ptr(),
            ffmpeg_sys_next::AV_HWFRAME_MAP_READ as libc::c_int,
        );
        if ret < 0 {
            return Err(format!("av_hwframe_map to DRM_PRIME failed (error {ret})"));
        }
    }
    let ret = unsafe { ffmpeg_sys_next::av_frame_copy_props(mapped.as_mut_ptr(), src.as_ptr()) };
    if ret < 0 {
        return Err(format!("copy DRM frame color/metadata: {ret}"));
    }
    Ok(mapped)
}

/// Drains all available decoded frames from the codec and ships them to
/// the extraction thread: hardware frames are exported as dma-bufs when
/// `zero_copy` is set (clearing it on failure), otherwise transferred to
/// CPU memory.
fn drain_decoded_frames(
    decoder: &mut FfmpegDecoder,
    raw_tx: &std::sync::mpsc::SyncSender<DecodeOutput>,
    stats: FrameStats,
    decode_start: std::time::Instant,
    zero_copy: &AtomicBool,
    idr_tx: &mpsc::Sender<()>,
) -> Result<(), DecodeError> {
    loop {
        // A fresh frame each iteration: ownership moves to the extraction
        // thread. receive_frame fills it from the decoder's buffer pool.
        let mut decoded_frame = ffmpeg_next::frame::Video::empty();
        match decoder.decoder.receive_frame(&mut decoded_frame) {
            Ok(()) => decoder.error_score = decoder.error_score.saturating_sub(1),
            Err(ffmpeg_next::Error::Other {
                errno: libc::EAGAIN,
            }) => break,
            Err(e) => {
                warn!("receive_frame error: {e}");
                decoder.error_score += 2;
                // The failed picture corrupts every frame that references
                // it; ask for a keyframe (rate-limited by the transport).
                let _ = idr_tx.try_send(());
                break;
            }
        }

        // Before DMA-BUF export OR CPU transfer: neither fallback may hide
        // PQ/HLG/wide gamut/range changes or silently truncate Main10.
        check_session_color(decoder, &decoded_frame)?;

        // If this is a hardware frame (VAAPI surface), transfer to CPU.
        let is_vaapi =
            decoder.hw_accel && decoded_frame.format() == ffmpeg_next::format::Pixel::VAAPI;

        if is_vaapi && zero_copy.load(Ordering::Relaxed) {
            match map_to_drm_prime(&decoded_frame) {
                Ok(mapped) => {
                    let frame = DmaBufFrame {
                        width: decoded_frame.width(),
                        height: decoded_frame.height(),
                        pts: decoded_frame.pts().map_or(0, i64::cast_unsigned),
                        stats: FrameStats {
                            decode_us: saturating_us(decode_start.elapsed()),
                            ..stats
                        },
                        frame: mapped,
                    };
                    if raw_tx
                        .send(DecodeOutput::Ready(VideoFrame::DmaBuf(frame)))
                        .is_err()
                    {
                        return Ok(());
                    }
                    continue;
                }
                Err(e) => {
                    // The renderer switches to CPU frames as soon as the
                    // flag clears; fall through to the transfer path.
                    warn!("Zero-copy dma-buf export failed ({e}), using CPU transfer");
                    zero_copy.store(false, Ordering::Relaxed);
                }
            }
        }

        let cpu_frame = if is_vaapi {
            let mut sw_frame = ffmpeg_next::frame::Video::empty();
            let ret = unsafe {
                ffmpeg_sys_next::av_hwframe_transfer_data(
                    sw_frame.as_mut_ptr(),
                    decoded_frame.as_ptr(),
                    0,
                )
            };
            if ret < 0 {
                warn!("av_hwframe_transfer_data failed (error {ret}), skipping frame");
                continue;
            }
            let ret = unsafe {
                ffmpeg_sys_next::av_frame_copy_props(sw_frame.as_mut_ptr(), decoded_frame.as_ptr())
            };
            if ret < 0 {
                return Err(DecodeError::FfmpegError(format!(
                    "copy decoded color properties: {ret}"
                )));
            }
            sw_frame
        } else {
            decoded_frame
        };

        // Log decoded frame details for first few frames.
        static DIAG_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let diag_n = DIAG_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if diag_n < 5 {
            info!(
                frame = diag_n,
                decoded_width = cpu_frame.width(),
                decoded_height = cpu_frame.height(),
                decoded_format = ?cpu_frame.format(),
                hw_accel = decoder.hw_accel,
                planes = cpu_frame.planes(),
                stride_0 = plane_stride(&cpu_frame, 0),
                stride_1 = plane_stride(&cpu_frame, 1),
                stride_2 = plane_stride(&cpu_frame, 2),
                "Decoded frame diagnostics"
            );
        }

        let raw = RawDecodedFrame {
            frame: cpu_frame,
            stats: FrameStats {
                decode_us: saturating_us(decode_start.elapsed()),
                ..stats
            },
        };

        if raw_tx.send(DecodeOutput::Raw(raw)).is_err() {
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
#[allow(clippy::many_single_char_names)] // y/u/v are the domain names
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

    let y = copy_plane(y_data, y_stride, w, h);
    let u = copy_plane(u_data, u_stride, cw, ch);
    let v = copy_plane(v_data, v_stride, cw, ch);

    DecodedFrame {
        pixels: stargaze_core::decode::FramePixels::I420 { y, u, v },
        hdr_metadata: stargaze_core::hdr::HdrMetadata::default(),
        width,
        height,
        pts: frame.pts().map_or(0, i64::cast_unsigned),
        stats: FrameStats::default(),
    }
}

/// Extracts NV12 planes (Y + interleaved UV) from a frame, stripping
/// stride padding. The UV plane stays interleaved — SDL renders NV12
/// textures natively, so deinterleaving here would be wasted work.
fn extract_nv12(frame: &ffmpeg_next::frame::Video, width: u32, height: u32) -> DecodedFrame {
    let w = width as usize;
    let h = height as usize;

    let y = copy_plane(frame.data(0), frame.stride(0), w, h);
    // UV plane: one byte pair per 2x2 block → `w` bytes per row, h/2 rows.
    let uv = copy_plane(frame.data(1), frame.stride(1), w, h / 2);

    DecodedFrame {
        pixels: stargaze_core::decode::FramePixels::Nv12 { y, uv },
        hdr_metadata: stargaze_core::hdr::HdrMetadata::default(),
        width,
        height,
        pts: frame.pts().map_or(0, i64::cast_unsigned),
        stats: FrameStats::default(),
    }
}

/// Keep P010 as-is, or repack planar Main10 words without reducing precision.
fn extract_p010(frame: &ffmpeg_next::frame::Video, width: u32, height: u32) -> DecodedFrame {
    let (w, h) = (width as usize, height as usize);
    let mut y = copy_plane(frame.data(0), frame.stride(0), w * 2, h);
    let uv = if frame.format() == ffmpeg_next::format::Pixel::P010LE {
        copy_plane(frame.data(1), frame.stride(1), w * 2, h / 2)
    } else {
        for word in y.as_chunks_mut::<2>().0 {
            *word = (u16::from_le_bytes(*word) << 6).to_le_bytes();
        }
        let mut uv = vec![0; w * h];
        for row in 0..h / 2 {
            for x in 0..w / 2 {
                for (plane, offset) in [(1, 0), (2, 2)] {
                    let at = row * frame.stride(plane) + x * 2;
                    let value =
                        u16::from_le_bytes([frame.data(plane)[at], frame.data(plane)[at + 1]]) << 6;
                    let dst = row * w * 2 + x * 4 + offset;
                    uv[dst..dst + 2].copy_from_slice(&value.to_le_bytes());
                }
            }
        }
        uv
    };
    DecodedFrame {
        pixels: stargaze_core::decode::FramePixels::P010 { y, uv },
        width,
        height,
        hdr_metadata: unsafe { stargaze_core::hdr::HdrMetadata::from_avframe(frame.as_ptr()) },
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

fn check_session_color(
    decoder: &mut FfmpegDecoder,
    frame: &ffmpeg_next::frame::Video,
) -> Result<(), DecodeError> {
    validate_negotiated_color(frame, decoder.color)?;
    if !decoder.color_logged {
        info!(stream = ?decoder.color, decoded_format = ?frame.format(),
            range = ?frame.color_range(), matrix = ?frame.color_space(),
            primaries = ?frame.color_primaries(), transfer = ?frame.color_transfer_characteristic(),
            hw_accel = decoder.hw_accel, "Decoded color mode validated before export/transfer");
        decoder.color_logged = true;
    }
    Ok(())
}

/// New peers must honor their explicit session contract. Only legacy peers
/// may omit metadata or use the historical BT.709 transfer label.
fn validate_negotiated_color(
    frame: &ffmpeg_next::frame::Video,
    expected: stargaze_core::color::ColorDescription,
) -> Result<(), DecodeError> {
    let raw = unsafe { &*frame.as_ptr() };
    if expected.is_hdr10() {
        validate_hdr_metadata(frame)?;
        let format = decoded_sw_format(frame);
        let valid = matches!(
            format,
            ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_P010LE
                | ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_YUV420P10LE
        ) && raw.color_range == ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG
            && raw.colorspace == ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT2020_NCL
            && raw.color_primaries == ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT2020
            && raw.color_trc == ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
        return if valid {
            Ok(())
        } else {
            Err(DecodeError::FfmpegError(format!(
                "decoded HDR precision/color differs from negotiated HDR10: {format:?}"
            )))
        };
    }
    validate_decoded_color(frame)?;
    if expected != stargaze_core::color::ColorDescription::LEGACY_SDR
        && (expected != stargaze_core::color::ColorDescription::SDR_STREAM
            || raw.color_range != ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG
            || raw.colorspace != ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT709
            || raw.color_primaries != ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT709
            || raw.color_trc
                != ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1)
    {
        return Err(DecodeError::FfmpegError(format!(
            "decoded color differs from negotiated {expected:?}; reconnect required"
        )));
    }
    Ok(())
}

fn validate_hdr_metadata(frame: &ffmpeg_next::frame::Video) -> Result<(), DecodeError> {
    let metadata = unsafe { stargaze_core::hdr::HdrMetadata::from_avframe(frame.as_ptr()) };
    metadata
        .validate()
        .map_err(|e| DecodeError::FfmpegError(e.into()))?;
    for (kind, missing) in [
        (
            ffmpeg_sys_next::AVFrameSideDataType::AV_FRAME_DATA_MASTERING_DISPLAY_METADATA,
            metadata.mastering.is_none(),
        ),
        (
            ffmpeg_sys_next::AVFrameSideDataType::AV_FRAME_DATA_CONTENT_LIGHT_LEVEL,
            metadata.content_light.is_none(),
        ),
    ] {
        if missing
            && !unsafe { ffmpeg_sys_next::av_frame_get_side_data(frame.as_ptr(), kind) }.is_null()
        {
            return Err(DecodeError::FfmpegError(
                "incomplete/invalid HDR static metadata; refusing unsafe presentation".into(),
            ));
        }
    }
    Ok(())
}

#[allow(clippy::cast_ptr_alignment)] // FFmpeg's hardware frame allocation is aligned.
fn decoded_sw_format(frame: &ffmpeg_next::frame::Video) -> ffmpeg_sys_next::AVPixelFormat {
    let raw = unsafe { &*frame.as_ptr() };
    if frame.format() == ffmpeg_next::format::Pixel::VAAPI && !raw.hw_frames_ctx.is_null() {
        unsafe {
            (*(*raw.hw_frames_ctx)
                .data
                .cast::<ffmpeg_sys_next::AVHWFramesContext>())
            .sw_format
        }
    } else {
        frame.format().into()
    }
}

/// Check the original `AVFrame`, since hardware transfer need not copy metadata.
/// Unspecified fields retain the legacy SDR contract, not inferred HDR.
#[allow(clippy::cast_ptr_alignment)] // FFmpeg allocates aligned AVHWFramesContext data.
fn validate_decoded_color(frame: &ffmpeg_next::frame::Video) -> Result<(), DecodeError> {
    use ffmpeg_sys_next::{
        AVColorPrimaries as P, AVColorRange as R, AVColorSpace as M,
        AVColorTransferCharacteristic as T, AVPixelFormat as F,
    };
    let raw = unsafe { &*frame.as_ptr() };
    let mut format = F::from(frame.format());
    if format == F::AV_PIX_FMT_VAAPI && !raw.hw_frames_ctx.is_null() {
        let context = unsafe {
            &*(*raw.hw_frames_ctx)
                .data
                .cast::<ffmpeg_sys_next::AVHWFramesContext>()
        };
        format = context.sw_format;
    }
    let supported = matches!(format, F::AV_PIX_FMT_NV12 | F::AV_PIX_FMT_YUV420P)
        && matches!(
            raw.color_range,
            R::AVCOL_RANGE_UNSPECIFIED | R::AVCOL_RANGE_MPEG
        )
        && matches!(
            raw.colorspace,
            M::AVCOL_SPC_UNSPECIFIED | M::AVCOL_SPC_BT709
        )
        && matches!(
            raw.color_primaries,
            P::AVCOL_PRI_UNSPECIFIED | P::AVCOL_PRI_BT709
        )
        && matches!(
            raw.color_trc,
            T::AVCOL_TRC_UNSPECIFIED | T::AVCOL_TRC_BT709 | T::AVCOL_TRC_IEC61966_2_1
        );
    let hdr_metadata = unsafe {
        !ffmpeg_sys_next::av_frame_get_side_data(
            frame.as_ptr(),
            ffmpeg_sys_next::AVFrameSideDataType::AV_FRAME_DATA_MASTERING_DISPLAY_METADATA,
        )
        .is_null()
            || !ffmpeg_sys_next::av_frame_get_side_data(
                frame.as_ptr(),
                ffmpeg_sys_next::AVFrameSideDataType::AV_FRAME_DATA_CONTENT_LIGHT_LEVEL,
            )
            .is_null()
    };
    if !supported || hdr_metadata {
        return Err(DecodeError::FfmpegError(format!(
            "unsafe decoded color mode: {format:?}, range={:?}, matrix={:?}, primaries={:?}, transfer={:?}, HDR metadata={hdr_metadata}; SDR-only presentation, no tone/gamut mapping",
            raw.color_range, raw.colorspace, raw.color_primaries, raw.color_trc,
        )));
    }
    Ok(())
}

/// Shared hardware fixture helper for actual decode→presentation tests.
#[cfg(test)]
pub(crate) fn decode_hdr_fixture(data: &[u8], dma_buf: bool) -> VideoFrame {
    let config = DecoderConfig {
        width: 2048,
        height: 256,
        codec: Codec::H265,
        color: stargaze_core::color::ColorDescription::HDR10,
    };
    let mut decoder = init_decoder(&config).unwrap();
    assert!(decoder.hw_accel, "test must use real VAAPI Main10 decoding");
    decoder
        .decoder
        .send_packet(&ffmpeg_next::Packet::copy(data))
        .unwrap();
    decoder.decoder.send_eof().unwrap();
    let mut picture = ffmpeg_next::frame::Video::empty();
    decoder.decoder.receive_frame(&mut picture).unwrap();
    check_session_color(&mut decoder, &picture).unwrap();
    if dma_buf {
        VideoFrame::DmaBuf(DmaBufFrame {
            width: picture.width(),
            height: picture.height(),
            pts: 0,
            stats: FrameStats::default(),
            frame: map_to_drm_prime(&picture).unwrap(),
        })
    } else {
        let mut cpu = ffmpeg_next::frame::Video::empty();
        assert_eq!(
            unsafe {
                ffmpeg_sys_next::av_hwframe_transfer_data(cpu.as_mut_ptr(), picture.as_ptr(), 0)
            },
            0
        );
        assert_eq!(cpu.format(), ffmpeg_next::format::Pixel::P010LE);
        assert_eq!(
            unsafe { ffmpeg_sys_next::av_frame_copy_props(cpu.as_mut_ptr(), picture.as_ptr()) },
            0
        );
        VideoFrame::Cpu(extract_p010(&cpu, picture.width(), picture.height()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stargaze_core::config::Codec;

    #[test]
    fn native_hdr_contract_requires_ten_bit_pq_2020_and_limited_range() {
        use stargaze_core::color::ColorDescription;
        let mut frame = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::P010LE, 16, 16);
        unsafe {
            let raw = &mut *frame.as_mut_ptr();
            raw.color_range = ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG;
            raw.colorspace = ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT2020_NCL;
            raw.color_primaries = ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT2020;
            raw.color_trc = ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
        }
        validate_negotiated_color(&frame, ColorDescription::HDR10).unwrap();
        assert!(validate_negotiated_color(&frame, ColorDescription::SDR_STREAM).is_err());
        unsafe {
            (*frame.as_mut_ptr()).color_range = ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_JPEG;
        }
        assert!(validate_negotiated_color(&frame, ColorDescription::HDR10).is_err());
        unsafe {
            (*frame.as_mut_ptr()).color_range = ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG;
            (*frame.as_mut_ptr()).color_trc =
                ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67;
        }
        assert!(validate_negotiated_color(&frame, ColorDescription::HDR10).is_err());
        unsafe {
            (*frame.as_mut_ptr()).color_trc =
                ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
        }
        validate_negotiated_color(&frame, ColorDescription::HDR10).unwrap();
    }

    #[test]
    fn software_main10_repacking_preserves_black_white_and_highlights() {
        use stargaze_core::decode::FramePixels;
        let mut frame =
            ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P10LE, 16, 4);
        let codes = [64_u16, 509, 573, 723, 940, 1023, 0, 512];
        for row in 0..4 {
            for x in 0..16 {
                let at = row * frame.stride(0) + x * 2;
                frame.data_mut(0)[at..at + 2]
                    .copy_from_slice(&codes[x % codes.len()].to_le_bytes());
            }
        }
        for plane in [1, 2] {
            for word in frame.data_mut(plane).as_chunks_mut::<2>().0 {
                *word = 512_u16.to_le_bytes();
            }
        }
        let output = extract_p010(&frame, 16, 4);
        let FramePixels::P010 { y, uv } = output.pixels else {
            panic!("P010 output required");
        };
        for (x, word) in y.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(u16::from_le_bytes(*word), codes[x % codes.len()] << 6);
        }
        assert!(
            uv.as_chunks::<2>()
                .0
                .iter()
                .all(|word| u16::from_le_bytes(*word) == 512 << 6)
        );
    }

    #[test]
    fn decoded_color_guard_rejects_unsafe_modes_before_export_or_transfer() {
        use ffmpeg_sys_next::{
            AVColorPrimaries as P, AVColorRange as R, AVColorSpace as M,
            AVColorTransferCharacteristic as T,
        };
        let mut frame = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::NV12, 16, 16);
        assert!(validate_decoded_color(&frame).is_ok()); // legacy unspecified
        assert!(
            validate_negotiated_color(&frame, stargaze_core::color::ColorDescription::SDR_STREAM)
                .is_err()
        );
        unsafe {
            let raw = frame.as_mut_ptr();
            (*raw).color_range = R::AVCOL_RANGE_MPEG;
            (*raw).colorspace = M::AVCOL_SPC_BT709;
            (*raw).color_primaries = P::AVCOL_PRI_BT709;
            (*raw).color_trc = T::AVCOL_TRC_IEC61966_2_1;
        }
        assert!(validate_decoded_color(&frame).is_ok());
        validate_negotiated_color(&frame, stargaze_core::color::ColorDescription::SDR_STREAM)
            .unwrap();
        for transfer in [
            T::AVCOL_TRC_SMPTE2084,
            T::AVCOL_TRC_ARIB_STD_B67,
            T::AVCOL_TRC_LINEAR,
        ] {
            unsafe {
                (*frame.as_mut_ptr()).color_trc = transfer;
            }
            assert!(validate_decoded_color(&frame).is_err());
        }
        unsafe {
            (*frame.as_mut_ptr()).color_trc = T::AVCOL_TRC_BT709;
            (*frame.as_mut_ptr()).color_range = R::AVCOL_RANGE_JPEG;
        }
        assert!(validate_decoded_color(&frame).is_err());
        unsafe {
            (*frame.as_mut_ptr()).color_range = R::AVCOL_RANGE_MPEG;
            (*frame.as_mut_ptr()).color_primaries = P::AVCOL_PRI_BT2020;
        }
        assert!(validate_decoded_color(&frame).is_err());
        unsafe {
            (*frame.as_mut_ptr()).color_primaries = P::AVCOL_PRI_BT709;
        }
        assert!(validate_decoded_color(&frame).is_ok()); // transition back
        let p010 = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::P010LE, 16, 16);
        assert!(validate_decoded_color(&p010).is_err()); // no silent truncation
    }

    /// Decode a one-frame Annex-B fixture on real VAAPI hardware. SDR must
    /// remain NV12 through export and transfer; HDR must fail the guard.
    /// `STARGAZE_TEST_HEVC=/tmp/sdr.hevc nix develop -c cargo test -p stargaze-client vaapi_color_fixture -- --ignored --nocapture`.
    /// For an HDR10 fixture also set `STARGAZE_TEST_EXPECT_HDR=1`.
    #[test]
    #[ignore = "requires VAAPI hardware and STARGAZE_TEST_HEVC one-frame fixture"]
    fn vaapi_color_fixture() {
        let data =
            std::fs::read(std::env::var("STARGAZE_TEST_HEVC").expect("fixture path")).unwrap();
        let expect_hdr = std::env::var("STARGAZE_TEST_EXPECT_HDR").is_ok_and(|v| v == "1");
        let config = DecoderConfig {
            width: 640,
            height: 480,
            codec: Codec::H265,
            color: stargaze_core::color::ColorDescription::SDR_STREAM,
        };
        let mut decoder = init_decoder(&config).unwrap();
        assert!(
            decoder.hw_accel,
            "test must exercise VAAPI, not software fallback"
        );
        decoder
            .decoder
            .send_packet(&ffmpeg_next::Packet::copy(&data))
            .unwrap();
        decoder.decoder.send_eof().unwrap();
        let mut picture = ffmpeg_next::frame::Video::empty();
        decoder.decoder.receive_frame(&mut picture).unwrap();
        assert_eq!(picture.format(), ffmpeg_next::format::Pixel::VAAPI);
        eprintln!(
            "VAAPI fixture: {:?} {:?} {:?} {:?}",
            picture.color_range(),
            picture.color_space(),
            picture.color_primaries(),
            picture.color_transfer_characteristic()
        );
        if expect_hdr {
            assert_eq!(
                picture.color_transfer_characteristic(),
                ffmpeg_next::color::TransferCharacteristic::SMPTE2084
            );
            assert_eq!(
                picture.color_primaries(),
                ffmpeg_next::color::Primaries::BT2020
            );
            assert!(validate_decoded_color(&picture).is_err());
        } else {
            assert_eq!(
                picture.color_transfer_characteristic(),
                ffmpeg_next::color::TransferCharacteristic::IEC61966_2_1
            );
            validate_negotiated_color(&picture, config.color).unwrap();
            let mapped = map_to_drm_prime(&picture).expect("SDR dma-buf export");
            assert_eq!(mapped.format(), ffmpeg_next::format::Pixel::DRM_PRIME);
            let mut cpu = ffmpeg_next::frame::Video::empty();
            assert_eq!(
                unsafe {
                    ffmpeg_sys_next::av_hwframe_transfer_data(cpu.as_mut_ptr(), picture.as_ptr(), 0)
                },
                0
            );
            assert_eq!(cpu.format(), ffmpeg_next::format::Pixel::NV12);
            assert_eq!(
                unsafe { ffmpeg_sys_next::av_frame_copy_props(cpu.as_mut_ptr(), picture.as_ptr()) },
                0
            );
            validate_decoded_color(&cpu).unwrap();
            // The documented SDR fixture is a neutral black frame.
            assert!((i16::from(cpu.data(0)[0]) - 16).abs() <= 1);
            assert!((i16::from(cpu.data(1)[0]) - 128).abs() <= 1);
            assert!((i16::from(cpu.data(1)[1]) - 128).abs() <= 1);
        }
    }

    #[test]
    fn hdr_side_data_cannot_be_hidden_by_sdr_vui() {
        let mut frame = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, 16, 16);
        let side = unsafe {
            ffmpeg_sys_next::av_frame_new_side_data(
                frame.as_mut_ptr(),
                ffmpeg_sys_next::AVFrameSideDataType::AV_FRAME_DATA_MASTERING_DISPLAY_METADATA,
                1, // Guard checks presence, never interprets untrusted HDR payloads.
            )
        };
        assert!(!side.is_null());
        assert!(validate_decoded_color(&frame).is_err());
    }

    #[test]
    fn decoder_init_h265_succeeds() {
        let config = DecoderConfig {
            width: 1920,
            height: 1080,
            codec: Codec::H265,
            color: stargaze_core::color::ColorDescription::LEGACY_SDR,
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

    /// NV12 extraction keeps the UV plane interleaved (the renderer
    /// uploads NV12 textures directly) and strips stride padding.
    #[test]
    fn extract_nv12_keeps_interleaved_uv_and_strips_stride() {
        use stargaze_core::decode::FramePixels;

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

        let out = extract_nv12(&frame, w, h);
        let (w, h) = (w as usize, h as usize);
        match &out.pixels {
            FramePixels::Nv12 { y, uv } => {
                assert_eq!(y.len(), w * h);
                assert_eq!(uv.len(), w * h / 2);
                assert!(y.iter().all(|&b| b == 0x11));
                for pair in uv.chunks_exact(2) {
                    assert_eq!(pair, [0xAA, 0x55], "UV must stay interleaved");
                }
            }
            _ => panic!("expected NV12 pixels"),
        }
        assert_eq!(out.pts, 42);
    }

    #[test]
    fn extract_yuv420p_strips_stride() {
        use stargaze_core::decode::FramePixels;

        ffmpeg_next::init().expect("ffmpeg init");
        let (w, h) = (64u32, 36u32);
        let mut frame = ffmpeg_next::frame::Video::new(ffmpeg_next::format::Pixel::YUV420P, w, h);
        frame.data_mut(0).fill(1);
        frame.data_mut(1).fill(2);
        frame.data_mut(2).fill(3);

        let out = extract_yuv420p(&frame, w, h);
        let (w, h) = (w as usize, h as usize);
        match &out.pixels {
            FramePixels::I420 { y, u, v } => {
                assert_eq!(y.len(), w * h);
                assert_eq!(u.len(), (w / 2) * (h / 2));
                assert_eq!(v.len(), (w / 2) * (h / 2));
                assert!(u.iter().all(|&b| b == 2));
                assert!(v.iter().all(|&b| b == 3));
            }
            _ => panic!("expected I420 pixels"),
        }
    }

    #[test]
    fn decoder_init_rejects_av1() {
        let config = DecoderConfig {
            width: 1920,
            height: 1080,
            codec: Codec::Av1,
            color: stargaze_core::color::ColorDescription::LEGACY_SDR,
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
