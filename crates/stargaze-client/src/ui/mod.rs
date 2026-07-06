//! Launcher UI: host list, host editing, settings, and connection
//! screens shown before (and between) streaming sessions.
//!
//! Everything is drawn with SDL canvas primitives and fontdue-rendered
//! text at a logical 1280x800 (the Steam Deck panel), scaled by SDL to
//! the actual window. Navigation is gamepad-first (D-pad/left stick +
//! A/B), with keyboard and pointer (Deck touch arrives as mouse
//! events) handled through the same [`widgets::NavEvent`] vocabulary.

pub mod font;
pub mod widgets;

use std::time::{Duration, Instant};

use sdl2::controller::{Axis, Button};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::mouse::MouseButton;

use widgets::NavEvent;

/// Left-stick deflection that counts as a navigation press.
const STICK_THRESHOLD: i16 = 8000;
/// Delay before a held stick direction starts repeating.
const STICK_REPEAT_DELAY: Duration = Duration::from_millis(400);
/// Interval between repeats while the stick stays held.
const STICK_REPEAT_INTERVAL: Duration = Duration::from_millis(130);

/// Stick direction currently held, for key-repeat emulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StickDir {
    Up,
    Down,
    Left,
    Right,
}

impl StickDir {
    fn nav(self) -> NavEvent {
        match self {
            Self::Up => NavEvent::Up,
            Self::Down => NavEvent::Down,
            Self::Left => NavEvent::Left,
            Self::Right => NavEvent::Right,
        }
    }
}

/// Maps SDL events to [`NavEvent`]s, adding key-repeat behavior to the
/// left stick (SDL only reports axis motion, not "still held").
pub struct InputMapper {
    stick_x: i16,
    stick_y: i16,
    held: Option<(StickDir, Instant)>,
}

impl Default for InputMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl InputMapper {
    /// A mapper with centered stick state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stick_x: 0,
            stick_y: 0,
            held: None,
        }
    }

    /// Maps one SDL event; `None` for events the UI doesn't consume.
    pub fn map(&mut self, event: &Event) -> Option<NavEvent> {
        match event {
            Event::KeyDown {
                keycode: Some(key), ..
            } => match *key {
                Keycode::Up => Some(NavEvent::Up),
                Keycode::Down => Some(NavEvent::Down),
                Keycode::Left => Some(NavEvent::Left),
                Keycode::Right => Some(NavEvent::Right),
                Keycode::Return | Keycode::KpEnter => Some(NavEvent::Activate),
                Keycode::Escape => Some(NavEvent::Back),
                Keycode::Delete => Some(NavEvent::Delete),
                Keycode::Backspace => Some(NavEvent::Backspace),
                _ => None,
            },
            Event::TextInput { text, .. } => Some(NavEvent::Text(text.clone())),
            Event::ControllerButtonDown { button, .. } => match button {
                Button::DPadUp => Some(NavEvent::Up),
                Button::DPadDown => Some(NavEvent::Down),
                Button::DPadLeft => Some(NavEvent::Left),
                Button::DPadRight => Some(NavEvent::Right),
                Button::A => Some(NavEvent::Activate),
                Button::B => Some(NavEvent::Back),
                Button::Y => Some(NavEvent::Delete),
                Button::Start => Some(NavEvent::ConnectShortcut),
                _ => None,
            },
            Event::ControllerAxisMotion { axis, value, .. } => {
                match axis {
                    Axis::LeftX => self.stick_x = *value,
                    Axis::LeftY => self.stick_y = *value,
                    _ => return None,
                }
                self.update_stick()
            }
            Event::MouseMotion { x, y, .. } => Some(NavEvent::PointerMove(*x, *y)),
            Event::MouseButtonDown {
                mouse_btn: MouseButton::Left,
                x,
                y,
                ..
            } => Some(NavEvent::PointerClick(*x, *y)),
            _ => None,
        }
    }

    /// Emits stick repeats; call once per frame.
    pub fn tick(&mut self) -> Option<NavEvent> {
        let (dir, due) = self.held?;
        if Instant::now() < due {
            return None;
        }
        self.held = Some((dir, Instant::now() + STICK_REPEAT_INTERVAL));
        Some(dir.nav())
    }

    /// Re-evaluates the held direction after stick movement, emitting
    /// the initial press on a direction change.
    fn update_stick(&mut self) -> Option<NavEvent> {
        let dir = Self::dominant_dir(self.stick_x, self.stick_y);
        match (dir, self.held) {
            (Some(dir), held) if held.map(|(d, _)| d) != Some(dir) => {
                self.held = Some((dir, Instant::now() + STICK_REPEAT_DELAY));
                Some(dir.nav())
            }
            (None, _) => {
                self.held = None;
                None
            }
            _ => None,
        }
    }

    fn dominant_dir(x: i16, y: i16) -> Option<StickDir> {
        if x.unsigned_abs() < STICK_THRESHOLD as u16 && y.unsigned_abs() < STICK_THRESHOLD as u16 {
            return None;
        }
        Some(if y.unsigned_abs() >= x.unsigned_abs() {
            if y < 0 { StickDir::Up } else { StickDir::Down }
        } else if x < 0 {
            StickDir::Left
        } else {
            StickDir::Right
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis_event(axis: Axis, value: i16) -> Event {
        Event::ControllerAxisMotion {
            timestamp: 0,
            which: 0,
            axis,
            value,
        }
    }

    #[test]
    fn stick_press_emits_once_then_repeats_after_delay() {
        let mut mapper = InputMapper::new();
        assert_eq!(
            mapper.map(&axis_event(Axis::LeftY, 20000)),
            Some(NavEvent::Down)
        );
        // Same direction held: no immediate re-emit.
        assert_eq!(mapper.map(&axis_event(Axis::LeftY, 25000)), None);
        // Repeat not due yet.
        assert_eq!(mapper.tick(), None);
        // Releasing clears the held state.
        assert_eq!(mapper.map(&axis_event(Axis::LeftY, 0)), None);
        assert_eq!(mapper.tick(), None);
    }

    #[test]
    fn stick_direction_change_emits_new_press() {
        let mut mapper = InputMapper::new();
        assert_eq!(
            mapper.map(&axis_event(Axis::LeftY, -20000)),
            Some(NavEvent::Up)
        );
        assert_eq!(
            mapper.map(&axis_event(Axis::LeftX, 30000)),
            Some(NavEvent::Right)
        );
    }

    #[test]
    fn small_deflection_is_ignored() {
        let mut mapper = InputMapper::new();
        assert_eq!(mapper.map(&axis_event(Axis::LeftY, 4000)), None);
        assert_eq!(mapper.tick(), None);
    }
}
