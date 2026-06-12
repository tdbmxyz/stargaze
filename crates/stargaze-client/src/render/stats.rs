//! On-screen stats overlay and session report (Moonlight-style).
//!
//! The overlay collects per-frame [`FrameStats`] into a rolling window,
//! formats them twice a second, and rasterizes the text with an embedded
//! 8x8 bitmap font (no `SDL_ttf` dependency). The recorder accumulates
//! every decoded frame for the whole session and can write a summary
//! report (avg/min/max/std/worst 5%) for offline analysis.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use font8x8::legacy::BASIC_LEGACY;
use stargaze_core::decode::FrameStats;

use crate::transport::NetStats;

/// Number of recent frames the rolling averages are computed over.
const WINDOW: usize = 120;

/// How often the overlay text is regenerated.
const TEXT_REFRESH: Duration = Duration::from_millis(500);

/// Pixel scale factor for the 8x8 font.
const FONT_SCALE: u32 = 2;

/// Padding around the text block, in screen pixels.
const PADDING: i32 = 8;

/// A frame sample: pipeline timings plus the render-side arrival time.
struct Sample {
    stats: FrameStats,
    rendered_at: Instant,
}

/// Snapshot of [`NetStats`] counters, used to compute rates between
/// overlay refreshes.
#[derive(Clone, Copy)]
struct NetSnapshot {
    bytes: u64,
    frames: u64,
    at: Instant,
}

/// Rolling pipeline statistics and cached overlay text.
pub(super) struct StatsOverlay {
    /// Whether the overlay is currently displayed.
    pub(super) visible: bool,
    samples: VecDeque<Sample>,
    /// Frames that arrived but were superseded before presentation.
    dropped: u64,
    /// Total frames presented.
    rendered: u64,
    /// Cached formatted text and when it was last refreshed.
    text: String,
    text_refreshed: Instant,
    /// Counter snapshot from the previous refresh, for rate computation.
    last_net: Option<NetSnapshot>,
}

impl StatsOverlay {
    pub(super) fn new() -> Self {
        Self {
            visible: false,
            samples: VecDeque::with_capacity(WINDOW),
            dropped: 0,
            rendered: 0,
            text: String::new(),
            text_refreshed: Instant::now(),
            last_net: None,
        }
    }

    /// Records a presented frame.
    pub(super) fn on_frame_rendered(&mut self, stats: FrameStats) {
        self.rendered += 1;
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample {
            stats,
            rendered_at: Instant::now(),
        });
    }

    /// Records frames that were decoded but superseded before presentation.
    pub(super) fn on_frames_dropped(&mut self, count: u64) {
        self.dropped += count;
    }

    /// Total frames presented so far.
    pub(super) fn rendered(&self) -> u64 {
        self.rendered
    }

    /// Total decoded frames superseded before presentation.
    pub(super) fn superseded(&self) -> u64 {
        self.dropped
    }

    /// Returns the overlay text, refreshing it at most every 500 ms.
    ///
    /// `rtt` is the current QUIC round-trip estimate, `video` is the
    /// session geometry string (e.g. "3440x1440"), `net` the shared
    /// receiver-side counters.
    pub(super) fn text(&mut self, rtt: Duration, video: &str, net: &NetStats) -> &str {
        if self.text.is_empty() || self.text_refreshed.elapsed() >= TEXT_REFRESH {
            let snapshot = NetSnapshot {
                bytes: net.video_bytes.load(Ordering::Relaxed),
                frames: net.video_frames.load(Ordering::Relaxed),
                at: Instant::now(),
            };
            let net_dropped = net.video_dropped.load(Ordering::Relaxed);
            self.text = self.format_text(rtt, video, snapshot, net_dropped);
            self.last_net = Some(snapshot);
            self.text_refreshed = Instant::now();
        }
        &self.text
    }

    // Sample window is bounded (≤ WINDOW = 120), precision loss is impossible.
    #[allow(clippy::cast_precision_loss)]
    fn format_text(
        &self,
        rtt: Duration,
        video: &str,
        net_now: NetSnapshot,
        net_dropped: u64,
    ) -> String {
        let n = self.samples.len().max(1) as f64;
        let avg = |f: fn(&FrameStats) -> u32| -> f64 {
            self.samples
                .iter()
                .map(|s| f64::from(f(&s.stats)))
                .sum::<f64>()
                / n
                / 1000.0
        };

        let capture_ms = avg(|s| s.capture_us);
        let convert_ms = avg(|s| s.convert_us);
        let encode_ms = avg(|s| s.encode_us);
        let queue_ms = avg(|s| s.queue_us);
        let decode_ms = avg(|s| s.decode_us);

        // Receive rate from the network counters (counts every complete
        // frame on the wire, including ones dropped before decode).
        let (recv_fps, mbps) = match self.last_net {
            Some(prev) => {
                let span = net_now.at.duration_since(prev.at).as_secs_f64();
                if span > 0.0 {
                    (
                        (net_now.frames - prev.frames) as f64 / span,
                        (net_now.bytes - prev.bytes) as f64 * 8.0 / span / 1_000_000.0,
                    )
                } else {
                    (0.0, 0.0)
                }
            }
            None => (0.0, 0.0),
        };

        // Render rate from the rolling window of presented frames.
        let render_fps = match (self.samples.front(), self.samples.back()) {
            (Some(first), Some(last)) if self.samples.len() >= 2 => {
                let span = last
                    .rendered_at
                    .duration_since(first.rendered_at)
                    .as_secs_f64();
                if span > 0.0 {
                    (self.samples.len() - 1) as f64 / span
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };

        format!(
            "VIDEO   {video} (recv {recv_fps:.1} fps / render {render_fps:.1} fps)\n\
             BITRATE {mbps:.1} Mbps\n\
             HOST    capture {capture_ms:.1} / convert {convert_ms:.1} / encode {encode_ms:.1} ms\n\
             NETWORK rtt {:.1} ms / {net_dropped} dropped\n\
             CLIENT  queue {queue_ms:.1} ms / decode {decode_ms:.1} ms\n\
             FRAMES  {} rendered / {} superseded",
            rtt.as_secs_f64() * 1000.0,
            self.rendered,
            self.dropped,
        )
    }
}

/// Summary statistics for one metric over the whole session.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Summary {
    avg: f64,
    min: f64,
    max: f64,
    std: f64,
    /// Mean of the worst 5% of samples (highest for latencies,
    /// lowest for rates — controlled by the caller).
    worst5: f64,
}

/// Computes avg/min/max/std and the worst-5% mean for `values`.
///
/// `worst_is_high` selects which tail is "worst": `true` for latencies
/// (high is bad), `false` for rates like fps (low is bad).
fn summarize(values: &[f64], worst_is_high: bool) -> Option<Summary> {
    if values.is_empty() {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = values.len() as f64;
    let avg = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - avg).powi(2)).sum::<f64>() / n;

    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let tail = values.len().div_ceil(20); // 5%, at least 1 sample
    let worst: &[f64] = if worst_is_high {
        &sorted[sorted.len() - tail..]
    } else {
        &sorted[..tail]
    };
    #[allow(clippy::cast_precision_loss)]
    let worst5 = worst.iter().sum::<f64>() / worst.len() as f64;

    Some(Summary {
        avg,
        min: sorted[0],
        max: sorted[sorted.len() - 1],
        std: var.sqrt(),
        worst5,
    })
}

/// Session-identifying metadata printed at the top of the report.
pub(super) struct ReportMeta<'a> {
    /// Session geometry string (e.g. "3440x1440").
    pub(super) video: &'a str,
    /// Server command line, sanitized of addresses and ports.
    pub(super) server_command: &'a str,
    /// Client command line, sanitized of addresses and ports.
    pub(super) client_command: &'a str,
}

/// Records every decoded frame for the whole session and produces a
/// text report for offline analysis (`--stats-file`).
pub(super) struct StatsRecorder {
    samples: Vec<FrameStats>,
    /// Arrival time of each decoded frame (for instantaneous fps).
    arrivals: Vec<Instant>,
    started: Instant,
}

impl StatsRecorder {
    pub(super) fn new() -> Self {
        Self {
            samples: Vec::new(),
            arrivals: Vec::new(),
            started: Instant::now(),
        }
    }

    /// Records one decoded frame (rendered or superseded).
    pub(super) fn record(&mut self, stats: FrameStats) {
        self.record_at(stats, Instant::now());
    }

    fn record_at(&mut self, stats: FrameStats, at: Instant) {
        self.samples.push(stats);
        self.arrivals.push(at);
    }

    /// Writes the session report to `path`.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the file cannot be written.
    pub(super) fn write_report(
        &self,
        path: &Path,
        meta: &ReportMeta<'_>,
        rendered: u64,
        superseded: u64,
        net: &NetStats,
    ) -> std::io::Result<()> {
        let text = self.report_text(meta, rendered, superseded, net);
        std::fs::write(path, text)
    }

    #[allow(clippy::cast_precision_loss)]
    fn report_text(
        &self,
        meta: &ReportMeta<'_>,
        rendered: u64,
        superseded: u64,
        net: &NetStats,
    ) -> String {
        let duration = self.started.elapsed().as_secs_f64();
        let decoded = self.samples.len();
        let net_bytes = net.video_bytes.load(Ordering::Relaxed);
        let net_frames = net.video_frames.load(Ordering::Relaxed);
        let net_dropped = net.video_dropped.load(Ordering::Relaxed);

        let mut out = String::new();
        let _ = writeln!(out, "Stargaze client session report");
        let _ = writeln!(out, "==============================");
        let _ = writeln!(out, "video:             {}", meta.video);
        let _ = writeln!(out, "server command:    {}", meta.server_command);
        let _ = writeln!(out, "client command:    {}", meta.client_command);
        let _ = writeln!(out, "duration:          {duration:.1} s");
        if duration > 0.0 {
            let _ = writeln!(
                out,
                "frames received:   {net_frames} ({:.1} fps on the wire)",
                net_frames as f64 / duration
            );
            let _ = writeln!(
                out,
                "frames decoded:    {decoded} ({:.1} fps)",
                decoded as f64 / duration
            );
            let _ = writeln!(
                out,
                "avg bitrate:       {:.2} Mbps",
                net_bytes as f64 * 8.0 / duration / 1_000_000.0
            );
        }
        let _ = writeln!(out, "frames rendered:   {rendered}");
        let _ = writeln!(out, "dropped (decoder backpressure): {net_dropped}");
        let _ = writeln!(out, "superseded (render kept newer): {superseded}");
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "{:<14} {:>9} {:>9} {:>9} {:>9} {:>9}",
            "metric", "avg", "min", "max", "std", "worst 5%"
        );

        let mut row = |name: &str, values: &[f64], worst_is_high: bool| {
            if let Some(s) = summarize(values, worst_is_high) {
                let _ = writeln!(
                    out,
                    "{name:<14} {:>9.2} {:>9.2} {:>9.2} {:>9.2} {:>9.2}",
                    s.avg, s.min, s.max, s.std, s.worst5
                );
            }
        };

        let ms = |f: fn(&FrameStats) -> u32| -> Vec<f64> {
            self.samples
                .iter()
                .map(|s| f64::from(f(s)) / 1000.0)
                .collect()
        };
        row("capture ms", &ms(|s| s.capture_us), true);
        row("convert ms", &ms(|s| s.convert_us), true);
        row("encode ms", &ms(|s| s.encode_us), true);
        row("queue ms", &ms(|s| s.queue_us), true);
        row("decode ms", &ms(|s| s.decode_us), true);
        let sizes: Vec<f64> = self
            .samples
            .iter()
            .map(|s| f64::from(s.packet_bytes) / 1024.0)
            .collect();
        row("frame KiB", &sizes, true);

        // Frames per wall-clock second. Bucketing is robust against the
        // bursty arrival pattern of the render loop (instantaneous 1/dt
        // explodes when frames drain back-to-back). For fps the *low*
        // tail is the worst one.
        row("fps (per sec)", &self.fps_buckets(), false);

        out
    }

    /// Decoded-frame counts per whole elapsed second. The trailing
    /// partial second is discarded so it doesn't read as a low outlier.
    fn fps_buckets(&self) -> Vec<f64> {
        let Some(first) = self.arrivals.first() else {
            return Vec::new();
        };
        let total_secs = self
            .arrivals
            .last()
            .map_or(0.0, |last| last.duration_since(*first).as_secs_f64());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let buckets = total_secs.floor() as usize;
        if buckets == 0 {
            return Vec::new();
        }
        let mut counts = vec![0u32; buckets];
        for t in &self.arrivals {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let idx = t.duration_since(*first).as_secs_f64().floor() as usize;
            if idx < buckets {
                counts[idx] += 1;
            }
        }
        counts.into_iter().map(f64::from).collect()
    }
}

/// Rasterizes overlay text into a tightly packed RGBA8 image with the
/// translucent background baked in — the GL renderer uploads this as a
/// texture. Returns `(pixels, width, height)`.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn rasterize_overlay(text: &str) -> (Vec<u8>, u32, u32) {
    const BG: [u8; 4] = [0, 0, 0, 160];
    const FG: [u8; 4] = [220, 220, 220, 255];

    let scale = FONT_SCALE as usize;
    let glyph = 8 * scale;
    let line_height = glyph + 2;
    let pad = PADDING.unsigned_abs() as usize;
    let lines: Vec<&str> = text.lines().collect();
    let widest = lines.iter().map(|l| l.len()).max().unwrap_or(0);
    let width = widest * glyph + 2 * pad;
    let height = lines.len() * line_height + 2 * pad;

    let mut pixels = vec![0u8; width * height * 4];
    for px in pixels.chunks_exact_mut(4) {
        px.copy_from_slice(&BG);
    }
    for (row, line) in lines.iter().enumerate() {
        let y0 = pad + row * line_height;
        for (col, ch) in line.chars().enumerate() {
            let bitmap = BASIC_LEGACY.get(ch as usize).unwrap_or(&BASIC_LEGACY[0]);
            let x0 = pad + col * glyph;
            for (py, bits) in bitmap.iter().enumerate() {
                for px in 0..8usize {
                    if bits & (1 << px) != 0 {
                        for sy in 0..scale {
                            let y = y0 + py * scale + sy;
                            let off = (y * width + x0 + px * scale) * 4;
                            for sx in 0..scale {
                                pixels[off + sx * 4..off + sx * 4 + 4].copy_from_slice(&FG);
                            }
                        }
                    }
                }
            }
        }
    }
    (pixels, width as u32, height as u32)
}

/// Draws multi-line text in the top-left corner with a translucent
/// background, using the embedded 8x8 font scaled by [`FONT_SCALE`].
// Geometry casts are bounded by line length and the 8x8 glyph grid.
#[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
pub(super) fn draw_overlay(
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    text: &str,
) -> Result<(), String> {
    let glyph = 8 * FONT_SCALE as i32;
    let line_height = glyph + 2;
    let lines: Vec<&str> = text.lines().collect();
    let widest = lines.iter().map(|l| l.len()).max().unwrap_or(0) as i32;

    let bg_w = (widest * glyph + 2 * PADDING).cast_unsigned();
    let bg_h = (lines.len() as i32 * line_height + 2 * PADDING).cast_unsigned();

    canvas.set_blend_mode(sdl2::render::BlendMode::Blend);
    canvas.set_draw_color(sdl2::pixels::Color::RGBA(0, 0, 0, 160));
    canvas.fill_rect(sdl2::rect::Rect::new(0, 0, bg_w, bg_h))?;

    // Collect all set pixels as rects and draw them in one call.
    let mut rects = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        let y0 = PADDING + row as i32 * line_height;
        for (col, ch) in line.chars().enumerate() {
            let bitmap = BASIC_LEGACY.get(ch as usize).unwrap_or(&BASIC_LEGACY[0]);
            let x0 = PADDING + col as i32 * glyph;
            for (py, bits) in bitmap.iter().enumerate() {
                for px in 0..8 {
                    if bits & (1 << px) != 0 {
                        rects.push(sdl2::rect::Rect::new(
                            x0 + px * FONT_SCALE as i32,
                            y0 + py as i32 * FONT_SCALE as i32,
                            FONT_SCALE,
                            FONT_SCALE,
                        ));
                    }
                }
            }
        }
    }
    canvas.set_draw_color(sdl2::pixels::Color::RGB(220, 220, 220));
    canvas.fill_rects(&rects)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(capture_us: u32, encode_us: u32, queue_us: u32, decode_us: u32) -> FrameStats {
        FrameStats {
            capture_us,
            convert_us: 1_000,
            encode_us,
            queue_us,
            decode_us,
            packet_bytes: 50_000,
        }
    }

    fn net(bytes: u64, frames: u64, dropped: u64) -> NetStats {
        let n = NetStats::default();
        n.video_bytes.store(bytes, Ordering::Relaxed);
        n.video_frames.store(frames, Ordering::Relaxed);
        n.video_dropped.store(dropped, Ordering::Relaxed);
        n
    }

    #[test]
    fn overlay_text_contains_all_sections() {
        let mut overlay = StatsOverlay::new();
        for _ in 0..10 {
            overlay.on_frame_rendered(stats(2_000, 3_000, 1_000, 4_000));
        }
        overlay.on_frames_dropped(3);

        let net = net(1_000_000, 60, 7);
        let text = overlay
            .text(Duration::from_millis(5), "1920x1080", &net)
            .to_string();
        assert!(text.contains("VIDEO"), "missing video line: {text}");
        assert!(text.contains("1920x1080"));
        assert!(text.contains("HOST"));
        assert!(text.contains("capture 2.0 / convert 1.0 / encode 3.0 ms"));
        assert!(text.contains("rtt 5.0 ms"));
        assert!(text.contains("7 dropped"));
        assert!(text.contains("queue 1.0 ms"));
        assert!(text.contains("decode 4.0 ms"));
        assert!(text.contains("10 rendered / 3 superseded"));
    }

    #[test]
    fn overlay_window_is_bounded() {
        let mut overlay = StatsOverlay::new();
        for _ in 0..(WINDOW + 50) {
            overlay.on_frame_rendered(stats(1, 1, 1, 1));
        }
        assert_eq!(overlay.samples.len(), WINDOW);
        assert_eq!(overlay.rendered, (WINDOW + 50) as u64);
    }

    #[test]
    fn overlay_text_handles_empty_window() {
        let mut overlay = StatsOverlay::new();
        let n = net(0, 0, 0);
        let text = overlay.text(Duration::ZERO, "0x0", &n).to_string();
        assert!(text.contains("VIDEO"));
        assert!(text.contains("0.0"));
    }

    #[test]
    fn summarize_computes_expected_values() {
        // 20 samples: 1..=19 plus one outlier at 100.
        let mut values: Vec<f64> = (1..=19).map(f64::from).collect();
        values.push(100.0);

        let s = summarize(&values, true).unwrap();
        assert!((s.min - 1.0).abs() < f64::EPSILON);
        assert!((s.max - 100.0).abs() < f64::EPSILON);
        // avg = (1+..+19 + 100)/20 = (190+100)/20 = 14.5
        assert!((s.avg - 14.5).abs() < 1e-9);
        // worst 5% of 20 samples = 1 sample = the outlier.
        assert!((s.worst5 - 100.0).abs() < f64::EPSILON);
        assert!(s.std > 0.0);

        // For rates, the worst tail is the low one.
        let s = summarize(&values, false).unwrap();
        assert!((s.worst5 - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn summarize_empty_returns_none() {
        assert!(summarize(&[], true).is_none());
    }

    #[test]
    fn report_contains_all_metrics() {
        let mut recorder = StatsRecorder::new();
        for i in 0..50u32 {
            recorder.record(stats(2_000 + i, 3_000, 1_000, 4_000));
        }
        let n = net(10_000_000, 60, 5);
        let meta = ReportMeta {
            video: "3440x1440",
            server_command: "stargaze-server --preset p1",
            client_command: "stargaze-client --fullscreen true",
        };
        let report = recorder.report_text(&meta, 45, 5, &n);

        assert!(report.contains("Stargaze client session report"));
        assert!(report.contains("video:             3440x1440"));
        assert!(report.contains("server command:    stargaze-server --preset p1"));
        assert!(report.contains("client command:    stargaze-client --fullscreen true"));
        assert!(report.contains("frames received:   60"));
        assert!(report.contains("frames decoded:    50"));
        assert!(report.contains("frames rendered:   45"));
        assert!(report.contains("dropped (decoder backpressure): 5"));
        assert!(report.contains("capture ms"));
        assert!(report.contains("convert ms"));
        assert!(report.contains("encode ms"));
        assert!(report.contains("queue ms"));
        assert!(report.contains("decode ms"));
        assert!(report.contains("frame KiB"));
        assert!(report.contains("worst 5%"));
    }

    #[test]
    fn fps_buckets_are_robust_to_bursty_arrivals() {
        let mut recorder = StatsRecorder::new();
        let t0 = Instant::now();
        // 3 full seconds at 30 fps, but delivered in bursts: all 30
        // frames of each second arrive within the same millisecond.
        for sec in 0..3u64 {
            for i in 0..30u64 {
                recorder.record_at(
                    stats(1, 1, 1, 1),
                    t0 + Duration::from_secs(sec) + Duration::from_micros(i * 30),
                );
            }
        }
        // A trailing partial second that must be discarded.
        recorder.record_at(stats(1, 1, 1, 1), t0 + Duration::from_secs(3));
        recorder.record_at(stats(1, 1, 1, 1), t0 + Duration::from_millis(3_100));

        let buckets = recorder.fps_buckets();
        assert_eq!(buckets.len(), 3);
        // Each full bucket holds ~30 frames; no million-fps outliers.
        for b in &buckets {
            assert!((29.0..=31.0).contains(b), "bucket out of range: {b}");
        }

        let s = summarize(&buckets, false).unwrap();
        assert!(s.max <= 31.0, "max fps must stay sane, got {}", s.max);
    }

    #[test]
    fn rasterize_overlay_produces_background_and_glyph_pixels() {
        let (pixels, w, h) = rasterize_overlay("AB\nlonger line");
        assert_eq!(pixels.len(), (w * h * 4) as usize);
        assert!(w > 0 && h > 0);
        // Corner is background (translucent black).
        assert_eq!(&pixels[0..4], &[0, 0, 0, 160]);
        // Some glyph pixels exist (opaque light gray).
        assert!(
            pixels.chunks_exact(4).any(|px| px == [220, 220, 220, 255]),
            "no foreground pixels rasterized"
        );
    }

    #[test]
    fn report_write_to_file() {
        let mut recorder = StatsRecorder::new();
        recorder.record(stats(1, 2, 3, 4));
        let n = net(100, 1, 0);

        let dir = std::env::temp_dir().join("stargaze-stats-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.txt");
        let meta = ReportMeta {
            video: "1x1",
            server_command: "stargaze-server",
            client_command: "stargaze-client",
        };
        recorder.write_report(&path, &meta, 1, 0, &n).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("session report"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
