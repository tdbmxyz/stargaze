use clap::Parser;
use stargaze_core::config::{self, Codec, Resolution, ServerConfig};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod capture;

/// Stargaze streaming server — captures screen and audio, encodes, and streams to clients.
#[derive(Parser, Debug)]
#[command(name = "stargaze-server", version, about)]
struct Cli {
    /// Address to bind the server to.
    #[arg(long)]
    bind: Option<String>,

    /// Port to listen on.
    #[arg(long)]
    port: Option<u16>,

    /// Video resolution as `WIDTHxHEIGHT` (e.g. 1920x1080).
    #[arg(long)]
    resolution: Option<Resolution>,

    /// Target framerate.
    #[arg(long)]
    framerate: Option<u32>,

    /// Target bitrate in Mbps.
    #[arg(long)]
    bitrate: Option<u32>,

    /// Video codec (h265, av1).
    #[arg(long)]
    codec: Option<Codec>,

    /// Path to config file (default: ~/.config/stargaze/server.toml).
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

/// Builds the final [`ServerConfig`] by loading from file and applying CLI overrides.
///
/// Config resolution order:
/// 1. If `--config` is provided, load from that path.
/// 2. Otherwise, if the default config file exists, load from it.
/// 3. If no file is found, use [`ServerConfig::default()`].
/// 4. Any CLI arguments that are `Some` override the loaded config values.
///
/// # Errors
///
/// Returns an error if the config file exists but cannot be read or parsed.
fn build_config(cli: &Cli) -> anyhow::Result<ServerConfig> {
    let config_path: Option<String> = if let Some(ref path) = cli.config {
        Some(path.clone())
    } else {
        let default_path = config::config_file_path("server");
        if default_path.exists() {
            default_path.to_str().map(String::from)
        } else {
            None
        }
    };

    let mut cfg: ServerConfig = config::load_config(config_path.as_deref())?;

    if let Some(ref bind) = cli.bind {
        cfg.bind_address.clone_from(bind);
    }
    if let Some(port) = cli.port {
        cfg.port = port;
    }
    if let Some(resolution) = cli.resolution {
        cfg.resolution = resolution;
    }
    if let Some(framerate) = cli.framerate {
        cfg.framerate = framerate;
    }
    if let Some(bitrate) = cli.bitrate {
        cfg.bitrate = bitrate;
    }
    if let Some(codec) = cli.codec {
        cfg.codec = codec;
    }

    Ok(cfg)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = build_config(&cli)?;

    info!(
        "Starting stargaze server on {}:{} ({}@{}fps, {} Mbps, {})",
        cfg.bind_address, cfg.port, cfg.resolution, cfg.framerate, cfg.bitrate, cfg.codec
    );

    Ok(())
}
