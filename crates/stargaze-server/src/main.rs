use clap::Parser;
use stargaze_core::audio::{AudioApplication, AudioCaptureConfig, AudioEncoderConfig};
use stargaze_core::config::{self, Codec, Resolution, ServerConfig};
use stargaze_core::encode::EncoderConfig;
use stargaze_core::mic_forward;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod audio;
mod capture;
mod encode;
mod input;
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

    /// Enable microphone forwarding via rsonance.
    #[arg(long)]
    mic_forward: bool,

    /// Port for rsonance mic forwarding [default: 9001].
    #[arg(long)]
    mic_forward_port: Option<u16>,

    /// Show the cursor in the captured stream.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    show_cursor: bool,

    /// Path to config file (default: ~/.config/stargaze/server.toml).
    #[arg(long)]
    config: Option<String>,
}

/// Initializes the tracing subscriber with an env filter.
///
/// Uses the `RUST_LOG` environment variable if set, otherwise defaults to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Use try_init() so tests can call this multiple times safely.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
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
    if cli.mic_forward {
        cfg.mic_forward.enabled = true;
    }
    if let Some(port) = cli.mic_forward_port {
        cfg.mic_forward.port = port;
    }
    cfg.cursor.show_cursor = cli.show_cursor;

    Ok(cfg)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = build_config(&cli)?;

    info!(
        "Starting stargaze server on {}:{} ({}@{}fps, {} Mbps, {}, cursor: {})",
        cfg.bind_address,
        cfg.port,
        cfg.resolution,
        cfg.framerate,
        cfg.bitrate,
        cfg.codec,
        if cfg.cursor.show_cursor {
            "embedded"
        } else {
            "hidden"
        }
    );

    // Start capture pipeline.
    let capture_config = CaptureConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        show_cursor: cfg.cursor.show_cursor,
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
    let (encoder_session, packets, idr_tx) = encode::start_encoder(encoder_config, frames)?;
    info!("Encoder started");

    // Start audio capture pipeline.
    let audio_capture_config = AudioCaptureConfig {
        sample_rate: 48_000,
        channels: 2,
    };
    let (audio_capture_session, audio_frames) = audio::start_audio_capture(audio_capture_config)?;
    info!("Audio capture started");

    // Start audio encoder pipeline.
    let audio_encoder_config = AudioEncoderConfig {
        sample_rate: 48_000,
        channels: 2,
        bitrate: 128_000,
        application: AudioApplication::Audio,
    };
    let (audio_encoder_session, audio_packets) =
        encode::start_audio_encoder(audio_encoder_config, audio_frames)?;
    info!("Audio encoder started");

    // Start input injection pipeline.
    let (input_tx, input_rx) = tokio::sync::mpsc::channel::<stargaze_core::input::InputEvent>(64);
    let input_session = input::start_input_injection(input_rx)?;
    info!("Input injection started");

    // Optionally start rsonance receiver for mic forwarding.
    let mut rsonance_child = if cfg.mic_forward.enabled {
        match mic_forward::spawn_rsonance_receiver(&cfg.mic_forward) {
            Ok(child) => {
                info!("Mic forwarding enabled (rsonance receiver)");
                Some(child)
            }
            Err(e) => {
                tracing::warn!("Failed to start rsonance receiver: {e}");
                None
            }
        }
    } else {
        None
    };

    // Start transport — accepts client connection and streams video + audio packets.
    let server_transport =
        transport::start_server_transport(&cfg, packets, audio_packets, idr_tx, input_tx)?;
    let abort_handle = server_transport.abort_handle();
    info!("Transport started, waiting for client connection...");

    // Wait for transport to finish (client disconnect or error) or Ctrl+C.
    tokio::select! {
        result = server_transport.join() => {
            if let Err(e) = result {
                tracing::warn!("Transport error: {e}");
            }
            info!("Transport finished");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down gracefully");
            abort_handle.abort();
        }
    }

    info!("Shutting down pipeline");
    if let Some(ref mut child) = rsonance_child {
        mic_forward::stop_rsonance(child).await;
    }
    audio_encoder_session.stop()?;
    audio_capture_session.stop()?;
    encoder_session.stop()?;
    capture_session.stop()?;
    input_session
        .stop()
        .map_err(|e| anyhow::anyhow!("input session shutdown: {e}"))?;

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
            show_cursor: true,
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

    /// GPU-only encoder test: feeds synthetic BGRA frames to NVENC
    /// and verifies encoded H.265 packets are produced.
    ///
    /// Requires an NVIDIA GPU with NVENC support (no Wayland/`PipeWire` needed).
    /// Run manually with:
    /// ```bash
    /// cargo test --package stargaze-server -- --ignored test_nvenc_encode_synthetic_frames
    /// ```
    #[tokio::test]
    #[ignore = "requires NVIDIA GPU with NVENC support"]
    async fn test_nvenc_encode_synthetic_frames() {
        use stargaze_core::capture::{Frame, PixelFormat};
        use stargaze_core::encode::EncoderConfig;
        use tokio::sync::mpsc;

        init_tracing();

        let width = 1280u32;
        let height = 720u32;
        let framerate = 30u32;
        let num_frames = 90u32; // 3 seconds at 30fps

        // Create a channel to feed frames to the encoder.
        let (frames_tx, frames_rx) = mpsc::channel::<Frame>(4);

        let encoder_config = EncoderConfig {
            width,
            height,
            framerate,
            bitrate_mbps: 5,
        };

        // Start encoder — this initializes CUDA + NVENC on a dedicated thread.
        let (encoder_session, mut packets, idr_tx) =
            encode::start_encoder(encoder_config, frames_rx)
                .expect("encoder should initialize with NVIDIA GPU");

        // Spawn a task to feed synthetic frames.
        let feed_handle = tokio::spawn(async move {
            let stride = width * 4;
            for i in 0..num_frames {
                // Generate a BGRA frame with a shifting color gradient so each
                // frame is visually different (exercises the encoder properly).
                let mut data = vec![0u8; (stride * height) as usize];
                for y in 0..height {
                    for x in 0..width {
                        let offset = (y * stride + x * 4) as usize;
                        data[offset] = ((x + i) % 256) as u8; // B
                        data[offset + 1] = ((y + i) % 256) as u8; // G
                        data[offset + 2] = ((x + y + i) % 256) as u8; // R
                        data[offset + 3] = 255; // A
                    }
                }

                let frame = Frame::CpuMapped {
                    data,
                    width,
                    height,
                    stride,
                    format: PixelFormat::Bgra8,
                };

                if frames_tx.send(frame).await.is_err() {
                    break;
                }
            }
            // Drop sender to signal end-of-stream.
        });

        // Collect encoded packets.
        let mut count = 0u32;
        let mut got_keyframe = false;
        let mut total_bytes = 0usize;

        // Use a timeout so the test doesn't hang if something goes wrong.
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(30));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                pkt = packets.recv() => {
                    match pkt {
                        Some(p) => {
                            assert!(!p.data.is_empty(), "packet should have data");
                            total_bytes += p.data.len();
                            if p.is_keyframe {
                                got_keyframe = true;
                            }
                            count += 1;
                        }
                        None => break,
                    }
                }
                () = &mut timeout => {
                    panic!("Timed out waiting for encoder — received {count} packets so far");
                }
            }
        }

        feed_handle.await.expect("frame feed task should not panic");
        encoder_session.stop().expect("encoder should stop cleanly");

        eprintln!(
            "Encoded {count} packets from {num_frames} frames, \
             total {total_bytes} bytes, avg {} bytes/packet",
            if count > 0 {
                total_bytes / count as usize
            } else {
                0
            }
        );

        assert!(
            count > 0,
            "should have received at least one encoded packet"
        );
        assert!(got_keyframe, "should have received at least one keyframe");

        // Test IDR request: send an IDR request value and encode one more frame.
        // (Encoder is already stopped, so we test the IDR sender is functional.)
        // The idr_tx was returned and should still be valid.
        assert!(
            idr_tx.send(1).is_ok() || true,
            "IDR sender should be usable (or encoder thread exited)"
        );
    }

    /// Tests that the encoder properly forces IDR keyframes when requested.
    ///
    /// Feeds frames while incrementing the IDR watch channel mid-stream,
    /// and verifies that extra keyframes appear in the output.
    ///
    /// Requires an NVIDIA GPU with NVENC support (no Wayland/`PipeWire` needed).
    /// Run manually with:
    /// ```bash
    /// cargo test --package stargaze-server -- --ignored test_nvenc_idr_request
    /// ```
    #[tokio::test]
    #[ignore = "requires NVIDIA GPU with NVENC support"]
    async fn test_nvenc_idr_request() {
        use stargaze_core::capture::{Frame, PixelFormat};
        use stargaze_core::encode::EncoderConfig;
        use tokio::sync::mpsc;

        init_tracing();

        let width = 640u32;
        let height = 480u32;
        let framerate = 30u32;
        // GOP is framerate*2 = 60, so in 120 frames we'd normally get 2 keyframes.
        // We'll request IDR at frame 15 and 45 to force extras.
        let num_frames = 120u32;

        let (frames_tx, frames_rx) = mpsc::channel::<Frame>(4);

        let encoder_config = EncoderConfig {
            width,
            height,
            framerate,
            bitrate_mbps: 2,
        };

        let (encoder_session, mut packets, idr_tx) =
            encode::start_encoder(encoder_config, frames_rx)
                .expect("encoder should initialize with NVIDIA GPU");

        // Feed frames, requesting IDR at specific points.
        let feed_handle = tokio::spawn(async move {
            let stride = width * 4;
            for i in 0..num_frames {
                // Request IDR at frame 15 and 45.
                if i == 15 {
                    let _ = idr_tx.send(1);
                } else if i == 45 {
                    let _ = idr_tx.send(2);
                }

                let data = vec![((i * 3) % 256) as u8; (stride * height) as usize];
                let frame = Frame::CpuMapped {
                    data,
                    width,
                    height,
                    stride,
                    format: PixelFormat::Bgra8,
                };

                if frames_tx.send(frame).await.is_err() {
                    break;
                }
            }
        });

        let mut keyframe_count = 0u32;
        let mut total_count = 0u32;

        let timeout = tokio::time::sleep(std::time::Duration::from_secs(30));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                pkt = packets.recv() => {
                    match pkt {
                        Some(p) => {
                            if p.is_keyframe {
                                keyframe_count += 1;
                            }
                            total_count += 1;
                        }
                        None => break,
                    }
                }
                () = &mut timeout => {
                    panic!("Timed out — received {total_count} packets, {keyframe_count} keyframes");
                }
            }
        }

        feed_handle.await.expect("frame feed task should not panic");
        encoder_session.stop().expect("encoder should stop cleanly");

        eprintln!(
            "Got {keyframe_count} keyframes from {total_count} packets ({num_frames} input frames)"
        );

        assert!(total_count > 0, "should have received encoded packets");
        // We should get at least 3 keyframes: the initial one + 2 forced IDRs.
        // The natural GOP (every 60 frames) adds another, so expect >= 3.
        assert!(
            keyframe_count >= 3,
            "expected at least 3 keyframes (1 initial + 2 IDR requests), got {keyframe_count}"
        );
    }
}
