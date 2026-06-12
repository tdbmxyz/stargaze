use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pipewire::context::ContextBox;
use pipewire::core::Core;
use pipewire::main_loop::MainLoopBox;
use pipewire::properties::properties;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, object, property};
use pipewire::spa::utils::{Direction, SpaTypes};
use pipewire::stream::{StreamBox, StreamFlags, StreamState};
use stargaze_core::audio::{AudioCaptureConfig, AudioError, AudioFrame};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

/// User data passed through `PipeWire` audio stream callbacks.
struct AudioCallbackData {
    /// Channel sender for delivering captured audio frames to the consumer.
    tx: mpsc::Sender<AudioFrame>,
    /// Shutdown flag — when set, the main loop should exit.
    shutdown: Arc<AtomicBool>,
    /// Negotiated sample rate in Hz.
    sample_rate: u32,
    /// Negotiated number of channels.
    channels: u16,
    /// Monotonic frame counter for PTS.
    pts: u64,
    /// Number of frames dropped because the encoder channel was full.
    dropped_count: u64,
}

/// Builds the SPA format pod for audio stream negotiation.
///
/// Requests `Audio/Raw` with f32le format at the given sample rate and channels.
fn build_audio_format_params(config: &AudioCaptureConfig) -> Vec<u8> {
    use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(config.sample_rate);
    audio_info.set_channels(u32::from(config.channels));

    let format_obj = object! {
        SpaTypes::ObjectParamFormat,
        pipewire::spa::param::ParamType::EnumFormat,
        property!(
            FormatProperties::MediaType,
            Id,
            MediaType::Audio
        ),
        property!(
            FormatProperties::MediaSubtype,
            Id,
            MediaSubtype::Raw
        ),
        property!(
            FormatProperties::AudioFormat,
            Id,
            AudioFormat::F32LE
        ),
        property!(
            FormatProperties::AudioRate,
            Int,
            i32::try_from(config.sample_rate).unwrap_or(48_000_i32)
        ),
        property!(
            FormatProperties::AudioChannels,
            Int,
            i32::from(config.channels)
        ),
    };

    let pod_value = pod::Value::Object(format_obj);
    let cursor = Cursor::new(Vec::new());
    let (bytes, _len) =
        PodSerializer::serialize(cursor, &pod_value).expect("failed to serialize audio format pod");
    bytes.into_inner()
}

/// Extracts the node name from a `default` metadata value.
///
/// Values in the session manager's `default` metadata are JSON objects
/// like `{"name":"alsa_output.pci-0000_2f_00.4.iec958-stereo"}`.
fn parse_metadata_node_name(value: &str) -> Option<String> {
    let after_key = value.split_once("\"name\"")?.1;
    let after_colon = after_key.split_once(':')?.1;
    let name = after_colon.split_once('"')?.1.split_once('"')?.0;
    (!name.is_empty()).then(|| name.to_string())
}

/// Performs one synchronous roundtrip to the `PipeWire` server, returning
/// once all events queued before the call have been delivered.
///
/// # Errors
///
/// Returns `AudioError::CaptureInit` if the sync request fails or the
/// server reports an error before completing it.
fn core_roundtrip(mainloop: &MainLoopBox, core: &Core) -> Result<(), AudioError> {
    use std::cell::Cell;
    use std::rc::Rc;

    let pending = core
        .sync(0)
        .map_err(|e| AudioError::CaptureInit(format!("PipeWire core sync failed: {e}")))?;

    let done = Rc::new(Cell::new(false));
    let done_cb = Rc::clone(&done);
    let mainloop_ptr = mainloop.as_raw_ptr();

    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pipewire::core::PW_ID_CORE && seq == pending {
                done_cb.set(true);
                unsafe {
                    pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                }
            }
        })
        .error(move |id, _seq, res, message| {
            error!(id, res, "PipeWire core error during roundtrip: {message}");
            unsafe {
                pipewire_sys::pw_main_loop_quit(mainloop_ptr);
            }
        })
        .register();

    mainloop.run();

    if done.get() {
        Ok(())
    } else {
        Err(AudioError::CaptureInit(
            "PipeWire core roundtrip aborted by server error".to_string(),
        ))
    }
}

/// Resolves the name of the default audio sink from the session manager's
/// `default` metadata object.
///
/// Returns `Ok(None)` if no `default` metadata or no default sink exists
/// (e.g. a session manager that does not publish defaults).
///
/// # Errors
///
/// Returns `AudioError::CaptureInit` if registry access or a server
/// roundtrip fails.
fn resolve_default_sink(
    mainloop: &MainLoopBox,
    core: &Core,
) -> Result<Option<String>, AudioError> {
    use std::cell::RefCell;
    use std::rc::Rc;

    use pipewire::metadata::Metadata;
    use pipewire::properties::PropertiesBox;
    use pipewire::registry::GlobalObject;
    use pipewire::types::ObjectType;

    let registry = core
        .get_registry()
        .map_err(|e| AudioError::CaptureInit(format!("failed to get PipeWire registry: {e}")))?;

    // Phase 1: enumerate globals to find the "default" metadata object.
    let default_meta: Rc<RefCell<Option<GlobalObject<PropertiesBox>>>> =
        Rc::new(RefCell::new(None));
    let default_meta_cb = Rc::clone(&default_meta);
    let reg_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ == ObjectType::Metadata
                && global
                    .props
                    .as_ref()
                    .and_then(|p| p.get("metadata.name"))
                    .is_some_and(|name| name == "default")
            {
                *default_meta_cb.borrow_mut() = Some(global.to_owned());
            }
        })
        .register();
    core_roundtrip(mainloop, core)?;
    drop(reg_listener);

    let Some(global) = default_meta.borrow_mut().take() else {
        return Ok(None);
    };

    // Phase 2: bind it and read the `default.audio.sink` property (the
    // server replays all current properties to a freshly bound listener).
    let metadata: Metadata = registry
        .bind(&global)
        .map_err(|e| AudioError::CaptureInit(format!("failed to bind default metadata: {e}")))?;

    let sink_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let sink_name_cb = Rc::clone(&sink_name);
    let _meta_listener = metadata
        .add_listener_local()
        .property(move |_subject, key, _type, value| {
            if key == Some("default.audio.sink")
                && let Some(name) = value.and_then(parse_metadata_node_name)
            {
                *sink_name_cb.borrow_mut() = Some(name);
            }
            0
        })
        .register();
    core_roundtrip(mainloop, core)?;

    let name = sink_name.borrow_mut().take();
    Ok(name)
}

/// Runs the `PipeWire` audio capture stream on the current thread (blocking).
///
/// Captures system audio output via a sink monitor. Sends raw f32 PCM
/// `AudioFrame`s through the provided mpsc sender until the shutdown flag is set.
///
/// # Errors
///
/// Returns `AudioError::CaptureInit` if `PipeWire` initialization or stream setup fails.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub(crate) fn run_audio_capture(
    config: &AudioCaptureConfig,
    frames_tx: mpsc::Sender<AudioFrame>,
    shutdown: Arc<AtomicBool>,
    init_tx: std::sync::mpsc::Sender<Result<(), AudioError>>,
) -> Result<(), AudioError> {
    // 1. Initialize PipeWire (idempotent — safe to call multiple times).
    pipewire::init();

    // 2. Create the main loop.
    let mainloop = MainLoopBox::new(None).map_err(|e| {
        AudioError::CaptureInit(format!("failed to create PipeWire main loop: {e}"))
    })?;

    // 3. Create context from the loop.
    let context = ContextBox::new(mainloop.loop_(), None)
        .map_err(|e| AudioError::CaptureInit(format!("failed to create PipeWire context: {e}")))?;

    // 4. Connect to PipeWire daemon.
    let core = context
        .connect(None)
        .map_err(|e| AudioError::CaptureInit(format!("failed to connect to PipeWire: {e}")))?;

    // 5. Resolve the default sink so the capture stream can target its
    //    monitor explicitly. `stream.capture.sink` alone is not reliably
    //    honored by WirePlumber for native streams — observed linking the
    //    stream to the default *source* (microphone) instead, which
    //    captures silence.
    let default_sink = match resolve_default_sink(&mainloop, &core) {
        Ok(Some(name)) => {
            info!(sink = %name, "Audio capture targeting default sink monitor");
            Some(name)
        }
        Ok(None) => {
            warn!("No default audio sink found, relying on session manager autoconnect");
            None
        }
        Err(e) => {
            warn!("Default sink lookup failed ({e}), relying on session manager autoconnect");
            None
        }
    };

    // 6. Create the audio capture stream targeting the sink monitor.
    let mut stream_props = properties! {
        "media.type" => "Audio",
        "media.category" => "Capture",
        "media.role" => "Game",
        "stream.capture.sink" => "true"
    };
    if let Some(ref name) = default_sink {
        stream_props.insert("target.object", name.as_str());
    }
    let stream = StreamBox::new(&core, "stargaze-audio-capture", stream_props)
        .map_err(|e| AudioError::CaptureInit(format!("failed to create PipeWire stream: {e}")))?;

    // 7. Build user data for callbacks.
    let user_data = AudioCallbackData {
        tx: frames_tx,
        shutdown: Arc::clone(&shutdown),
        sample_rate: config.sample_rate,
        channels: config.channels,
        pts: 0,
        dropped_count: 0,
    };

    let mainloop_ptr = mainloop.as_raw_ptr();

    // 8. Register stream callbacks.
    let _listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(move |_stream, _data, old, new| {
            info!("PipeWire audio stream state: {old:?} -> {new:?}");

            if let StreamState::Error(ref msg) = new {
                error!("PipeWire audio stream error: {msg}");
                unsafe {
                    pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                }
            }
        })
        .param_changed(|_stream, data, id, param| {
            use pipewire::spa::param::audio::AudioInfoRaw;

            let Some(param) = param else {
                return;
            };

            if id != pipewire::spa::param::ParamType::Format.as_raw() {
                return;
            }

            let mut audio_info = AudioInfoRaw::new();
            if audio_info.parse(param).is_ok() {
                data.sample_rate = audio_info.rate();
                data.channels = u16::try_from(audio_info.channels()).unwrap_or(2);

                info!(
                    sample_rate = data.sample_rate,
                    channels = data.channels,
                    "PipeWire audio format negotiated"
                );
            }
        })
        .process(move |stream, data| {
            if data.shutdown.load(Ordering::Relaxed) {
                debug!("Audio shutdown signaled, quitting PipeWire main loop");
                unsafe {
                    pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                }
                return;
            }

            let Some(mut buffer) = stream.dequeue_buffer() else {
                trace!("No audio buffer available from PipeWire stream");
                return;
            };

            let datas = buffer.datas_mut();
            if datas.is_empty() {
                warn!("PipeWire audio buffer has no data planes");
                return;
            }

            let d = &mut datas[0];
            let chunk_size = d.chunk().size() as usize;

            if chunk_size == 0 {
                trace!("Skipping empty PipeWire audio buffer chunk");
                return;
            }

            let Some(raw_bytes) = d.data() else {
                warn!("PipeWire audio buffer has null data pointer");
                return;
            };

            if chunk_size > raw_bytes.len() {
                warn!(
                    "Audio chunk size ({chunk_size}) exceeds buffer capacity ({})",
                    raw_bytes.len()
                );
                return;
            }

            // Reinterpret bytes as f32 samples.
            let byte_slice = &raw_bytes[..chunk_size];
            let num_samples = chunk_size / std::mem::size_of::<f32>();
            let mut samples = vec![0.0f32; num_samples];
            for (i, chunk) in byte_slice.chunks_exact(4).enumerate() {
                samples[i] = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            }

            let frame = AudioFrame {
                data: samples,
                sample_rate: data.sample_rate,
                channels: data.channels,
                pts: data.pts,
            };
            data.pts += 1;

            // Never block the PipeWire realtime thread: while no client is
            // connected the encoder backs up and the channel fills, and
            // blocking here wedges the stream's data loop so audio never
            // recovers. Drop the frame instead — same policy as the video
            // capture.
            match data.tx.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    data.dropped_count += 1;
                    if data.dropped_count.is_multiple_of(500) {
                        info!(
                            dropped = data.dropped_count,
                            "Audio encoder behind, dropping captured audio frames"
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    info!("Audio frame receiver dropped, stopping capture");
                    unsafe {
                        pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                    }
                }
            }
        })
        .register()
        .map_err(|e| {
            AudioError::CaptureInit(format!("failed to register audio stream listener: {e}"))
        })?;

    // 9. Build and connect with format parameters.
    let param_bytes = build_audio_format_params(config);
    let param_pod = pipewire::spa::pod::Pod::from_bytes(&param_bytes)
        .ok_or_else(|| AudioError::CaptureInit("failed to build audio format pod".to_string()))?;
    let mut params = [param_pod];

    stream
        .connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|e| AudioError::CaptureInit(format!("failed to connect audio stream: {e}")))?;

    info!("PipeWire audio capture stream connected, entering main loop");

    // 10. Signal successful init to the caller.
    let _ = init_tx.send(Ok(()));

    // 11. Register a timer to periodically check the shutdown flag.
    {
        use std::time::Duration;

        let timer = mainloop.loop_().add_timer(move |_| {
            if shutdown.load(Ordering::Relaxed) {
                unsafe {
                    pipewire_sys::pw_main_loop_quit(mainloop_ptr);
                }
            }
        });
        let _ = timer.update_timer(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(100)),
        );

        // Run the main loop (blocks until quit is called).
        mainloop.run();

        drop(timer);
    }

    info!("PipeWire audio capture stream exited");

    let _ = params;
    drop(param_bytes);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_node_name_from_metadata_json() {
        assert_eq!(
            parse_metadata_node_name(r#"{"name":"alsa_output.pci-0000_2f_00.4.iec958-stereo"}"#),
            Some("alsa_output.pci-0000_2f_00.4.iec958-stereo".to_string())
        );
    }

    #[test]
    fn parses_node_name_with_whitespace() {
        assert_eq!(
            parse_metadata_node_name(r#"{ "name" : "easyeffects_sink" }"#),
            Some("easyeffects_sink".to_string())
        );
    }

    #[test]
    fn rejects_values_without_name() {
        assert_eq!(parse_metadata_node_name(r#"{"id":42}"#), None);
        assert_eq!(parse_metadata_node_name(r#"{"name":""}"#), None);
        assert_eq!(parse_metadata_node_name(""), None);
    }
}
