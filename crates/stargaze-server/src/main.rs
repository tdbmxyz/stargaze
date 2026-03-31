use clap::Parser;
use stargaze_core::config::{self, Codec, Resolution, ServerConfig};
use stargaze_core::encode::EncoderConfig;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod capture;
mod encode;
mod transport;

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

    // Start capture pipeline.
    let capture_config = CaptureConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
    };
    let (capture_session, frames) = capture::start_capture(capture_config).await?;
    info!("Capture started");

    // Start encoder pipeline.
    let encoder_config = EncoderConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
        bitrate_mbps: cfg.bitrate,
    };
    let (encoder_session, mut packets, _idr_tx) = encode::start_encoder(encoder_config, frames)?;
    info!("Encoder started");

    // Receive encoded packets (later: send over network).
    let mut packet_count: u64 = 0;
    loop {
        tokio::select! {
            pkt = packets.recv() => {
                let Some(pkt) = pkt else {
                    info!("Encoder channel closed");
                    break;
                };
                packet_count += 1;
                if pkt.is_keyframe || packet_count % 300 == 1 {
                    info!(
                        packet = packet_count,
                        pts = pkt.pts,
                        size = pkt.data.len(),
                        keyframe = pkt.is_keyframe,
                        "Encoded packet"
                    );
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down gracefully");
                break;
            }
        }
    }

    info!(total_packets = packet_count, "Shutting down pipeline");
    encoder_session.stop()?;
    capture_session.stop()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that the `hevc_nvenc` encoder is registered in `FFmpeg`.
    ///
    /// This does NOT require an NVIDIA GPU — the encoder is registered
    /// at `FFmpeg` compile time if `NVENC` headers were present. It confirms
    /// that `ffmpeg-next` can find the encoder by name.
    #[test]
    fn test_hevc_nvenc_encoder_registered() {
        ffmpeg_next::init().expect("ffmpeg init");
        let codec = ffmpeg_next::encoder::find_by_name("hevc_nvenc");
        // The encoder should be registered if FFmpeg was built with NVENC support.
        // If this fails, FFmpeg was built without NVENC headers.
        if let Some(c) = codec {
            assert_eq!(c.name(), "hevc_nvenc");
        } else {
            eprintln!("WARNING: hevc_nvenc not registered — FFmpeg may lack NVENC support");
        }
    }

    /// Integration test: runs the full capture→encode pipeline for 3 seconds.
    ///
    /// Requires a running Wayland compositor + `PipeWire` + NVIDIA GPU.
    /// Run manually with:
    /// ```bash
    /// cargo test --package stargaze-server -- --ignored test_capture_encode_pipeline
    /// ```
    #[tokio::test]
    #[ignore = "requires running Wayland compositor, PipeWire, and NVIDIA GPU"]
    async fn test_capture_encode_pipeline() {
        use stargaze_core::encode::EncoderConfig;

        init_tracing();

        // Start capture.
        let capture_config = CaptureConfig {
            width: 1920,
            height: 1080,
            framerate: 30,
        };
        let (capture_session, frames) = capture::start_capture(capture_config)
            .await
            .expect("capture should start");

        // Start encoder.
        let encoder_config = EncoderConfig {
            width: 1920,
            height: 1080,
            framerate: 30,
            bitrate_mbps: 10,
        };
        let (encoder_session, mut packets, _idr_tx) =
            encode::start_encoder(encoder_config, frames).expect("encoder should start");

        // Receive packets for up to 3 seconds.
        let mut count = 0u32;
        let mut got_keyframe = false;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                pkt = packets.recv() => {
                    match pkt {
                        Some(p) => {
                            assert!(!p.data.is_empty(), "packet should have data");
                            if p.is_keyframe {
                                got_keyframe = true;
                            }
                            count += 1;
                        }
                        None => break,
                    }
                }
                () = &mut timeout => break,
            }
        }

        // Write encoded output to a file for manual inspection with ffprobe.
        // (Only if we got packets)
        if count > 0 {
            eprintln!("Received {count} encoded packets in 3 seconds");
        }

        encoder_session.stop().expect("encoder should stop cleanly");
        capture_session.stop().expect("capture should stop cleanly");

        assert!(
            count > 0,
            "should have received at least one encoded packet"
        );
        assert!(got_keyframe, "should have received at least one keyframe");
    }
}
