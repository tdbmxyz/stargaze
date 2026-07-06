//! Widget drawing and focus/navigation primitives for the launcher UI.
//!
//! There is no retained widget tree: each screen draws its widgets
//! every frame from its own state, and layout functions return the
//! widget rectangles so pointer hit-testing and unit tests share the
//! same geometry.
//!
//! Layout happens in a logical 1280x800 space (the Steam Deck panel);
//! drawing maps it to the real window through a [`ViewTransform`] and
//! rasterizes text at the physical pixel size, so glyphs stay crisp on
//! any display instead of being GPU-upscaled from a 1280x800 canvas.

use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, TextureCreator};
use sdl2::video::{Window, WindowContext};

use super::font::TextRenderer;

/// Logical UI width all layout code targets.
pub const UI_WIDTH: u32 = 1280;
/// Logical UI height.
pub const UI_HEIGHT: u32 = 800;

/// UI color palette.
pub mod palette {
    use sdl2::pixels::Color;

    /// Window background.
    pub const BACKGROUND: Color = Color::RGB(16, 18, 26);
    /// Unfocused widget background.
    pub const WIDGET: Color = Color::RGB(32, 36, 48);
    /// Focused widget background.
    pub const WIDGET_FOCUS: Color = Color::RGB(56, 64, 92);
    /// Focused widget outline.
    pub const OUTLINE_FOCUS: Color = Color::RGB(120, 150, 255);
    /// Primary text.
    pub const TEXT: Color = Color::RGB(230, 232, 240);
    /// Secondary/hint text.
    pub const TEXT_DIM: Color = Color::RGB(140, 145, 160);
    /// Enabled-state accent (toggles).
    pub const ACCENT: Color = Color::RGB(110, 200, 130);
    /// Error banner background.
    pub const ERROR_BG: Color = Color::RGB(96, 32, 36);
    /// Error banner text.
    pub const ERROR_TEXT: Color = Color::RGB(255, 200, 200);
}

/// A device-independent navigation event, mapped from SDL keyboard,
/// game controller, and pointer events. Pointer coordinates are in
/// logical space (already mapped through the [`ViewTransform`]).
#[derive(Debug, Clone, PartialEq)]
pub enum NavEvent {
    /// Move focus up.
    Up,
    /// Move focus down.
    Down,
    /// Decrease / previous value (choice spinners), or cursor left.
    Left,
    /// Increase / next value (choice spinners), or cursor right.
    Right,
    /// Activate the focused widget (A / Enter / click).
    Activate,
    /// Leave the screen / cancel (B / Esc).
    Back,
    /// Delete the focused item (Y / Delete).
    Delete,
    /// Edit the focused item (X / E).
    Edit,
    /// Connect to the focused or default host (Start).
    ConnectShortcut,
    /// Text input (physical keyboard or the platform's on-screen one).
    Text(String),
    /// Backspace in a text field.
    Backspace,
    /// Pointer moved to logical coordinates (hover focuses).
    PointerMove(i32, i32),
    /// Pointer pressed at logical coordinates (click activates).
    PointerClick(i32, i32),
}

/// Maps the logical 1280x800 layout space onto the real drawable:
/// uniform scale, letterboxed centering.
#[derive(Debug, Clone, Copy)]
pub struct ViewTransform {
    /// Logical → physical scale factor.
    pub scale: f32,
    /// Horizontal letterbox offset in physical pixels.
    pub offset_x: i32,
    /// Vertical letterbox offset in physical pixels.
    pub offset_y: i32,
}

impl ViewTransform {
    /// Transform for a drawable of `width` x `height` physical pixels.
    #[must_use]
    pub fn for_output(width: u32, height: u32) -> Self {
        let scale = (width as f32 / UI_WIDTH as f32)
            .min(height as f32 / UI_HEIGHT as f32)
            .max(0.1);
        let offset_x = ((width as f32 - UI_WIDTH as f32 * scale) / 2.0) as i32;
        let offset_y = ((height as f32 - UI_HEIGHT as f32 * scale) / 2.0) as i32;
        Self {
            scale,
            offset_x,
            offset_y,
        }
    }

    /// Logical rect → physical rect.
    #[must_use]
    pub fn rect(&self, r: Rect) -> Rect {
        Rect::new(
            self.x(r.x()),
            self.y(r.y()),
            ((r.width() as f32) * self.scale).round() as u32,
            ((r.height() as f32) * self.scale).round() as u32,
        )
    }

    /// Logical x → physical x.
    #[must_use]
    pub fn x(&self, x: i32) -> i32 {
        (x as f32 * self.scale).round() as i32 + self.offset_x
    }

    /// Logical y → physical y.
    #[must_use]
    pub fn y(&self, y: i32) -> i32 {
        (y as f32 * self.scale).round() as i32 + self.offset_y
    }

    /// Logical font size → physical font size (what fontdue rasterizes).
    #[must_use]
    pub fn px(&self, px: u16) -> u16 {
        ((f32::from(px) * self.scale).round() as u16).max(6)
    }

    /// Physical pointer coordinates → logical, for hit-testing.
    #[must_use]
    pub fn pointer_to_logical(&self, x: i32, y: i32) -> (i32, i32) {
        (
            ((x - self.offset_x) as f32 / self.scale).round() as i32,
            ((y - self.offset_y) as f32 / self.scale).round() as i32,
        )
    }
}

/// Focus position within a screen's widget list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusList {
    /// Number of focusable widgets.
    pub count: usize,
    /// Index of the focused widget.
    pub focus: usize,
}

impl FocusList {
    /// A focus list over `count` widgets, focused on the first.
    #[must_use]
    pub fn new(count: usize) -> Self {
        Self { count, focus: 0 }
    }

    /// Moves focus up, saturating at the top.
    pub fn up(&mut self) {
        self.focus = self.focus.saturating_sub(1);
    }

    /// Moves focus down, saturating at the bottom.
    pub fn down(&mut self) {
        if self.count > 0 && self.focus + 1 < self.count {
            self.focus += 1;
        }
    }

    /// Clamps focus after the widget count changed (e.g. host deleted).
    pub fn resize(&mut self, count: usize) {
        self.count = count;
        if count == 0 {
            self.focus = 0;
        } else if self.focus >= count {
            self.focus = count - 1;
        }
    }

    /// Focuses the widget whose rect contains (x, y), if any. Returns
    /// the focused index.
    pub fn focus_at(&mut self, rects: &[Rect], x: i32, y: i32) -> Option<usize> {
        let hit = hit_test(rects, x, y)?;
        if hit < self.count {
            self.focus = hit;
            Some(hit)
        } else {
            None
        }
    }
}

/// Index of the first rect containing (x, y).
#[must_use]
pub fn hit_test(rects: &[Rect], x: i32, y: i32) -> Option<usize> {
    rects
        .iter()
        .position(|r| r.contains_point(sdl2::rect::Point::new(x, y)))
}

/// Lays out `count` full-width rows starting at `top`, `height` tall
/// with `gap` between them, inside horizontal `margin`.
#[must_use]
pub fn layout_rows(count: usize, top: i32, height: u32, gap: u32, margin: i32) -> Vec<Rect> {
    let width = UI_WIDTH as i32 - 2 * margin;
    (0..count)
        .map(|i| {
            Rect::new(
                margin,
                top + i as i32 * (height + gap) as i32,
                width as u32,
                height,
            )
        })
        .collect()
}

/// Shared context for widget drawing. All coordinates passed to its
/// methods are logical; the transform maps them to the drawable.
pub struct Ui<'a> {
    /// Canvas to draw on.
    pub canvas: &'a mut Canvas<Window>,
    /// Texture creator owning glyph textures.
    pub textures: &'a TextureCreator<WindowContext>,
    /// Glyph cache (must belong to `canvas`, see [`TextRenderer`]).
    pub text: &'a mut TextRenderer,
    /// Logical → physical mapping for this frame.
    pub view: ViewTransform,
}

impl Ui<'_> {
    /// Draws `text` at logical (x, y); rasterized at physical size.
    pub fn text(&mut self, s: &str, x: i32, y: i32, px: u16, color: Color) {
        let (px_phys, x_phys, y_phys) = (self.view.px(px), self.view.x(x), self.view.y(y));
        self.text.draw(
            self.canvas,
            self.textures,
            s,
            x_phys,
            y_phys,
            px_phys,
            color,
        );
    }

    /// Draws `text` right-aligned so it ends at logical `right`.
    pub fn text_right(&mut self, s: &str, right: i32, y: i32, px: u16, color: Color) {
        let (px_phys, right_phys, y_phys) = (self.view.px(px), self.view.x(right), self.view.y(y));
        let (w, _) = self.text.measure(s, px_phys);
        self.text.draw(
            self.canvas,
            self.textures,
            s,
            right_phys - w as i32,
            y_phys,
            px_phys,
            color,
        );
    }

    /// Vertical origin (logical) that centers one `px`-sized text line
    /// in `rect`. Kept in logical space so callers stay transform-free.
    pub fn centered_text_y(&mut self, rect: Rect, px: u16) -> i32 {
        let line_phys = self.text.line_height(self.view.px(px));
        let line_logical = (line_phys as f32 / self.view.scale).round() as i32;
        rect.y() + (rect.height() as i32 - line_logical) / 2
    }

    /// Fills a logical rect with a color.
    pub fn fill(&mut self, rect: Rect, color: Color) {
        self.canvas.set_draw_color(color);
        let _ = self.canvas.fill_rect(self.view.rect(rect));
    }

    /// Fills a widget background, outlined when focused.
    pub fn widget_box(&mut self, rect: Rect, focused: bool) {
        let phys = self.view.rect(rect);
        self.canvas.set_draw_color(if focused {
            palette::WIDGET_FOCUS
        } else {
            palette::WIDGET
        });
        let _ = self.canvas.fill_rect(phys);
        if focused {
            self.canvas.set_draw_color(palette::OUTLINE_FOCUS);
            let _ = self.canvas.draw_rect(phys);
        }
    }

    /// A full-width button row with a left-aligned label.
    pub fn button(&mut self, rect: Rect, label: &str, focused: bool) {
        self.widget_box(rect, focused);
        let y = self.centered_text_y(rect, 22);
        self.text(label, rect.x() + 16, y, 22, palette::TEXT);
    }

    /// A labeled on/off toggle row.
    pub fn toggle(&mut self, rect: Rect, label: &str, value: bool, focused: bool) {
        self.widget_box(rect, focused);
        let y = self.centered_text_y(rect, 22);
        self.text(label, rect.x() + 16, y, 22, palette::TEXT);
        let (state, color) = if value {
            ("on", palette::ACCENT)
        } else {
            ("off", palette::TEXT_DIM)
        };
        self.text_right(state, rect.x() + rect.width() as i32 - 16, y, 22, color);
    }

    /// A labeled left/right choice spinner row (`< value >`).
    pub fn choice(&mut self, rect: Rect, label: &str, value: &str, focused: bool) {
        self.widget_box(rect, focused);
        let y = self.centered_text_y(rect, 22);
        self.text(label, rect.x() + 16, y, 22, palette::TEXT);
        let arrows = if focused {
            palette::OUTLINE_FOCUS
        } else {
            palette::TEXT_DIM
        };
        let right = rect.x() + rect.width() as i32 - 16;
        self.text_right(">", right, y, 22, arrows);
        let value_width_phys = self.text.measure(value, self.view.px(22)).0;
        let value_width = (value_width_phys as f32 / self.view.scale).round() as i32;
        self.text_right(value, right - 24, y, 22, palette::TEXT);
        self.text_right("<", right - 32 - value_width, y, 22, arrows);
    }

    /// A labeled text field row with a cursor when editing.
    pub fn text_field(
        &mut self,
        rect: Rect,
        label: &str,
        value: &str,
        focused: bool,
        editing: bool,
    ) {
        self.widget_box(rect, focused);
        let y = self.centered_text_y(rect, 22);
        self.text(label, rect.x() + 16, y, 22, palette::TEXT);
        let shown = if editing {
            format!("{value}_")
        } else if value.is_empty() {
            "—".to_string()
        } else {
            value.to_string()
        };
        let color = if value.is_empty() && !editing {
            palette::TEXT_DIM
        } else {
            palette::TEXT
        };
        self.text_right(&shown, rect.x() + rect.width() as i32 - 16, y, 22, color);
    }

    /// A banner across the top of the screen.
    pub fn banner(&mut self, message: &str, background: Color, foreground: Color) {
        self.fill(Rect::new(0, 0, UI_WIDTH, 44), background);
        self.text(message, 16, 10, 18, foreground);
    }

    /// A dismissible error banner across the top of the screen.
    pub fn error_banner(&mut self, message: &str) {
        self.banner(message, palette::ERROR_BG, palette::ERROR_TEXT);
    }

    /// A dim hint line (button legend) at the bottom of the screen.
    pub fn hint_bar(&mut self, hint: &str) {
        let y = UI_HEIGHT as i32 - 40;
        self.text(hint, 24, y, 16, palette::TEXT_DIM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_list_saturates_at_both_ends() {
        let mut focus = FocusList::new(3);
        focus.up();
        assert_eq!(focus.focus, 0);
        focus.down();
        focus.down();
        assert_eq!(focus.focus, 2);
        focus.down();
        assert_eq!(focus.focus, 2);
    }

    #[test]
    fn focus_list_resize_clamps() {
        let mut focus = FocusList::new(4);
        focus.down();
        focus.down();
        focus.down();
        assert_eq!(focus.focus, 3);
        focus.resize(2);
        assert_eq!(focus.focus, 1);
        focus.resize(0);
        assert_eq!(focus.focus, 0);
    }

    #[test]
    fn hit_test_finds_row() {
        let rects = layout_rows(3, 100, 56, 8, 40);
        assert_eq!(hit_test(&rects, 50, 110), Some(0));
        assert_eq!(hit_test(&rects, 50, 100 + 64 + 10), Some(1));
        assert_eq!(hit_test(&rects, 50, 10), None);
        // Outside the horizontal margin.
        assert_eq!(hit_test(&rects, 10, 110), None);
    }

    #[test]
    fn focus_at_updates_focus() {
        let rects = layout_rows(3, 100, 56, 8, 40);
        let mut focus = FocusList::new(3);
        assert_eq!(focus.focus_at(&rects, 50, 170), Some(1));
        assert_eq!(focus.focus, 1);
        assert_eq!(focus.focus_at(&rects, 50, 10), None);
        assert_eq!(focus.focus, 1);
    }

    #[test]
    fn layout_rows_geometry() {
        let rects = layout_rows(2, 100, 56, 8, 40);
        assert_eq!(rects[0], Rect::new(40, 100, 1200, 56));
        assert_eq!(rects[1], Rect::new(40, 164, 1200, 56));
    }

    #[test]
    fn view_transform_scales_and_centers() {
        // 2x integer scale, no letterbox.
        let view = ViewTransform::for_output(2560, 1600);
        assert!((view.scale - 2.0).abs() < f32::EPSILON);
        assert_eq!(view.offset_x, 0);
        assert_eq!(
            view.rect(Rect::new(40, 100, 1200, 56)),
            Rect::new(80, 200, 2400, 112)
        );
        assert_eq!(view.px(22), 44);

        // 16:9 display: uniform scale on height, horizontal letterbox.
        let view = ViewTransform::for_output(1920, 1080);
        assert!((view.scale - 1.35).abs() < 0.01);
        assert!(view.offset_x > 0);
        assert_eq!(view.offset_y, 0);

        // Pointer round-trips back to logical space.
        let (lx, ly) = view.pointer_to_logical(view.x(640), view.y(400));
        assert!((lx - 640).abs() <= 1);
        assert!((ly - 400).abs() <= 1);
    }

    #[test]
    fn view_transform_native_deck_is_identity() {
        let view = ViewTransform::for_output(1280, 800);
        assert!((view.scale - 1.0).abs() < f32::EPSILON);
        assert_eq!(view.offset_x, 0);
        assert_eq!(view.offset_y, 0);
        assert_eq!(view.px(22), 22);
    }
}
