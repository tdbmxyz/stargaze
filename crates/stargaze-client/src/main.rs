use anyhow::bail;
use clap::Parser;
use stargaze_core::config::{self, ClientConfig};
use tracing::info;
use tracing_subscriber::EnvFilter;

pub mod transport;

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

    /// Path to config file (default: ~/.config/stargaze/client.toml).
    #[arg(long)]
    config: Option<String>,
}

/// Initializes the tracing subscriber with an env filter.
///
/// Uses the `RUST_LOG` environment variable if set, otherwise defaults to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Builds the final [`ClientConfig`] by loading from file and applying CLI overrides.
///
/// Config resolution order:
/// 1. If `--config` is provided, load from that path.
/// 2. Otherwise, if the default config file exists, load from it.
/// 3. If no file is found, use [`ClientConfig::default()`].
/// 4. Any CLI arguments that are `Some` override the loaded config values.
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

    if cfg.server_address.is_empty() {
        bail!("Server address is required — pass --server <address> or set it in a config file");
    }

    Ok(cfg)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = build_config(&cli)?;

    info!(
        "Connecting to {}:{} (fullscreen: {})",
        cfg.server_address, cfg.port, cfg.fullscreen
    );

    // Connect to server.
    // TODO: derive session parameters from ClientConfig instead of hardcoding.
    // The server currently ignores the client's request and uses its own config,
    // but these should match once session negotiation is implemented.
    let session_request = transport::SessionRequest {
        width: 1920,
        height: 1080,
        framerate: 60,
        codec: stargaze_core::config::Codec::H265,
    };

    let (client_transport, mut frames) = transport::connect(&cfg, session_request).await?;

    info!("Connected, receiving frames...");

    // Receive frames until disconnect or Ctrl+C.
    let mut frame_count: u64 = 0;
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else {
                    info!("Frame channel closed");
                    break;
                };
                frame_count += 1;
                if frame.is_keyframe || frame_count % 300 == 1 {
                    info!(
                        frame = frame_count,
                        pts = frame.pts,
                        size = frame.data.len(),
                        keyframe = frame.is_keyframe,
                        "Received frame"
                    );
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, disconnecting");
                client_transport.abort();
                break;
            }
        }
    }

    info!(total_frames = frame_count, "Client shutting down");

    Ok(())
}
