//! Widget drawing and focus/navigation primitives for the launcher UI.
//!
//! There is no retained widget tree: each screen draws its widgets
//! every frame from its own state, and layout functions return the
//! widget rectangles so pointer hit-testing and unit tests share the
//! same geometry.

use sdl2::pixels::Color;
use sdl2::rect::Rect;
use sdl2::render::{Canvas, TextureCreator};
use sdl2::video::{Window, WindowContext};

use super::font::TextRenderer;

/// Logical UI resolution; SDL scales it to the actual window (matches
/// the Steam Deck panel, and scales cleanly elsewhere).
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
/// game controller, and pointer events.
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

/// Shared context for widget drawing.
pub struct Ui<'a> {
    /// Canvas to draw on.
    pub canvas: &'a mut Canvas<Window>,
    /// Texture creator owning glyph textures.
    pub textures: &'a TextureCreator<WindowContext>,
    /// Glyph cache (must belong to `canvas`, see [`TextRenderer`]).
    pub text: &'a mut TextRenderer,
}

impl Ui<'_> {
    /// Draws `text` at (x, y); returns the x-advance.
    pub fn text(&mut self, s: &str, x: i32, y: i32, px: u16, color: Color) -> i32 {
        self.text
            .draw(self.canvas, self.textures, s, x, y, px, color)
    }

    /// Draws `text` right-aligned so it ends at `right`.
    pub fn text_right(&mut self, s: &str, right: i32, y: i32, px: u16, color: Color) {
        let (w, _) = self.text.measure(s, px);
        self.text(s, right - w as i32, y, px, color);
    }

    /// Fills a widget background, outlined when focused.
    pub fn widget_box(&mut self, rect: Rect, focused: bool) {
        self.canvas.set_draw_color(if focused {
            palette::WIDGET_FOCUS
        } else {
            palette::WIDGET
        });
        let _ = self.canvas.fill_rect(rect);
        if focused {
            self.canvas.set_draw_color(palette::OUTLINE_FOCUS);
            let _ = self.canvas.draw_rect(rect);
        }
    }

    /// A full-width button row with a left-aligned label.
    pub fn button(&mut self, rect: Rect, label: &str, focused: bool) {
        self.widget_box(rect, focused);
        let y = rect.y() + (rect.height() as i32 - self.text.line_height(22) as i32) / 2;
        self.text(label, rect.x() + 16, y, 22, palette::TEXT);
    }

    /// A labeled on/off toggle row.
    pub fn toggle(&mut self, rect: Rect, label: &str, value: bool, focused: bool) {
        self.widget_box(rect, focused);
        let y = rect.y() + (rect.height() as i32 - self.text.line_height(22) as i32) / 2;
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
        let y = rect.y() + (rect.height() as i32 - self.text.line_height(22) as i32) / 2;
        self.text(label, rect.x() + 16, y, 22, palette::TEXT);
        let arrows = if focused {
            palette::OUTLINE_FOCUS
        } else {
            palette::TEXT_DIM
        };
        let right = rect.x() + rect.width() as i32 - 16;
        self.text_right(">", right, y, 22, arrows);
        let (value_width, _) = self.text.measure(value, 22);
        self.text_right(value, right - 24, y, 22, palette::TEXT);
        self.text_right("<", right - 32 - value_width as i32, y, 22, arrows);
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
        let y = rect.y() + (rect.height() as i32 - self.text.line_height(22) as i32) / 2;
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

    /// A dismissible error banner across the top of the screen.
    pub fn error_banner(&mut self, message: &str) {
        let rect = Rect::new(0, 0, UI_WIDTH, 44);
        self.canvas.set_draw_color(palette::ERROR_BG);
        let _ = self.canvas.fill_rect(rect);
        self.text(message, 16, 10, 18, palette::ERROR_TEXT);
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
}
