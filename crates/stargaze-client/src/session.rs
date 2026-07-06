//! One streaming session: takes an established connection and runs
//! decoders, input forwarding, and the renderer until the session ends.
//!
//! Extracted from `main` so the launcher UI can run sessions in a loop
//! (menu → session → back to menu). Everything here is re-runnable;
//! once-only process state (tracing, rustls provider, `sdl2::init()`)
//! stays in `main`.

use anyhow::bail;
use stargaze_core::audio::AudioDecoderConfig;
use stargaze_core::config::{self, ClientConfig};
use stargaze_core::decode::DecoderConfig;
use stargaze_core::mic_forward;
use tracing::info;

use crate::transport::ConnectedSession;
use crate::{decode, gamepad, render, usb};

/// Runs a full session on an established connection, returning when the
/// renderer exits (window closed, quit shortcut, or transport death).
///
/// `cfg` carries both the toggles (fullscreen, gamepad passthrough, USB
/// forward, mic forward) and the session quality: `cfg.codec` must be
/// the codec that was requested in the handshake.
///
/// # Errors
///
/// Returns an error if a decoder or the renderer fails to start.
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

    // USB forwarding: tunnel Valve controller hardware to the server
    // over the session connection (Steam needs the real USB device).
    if cfg.usb_forward {
        usb::start(usb_connection);
    } else {
        drop(usb_connection);
    }

    // Use the server-confirmed resolution for decoding and rendering.
    // The server may advertise a different resolution than what the client
    // requested (e.g. 3440x1440 on an ultrawide display).
    let decoder_config = DecoderConfig {
        width: session_params.width,
        height: session_params.height,
        codec: cfg.codec,
    };

    info!(
        "Connected, session: {}x{} @ {}fps, {} Mbps",
        session_params.width,
        session_params.height,
        session_params.framerate,
        session_params.bitrate_mbps
    );

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
    if let Err(e) = std::thread::Builder::new()
        .name("stargaze-input-bridge".into())
        .spawn(move || {
            while let Ok(event) = sdl_input_rx.recv() {
                if transport_input_tx.blocking_send(event).is_err() {
                    break;
                }
            }
        })
    {
        bail!("Failed to spawn input bridge thread: {e}");
    }

    // Gamepads: evdev pass-through (server clones the real device) with
    // automatic per-device fallback to SDL → Xbox 360 emulation. Started
    // before the SDL loop so devices present at startup are grabbed
    // before SDL delivers their hotplug events.
    let gamepads = gamepad::SharedGamepads::new();
    let passthrough = if cfg.gamepad_passthrough {
        Some(gamepad::start_passthrough(
            gamepads.clone(),
            sdl_input_tx.clone(),
        ))
    } else {
        info!("Gamepad pass-through disabled; using Xbox 360 emulation");
        None
    };

    let audio_decoder_config = AudioDecoderConfig {
        sample_rate: 48_000,
        channels: 2,
    };

    // Start the audio decoder thread — sends decoded PCM to a channel.
    let (audio_decoder_session, audio_pcm_rx) =
        decode::start_audio_decoder(audio_decoder_config, audio_frames)?;

    // Start the video decoder thread.
    let (video_decoder_session, decoded_rx, zero_copy) =
        decode::start_decoder(decoder_config.clone(), video_frames, decoder_idr_tx)?;

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
            cfg.fullscreen,
            sdl_input_tx,
            rtt_probe,
            net_stats,
            stats_file,
            &session_commands,
            &zero_copy,
            &gamepads,
        )
    });

    info!("Renderer closed, shutting down session");
    // Release grabbed controllers so whatever runs next (the launcher
    // menu, the desktop) sees them again.
    if let Some(handle) = passthrough {
        handle.stop();
    }
    if let Some(ref mut child) = rsonance_child {
        mic_forward::stop_rsonance(child).await;
    }
    video_decoder_session.stop().ok();
    audio_decoder_session.stop().ok();
    client_transport.abort();

    render_result?;
    info!("Session ended");
    Ok(())
}
