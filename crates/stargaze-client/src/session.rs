//! One streaming session: takes an established connection and runs
//! decoders, input forwarding, and the renderer until the session ends.
//!
//! Extracted from `main` so the launcher UI can run sessions in a loop
//! (menu → session → back to menu). Everything here is re-runnable and
//! every exit path (including decoder start failures) runs the same
//! teardown — the pre-launcher code could rely on process exit to
//! release controllers, kill rsonance, and close the connection; this
//! code cannot. Once-only process state (tracing, rustls provider,
//! `sdl2::init()`) stays in `main`.

use anyhow::anyhow;
use stargaze_core::audio::AudioDecoderConfig;
use stargaze_core::config::{self, ClientConfig};
use stargaze_core::decode::DecoderConfig;
use stargaze_core::mic_forward;
use tracing::{info, warn};

use crate::transport::{ConnectedSession, SessionParams};
use crate::{decode, gamepad, render, usb};

/// Runs a full session on an established connection, returning when the
/// renderer exits (window closed, quit shortcut, or transport death).
///
/// `cfg` carries the toggles (fullscreen, gamepad passthrough, USB
/// forward, mic forward); the decode codec comes from the
/// server-confirmed session parameters, not the request.
///
/// # Errors
///
/// Returns an error if a decoder or the renderer fails to start or
/// fails fatally; the session is fully torn down either way.
pub async fn run_session(
    sdl: &sdl2::Sdl,
    cfg: &ClientConfig,
    conn: ConnectedSession,
    stats_file: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let ConnectedSession {
        transport: client_transport,
        session_params,
        video_frames,
        audio_frames,
        input_tx: transport_input_tx,
        idr_tx: decoder_idr_tx,
        rtt_probe,
        net_stats,
        usb_connection,
    } = conn;

    // Handle for closing the connection at teardown: without an
    // explicit close, the USB forwarder (which owns its own clone)
    // would keep Valve hardware tunneled to a dead session until the
    // server's idle timeout.
    let connection = usb_connection.clone();

    // USB forwarding: tunnel Valve controller hardware to the server
    // over the session connection (Steam needs the real USB device).
    // With forward_builtin_controller the Deck's own controller is
    // included — full handoff, the remote Steam sees the real device.
    let builtin_handoff = cfg.usb_forward && cfg.forward_builtin_controller;
    let usb_forwarder = if cfg.usb_forward {
        Some(usb::start(usb_connection, cfg.forward_builtin_controller))
    } else {
        drop(usb_connection);
        None
    };

    info!(
        "Connected, session: {}x{} @ {}fps, {} Mbps, codec {}",
        session_params.width,
        session_params.height,
        session_params.framerate,
        session_params.bitrate_mbps,
        session_params.codec,
    );
    if session_params.codec != cfg.codec {
        warn!(
            "Server encodes {} (its configured codec), not the requested {}; decoding {0}",
            session_params.codec, cfg.codec
        );
    }

    // Optionally start rsonance transmitter for mic forwarding.
    let mut rsonance_child = if cfg.mic_forward.enabled {
        match mic_forward::spawn_rsonance_transmitter(&cfg.mic_forward, &cfg.server_address) {
            Ok(child) => {
                info!("Mic forwarding enabled (rsonance transmitter)");
                Some(child)
            }
            Err(e) => {
                tracing::warn!("Failed to start rsonance transmitter: {e}");
                None
            }
        }
    } else {
        None
    };

    // Bridge: SDL event loop (std::sync::mpsc) → tokio channel → transport.
    // A plain detached OS thread, not spawn_blocking: it blocks in recv()
    // with senders held by long-lived gamepad scanner/reader threads, so
    // it only wakes on the next input event — a tokio blocking task would
    // stall runtime shutdown until then (client hung on quit).
    let (sdl_input_tx, sdl_input_rx) =
        std::sync::mpsc::channel::<stargaze_core::input::InputEvent>();
    let bridge = std::thread::Builder::new()
        .name("stargaze-input-bridge".into())
        .spawn(move || {
            while let Ok(event) = sdl_input_rx.recv() {
                if transport_input_tx.blocking_send(event).is_err() {
                    break;
                }
            }
        });

    // Everything below must funnel through the shared teardown at the
    // bottom, whatever fails.
    let mut passthrough = None;
    let mut volume_watch_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    let result = match bridge {
        Err(e) => Err(anyhow!("Failed to spawn input bridge thread: {e}")),
        Ok(_) => {
            // Gamepads: evdev pass-through (server clones the real
            // device) with automatic per-device fallback to SDL →
            // Xbox 360 emulation. Started before the SDL loop so
            // devices present at startup are grabbed before SDL
            // delivers their hotplug events.
            let gamepads = gamepad::SharedGamepads::new();
            // Escape hatch for the handoff: the tunneled controller
            // can't carry Select+Start locally, but the volume keys
            // stay local.
            if builtin_handoff {
                volume_watch_stop = Some(gamepad::start_volume_quit_watch(gamepads.clone()));
            }
            if builtin_handoff {
                // Full handoff: the controls travel as a real USB device;
                // emulating Steam's (now orphaned) local virtual gamepad
                // on top would put ghost controllers next to the genuine
                // one on the server.
                info!(
                    "Built-in controller handoff: skipping gamepad \
                     pass-through and Xbox 360 emulation"
                );
            } else if cfg.gamepad_passthrough {
                passthrough = Some(gamepad::start_passthrough(
                    gamepads.clone(),
                    sdl_input_tx.clone(),
                ));
            } else {
                info!("Gamepad pass-through disabled; using Xbox 360 emulation");
            }
            run_decoders_and_renderer(
                sdl,
                cfg,
                &session_params,
                video_frames,
                audio_frames,
                sdl_input_tx,
                decoder_idr_tx,
                rtt_probe,
                net_stats,
                stats_file,
                &gamepads,
                !builtin_handoff,
            )
        }
    };

    info!("Shutting down session");
    // Release grabbed controllers so whatever runs next (the launcher
    // menu, the desktop) sees them again; stop() waits for the release
    // so an immediate reconnect's scan cannot hit EBUSY.
    if let Some(handle) = passthrough {
        handle.stop();
    }
    if let Some(stop) = volume_watch_stop {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(ref mut child) = rsonance_child {
        mic_forward::stop_rsonance(child).await;
    }
    // Closing the connection (not just aborting the receive task) ends
    // the USB forwarder and releases tunneled devices locally. Wait for
    // that release to finish before returning, so an immediate reconnect
    // (or process exit tearing down the runtime) can't strand a tunneled
    // controller on the usbip-host stub.
    connection.close(0u32.into(), b"session ended");
    if let Some(forwarder) = usb_forwarder {
        forwarder.shutdown().await;
    }
    client_transport.abort();

    result?;
    info!("Session ended");
    Ok(())
}

/// Starts both decoders and runs the blocking SDL render loop; stops
/// the decoders on every exit path.
#[allow(clippy::too_many_arguments)]
fn run_decoders_and_renderer(
    sdl: &sdl2::Sdl,
    cfg: &ClientConfig,
    session_params: &SessionParams,
    video_frames: tokio::sync::mpsc::Receiver<stargaze_core::transport::ReassembledFrame>,
    audio_frames: tokio::sync::mpsc::Receiver<stargaze_core::transport::ReassembledFrame>,
    sdl_input_tx: std::sync::mpsc::Sender<stargaze_core::input::InputEvent>,
    decoder_idr_tx: tokio::sync::mpsc::Sender<()>,
    rtt_probe: crate::transport::RttProbe,
    net_stats: std::sync::Arc<crate::transport::NetStats>,
    stats_file: Option<std::path::PathBuf>,
    gamepads: &gamepad::SharedGamepads,
    emulate_gamepads: bool,
) -> anyhow::Result<()> {
    // Use the server-confirmed parameters for decoding and rendering:
    // the server may override the resolution (e.g. its display's
    // native size) and encodes with its own configured codec.
    let decoder_config = DecoderConfig {
        width: session_params.width,
        height: session_params.height,
        codec: session_params.codec,
    };

    let audio_decoder_config = AudioDecoderConfig {
        sample_rate: 48_000,
        channels: session_params.audio_channels,
    };

    // Start the audio decoder thread — sends decoded PCM to a channel.
    let (audio_decoder_session, audio_pcm_rx) =
        decode::start_audio_decoder(audio_decoder_config, audio_frames)?;

    // Start the video decoder thread.
    let (video_decoder_session, decoded_rx, zero_copy) =
        match decode::start_decoder(decoder_config.clone(), video_frames, decoder_idr_tx.clone()) {
            Ok(started) => started,
            Err(e) => {
                audio_decoder_session.stop().ok();
                return Err(e.into());
            }
        };

    let session_commands = render::SessionCommands {
        server: session_params.server_command.clone(),
        client: config::sanitized_command_line(),
    };

    // SDL2 event loop must run on the main OS thread.
    // Audio PCM is queued to the SDL2 AudioQueue inside the event loop.
    let render_result = tokio::task::block_in_place(|| {
        render::start_renderer(
            sdl,
            &decoder_config,
            decoded_rx,
            audio_pcm_rx,
            session_params.audio_channels,
            cfg.fullscreen,
            sdl_input_tx,
            rtt_probe,
            net_stats,
            stats_file,
            &session_commands,
            &zero_copy,
            gamepads,
            &decoder_idr_tx,
            emulate_gamepads,
        )
    });

    info!("Renderer closed");
    video_decoder_session.stop().ok();
    audio_decoder_session.stop().ok();
    render_result
}
