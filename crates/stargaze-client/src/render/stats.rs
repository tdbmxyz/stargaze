//! On-screen stats overlay (Moonlight-style), drawn in the top-left corner.
//!
//! Collects per-frame [`FrameStats`] into a rolling window, formats them
//! into text twice a second, and rasterizes the text with an embedded
//! 8x8 bitmap font (no `SDL_ttf` dependency).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use font8x8::legacy::BASIC_LEGACY;
use stargaze_core::decode::FrameStats;

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

    /// Returns the overlay text, refreshing it at most every 500 ms.
    ///
    /// `rtt` is the current QUIC round-trip estimate, `video` is the
    /// session geometry string (e.g. "3440x1440 @ 60").
    pub(super) fn text(&mut self, rtt: Duration, video: &str) -> &str {
        if self.text.is_empty() || self.text_refreshed.elapsed() >= TEXT_REFRESH {
            self.text = self.format_text(rtt, video);
            self.text_refreshed = Instant::now();
        }
        &self.text
    }

    // Sample window is bounded (≤ WINDOW = 120), precision loss is impossible.
    #[allow(clippy::cast_precision_loss)]
    fn format_text(&self, rtt: Duration, video: &str) -> String {
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
        let encode_ms = avg(|s| s.encode_us);
        let queue_ms = avg(|s| s.queue_us);
        let decode_ms = avg(|s| s.decode_us);

        // FPS and bitrate over the sample window's real elapsed time.
        let (fps, mbps) = match (self.samples.front(), self.samples.back()) {
            (Some(first), Some(last)) if self.samples.len() >= 2 => {
                let span = last
                    .rendered_at
                    .duration_since(first.rendered_at)
                    .as_secs_f64();
                if span > 0.0 {
                    let frames = (self.samples.len() - 1) as f64;
                    let bytes: f64 = self
                        .samples
                        .iter()
                        .map(|s| f64::from(s.stats.packet_bytes))
                        .sum();
                    (frames / span, bytes * 8.0 / span / 1_000_000.0)
                } else {
                    (0.0, 0.0)
                }
            }
            _ => (0.0, 0.0),
        };

        format!(
            "VIDEO   {video} (render {fps:.1} fps)\n\
             BITRATE {mbps:.1} Mbps\n\
             HOST    capture {capture_ms:.1} ms / encode {encode_ms:.1} ms\n\
             NETWORK rtt {:.1} ms\n\
             CLIENT  queue {queue_ms:.1} ms / decode {decode_ms:.1} ms\n\
             FRAMES  {} rendered / {} dropped",
            rtt.as_secs_f64() * 1000.0,
            self.rendered,
            self.dropped,
        )
    }
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
            encode_us,
            queue_us,
            decode_us,
            packet_bytes: 50_000,
        }
    }

    #[test]
    fn overlay_text_contains_all_sections() {
        let mut overlay = StatsOverlay::new();
        for _ in 0..10 {
            overlay.on_frame_rendered(stats(2_000, 3_000, 1_000, 4_000));
        }
        overlay.on_frames_dropped(3);

        let text = overlay
            .text(Duration::from_millis(5), "1920x1080 @ 60")
            .to_string();
        assert!(text.contains("VIDEO"), "missing video line: {text}");
        assert!(text.contains("1920x1080 @ 60"));
        assert!(text.contains("HOST"));
        assert!(text.contains("capture 2.0 ms"));
        assert!(text.contains("encode 3.0 ms"));
        assert!(text.contains("rtt 5.0 ms"));
        assert!(text.contains("queue 1.0 ms"));
        assert!(text.contains("decode 4.0 ms"));
        assert!(text.contains("10 rendered / 3 dropped"));
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
        let text = overlay.text(Duration::ZERO, "0x0 @ 0").to_string();
        assert!(text.contains("VIDEO"));
        assert!(text.contains("0.0"));
    }
}
