//! Hardware-only capture endurance and source-transition checks.
//! Never run these against the user's desktop: source-change tests mutate outputs.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use stargaze_core::color::ColorDescription;
use stargaze_core::encode::{EncodedPacket, EncoderConfig};

use super::{CaptureConfig, wayland};

fn lab_runtime() -> PathBuf {
    assert_eq!(std::env::var("HDR_CAPTURE_LAB").as_deref(), Ok("1"));
    let runtime = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").expect("private runtime"));
    assert!(runtime.to_string_lossy().starts_with("/tmp/hh."));
    let socket = std::env::var("WAYLAND_DISPLAY").expect("explicit lab socket");
    assert!(
        !socket.contains('/'),
        "use a socket inside the private runtime"
    );
    assert!(runtime.join(socket).exists());
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").expect("private IPC endpoint");
    assert!(!signature.contains('/'));
    assert!(
        runtime
            .join("hypr")
            .join(signature)
            .join("hyprland.lock")
            .exists()
    );
    runtime
}

fn env_number(name: &str, default: u32) -> u32 {
    std::env::var(name).map_or(default, |value| value.parse().expect("numeric test option"))
}

fn resident_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn percentile(values: &mut [u32], percent: usize) -> u32 {
    values.sort_unstable();
    values[(values.len() - 1) * percent / 100]
}

fn timing_json(values: &mut [u32]) -> String {
    format!(
        "{{\"p50\":{},\"p95\":{},\"p99\":{},\"max\":{}}}",
        percentile(values, 50),
        percentile(values, 95),
        percentile(values, 99),
        values[values.len() - 1]
    )
}

/// Requires native fixture, isolated patched compositor and NVIDIA NVENC/NVRTC.
/// Run with `nix develop .#cuda -c cargo test -p stargaze-server
/// wayland_capture_encode_endurance -- --ignored --nocapture --test-threads=1`.
/// `STARGAZE_TEST_SECONDS` (default 60), `STARGAZE_TEST_FPS` (145),
/// `STARGAZE_TEST_SHM=1`, `STARGAZE_TEST_NATIVE=0` (non-fixture content), and
/// `STARGAZE_TEST_REPORT` (JSON path; also saves the final decoded planar frame).
#[tokio::test]
#[ignore = "requires isolated HDR lab and NVIDIA; sustained hardware benchmark"]
#[allow(clippy::too_many_lines)] // One timed scenario; decode/validation is outside its measured interval.
async fn wayland_capture_encode_endurance() {
    let runtime = lab_runtime();
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    let seconds = env_number("STARGAZE_TEST_SECONDS", 60);
    assert!((5..=600).contains(&seconds));
    let rate = env_number("STARGAZE_TEST_FPS", 145);
    let shm = env_number("STARGAZE_TEST_SHM", 0) != 0;
    let native = env_number("STARGAZE_TEST_NATIVE", 1) != 0;
    let config = CaptureConfig {
        wayland_output: Some("Main".into()),
        wayland_framerate: rate,
        wayland_shm: shm,
        ..CaptureConfig::default()
    };
    let (capture, frames, format) = wayland::start(config).unwrap();
    let format = tokio::time::timeout(Duration::from_secs(10), format)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(format.color, ColorDescription::HDR10_RGB);
    let config = EncoderConfig {
        width: format.resolution.width,
        height: format.resolution.height,
        framerate: rate,
        bitrate_mbps: 80,
        color: ColorDescription::HDR10,
        tuning: stargaze_core::config::EncoderTuning::default(),
    };
    let (encoder, mut packets, idr, _bitrate) =
        crate::encode::start_encoder(config, frames).unwrap();
    let start = Instant::now() + Duration::from_secs(3); // Exclude driver/kernel initialization.
    let end = start + Duration::from_secs(u64::from(seconds));
    let mut last_gop = Vec::new();
    let mut queue_us = Vec::new();
    let mut convert_us = Vec::new();
    let mut encode_us = Vec::new();
    let mut previous_pts = None;
    let mut byte_count = 0_u64;
    let mut initial_resources = None;
    let mut resource_samples = Vec::new();
    let mut last_resource_sample = start;
    let mut retained_bytes = 0;
    let mut requested_validation_idr = false;
    let mut retain_packets = false;
    while Instant::now() < end {
        let packet = tokio::time::timeout(Duration::from_secs(5), packets.recv())
            .await
            .unwrap()
            .expect("pipeline stopped");
        if let Some(previous) = previous_pts {
            assert_eq!(packet.pts, previous + 1, "encoder skipped a frame");
        }
        previous_pts = Some(packet.pts);
        if Instant::now() >= start {
            initial_resources.get_or_insert_with(|| (resident_kib(), fd_count()));
            queue_us.push(packet.capture_us);
            convert_us.push(packet.convert_us);
            encode_us.push(packet.encode_us);
            byte_count += u64::try_from(packet.data.len()).unwrap();
            if last_resource_sample.elapsed() >= Duration::from_secs(1) {
                resource_samples.push(format!("{{\"second\":{},\"rss_kib\":{},\"fds\":{},\"retained_packet_bytes\":{retained_bytes}}}", start.elapsed().as_secs(), resident_kib(), fd_count()));
                last_resource_sample = Instant::now();
            }
        }
        // Production deliberately uses an infinite GOP. Retaining everything
        // since its first IDR would make the benchmark itself leak memory.
        // Request ONE recovery IDR near the end, then retain only that suffix
        // for post-measurement software decoding and image comparison.
        if !requested_validation_idr
            && end.saturating_duration_since(Instant::now()) <= Duration::from_secs(2)
        {
            idr.send_modify(|value| *value += 1);
            requested_validation_idr = true;
        } else if requested_validation_idr && packet.is_keyframe {
            last_gop.clear();
            retained_bytes = 0;
            retain_packets = true;
        }
        if retain_packets {
            retained_bytes += packet.data.len();
            last_gop.push(packet);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let final_resources = (resident_kib(), fd_count());
    capture.stop().unwrap();
    // The encoder must not block on a full packet channel while being joined.
    drop(packets);
    encoder.stop().unwrap();
    let (initial_rss, initial_fds) = initial_resources.expect("no measured frames");
    let count = queue_us.len();
    assert!(
        count >= usize::try_from(seconds).unwrap(),
        "capture stalled"
    );
    let report = format!(
        concat!(
            "{{\"width\":{},\"height\":{},\"backend\":\"{}\",\"target_fps\":{},",
            "\"seconds\":{elapsed:.6},\"frames\":{count},\"fps\":{:.3},\"mbps\":{:.3},",
            "\"queue_us\":{},\"convert_us\":{},\"encode_us\":{},",
            "\"rss_kib_start\":{initial_rss},\"rss_kib_end\":{},\"fds_start\":{initial_fds},\"fds_end\":{},\"resources\":[{resources}]}}"
        ),
        format.resolution.width,
        format.resolution.height,
        if shm { "shm" } else { "dmabuf" },
        rate,
        f64::from(u32::try_from(count).unwrap()) / elapsed,
        f64::from(u32::try_from(byte_count / 1024).unwrap()) * 1024.0 * 8.0 / elapsed / 1_000_000.0,
        timing_json(&mut queue_us),
        timing_json(&mut convert_us),
        timing_json(&mut encode_us),
        final_resources.0,
        final_resources.1,
        elapsed = elapsed,
        count = count,
        initial_rss = initial_rss,
        initial_fds = initial_fds,
        resources = resource_samples.join(","),
    );
    println!("ENDURANCE {report}");
    let report_path = std::env::var_os("STARGAZE_TEST_REPORT")
        .map_or_else(|| runtime.join("endurance.json"), PathBuf::from);
    std::fs::write(&report_path, report).unwrap();
    assert!(
        final_resources.1 <= initial_fds + 8,
        "FD count grows over time"
    );
    assert!(
        final_resources.0 <= initial_rss + 128 * 1024,
        "resident-memory increase exceeded 128 MiB budget: {initial_rss} -> {} KiB",
        final_resources.0
    );
    validate_gop(&last_gop, native, &report_path.with_extension("yuv"));
}

fn validate_gop(packets: &[EncodedPacket], native: bool, dump: &Path) {
    use std::io::Write;

    assert!(packets[0].is_keyframe);
    let mut bitstream =
        std::io::BufWriter::new(std::fs::File::create(dump.with_extension("hevc")).unwrap());
    for packet in packets {
        bitstream.write_all(&packet.data).unwrap();
    }
    bitstream.flush().unwrap();
    let codec = ffmpeg_next::decoder::find(ffmpeg_next::codec::Id::HEVC).unwrap();
    let mut decoder = ffmpeg_next::codec::context::Context::new_with_codec(codec)
        .decoder()
        .video()
        .unwrap();
    let mut picture = ffmpeg_next::frame::Video::empty();
    let mut last = None;
    let mut count = 0;
    for packet in packets
        .iter()
        .map(|p| Some(ffmpeg_next::Packet::copy(&p.data)))
        .chain(std::iter::once(None))
    {
        if let Some(packet) = packet {
            decoder.send_packet(&packet).unwrap();
        } else {
            decoder.send_eof().unwrap();
        }
        while decoder.receive_frame(&mut picture).is_ok() {
            assert_eq!(picture.format(), ffmpeg_next::format::Pixel::YUV420P10LE);
            assert_eq!(
                picture.color_primaries(),
                ffmpeg_next::color::Primaries::BT2020
            );
            assert_eq!(
                picture.color_transfer_characteristic(),
                ffmpeg_next::color::TransferCharacteristic::SMPTE2084
            );
            assert_eq!(picture.color_space(), ffmpeg_next::color::Space::BT2020NCL);
            assert_eq!(picture.color_range(), ffmpeg_next::color::Range::MPEG);
            if native {
                let row = &picture.data(0)[32 * picture.stride(0)..][..2048];
                let samples: Vec<_> = row
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| u16::from_le_bytes(*p))
                    .collect();
                assert!(samples[0].abs_diff(64) <= 4);
                assert!(samples[1023].abs_diff(940) <= 4);
                assert!(samples.windows(2).filter(|v| v[0] != v[1]).count() > 300);
            }
            count += 1;
            last = Some(picture.clone());
        }
    }
    assert_eq!(count, packets.len());
    let picture = last.unwrap();
    let mut pixels = Vec::new();
    for plane in 0..3 {
        let width = usize::try_from(picture.plane_width(plane)).unwrap() * 2;
        for row in picture
            .data(plane)
            .chunks(picture.stride(plane))
            .take(usize::try_from(picture.plane_height(plane)).unwrap())
        {
            pixels.extend_from_slice(&row[..width]);
        }
    }
    std::fs::write(dump, pixels).unwrap();
}

fn control(arguments: &[&str]) {
    let output = std::process::Command::new("hyprctl")
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .args(arguments)
        .output()
        .expect("hyprctl");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("error"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Mutates ONLY a validated isolated lab's output: resize, color, then removal.
/// Run alone with `--ignored --nocapture --test-threads=1`; do not run alongside
/// another live capture test. The output/fixture may need recreation afterwards.
#[tokio::test]
#[ignore = "requires private patched compositor; mutates lab output"]
async fn wayland_capture_source_changes() {
    let _runtime = lab_runtime();
    let _ = tracing_subscriber::fmt().with_env_filter("info").try_init();
    for change in ["resize", "color", "remove"] {
        let config = CaptureConfig {
            wayland_output: Some("Main".into()),
            ..CaptureConfig::default()
        };
        let (capture, mut frames, format) = wayland::start(config).unwrap();
        let format = tokio::time::timeout(Duration::from_secs(10), format)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(format.color, ColorDescription::HDR10_RGB);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), frames.recv())
                .await
                .unwrap()
                .is_some()
        );
        match change {
            "resize" => {
                let mode = if format.resolution.width == 1920 {
                    "1280x720@145"
                } else {
                    "1920x1080@145"
                };
                control(&[
                    "eval",
                    &format!("hl.monitor({{output='Main',mode='{mode}',position='0x0',scale=1}})"),
                ]);
            }
            "color" => control(&["eval", "hl.monitor({output='Main',cm='srgb'})"]),
            "remove" => control(&["output", "remove", "Main"]),
            _ => unreachable!(),
        }
        // Already captured old-contract frames may still be queued. No frame
        // with a changed contract may reach the fixed-mode encoder.
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(frame) = frames.recv().await {
                assert_eq!(frame.color, format.color);
                let (width, height) = match frame.frame {
                    stargaze_core::capture::Frame::DmaBuf(info) => (info.width, info.height),
                    stargaze_core::capture::Frame::CpuMapped { width, height, .. } => {
                        (width, height)
                    }
                };
                assert_eq!(width, format.resolution.width);
                assert_eq!(height, format.resolution.height);
            }
        })
        .await
        .expect("capture did not terminate on source change");
        capture.stop().unwrap();
        println!("SOURCE_CHANGE {change}: terminated cleanly");
        if change != "remove" {
            control(&[
                "eval",
                &format!(
                    "hl.monitor({{output='Main',mode='{}x{}@145',position='0x0',scale=1,bitdepth=10,cm='hdr',supports_wide_color=1,supports_hdr=1,sdr_min_luminance=0,sdr_max_luminance=160}})",
                    format.resolution.width, format.resolution.height
                ),
            ]);
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}
