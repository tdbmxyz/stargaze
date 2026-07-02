use anyhow::{anyhow, bail};
use clap::Parser;
use stargaze_core::audio::AudioDecoderConfig;
use stargaze_core::config::{self, ClientConfig, Codec};
use stargaze_core::decode::DecoderConfig;
use stargaze_core::mic_forward;
use tracing::info;
use tracing_subscriber::EnvFilter;

use stargaze_client::{decode, gamepad, hidpass, render, transport};

/// Stargaze streaming client — connects to a server, decodes video/audio, and forwards input.
// Doc comments here are clap help text rendered verbatim; list items align
// continuation lines for terminal readability, not rustdoc conventions.
#[allow(clippy::doc_overindented_list_items)]
#[derive(Parser, Debug)]
#[command(name = "stargaze-client", version, about)]
struct Cli {
    /// Address of the server to connect to.
    #[arg(long)]
    server: Option<String>,

    /// Port to connect on [default: 9000].
    #[arg(long)]
    port: Option<u16>,

    /// Whether to run in fullscreen mode [default: true].
    ///
    /// - true:  borderless fullscreen on the client display.
    /// - false: resizable window — handy alongside other work, or when
    ///          the stream resolution differs from the local display.
    #[arg(long, verbatim_doc_comment)]
    fullscreen: Option<bool>,

    /// Enable microphone forwarding via rsonance.
    ///
    /// Spawns `rsonance transmitter` next to the client so the local
    /// microphone is forwarded to a virtual `PulseAudio` source on the
    /// server. Requires the rsonance binary on both ends.
    #[arg(long)]
    mic_forward: bool,

    /// Port for rsonance mic forwarding [default: 9001].
    #[arg(long)]
    mic_forward_port: Option<u16>,

    /// Forward physical gamepads to the server [default: true].
    ///
    /// - true:  Valve controllers (Steam Controller, Steam Deck) are
    ///          forwarded at the HID level — the host rebuilds the real
    ///          device via uhid and Steam Input drives it natively.
    ///          Other pads are cloned at the evdev level. Devices that
    ///          cannot be claimed fall back to Xbox 360 emulation.
    /// - false: every controller is emulated as an Xbox 360 pad.
    #[arg(long, verbatim_doc_comment)]
    gamepad_passthrough: Option<bool>,

    /// Periodically log pipeline progress (received frame counts).
    ///
    /// Off by default: a healthy session would otherwise log a progress
    /// line every few seconds. One-shot lifecycle logs (connect, first
    /// frame, keyframes, decoder events) are always emitted at info level.
    #[arg(long)]
    log_progress: bool,

    /// Path to config file (default: ~/.config/stargaze/client.toml).
    #[arg(long)]
    config: Option<String>,

    /// Write a session stats report to this file on exit.
    ///
    /// The report covers the whole session: frame counts, average
    /// bitrate, per-stage timings (capture/convert/encode/queue/decode)
    /// with avg/min/max/std/worst-5%, and the sanitized server and
    /// client command lines.
    #[arg(long)]
    stats_file: Option<std::path::PathBuf>,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Route FFmpeg's own diagnostics (e.g. HEVC reference errors during
    // loss recovery) through tracing instead of raw stderr.
    stargaze_core::avlog::install_ffmpeg_log_bridge();
}

/// Builds the final [`ClientConfig`] by loading from file and applying CLI overrides.
///
/// # Errors
///
/// Returns an error if the config file exists but cannot be read or parsed,
/// or if the final `server_address` is empty.
fn build_config(cli: &Cli) -> anyhow::Result<ClientConfig> {
    let config_path: Option<String> = if let Some(ref path) = cli.config {
        Some(path.clone())
    } else {
        let default_path = config::config_file_path("client");
        if default_path.exists() {
            default_path.to_str().map(String::from)
        } else {
            None
        }
    };

    let mut cfg: ClientConfig = config::load_config(config_path.as_deref())?;

    if let Some(ref server) = cli.server {
        cfg.server_address.clone_from(server);
    }
    if let Some(port) = cli.port {
        cfg.port = port;
    }
    if let Some(fullscreen) = cli.fullscreen {
        cfg.fullscreen = fullscreen;
    }
    if cli.mic_forward {
        cfg.mic_forward.enabled = true;
    }
    if let Some(port) = cli.mic_forward_port {
        cfg.mic_forward.port = port;
    }
    if let Some(passthrough) = cli.gamepad_passthrough {
        cfg.gamepad_passthrough = passthrough;
    }

    if cfg.server_address.is_empty() {
        bail!("Server address is required — pass --server <address> or set it in a config file");
    }

    Ok(cfg)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install the ring crypto provider for rustls/quinn before any TLS operation.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    init_tracing();

    let cli = Cli::parse();
    let cfg = build_config(&cli)?;
    stargaze_core::logging::set_progress_logging(cli.log_progress);

    info!(
        "Connecting to {}:{} (fullscreen: {})",
        cfg.server_address, cfg.port, cfg.fullscreen
    );

    // Connect to server.
    // TODO: derive session parameters from ClientConfig instead of hardcoding.
    let session_request = transport::SessionRequest {
        width: 1920,
        height: 1080,
        framerate: 60,
        codec: Codec::H265,
    };

    let audio_decoder_config = AudioDecoderConfig {
        sample_rate: 48_000,
        channels: 2,
    };

    let (
        client_transport,
        session_params,
        video_frames,
        audio_frames,
        transport_input_tx,
        decoder_idr_tx,
        rtt_probe,
        net_stats,
        hid_request_rx,
    ) = transport::connect(&cfg, session_request).await?;

    // Use the server-confirmed resolution for decoding and rendering.
    // The server may advertise a different resolution than what the client
    // requested (e.g. 3440x1440 on an ultrawide display).
    let decoder_config = DecoderConfig {
        width: session_params.width,
        height: session_params.height,
        codec: Codec::H265,
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

    // Gamepads, started before SDL init so devices present at startup
    // are claimed before SDL enumerates them:
    // - Valve controllers are forwarded at the HID level (server rebuilds
    //   them via uhid so Steam Input sees the real device);
    // - other pads are passed through at the evdev level;
    // - anything unclaimed falls back to SDL → Xbox 360 emulation.
    let gamepads = gamepad::SharedGamepads::new();
    if cfg.gamepad_passthrough {
        let hid_claimed = hidpass::start(gamepads.clone(), sdl_input_tx.clone(), hid_request_rx);
        if hid_claimed {
            // Keep SDL's HIDAPI drivers off the forwarded devices: they
            // would fight over the hidraw node (mode switches, lizard
            // handling) while we forward its reports.
            if !sdl2::hint::set("SDL_HIDAPI_IGNORE_DEVICES", "0x28de/0x0000") {
                tracing::warn!(
                    "Could not set SDL_HIDAPI_IGNORE_DEVICES; SDL may \
                     interfere with HID pass-through"
                );
            }
        }
        gamepad::start_passthrough(gamepads.clone(), sdl_input_tx.clone());
    } else {
        info!("Gamepad pass-through disabled; using Xbox 360 emulation");
    }

    // SDL2 must be initialized on the main thread.
    let sdl = sdl2::init().map_err(|e| anyhow!("SDL2 init failed: {e}"))?;

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
    tokio::task::block_in_place(|| {
        render::start_renderer(
            &sdl,
            &decoder_config,
            decoded_rx,
            audio_pcm_rx,
            cfg.fullscreen,
            sdl_input_tx,
            rtt_probe,
            net_stats,
            cli.stats_file.clone(),
            &session_commands,
            &zero_copy,
            &gamepads,
        )
    })?;

    info!("Renderer closed, shutting down");
    if let Some(ref mut child) = rsonance_child {
        mic_forward::stop_rsonance(child).await;
    }
    video_decoder_session.stop().ok();
    audio_decoder_session.stop().ok();
    client_transport.abort();

    info!("Client shut down");

    Ok(())
}
