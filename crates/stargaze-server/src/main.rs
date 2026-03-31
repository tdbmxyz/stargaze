use clap::Parser;
use stargaze_core::capture::Frame;
use stargaze_core::config::{self, Codec, Resolution, ServerConfig};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod capture;

use capture::CaptureConfig;

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

    let capture_config = CaptureConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
    };

    let (session, mut frames) = capture::start_capture(capture_config).await?;

    info!("Capture started, receiving frames...");

    let mut frame_count: u64 = 0;
    while let Some(frame) = frames.recv().await {
        frame_count += 1;
        match &frame {
            Frame::DmaBuf(info) => {
                if frame_count % 60 == 1 {
                    info!(
                        frame = frame_count,
                        width = info.width,
                        height = info.height,
                        format = %info.format,
                        "DMA-BUF frame"
                    );
                }
            }
            Frame::CpuMapped {
                width,
                height,
                format,
                ..
            } => {
                if frame_count % 60 == 1 {
                    info!(
                        frame = frame_count,
                        width,
                        height,
                        format = %format,
                        "CPU-mapped frame"
                    );
                }
            }
        }
    }

    info!(total_frames = frame_count, "Capture stream ended");
    session.stop()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integration test: runs capture for 3 seconds and verifies frames arrive.
    ///
    /// Requires a running Wayland compositor + `PipeWire`.
    /// Run manually with: `cargo test --package stargaze-server -- --ignored test_capture_receives_frames`
    #[tokio::test]
    #[ignore = "requires running Wayland compositor and PipeWire"]
    async fn test_capture_receives_frames() {
        init_tracing();

        let config = CaptureConfig {
            width: 1920,
            height: 1080,
            framerate: 30,
        };

        let (session, mut frames) = capture::start_capture(config)
            .await
            .expect("capture should start");

        // Receive a few frames (with timeout).
        let mut count = 0u32;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                frame = frames.recv() => {
                    match frame {
                        Some(Frame::DmaBuf(info)) => {
                            assert!(info.width > 0);
                            assert!(info.height > 0);
                            count += 1;
                        }
                        Some(Frame::CpuMapped { width, height, data, .. }) => {
                            assert!(width > 0);
                            assert!(height > 0);
                            assert!(!data.is_empty());

                            // Write first frame to PPM for visual inspection.
                            if count == 0 {
                                write_ppm("/tmp/stargaze_test_frame.ppm", &data, width, height);
                                eprintln!("Wrote test frame to /tmp/stargaze_test_frame.ppm");
                            }
                            count += 1;
                        }
                        None => break,
                    }
                }
                () = &mut timeout => break,
            }
        }

        session.stop().expect("session should stop cleanly");
        assert!(count > 0, "should have received at least one frame");
        eprintln!("Received {count} frames in 3 seconds");
    }

    /// Writes raw BGRA pixel data as a PPM file (converts BGRA to RGB).
    fn write_ppm(path: &str, data: &[u8], width: u32, height: u32) {
        use std::io::Write;

        let mut file = std::fs::File::create(path).expect("create PPM file");
        write!(file, "P6\n{width} {height}\n255\n").expect("write PPM header");

        // Convert BGRA to RGB, writing pixel by pixel.
        for y in 0..height {
            for x in 0..width {
                let offset = ((y * width + x) * 4) as usize;
                if offset + 2 < data.len() {
                    let b = data[offset];
                    let g = data[offset + 1];
                    let r = data[offset + 2];
                    file.write_all(&[r, g, b]).expect("write pixel");
                }
            }
        }
    }
}
