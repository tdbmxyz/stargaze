use anyhow::bail;
use clap::Parser;
use stargaze_core::config::{self, Codec, ClientConfig};
use stargaze_core::decode::DecoderConfig;
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
        codec: Codec::H265,
    };

    let decoder_config = DecoderConfig {
        width: session_request.width,
        height: session_request.height,
        codec: session_request.codec,
    };

    let (client_transport, frames) = transport::connect(&cfg, session_request).await?;

    info!("Connected, starting decoder and renderer...");

    let (decoder_session, decoded_rx) =
        decode::start_decoder(decoder_config.clone(), frames)?;

    // SDL2 event loop must run on the main OS thread. Use block_in_place
    // to allow blocking within the tokio runtime without starving it.
    tokio::task::block_in_place(|| {
        render::start_renderer(&decoder_config, decoded_rx, cfg.fullscreen)
    })?;

    info!("Renderer closed, shutting down");
    decoder_session.stop().ok();
    client_transport.abort();

    info!("Client shut down");

    Ok(())
}
