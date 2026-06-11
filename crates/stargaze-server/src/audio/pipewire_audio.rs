use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pipewire::context::ContextBox;
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

    // 5. Create the audio capture stream targeting the sink monitor.
    let stream = StreamBox::new(
        &core,
        "stargaze-audio-capture",
        properties! {
            "media.type" => "Audio",
            "media.category" => "Capture",
            "media.role" => "Game",
            "stream.capture.sink" => "true"
        },
    )
    .map_err(|e| AudioError::CaptureInit(format!("failed to create PipeWire stream: {e}")))?;

    // 6. Build user data for callbacks.
    let user_data = AudioCallbackData {
        tx: frames_tx,
        shutdown: Arc::clone(&shutdown),
        sample_rate: config.sample_rate,
        channels: config.channels,
        pts: 0,
        dropped_count: 0,
    };

    let mainloop_ptr = mainloop.as_raw_ptr();

    // 7. Register stream callbacks.
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

    // 8. Build and connect with format parameters.
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

    // 9. Signal successful init to the caller.
    let _ = init_tx.send(Ok(()));

    // 10. Register a timer to periodically check the shutdown flag.
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
