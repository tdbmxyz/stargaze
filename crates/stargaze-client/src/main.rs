use anyhow::{anyhow, bail};
use clap::Parser;
use stargaze_core::audio::AudioDecoderConfig;
use stargaze_core::config::{self, ClientConfig, Codec};
use stargaze_core::decode::DecoderConfig;
use stargaze_core::mic_forward;
use tracing::info;
use tracing_subscriber::EnvFilter;

use stargaze_client::{decode, render, transport};

/// Stargaze streaming client — connects to a server, decodes video/audio, and forwards input.
#[derive(Parser, Debug)]
#[command(name = "stargaze-client", version, about)]
struct Cli {
    /// Address of the server to connect to.
    #[arg(long)]
    server: Option<String>,

    /// Port to connect on.
    #[arg(long)]
    port: Option<u16>,

    /// Whether to run in fullscreen mode.
    #[arg(long)]
    fullscreen: Option<bool>,

    /// Enable microphone forwarding via rsonance.
    #[arg(long)]
    mic_forward: bool,

    /// Port for rsonance mic forwarding [default: 9001].
    #[arg(long)]
    mic_forward_port: Option<u16>,

    /// Path to config file (default: ~/.config/stargaze/client.toml).
    #[arg(long)]
    config: Option<String>,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
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

    let (client_transport, session_params, video_frames, audio_frames, transport_input_tx) =
        transport::connect(&cfg, session_request).await?;

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

    // SDL2 must be initialized on the main thread.
    let sdl = sdl2::init().map_err(|e| anyhow!("SDL2 init failed: {e}"))?;

    // Bridge: SDL event loop (std::sync::mpsc) → tokio channel → transport.
    // The std receiver blocks, so this must run on a blocking thread rather
    // than a tokio worker.
    let (sdl_input_tx, sdl_input_rx) =
        std::sync::mpsc::channel::<stargaze_core::input::InputEvent>();
    let bridge_handle = tokio::task::spawn_blocking(move || {
        while let Ok(event) = sdl_input_rx.recv() {
            if transport_input_tx.blocking_send(event).is_err() {
                break;
            }
        }
    });

    // Start the audio decoder thread — sends decoded PCM to a channel.
    let (audio_decoder_session, audio_pcm_rx) =
        decode::start_audio_decoder(audio_decoder_config, audio_frames)?;

    // Start the video decoder thread.
    let (video_decoder_session, decoded_rx) =
        decode::start_decoder(decoder_config.clone(), video_frames)?;

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
        )
    })?;

    info!("Renderer closed, shutting down");
    bridge_handle.abort();
    if let Some(ref mut child) = rsonance_child {
        mic_forward::stop_rsonance(child).await;
    }
    video_decoder_session.stop().ok();
    audio_decoder_session.stop().ok();
    client_transport.abort();

    info!("Client shut down");

    Ok(())
}
