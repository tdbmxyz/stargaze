use anyhow::{anyhow, bail};
use clap::Parser;
use stargaze_core::config::{self, ClientConfig, Codec};
use tracing::info;
use tracing_subscriber::EnvFilter;

use stargaze_client::{session, transport};

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

    /// Forward physical gamepads at the evdev level [default: true].
    ///
    /// - true:  the server clones the real controller (name, vendor/
    ///          product ids, button/axis layout), so the host sees e.g.
    ///          an actual Steam Controller. Devices that cannot be
    ///          opened or grabbed fall back to emulation automatically.
    /// - false: every controller is emulated as an Xbox 360 pad.
    #[arg(long, verbatim_doc_comment)]
    gamepad_passthrough: Option<bool>,

    /// Forward Valve USB controller hardware (e.g. the Steam Controller
    /// dongle) to the server over USB/IP tunneled through the session
    /// connection [default: true].
    ///
    /// The device disappears from this machine for the duration of the
    /// session and the server sees the real USB hardware (required for
    /// Steam to accept a Steam Controller). Needs the sysfs permissions
    /// from the stargaze flake's nixosModules.usb-client.
    #[arg(long, verbatim_doc_comment)]
    usb_forward: Option<bool>,

    /// Requested stream resolution, e.g. 1920x1080 [default: 1920x1080].
    ///
    /// The server may confirm a different resolution (e.g. its display's
    /// native size); the confirmed value is what gets decoded and shown.
    #[arg(long, verbatim_doc_comment)]
    resolution: Option<stargaze_core::config::Resolution>,

    /// Requested stream framerate [default: 60].
    #[arg(long)]
    fps: Option<u32>,

    /// Video codec to request: h265 or av1 [default: h265].
    #[arg(long)]
    codec: Option<Codec>,

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
    if let Some(usb_forward) = cli.usb_forward {
        cfg.usb_forward = usb_forward;
    }
    if let Some(resolution) = cli.resolution {
        cfg.resolution = resolution;
    }
    if let Some(fps) = cli.fps {
        cfg.framerate = fps;
    }
    if let Some(codec) = cli.codec {
        cfg.codec = codec;
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
    let session_request = transport::SessionRequest {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
        codec: cfg.codec,
    };

    let conn = transport::connect(&cfg, session_request).await?;

    // SDL2 must be initialized on the main thread.
    let sdl = sdl2::init().map_err(|e| anyhow!("SDL2 init failed: {e}"))?;

    session::run_session(&sdl, &cfg, conn, cli.stats_file.clone()).await?;

    info!("Client shut down");

    Ok(())
}
