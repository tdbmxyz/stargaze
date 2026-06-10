use std::io::Cursor;
use std::os::fd::FromRawFd;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pipewire::context::ContextBox;
use pipewire::main_loop::MainLoopBox;
use pipewire::properties::properties;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, object, property};
use pipewire::spa::utils::{Direction, Fraction, Rectangle, SpaTypes};
use pipewire::stream::{StreamBox, StreamFlags, StreamState};
use pipewire_sys;
use stargaze_core::capture::{CaptureError, CapturedFrame, DmaBufInfo, Frame, PixelFormat};
use stargaze_core::config::Resolution;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

use super::CaptureConfig;

/// User data passed through `PipeWire` stream callbacks.
///
/// Stores shared state that the process callback needs to construct
/// and send frames.
struct CaptureCallbackData {
    /// Channel sender for delivering captured frames to the consumer.
    tx: mpsc::Sender<CapturedFrame>,
    /// Shutdown flag — when set, the main loop should exit.
    shutdown: Arc<AtomicBool>,
    /// Negotiated frame width (set from config, may be updated on format change).
    width: u32,
    /// Negotiated frame height (set from config, may be updated on format change).
    height: u32,
    /// Negotiated pixel format (defaults to `Bgra8`).
    format: PixelFormat,
    /// Negotiated DRM format modifier (e.g. tiling/compression layout).
    modifier: u64,
    /// Oneshot sender for the negotiated resolution — fires once on first
    /// `param_changed` with a valid format. `None` after the first send.
    resolution_tx: Option<oneshot::Sender<Resolution>>,
    /// Frame counter for diagnostic logging.
    frame_count: u64,
    /// Frames dropped because the encoder was behind (channel full).
    dropped_count: u64,
    /// Whether `ack_format` has been called for the current format.
    /// Prevents re-entrant `update_params` → `param_changed` cycling.
    format_acked: bool,
}

/// Maps a SPA video format to our internal `PixelFormat`.
///
/// Returns `None` for unsupported formats.
fn spa_format_to_pixel_format(raw: u32) -> Option<PixelFormat> {
    use pipewire::spa::param::video::VideoFormat;

    let fmt = VideoFormat::from_raw(raw);
    match fmt {
        VideoFormat::BGRA | VideoFormat::BGRx => Some(PixelFormat::Bgra8),
        VideoFormat::RGBA | VideoFormat::RGBx => Some(PixelFormat::Rgba8),
        VideoFormat::NV12 => Some(PixelFormat::Nv12),
        // 10-bit 2:10:10:10 packed formats exposed by portals on 10-bit displays.
        VideoFormat::xBGR_210LE
        | VideoFormat::BGRx_102LE
        | VideoFormat::ABGR_210LE
        | VideoFormat::BGRA_102LE => Some(PixelFormat::Bgra10),
        VideoFormat::xRGB_210LE
        | VideoFormat::RGBx_102LE
        | VideoFormat::ARGB_210LE
        | VideoFormat::RGBA_102LE => Some(PixelFormat::Rgba10),
        _ => None,
    }
}

const DMABUF_FORMATS: &[pipewire::spa::param::video::VideoFormat] = &[
    // 8-bit
    pipewire::spa::param::video::VideoFormat::BGRA,
    pipewire::spa::param::video::VideoFormat::BGRx,
    pipewire::spa::param::video::VideoFormat::RGBA,
    pipewire::spa::param::video::VideoFormat::RGBx,
    // 10-bit (2:10:10:10) — required for portals on 10-bit displays
    pipewire::spa::param::video::VideoFormat::xBGR_210LE,
    pipewire::spa::param::video::VideoFormat::ABGR_210LE,
    pipewire::spa::param::video::VideoFormat::xRGB_210LE,
    pipewire::spa::param::video::VideoFormat::ARGB_210LE,
];

/// `DRM_FORMAT_MOD_INVALID` — accept any modifier the source offers.
const DRM_FORMAT_MOD_INVALID: i64 = (1 << 56) - 1;

/// Builds SPA format pods for video stream negotiation.
///
/// Creates **one pod per pixel format** with `VideoModifier`
/// (`MANDATORY | DONT_FIXATE`) for DMA-BUF negotiation, plus a single
/// **fallback pod** (format enum, no modifier) for SHM/`MemPtr` sources.
///
/// This per-format pod pattern matches what Sunshine, xdg-desktop-portal-hyprland,
/// and `WayVR` use. A single pod with a format enum *and* a modifier fails
/// intersection because `PipeWire` needs each format paired with its own
/// modifier list.
fn build_format_params(config: &CaptureConfig) -> Vec<Vec<u8>> {
    let mut pods: Vec<Vec<u8>> = DMABUF_FORMATS
        .iter()
        .map(|fmt| build_dmabuf_format_pod(config, *fmt))
        .collect();

    // SHM fallback pod (no modifier) — used if DMA-BUF negotiation fails.
    pods.push(build_shm_fallback_pod(config));

    info!(
        pod_count = pods.len(),
        pod_sizes = ?pods.iter().map(Vec::len).collect::<Vec<_>>(),
        "Built format negotiation pods (DMA-BUF per-format + SHM fallback)"
    );

    pods
}

/// Builds a single DMA-BUF format pod for one pixel format.
///
/// The `VideoModifier` property uses `MANDATORY` with `DRM_FORMAT_MOD_INVALID`
/// so the portal can offer its preferred modifier.
#[allow(dead_code)] // Retained for future DMA-BUF import support.
fn build_dmabuf_format_pod(
    config: &CaptureConfig,
    video_format: pipewire::spa::param::video::VideoFormat,
) -> Vec<u8> {
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::pod::{Property, PropertyFlags, Value};

    let mut format_obj = object! {
        SpaTypes::ObjectParamFormat,
        pipewire::spa::param::ParamType::EnumFormat,
        property!(
            FormatProperties::MediaType,
            Id,
            MediaType::Video
        ),
        property!(
            FormatProperties::MediaSubtype,
            Id,
            MediaSubtype::Raw
        ),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            video_format,
            video_format,
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: config.width,
                height: config.height,
            },
            Rectangle {
                width: 1,
                height: 1,
            },
            Rectangle {
                width: 8192,
                height: 8192,
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 0, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 1000, denom: 1 }
        ),
    };

    // VideoModifier: MANDATORY | DONT_FIXATE with a Choice::Enum.
    //
    // DONT_FIXATE tells PipeWire not to lock onto our offered value but to
    // let the compositor's video-src-fixate propose the real DRM modifier
    // (e.g. NVIDIA block-linear tiling). Without DONT_FIXATE, PipeWire keeps
    // DRM_FORMAT_MOD_INVALID and the compositor sends tiled DMA-BUFs that we
    // misinterpret as linear — causing horizontal banding artifacts.
    //
    // The Choice::Enum contains DRM_FORMAT_MOD_INVALID as both default and
    // sole alternative, meaning "I accept any modifier." During fixation the
    // compositor replaces this with its preferred modifier.
    //
    // This matches Sunshine's portalgrab.cpp pattern (MANDATORY | DONT_FIXATE
    // + SPA_CHOICE_Enum of modifiers).
    format_obj.properties.push(Property {
        key: FormatProperties::VideoModifier.as_raw(),
        flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
        value: Value::Choice(pod::ChoiceValue::Long(pipewire::spa::utils::Choice(
            pipewire::spa::utils::ChoiceFlags::empty(),
            pipewire::spa::utils::ChoiceEnum::Enum {
                default: DRM_FORMAT_MOD_INVALID,
                alternatives: vec![DRM_FORMAT_MOD_INVALID],
            },
        ))),
    });

    serialize_pod(format_obj)
}

/// Builds the SHM/`MemPtr` fallback pod with a format enum and no modifier.
///
/// If DMA-BUF negotiation fails for every per-format pod, `PipeWire` falls
/// through to this one and uses shared-memory buffers instead.
fn build_shm_fallback_pod(config: &CaptureConfig) -> Vec<u8> {
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};

    let format_obj = object! {
        SpaTypes::ObjectParamFormat,
        pipewire::spa::param::ParamType::EnumFormat,
        property!(
            FormatProperties::MediaType,
            Id,
            MediaType::Video
        ),
        property!(
            FormatProperties::MediaSubtype,
            Id,
            MediaSubtype::Raw
        ),
        property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pipewire::spa::param::video::VideoFormat::BGRA,
            pipewire::spa::param::video::VideoFormat::BGRx,
            pipewire::spa::param::video::VideoFormat::RGBA,
            pipewire::spa::param::video::VideoFormat::RGBx,
            pipewire::spa::param::video::VideoFormat::NV12,
            pipewire::spa::param::video::VideoFormat::xBGR_210LE,
            pipewire::spa::param::video::VideoFormat::ABGR_210LE,
            pipewire::spa::param::video::VideoFormat::xRGB_210LE,
            pipewire::spa::param::video::VideoFormat::ARGB_210LE,
        ),
        property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: config.width,
                height: config.height,
            },
            Rectangle {
                width: 1,
                height: 1,
            },
            Rectangle {
                width: 8192,
                height: 8192,
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 0, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 1000, denom: 1 }
        ),
    };

    serialize_pod(format_obj)
}

fn serialize_pod(obj: pod::Object) -> Vec<u8> {
    let pod_value = pod::Value::Object(obj);
    let cursor = Cursor::new(Vec::new());
    let (bytes, _len) =
        PodSerializer::serialize(cursor, &pod_value).expect("failed to serialize format pod");
    bytes.into_inner()
}

/// SPA data-type constants (from `spa/param/param.h`).
const SPA_DATA_MEM_PTR: i32 = 1;
const SPA_DATA_MEM_FD: i32 = 2;
const SPA_DATA_DMA_BUF: i32 = 3;

/// `SPA_PARAM_BUFFERS_dataType` property key.
const SPA_PARAM_BUFFERS_DATA_TYPE: u32 = 6;

/// `SPA_PARAM_META_*` property keys.
const SPA_PARAM_META_TYPE: u32 = 1;
const SPA_PARAM_META_SIZE: u32 = 2;

/// `SPA_META_Header` id.
const SPA_META_HEADER: u32 = 1;

/// Size of `spa_meta_header` (from libspa bindings: 32 bytes).
const SPA_META_HEADER_SIZE: i32 = 32;

/// `SPA_META_VideoDamage` id (from `spa/param/param.h`).
const SPA_META_VIDEO_DAMAGE: u32 = 3;

/// Size of a single `spa_meta_region` (from libspa bindings: 16 bytes).
const SPA_META_REGION_SIZE: i32 = 16;

/// Acknowledge a negotiated format by calling `stream.update_params()` with
/// buffer-type and meta params. Matches Sunshine's `on_param_changed` exactly:
/// only `dataType` in Buffers (let the producer own allocation), plus Meta
/// Header and Meta `VideoDamage` (choice-range).
fn ack_format(stream: &pipewire::stream::Stream, modifier: u64) {
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::{Property, PropertyFlags, Value};
    use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

    const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
    let is_dmabuf = modifier != 0 && modifier != DRM_FORMAT_MOD_INVALID;

    let buffer_types: i32 = if is_dmabuf {
        info!("Requesting DMA-BUF buffers (modifier 0x{modifier:x})");
        1 << SPA_DATA_DMA_BUF
    } else {
        info!("Requesting MemPtr + MemFd + DMA-BUF buffers (no real DRM modifier)");
        (1 << SPA_DATA_MEM_PTR) | (1 << SPA_DATA_MEM_FD) | (1 << SPA_DATA_DMA_BUF)
    };

    // 1. SPA_PARAM_Buffers — only dataType (producer owns buffer layout).
    let buffers_obj = pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![Property {
            key: SPA_PARAM_BUFFERS_DATA_TYPE,
            flags: PropertyFlags::empty(),
            value: Value::Int(buffer_types),
        }],
    };

    // 2. SPA_PARAM_Meta — Header (fixed size).
    let meta_header_obj = pod::Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property {
                key: SPA_PARAM_META_TYPE,
                flags: PropertyFlags::empty(),
                value: Value::Id(pipewire::spa::utils::Id(SPA_META_HEADER)),
            },
            Property {
                key: SPA_PARAM_META_SIZE,
                flags: PropertyFlags::empty(),
                value: Value::Int(SPA_META_HEADER_SIZE),
            },
        ],
    };

    // 3. SPA_PARAM_Meta — VideoDamage (choice-range size, matching Sunshine).
    let video_damage_region_count: i32 = 16;
    let meta_video_damage_obj = pod::Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            Property {
                key: SPA_PARAM_META_TYPE,
                flags: PropertyFlags::empty(),
                value: Value::Id(pipewire::spa::utils::Id(SPA_META_VIDEO_DAMAGE)),
            },
            Property {
                key: SPA_PARAM_META_SIZE,
                flags: PropertyFlags::empty(),
                value: Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: SPA_META_REGION_SIZE * video_damage_region_count,
                        min: SPA_META_REGION_SIZE,
                        max: SPA_META_REGION_SIZE * video_damage_region_count,
                    },
                ))),
            },
        ],
    };

    let buf_bytes = serialize_pod(buffers_obj);
    let meta_hdr_bytes = serialize_pod(meta_header_obj);
    let meta_vd_bytes = serialize_pod(meta_video_damage_obj);

    let Some(buf_pod) = pipewire::spa::pod::Pod::from_bytes(&buf_bytes) else {
        error!("Failed to build SPA_PARAM_Buffers pod");
        return;
    };
    let Some(meta_hdr_pod) = pipewire::spa::pod::Pod::from_bytes(&meta_hdr_bytes) else {
        error!("Failed to build SPA_PARAM_Meta (Header) pod");
        return;
    };
    let Some(meta_vd_pod) = pipewire::spa::pod::Pod::from_bytes(&meta_vd_bytes) else {
        error!("Failed to build SPA_PARAM_Meta (VideoDamage) pod");
        return;
    };

    if let Err(e) = stream.update_params(&mut [buf_pod, meta_hdr_pod, meta_vd_pod]) {
        error!(%e, "stream.update_params failed");
    } else {
        info!(
            is_dmabuf,
            "Acknowledged format with 3 params (Buffers + Meta Header + Meta VideoDamage)"
        );
    }
}

/// Runs the `PipeWire` capture stream on the current thread (blocking).
///
/// This function blocks until the shutdown signal is set or an error occurs.
/// It initializes a `PipeWire` main loop, connects to the daemon using the
/// portal-provided file descriptor, creates a video capture stream targeting
/// the given `pw_node_id`, and processes incoming buffers.
///
/// Supports both `DMA-BUF` (zero-copy GPU) and `MemPtr` (CPU-mapped) buffer
/// types. Frames are sent to the consumer via the provided `mpsc` channel.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn run_capture_stream(
    pw_fd: OwnedFd,
    pw_node_id: u32,
    config: CaptureConfig,
    tx: mpsc::Sender<CapturedFrame>,
    shutdown: Arc<AtomicBool>,
    resolution_tx: oneshot::Sender<Resolution>,
) -> Result<(), CaptureError> {
    // 1. Initialize PipeWire (idempotent — safe to call multiple times).
    pipewire::init();

    // 2. Create the main loop.
    let mainloop = MainLoopBox::new(None)
        .map_err(|e| CaptureError::PipeWireError(format!("failed to create main loop: {e}")))?;

    // 3. Create context from the loop.
    let context = ContextBox::new(mainloop.loop_(), None)
        .map_err(|e| CaptureError::PipeWireError(format!("failed to create context: {e}")))?;

    // 4. Connect to PipeWire using the portal fd.
    let core = context
        .connect_fd(pw_fd, None)
        .map_err(|e| CaptureError::PipeWireError(format!("failed to connect fd: {e}")))?;

    // 5. Create the capture stream.
    let stream = StreamBox::new(
        &core,
        "stargaze-capture",
        properties! {
            "media.type" => "Video",
            "media.category" => "Capture",
            "media.role" => "Screen"
        },
    )
    .map_err(|e| CaptureError::PipeWireError(format!("failed to create stream: {e}")))?;

    // 6. Build user data for callbacks.
    let user_data = CaptureCallbackData {
        tx,
        shutdown: Arc::clone(&shutdown),
        width: config.width,
        height: config.height,
        format: PixelFormat::Bgra8,
        modifier: 0,
        resolution_tx: Some(resolution_tx),
        frame_count: 0,
        dropped_count: 0,
        format_acked: false,
    };

    // We need a reference to the mainloop inside callbacks.
    // The mainloop is !Send, but all callbacks run on this same thread.
    let mainloop_ptr = mainloop.as_raw_ptr();

    // 7. Register stream callbacks.
    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(move |_stream, _data, old, new| {
            info!("PipeWire stream state: {old:?} -> {new:?}");

            if let StreamState::Error(ref msg) = new {
                error!("PipeWire stream error: {msg}");
                // Quit the main loop on error.
                unsafe {
                    pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                }
            }
        })
        .param_changed(|stream, data, id, param| {
            let Some(param) = param else {
                info!(param_id = id, "param_changed with null param");
                return;
            };

            let format_id = pipewire::spa::param::ParamType::Format.as_raw();
            if id != format_id {
                info!(
                    param_id = id,
                    param_len = param.size(),
                    "param_changed (non-Format)"
                );
                return;
            }

            // Try to parse the negotiated video format.
            let mut video_info = pipewire::spa::param::video::VideoInfoRaw::new();
            if video_info.parse(param).is_ok() {
                let size = video_info.size();
                data.width = size.width;
                data.height = size.height;

                if let Some(pf) = spa_format_to_pixel_format(video_info.format().as_raw()) {
                    data.format = pf;
                }

                data.modifier = video_info.modifier();

                info!(
                    width = data.width,
                    height = data.height,
                    format = %data.format,
                    modifier = data.modifier,
                    "PipeWire format negotiated"
                );

                // Notify main thread of actual capture resolution (once).
                if let Some(tx) = data.resolution_tx.take() {
                    let _ = tx.send(Resolution {
                        width: data.width,
                        height: data.height,
                    });
                }

                // ACK the format by telling PipeWire which buffer types we
                // accept.  Only do this once per negotiation — calling
                // update_params re-triggers param_changed, causing a
                // Paused↔Streaming cycle that can corrupt buffer state.
                if !data.format_acked {
                    data.format_acked = true;
                    ack_format(stream, data.modifier);
                }
            }
        })
        .process(move |stream, data| {
            // Check shutdown flag.
            if data.shutdown.load(Ordering::Relaxed) {
                debug!("Shutdown signaled, quitting PipeWire main loop");
                // Safety: we stored the raw pointer to the main loop above,
                // and this callback runs on the same thread.
                unsafe {
                    pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                }
                return;
            }

            // Dequeue a buffer from the stream.
            let Some(mut buffer) = stream.dequeue_buffer() else {
                trace!("No buffer available from PipeWire stream");
                return;
            };

            let datas = buffer.datas_mut();
            if datas.is_empty() {
                warn!("PipeWire buffer has no data planes");
                return;
            }

            let d = &mut datas[0];

            // Extract chunk metadata before any mutable borrows on `d`.
            let chunk_size = d.chunk().size();
            let chunk_stride = d.chunk().stride();
            let chunk_offset = d.chunk().offset();

            static PW_DIAG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let pn = PW_DIAG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if pn < 3 {
                info!(
                    frame = pn,
                    chunk_size,
                    chunk_stride,
                    chunk_offset,
                    data_type = ?d.type_(),
                    width = data.width,
                    height = data.height,
                    modifier = format_args!("0x{:x}", data.modifier),
                    "PipeWire buffer chunk diagnostics"
                );
            }

            // Skip empty or corrupt chunks.
            if chunk_size == 0 {
                trace!("Skipping empty PipeWire buffer chunk");
                return;
            }

            let data_type = d.type_();

            let frame = if data_type == pipewire::spa::buffer::DataType::DmaBuf {
                // DMA-BUF path: dup the fd so we own it after the buffer is returned.
                let raw_fd = d.fd();
                if raw_fd < 0 {
                    warn!("DMA-BUF buffer has invalid fd: {raw_fd}");
                    return;
                }

                let duped_fd = unsafe { libc::dup(raw_fd) };
                if duped_fd < 0 {
                    warn!("Failed to dup DMA-BUF fd");
                    return;
                }

                let owned_fd = unsafe { OwnedFd::from_raw_fd(duped_fd) };

                let stride = if chunk_stride > 0 {
                    #[allow(clippy::cast_sign_loss)]
                    {
                        chunk_stride as u32
                    }
                } else {
                    // Fallback: assume 4 bytes per pixel for BGRA.
                    data.width * 4
                };

                Frame::DmaBuf(DmaBufInfo {
                    fd: owned_fd,
                    width: data.width,
                    height: data.height,
                    format: data.format,
                    modifier: data.modifier,
                    stride,
                    offset: chunk_offset,
                })
            } else if data_type == pipewire::spa::buffer::DataType::MemPtr
                || data_type == pipewire::spa::buffer::DataType::MemFd
            {
                // CPU-mapped path: copy bytes out of the PipeWire buffer.
                // MemFd buffers are also memory-mapped by PipeWire (MAP_BUFFERS flag),
                // so d.data() works identically for both MemPtr and MemFd.
                // Grab the fd before taking a mutable borrow via d.data().
                let memfd_raw = if data_type == pipewire::spa::buffer::DataType::MemFd {
                    Some(d.fd())
                } else {
                    None
                };

                let size = chunk_size as usize;

                // For MemFd buffers, PipeWire's MAP_BUFFERS mapping can be
                // unreliable (the data pointer exists but pages aren't
                // readable, causing SIGBUS/SIGSEGV on first access).  Map the
                // fd ourselves with explicit PROT_READ to get a guaranteed-
                // valid mapping.
                let pixels = if let Some(raw_fd) = memfd_raw {
                    let mapped = unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            size,
                            libc::PROT_READ,
                            libc::MAP_SHARED,
                            raw_fd,
                            chunk_offset.into(),
                        )
                    };
                    if mapped == libc::MAP_FAILED {
                        warn!(
                            "mmap MemFd fd={raw_fd} failed: {}",
                            std::io::Error::last_os_error()
                        );
                        return;
                    }
                    let buf =
                        unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), size) }.to_vec();
                    unsafe {
                        libc::munmap(mapped, size);
                    }
                    buf
                } else {
                    // MemPtr path: use PipeWire's data() as before.
                    let Some(slice) = d.data() else {
                        warn!("MemPtr buffer has null data pointer");
                        return;
                    };
                    if size > slice.len() {
                        warn!(
                            "Chunk size ({size}) exceeds buffer capacity ({})",
                            slice.len()
                        );
                        return;
                    }
                    slice[..size].to_vec()
                };

                let stride = if chunk_stride > 0 {
                    #[allow(clippy::cast_sign_loss)]
                    {
                        chunk_stride as u32
                    }
                } else {
                    data.width * 4
                };

                Frame::CpuMapped {
                    data: pixels,
                    width: data.width,
                    height: data.height,
                    stride,
                    format: data.format,
                }
            } else {
                trace!("Ignoring buffer with unsupported data type: {data_type:?}");
                return;
            };

            // Send frame to the consumer. If the receiver is dropped, stop.
            data.frame_count += 1;
            if data.frame_count == 1 || data.frame_count.is_multiple_of(300) {
                info!(
                    frame = data.frame_count,
                    data_type = ?data_type,
                    width = data.width,
                    height = data.height,
                    "Captured frame"
                );
            }
            // Never block the PipeWire loop: blocking here delays buffer
            // recycling to the compositor and queues stale frames, adding
            // latency. If the encoder is behind, drop this frame — the next
            // capture is fresher anyway.
            match data.tx.try_send(frame.into()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    data.dropped_count += 1;
                    if data.dropped_count.is_multiple_of(300) {
                        info!(
                            dropped = data.dropped_count,
                            "Encoder behind, dropping captured frames"
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    info!("Frame receiver dropped, stopping capture");
                    unsafe {
                        pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                    }
                }
            }
        })
        .register()
        .map_err(|e| CaptureError::PipeWireError(format!("failed to register listener: {e}")))?;

    // 8. Build format parameters for negotiation.
    let param_bytes_list = build_format_params(&config);
    let param_pods: Vec<&pipewire::spa::pod::Pod> = param_bytes_list
        .iter()
        .filter_map(|bytes| pipewire::spa::pod::Pod::from_bytes(bytes))
        .collect();

    if param_pods.is_empty() {
        return Err(CaptureError::NegotiationError(
            "failed to build any format pods".to_string(),
        ));
    }

    let mut params: Vec<&pipewire::spa::pod::Pod> = param_pods;

    info!(
        param_count = params.len(),
        "Connecting PipeWire stream with format params"
    );

    // 9. Connect the stream to the portal node.
    stream
        .connect(
            Direction::Input,
            Some(pw_node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| CaptureError::PipeWireError(format!("failed to connect stream: {e}")))?;

    info!(
        node_id = pw_node_id,
        "PipeWire capture stream connected, entering main loop"
    );

    // 10. Register a timer to periodically check the shutdown flag.
    // This ensures we exit even if no frames arrive.
    {
        use std::time::Duration;

        let timer = mainloop.loop_().add_timer(move |_| {
            if shutdown.load(Ordering::Relaxed) {
                unsafe {
                    pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                }
            }
        });
        // Check every 100 ms.
        let _ = timer.update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        );

        // Run the main loop (blocks until quit is called).
        mainloop.run();

        // Timer is dropped here, unregistering it.
        drop(timer);
    }

    info!("PipeWire capture stream exited");

    // Ensure the param buffers live long enough (referenced by Pod slices).
    let _ = params;
    drop(param_bytes_list);

    Ok(())
}
