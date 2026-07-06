//! Text rendering for the launcher UI: fontdue rasterization of an
//! embedded DejaVu Sans, cached as per-glyph SDL textures.
//!
//! Glyphs are rasterized once per (character, pixel size) into white
//! ARGB textures whose alpha channel carries the coverage, then tinted
//! with `set_color_mod` at draw time — one cached texture serves every
//! text color.
//!
//! Lifetime invariant: with the `unsafe_textures` feature, textures are
//! only freed when their renderer is destroyed. A [`TextRenderer`] must
//! therefore live and die with the `Canvas` it draws to — never keep
//! one across a window/canvas recreation.

use std::collections::HashMap;

use anyhow::anyhow;
use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::Rect;
use sdl2::render::{BlendMode, Canvas, Texture, TextureCreator};
use sdl2::video::{Window, WindowContext};

/// DejaVu Sans, embedded so the AppImage needs no font on the host.
/// License: assets/fonts/LICENSE-DejaVu (Bitstream Vera derivative).
const FONT_BYTES: &[u8] = include_bytes!("../../../../assets/fonts/DejaVuSans.ttf");

struct Glyph {
    /// `None` for glyphs with no coverage (spaces).
    texture: Option<Texture>,
    width: u32,
    height: u32,
    /// Offset from the pen position to the bitmap's top-left corner.
    xmin: i32,
    ymin: i32,
    advance: f32,
}

/// Rasterizes and draws text on an SDL canvas. See the module docs for
/// the cache lifetime invariant.
pub struct TextRenderer {
    font: fontdue::Font,
    cache: HashMap<(char, u16), Glyph>,
}

impl TextRenderer {
    /// # Errors
    ///
    /// Returns an error if the embedded font fails to parse (build
    /// defect; cannot happen at runtime with a committed asset).
    pub fn new() -> anyhow::Result<Self> {
        let font = fontdue::Font::from_bytes(FONT_BYTES, fontdue::FontSettings::default())
            .map_err(|e| anyhow!("embedded font failed to parse: {e}"))?;
        Ok(Self {
            font,
            cache: HashMap::new(),
        })
    }

    /// Width and height of `text` at `px` pixels.
    pub fn measure(&mut self, text: &str, px: u16) -> (u32, u32) {
        let mut width = 0.0f32;
        for ch in text.chars() {
            let metrics = self.font.metrics(ch, f32::from(px));
            width += metrics.advance_width;
        }
        (width.ceil() as u32, self.line_height(px))
    }

    /// Line height (ascent to descent) at `px` pixels.
    pub fn line_height(&self, px: u16) -> u32 {
        let metrics = self
            .font
            .horizontal_line_metrics(f32::from(px))
            .expect("horizontal font has line metrics");
        metrics.new_line_size.ceil() as u32
    }

    /// Draws `text` with its top-left corner at (x, y). Returns the
    /// x-advance of the drawn text in pixels.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        canvas: &mut Canvas<Window>,
        texture_creator: &TextureCreator<WindowContext>,
        text: &str,
        x: i32,
        y: i32,
        px: u16,
        color: Color,
    ) -> i32 {
        let ascent = self
            .font
            .horizontal_line_metrics(f32::from(px))
            .expect("horizontal font has line metrics")
            .ascent;
        let baseline = y + ascent.ceil() as i32;

        let mut pen = x as f32;
        for ch in text.chars() {
            let glyph = self.glyph(texture_creator, ch, px);
            // Borrow dance: pull the fields we need, then re-borrow the
            // texture mutably for the tint.
            let (w, h, xmin, ymin, advance) = (
                glyph.width,
                glyph.height,
                glyph.xmin,
                glyph.ymin,
                glyph.advance,
            );
            if let Some(texture) = self
                .cache
                .get_mut(&(ch, px))
                .and_then(|g| g.texture.as_mut())
            {
                texture.set_color_mod(color.r, color.g, color.b);
                texture.set_alpha_mod(color.a);
                let dst = Rect::new(pen.round() as i32 + xmin, baseline - ymin - h as i32, w, h);
                let _ = canvas.copy(texture, None, dst);
            }
            pen += advance;
        }
        (pen - x as f32).round() as i32
    }

    fn glyph(
        &mut self,
        texture_creator: &TextureCreator<WindowContext>,
        ch: char,
        px: u16,
    ) -> &Glyph {
        self.cache.entry((ch, px)).or_insert_with(|| {
            let (metrics, coverage) = self.font.rasterize(ch, f32::from(px));
            let texture = if metrics.width == 0 || metrics.height == 0 {
                None
            } else {
                texture_creator
                    .create_texture_static_from_pixels(metrics, &coverage)
                    .ok()
            };
            Glyph {
                texture,
                width: metrics.width as u32,
                height: metrics.height as u32,
                xmin: metrics.xmin,
                ymin: metrics.ymin,
                advance: metrics.advance_width,
            }
        })
    }
}

/// Builds a white ARGB texture whose alpha is the fontdue coverage.
trait GlyphTexture {
    fn create_texture_static_from_pixels(
        &self,
        metrics: fontdue::Metrics,
        coverage: &[u8],
    ) -> Result<Texture, String>;
}

impl GlyphTexture for TextureCreator<WindowContext> {
    fn create_texture_static_from_pixels(
        &self,
        metrics: fontdue::Metrics,
        coverage: &[u8],
    ) -> Result<Texture, String> {
        let mut pixels = Vec::with_capacity(coverage.len() * 4);
        for &alpha in coverage {
            pixels.extend_from_slice(&[255, 255, 255, alpha]);
        }
        let mut texture = self
            .create_texture_static(
                PixelFormatEnum::ABGR8888,
                metrics.width as u32,
                metrics.height as u32,
            )
            .map_err(|e| e.to_string())?;
        texture
            .update(None, &pixels, metrics.width * 4)
            .map_err(|e| e.to_string())?;
        texture.set_blend_mode(BlendMode::Blend);
        Ok(texture)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_font_parses() {
        let renderer = TextRenderer::new();
        assert!(renderer.is_ok());
    }

    #[test]
    fn measure_scales_with_size_and_length() {
        let mut renderer = TextRenderer::new().unwrap();
        let (w_small, h_small) = renderer.measure("Stargaze", 16);
        let (w_large, h_large) = renderer.measure("Stargaze", 32);
        assert!(w_large > w_small);
        assert!(h_large > h_small);
        let (w_longer, _) = renderer.measure("Stargaze — client", 16);
        assert!(w_longer > w_small);
        assert_eq!(renderer.measure("", 16).0, 0);
    }
}
