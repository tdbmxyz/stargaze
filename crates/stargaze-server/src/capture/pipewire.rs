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

use super::color::SpaColor;
use super::{CaptureConfig, CaptureFormat};

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
    /// Exact DRM fourcc paired with the negotiated SPA format.
    drm_fourcc: u32,
    /// Negotiated DRM format modifier (e.g. tiling/compression layout).
    modifier: u64,
    /// Oneshot sender for the negotiated resolution — fires once on first
    /// `param_changed` with a valid format. `None` after the first send.
    resolution_tx: Option<oneshot::Sender<CaptureFormat>>,
    /// Frame counter for diagnostic logging.
    frame_count: u64,
    /// Frames dropped because the encoder was behind (channel full).
    dropped_count: u64,
    /// Buffer memory type last acknowledged to `PipeWire`.
    /// Prevents re-entrant `update_params` cycles while allowing DMA-BUF
    /// fixation to replace an initial shared-memory format callback.
    acked_dmabuf: Option<bool>,
    /// None until a supported color description has been validated.
    color: Option<stargaze_core::color::ColorDescription>,
    require_color_metadata: bool,
    hdr_metadata: stargaze_core::hdr::HdrMetadata,
    active_stream: Option<stargaze_core::color::ColorDescription>,
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
        VideoFormat::xBGR_210LE | VideoFormat::ABGR_210LE => Some(PixelFormat::Bgra10),
        VideoFormat::xRGB_210LE | VideoFormat::ARGB_210LE => Some(PixelFormat::Rgba10),
        _ => None,
    }
}

/// DRM fourcc assembled from four ASCII bytes.
const fn drm_fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// Exact SPA/DRM format pair used for modifier negotiation and EGL import.
struct DmaBufFormat {
    spa_format: pipewire::spa::param::video::VideoFormat,
    drm_fourcc: u32,
}

const DMABUF_FORMATS: &[DmaBufFormat] = &[
    // 8-bit packed RGB.
    DmaBufFormat {
        spa_format: pipewire::spa::param::video::VideoFormat::BGRA,
        drm_fourcc: drm_fourcc(b'A', b'R', b'2', b'4'), // DRM_FORMAT_ARGB8888
    },
    DmaBufFormat {
        spa_format: pipewire::spa::param::video::VideoFormat::BGRx,
        drm_fourcc: drm_fourcc(b'X', b'R', b'2', b'4'), // DRM_FORMAT_XRGB8888
    },
    DmaBufFormat {
        spa_format: pipewire::spa::param::video::VideoFormat::RGBA,
        drm_fourcc: drm_fourcc(b'A', b'B', b'2', b'4'), // DRM_FORMAT_ABGR8888
    },
    DmaBufFormat {
        spa_format: pipewire::spa::param::video::VideoFormat::RGBx,
        drm_fourcc: drm_fourcc(b'X', b'B', b'2', b'4'), // DRM_FORMAT_XBGR8888
    },
    // 10-bit packed RGB formats exposed by 10-bit compositors.
    DmaBufFormat {
        spa_format: pipewire::spa::param::video::VideoFormat::xBGR_210LE,
        drm_fourcc: drm_fourcc(b'X', b'B', b'3', b'0'), // DRM_FORMAT_XBGR2101010
    },
    DmaBufFormat {
        spa_format: pipewire::spa::param::video::VideoFormat::ABGR_210LE,
        drm_fourcc: drm_fourcc(b'A', b'B', b'3', b'0'), // DRM_FORMAT_ABGR2101010
    },
    DmaBufFormat {
        spa_format: pipewire::spa::param::video::VideoFormat::xRGB_210LE,
        drm_fourcc: drm_fourcc(b'X', b'R', b'3', b'0'), // DRM_FORMAT_XRGB2101010
    },
    DmaBufFormat {
        spa_format: pipewire::spa::param::video::VideoFormat::ARGB_210LE,
        drm_fourcc: drm_fourcc(b'A', b'R', b'3', b'0'), // DRM_FORMAT_ARGB2101010
    },
];

fn spa_format_to_drm_fourcc(raw: u32) -> Option<u32> {
    DMABUF_FORMATS
        .iter()
        .find(|format| format.spa_format.as_raw() == raw)
        .map(|format| format.drm_fourcc)
}

/// Builds SPA format pods for video stream negotiation.
///
/// Creates one pod per EGL-supported SPA/DRM format with its concrete
/// modifier list, plus a fallback pod without modifiers for shared memory.
fn build_format_params(config: &CaptureConfig) -> Vec<Vec<u8>> {
    let desired_fourccs = DMABUF_FORMATS
        .iter()
        .map(|format| format.drm_fourcc)
        .collect::<Vec<_>>();
    let modifier_sets = match crate::encode::egl_cuda::query_dmabuf_modifiers(&desired_fourccs) {
        Ok(sets) => sets,
        Err(e) => {
            warn!(error = %e, "Cannot query EGL DMA-BUF modifiers; using shared-memory capture");
            Vec::new()
        }
    };

    build_format_params_with_modifiers(config, &modifier_sets)
}

fn build_format_params_with_modifiers(
    config: &CaptureConfig,
    modifier_sets: &[crate::encode::egl_cuda::DmaBufModifierSet],
) -> Vec<Vec<u8>> {
    let mut pods = Vec::new();
    for format in DMABUF_FORMATS {
        let Some(set) = modifier_sets
            .iter()
            .find(|set| set.drm_fourcc == format.drm_fourcc)
        else {
            continue;
        };
        if set.modifiers.is_empty() {
            continue;
        }
        pods.push(build_dmabuf_format_pod(
            config,
            format.spa_format,
            &set.modifiers,
        ));
    }

    // SHM fallback pod (no modifier) — used if DMA-BUF negotiation fails.
    let dmabuf_pod_count = pods.len();
    pods.push(build_shm_fallback_pod(config));

    info!(
        dmabuf_pod_count,
        pod_count = pods.len(),
        pod_sizes = ?pods.iter().map(Vec::len).collect::<Vec<_>>(),
        "Built format negotiation pods (explicit DMA-BUF modifiers + SHM fallback)"
    );

    pods
}

/// Builds a single DMA-BUF format pod with explicit EGL modifiers.
fn build_dmabuf_format_pod(
    config: &CaptureConfig,
    video_format: pipewire::spa::param::video::VideoFormat,
    modifiers: &[u64],
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
        property!(FormatProperties::VideoFormat, Id, video_format),
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

    // DONT_FIXATE lets the producer choose the best concrete layout from
    // EGL's list. The first item is the preferred/default choice; PipeWire's
    // Enum representation also expects it among the alternatives.
    let modifiers = modifiers
        .iter()
        .copied()
        .map(u64::cast_signed)
        .collect::<Vec<_>>();
    let default_modifier = modifiers[0];
    format_obj.properties.push(Property {
        key: FormatProperties::VideoModifier.as_raw(),
        flags: PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
        value: Value::Choice(pod::ChoiceValue::Long(pipewire::spa::utils::Choice(
            pipewire::spa::utils::ChoiceFlags::empty(),
            pipewire::spa::utils::ChoiceEnum::Enum {
                default: default_modifier,
                alternatives: modifiers,
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

/// Implicit/unspecified DRM layout sentinel.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Copy a bounded `MemFd` subregion without relying on `PipeWire`'s mapping.
/// Mapping from offset zero handles non-page-aligned chunks correctly.
fn copy_memfd(fd: std::os::fd::RawFd, offset: usize, size: usize) -> std::io::Result<Vec<u8>> {
    let mapped_size = offset.checked_add(size).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "MemFd range overflow")
    })?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if size == 0
        || u64::try_from(stat.st_size).unwrap_or(0) < u64::try_from(mapped_size).unwrap_or(u64::MAX)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "MemFd chunk exceeds backing file",
        ));
    }
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            mapped_size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error());
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(mapped.cast::<u8>().add(offset), size) }.to_vec();
    unsafe {
        libc::munmap(mapped, mapped_size);
    }
    Ok(bytes)
}

/// Acknowledge a negotiated format by calling `stream.update_params()` with
/// buffer-type and meta params. Matches Sunshine's `on_param_changed` exactly:
/// only `dataType` in Buffers (let the producer own allocation), plus Meta
/// Header and Meta `VideoDamage` (choice-range).
fn ack_format(stream: &pipewire::stream::Stream, modifier: u64) {
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::{Property, PropertyFlags, Value};
    use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

    let is_dmabuf = modifier != 0 && modifier != DRM_FORMAT_MOD_INVALID;

    let buffer_types: i32 = if is_dmabuf {
        info!("Requesting DMA-BUF buffers (modifier 0x{modifier:x})");
        1 << SPA_DATA_DMA_BUF
    } else {
        // No DMA-BUF here on purpose: with a LINEAR (0) or invalid
        // modifier the NVIDIA EGL/CUDA import reads the buffer scrambled
        // (verified on driver 595.71.05 — the GPU-side view of a linear
        // DMA-BUF doesn't match its actual memory). Shared-memory buffers
        // are correct and the GPU converter keeps them fast.
        info!("Requesting MemPtr + MemFd buffers (no real DRM modifier)");
        (1 << SPA_DATA_MEM_PTR) | (1 << SPA_DATA_MEM_FD)
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
    pw_fd: Option<OwnedFd>,
    pw_node_id: u32,
    config: CaptureConfig,
    tx: mpsc::Sender<CapturedFrame>,
    shutdown: Arc<AtomicBool>,
    resolution_tx: oneshot::Sender<CaptureFormat>,
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
    let core = match pw_fd {
        Some(fd) => context.connect_fd(fd, None),
        None => context.connect(None),
    }
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
        drm_fourcc: 0,
        modifier: 0,
        resolution_tx: Some(resolution_tx),
        frame_count: 0,
        dropped_count: 0,
        acked_dmabuf: None,
        color: None,
        require_color_metadata: config.require_color_metadata || config.node_id.is_some(),
        hdr_metadata: config.hdr_metadata,
        active_stream: None,
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
        .param_changed(move |stream, data, id, param| {
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

            // Invalidate first: an unsupported transition must never keep the
            // previous SDR interpretation, even if another buffer is queued.
            data.color = None;
            // Try to parse the negotiated video format.
            let mut video_info = pipewire::spa::param::video::VideoInfoRaw::new();
            if video_info.parse(param).is_ok() {
                let size = video_info.size();

                // A resolution change after the initial negotiation means
                // the fixed-size encoder must be rebuilt. Modifier and pixel
                // format changes are part of normal DMA-BUF fixation before
                // streaming and are safe to update in-place.
                let spa_format = video_info.format().as_raw();
                let new_format = spa_format_to_pixel_format(spa_format);
                let new_drm_fourcc = spa_format_to_drm_fourcc(spa_format);
                let source_color = SpaColor {
                    range: video_info.color_range(),
                    matrix: video_info.color_matrix(),
                    transfer: video_info.transfer_function(),
                    primaries: video_info.color_primaries(),
                };
                info!(?source_color, spa_format, "Capture source color metadata (SPA enums; zero = absent)");
                let validated = new_format.ok_or("unsupported capture pixel format")
                    .and_then(|format| source_color.validate(format, data.require_color_metadata));
                let (color, assumed) = match validated {
                    Ok(value) => value,
                    Err(reason) => {
                        error!(?source_color, reason, "Unsafe capture color mode; stopping before conversion (no tone/gamut mapping)");
                        unsafe { pipewire_sys::pw_main_loop_quit(mainloop_ptr); }
                        return;
                    }
                };
                if data.active_stream.is_some_and(|active| Some(active) != color.stream()) {
                    error!(?color, "Capture SDR/HDR mode changed; stopping for encoder/session renegotiation on restart");
                    unsafe { pipewire_sys::pw_main_loop_quit(mainloop_ptr); }
                    return;
                }
                if data.hdr_metadata != stargaze_core::hdr::HdrMetadata::default() && !color.is_supported_hdr_rgb() {
                    error!("HDR metadata supplied for an SDR capture source; refusing relabeling");
                    unsafe { pipewire_sys::pw_main_loop_quit(mainloop_ptr); }
                    return;
                }
                if assumed {
                    warn!(?source_color, "Capture has no complete color metadata: using Hyprland/XDPH's explicit sRGB screenshare-copy contract, not output HDR state. Use --strict-capture-color for other producers");
                }
                info!(source = ?color, stream = ?color.stream(),
                    "Validated capture color mode (native HDR10 when explicitly described; no tone/gamut relabeling)");
                let resolution_changed = data.resolution_tx.is_none()
                    && (size.width != data.width || size.height != data.height);
                if resolution_changed {
                    error!(
                        old_width = data.width,
                        old_height = data.height,
                        new_width = size.width,
                        new_height = size.height,
                        "Capture resolution changed mid-stream, stopping capture for a pipeline restart"
                    );
                    unsafe {
                        pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                    }
                    return;
                }

                data.color = Some(color);
                data.width = size.width;
                data.height = size.height;

                if let Some(pf) = new_format {
                    data.format = pf;
                }

                if let Some(drm_fourcc) = new_drm_fourcc {
                    data.drm_fourcc = drm_fourcc;
                }
                data.modifier = video_info.modifier();

                info!(
                    width = data.width,
                    height = data.height,
                    format = %data.format,
                    drm_fourcc = format_args!("0x{:08x}", data.drm_fourcc),
                    modifier = format_args!("0x{:x}", data.modifier),
                    "PipeWire format negotiated"
                );

                // ACK the selected memory type. DMA-BUF fixation first emits
                // an implicit format and then a concrete modifier, so update
                // the ACK once when that transition occurs. Store the state
                // before update_params because it re-enters param_changed.
                let is_dmabuf = data.modifier != 0 && data.modifier != DRM_FORMAT_MOD_INVALID;
                if data.acked_dmabuf != Some(is_dmabuf) {
                    data.acked_dmabuf = Some(is_dmabuf);
                    ack_format(stream, data.modifier);
                }
            } else {
                error!("Cannot parse capture format/color; stopping capture");
                unsafe { pipewire_sys::pw_main_loop_quit(mainloop_ptr); }
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

            let Some(color) = data.color else { return; };

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

                if data.drm_fourcc == 0 {
                    warn!("DMA-BUF frame has no DRM fourcc mapping; skipping buffer");
                    return;
                }

                Frame::DmaBuf(DmaBufInfo {
                    fd: owned_fd,
                    width: data.width,
                    height: data.height,
                    format: data.format,
                    drm_fourcc: data.drm_fourcc,
                    modifier: data.modifier,
                    stride,
                    offset: if let Some(offset) = d.as_raw().mapoffset.checked_add(chunk_offset) {
                        offset
                    } else {
                        warn!("DMA-BUF plane offset overflow");
                        return;
                    },
                })
            } else if data_type == pipewire::spa::buffer::DataType::MemPtr
                || data_type == pipewire::spa::buffer::DataType::MemFd
            {
                // MemFd uses the real backing-file offset, not just chunk.offset.
                // MAP_BUFFERS is unreliable on the target; MemPtr is already mapped.
                let start = chunk_offset as usize;
                let size = chunk_size as usize;
                let Some(end) = start.checked_add(size).filter(|end| *end <= d.as_raw().maxsize as usize) else {
                    warn!("Capture chunk exceeds its data region"); return;
                };
                let map_offset = d.as_raw().mapoffset as usize;
                let memfd_raw = if data_type == pipewire::spa::buffer::DataType::MemFd {
                    Some(d.fd())
                } else {
                    None
                };

                let pixels = if let Some(raw_fd) = memfd_raw {
                    let Some(offset) = map_offset.checked_add(start) else { warn!("MemFd offset overflow"); return; };
                    match copy_memfd(raw_fd, offset, size) {
                        Ok(bytes) => bytes,
                        Err(error) => { warn!(%error, "Cannot map capture MemFd safely"); return; }
                    }
                } else {
                    // MemPtr path: use PipeWire's data() as before.
                    let Some(slice) = d.data() else {
                        warn!("MemPtr buffer has null data pointer");
                        return;
                    };
                    let Some(pixels) = slice.get(start..end) else { warn!("MemPtr chunk exceeds mapping"); return; };
                    pixels.to_vec()
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
            if data.frame_count == 1
                || (stargaze_core::logging::progress_logging()
                    && data.frame_count.is_multiple_of(300))
            {
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
            let mut captured = CapturedFrame::from(frame);
            captured.color = color;
            captured.hdr_metadata = data.hdr_metadata;
            // Fixation may refine metadata/modifiers after the initial Format
            // callback. Select the encoder from the first actual buffer.
            if let Some(tx) = data.resolution_tx.take() {
                data.active_stream = color.stream();
                let _ = tx.send(CaptureFormat { resolution: Resolution { width: data.width, height: data.height }, color });
            }
            match data.tx.try_send(captured) {
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

#[cfg(test)]
mod tests {
    use pipewire::spa::param::format::FormatProperties;
    use pipewire::spa::pod::Value;
    use pipewire::spa::pod::deserialize::PodDeserializer;
    use pipewire::spa::utils::ChoiceEnum;

    use super::*;

    fn config() -> CaptureConfig {
        CaptureConfig {
            width: 3440,
            height: 1440,
            show_cursor: true,
            require_color_metadata: false,
            ..CaptureConfig::default()
        }
    }

    #[test]
    fn memfd_copy_honors_unaligned_offsets_and_rejects_out_of_bounds() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let fd = unsafe { libc::memfd_create(c"capture-offset".as_ptr(), 0) };
        assert!(fd >= 0);
        let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        file.write_all(b"header-actual-pixels-tail").unwrap();
        assert_eq!(
            copy_memfd(file.as_raw_fd(), 7, 13).unwrap(),
            b"actual-pixels"
        );
        assert!(copy_memfd(file.as_raw_fd(), 1000, 4).is_err());
        assert!(copy_memfd(file.as_raw_fd(), usize::MAX, 4).is_err());
    }

    #[test]
    fn spa_formats_preserve_exact_drm_fourcc() {
        use pipewire::spa::param::video::VideoFormat;

        assert_eq!(
            spa_format_to_drm_fourcc(VideoFormat::BGRA.as_raw()),
            Some(drm_fourcc(b'A', b'R', b'2', b'4'))
        );
        assert_eq!(
            spa_format_to_drm_fourcc(VideoFormat::BGRx.as_raw()),
            Some(drm_fourcc(b'X', b'R', b'2', b'4'))
        );
        assert_eq!(
            spa_format_to_drm_fourcc(VideoFormat::RGBA.as_raw()),
            Some(drm_fourcc(b'A', b'B', b'2', b'4'))
        );
        assert_eq!(
            spa_format_to_drm_fourcc(VideoFormat::RGBx.as_raw()),
            Some(drm_fourcc(b'X', b'B', b'2', b'4'))
        );
        assert_eq!(
            spa_format_to_drm_fourcc(VideoFormat::xBGR_210LE.as_raw()),
            Some(drm_fourcc(b'X', b'B', b'3', b'0'))
        );
        assert_eq!(
            spa_format_to_drm_fourcc(VideoFormat::ABGR_210LE.as_raw()),
            Some(drm_fourcc(b'A', b'B', b'3', b'0'))
        );
        assert_eq!(
            spa_format_to_drm_fourcc(VideoFormat::xRGB_210LE.as_raw()),
            Some(drm_fourcc(b'X', b'R', b'3', b'0'))
        );
        assert_eq!(
            spa_format_to_drm_fourcc(VideoFormat::ARGB_210LE.as_raw()),
            Some(drm_fourcc(b'A', b'R', b'3', b'0'))
        );
    }

    #[test]
    fn format_params_embed_explicit_modifiers_and_keep_shm_fallback() {
        let drm_fourcc = drm_fourcc(b'A', b'R', b'2', b'4');
        let modifiers = [0x0300_0000_0000_0010, 0x0300_0000_0000_0012];
        let pods = build_format_params_with_modifiers(
            &config(),
            &[crate::encode::egl_cuda::DmaBufModifierSet {
                drm_fourcc,
                modifiers: modifiers.to_vec(),
            }],
        );

        assert_eq!(pods.len(), 2, "one DMA-BUF pod plus SHM fallback");
        let (_, value) = PodDeserializer::deserialize_any_from(&pods[0]).unwrap();
        let Value::Object(object) = value else {
            panic!("format pod must deserialize as an object");
        };
        let modifier = object
            .properties
            .iter()
            .find(|property| property.key == FormatProperties::VideoModifier.as_raw())
            .expect("DMA-BUF pod must contain VideoModifier");
        let Value::Choice(pod::ChoiceValue::Long(choice)) = &modifier.value else {
            panic!("VideoModifier must be a long choice");
        };
        let ChoiceEnum::Enum {
            default,
            alternatives,
        } = &choice.1
        else {
            panic!("VideoModifier must be an enum choice");
        };
        assert_eq!(*default, modifiers[0].cast_signed());
        assert_eq!(
            alternatives,
            &modifiers.map(u64::cast_signed),
            "all concrete EGL modifiers must be offered"
        );
    }

    #[test]
    fn no_egl_modifiers_builds_only_shm_fallback() {
        let pods = build_format_params_with_modifiers(&config(), &[]);
        assert_eq!(pods.len(), 1);
    }
}
