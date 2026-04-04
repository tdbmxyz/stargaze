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
use stargaze_core::capture::{CaptureError, DmaBufInfo, Frame, PixelFormat};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use super::CaptureConfig;

/// User data passed through `PipeWire` stream callbacks.
///
/// Stores shared state that the process callback needs to construct
/// and send frames.
struct CaptureCallbackData {
    /// Channel sender for delivering captured frames to the consumer.
    tx: mpsc::Sender<Frame>,
    /// Shutdown flag — when set, the main loop should exit.
    shutdown: Arc<AtomicBool>,
    /// Negotiated frame width (set from config, may be updated on format change).
    width: u32,
    /// Negotiated frame height (set from config, may be updated on format change).
    height: u32,
    /// Negotiated pixel format (defaults to `Bgra8`).
    format: PixelFormat,
    /// Negotiated DRM format modifier (e.g. tiling/compression layout).
    ///
    /// Extracted from `PipeWire` format negotiation. Critical for correct
    /// interpretation of `DMA-BUF` frames by downstream consumers (e.g. NVENC).
    modifier: u64,
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
        _ => None,
    }
}

/// Builds the SPA format pod for video stream negotiation.
///
/// Offers `Video/Raw` with a choice of pixel formats, a size range up to
/// the configured resolution, and a framerate of `0/1` (variable) so the
/// portal node can drive timing.
///
/// `VideoMaxFramerate` is intentionally omitted — some portal backends
/// (notably xdg-desktop-portal-hyprland) reject it during format
/// negotiation, causing "no more input formats" errors.
fn build_format_params(config: &CaptureConfig) -> Vec<u8> {
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
                width: config.width,
                height: config.height,
            }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
    };

    let pod_value = pod::Value::Object(format_obj);
    let cursor = Cursor::new(Vec::new());
    let (bytes, _len) =
        PodSerializer::serialize(cursor, &pod_value).expect("failed to serialize format pod");
    bytes.into_inner()
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
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
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
        .param_changed(|_stream, data, id, param| {
            let Some(param) = param else {
                return;
            };

            // We only care about the Format param.
            if id != pipewire::spa::param::ParamType::Format.as_raw() {
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
            } else if data_type == pipewire::spa::buffer::DataType::MemPtr {
                // CPU-mapped path: copy bytes out of the PipeWire buffer.
                let Some(slice) = d.data() else {
                    warn!("MemPtr buffer has null data pointer");
                    return;
                };

                let size = chunk_size as usize;
                if size > slice.len() {
                    warn!(
                        "Chunk size ({size}) exceeds buffer capacity ({})",
                        slice.len()
                    );
                    return;
                }

                let pixels = slice[..size].to_vec();

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
            if data.tx.blocking_send(frame).is_err() {
                info!("Frame receiver dropped, stopping capture");
                unsafe {
                    pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                }
            }
        })
        .register()
        .map_err(|e| CaptureError::PipeWireError(format!("failed to register listener: {e}")))?;

    // 8. Build format parameters for negotiation.
    let param_bytes = build_format_params(&config);
    let param_pod = pipewire::spa::pod::Pod::from_bytes(&param_bytes)
        .ok_or_else(|| CaptureError::NegotiationError("failed to build format pod".to_string()))?;
    let mut params = [param_pod];

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

    // Ensure the param_bytes buffer lives long enough (referenced by params pod).
    let _ = params;
    drop(param_bytes);

    Ok(())
}
