//! Launcher UI: host list, host editing, settings, and connection
//! screens shown before (and between) streaming sessions.
//!
//! Everything is drawn with SDL canvas primitives and fontdue-rendered
//! text at a logical 1280x800 (the Steam Deck panel), scaled by SDL to
//! the actual window. Navigation is gamepad-first (D-pad/left stick +
//! A/B), with keyboard and pointer (Deck touch arrives as mouse
//! events) handled through the same [`widgets::NavEvent`] vocabulary.

pub mod font;
pub mod launcher;
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
/// Window in which the same nav event from a *different* device is
/// treated as a duplicate of one physical press. Steam Input mirrors
/// the controller: it exposes a virtual gamepad alongside the real one
/// and its desktop layout synthesizes keyboard presses (D-pad → arrow
/// keys, B → Escape), so one press can arrive as two or three SDL
/// events. Humans can't repeat a press this fast; the mirrors arrive
/// within a few milliseconds.
const CROSS_DEVICE_DEDUP_WINDOW: Duration = Duration::from_millis(50);

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

/// The device a button-like nav event came from, for cross-device
/// duplicate suppression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavSource {
    Keyboard,
    Pad(u32),
}

/// Maps SDL events to [`NavEvent`]s, adding key-repeat behavior to the
/// left stick (SDL only reports axis motion, not "still held").
pub struct InputMapper {
    stick_x: i16,
    stick_y: i16,
    held: Option<(StickDir, Instant)>,
    /// Last button-like nav event, for cross-device dedup.
    last_nav: Option<(NavEvent, NavSource, Instant)>,
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
            last_nav: None,
        }
    }

    /// Maps one SDL event; `None` for events the UI doesn't consume.
    pub fn map(&mut self, event: &Event) -> Option<NavEvent> {
        match event {
            Event::KeyDown {
                keycode: Some(key), ..
            } => {
                let nav = match *key {
                    Keycode::Up => NavEvent::Up,
                    Keycode::Down => NavEvent::Down,
                    Keycode::Left => NavEvent::Left,
                    Keycode::Right => NavEvent::Right,
                    Keycode::Return | Keycode::KpEnter => NavEvent::Activate,
                    Keycode::Escape => NavEvent::Back,
                    Keycode::Delete => NavEvent::Delete,
                    Keycode::E => NavEvent::Edit,
                    Keycode::Backspace => NavEvent::Backspace,
                    _ => return None,
                };
                self.dedup(nav, NavSource::Keyboard)
            }
            Event::TextInput { text, .. } => Some(NavEvent::Text(text.clone())),
            Event::ControllerButtonDown { button, which, .. } => {
                let nav = match button {
                    Button::DPadUp => NavEvent::Up,
                    Button::DPadDown => NavEvent::Down,
                    Button::DPadLeft => NavEvent::Left,
                    Button::DPadRight => NavEvent::Right,
                    Button::A => NavEvent::Activate,
                    Button::B => NavEvent::Back,
                    Button::Y => NavEvent::Delete,
                    Button::X => NavEvent::Edit,
                    Button::Start => NavEvent::ConnectShortcut,
                    _ => return None,
                };
                self.dedup(nav, NavSource::Pad(*which))
            }
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

    /// Suppresses the same nav event arriving from a different device
    /// within [`CROSS_DEVICE_DEDUP_WINDOW`] — one physical press echoed
    /// by Steam Input's virtual gamepad or its synthesized key presses.
    /// Same-device repeats (held keys, distinct presses) pass through.
    fn dedup(&mut self, nav: NavEvent, source: NavSource) -> Option<NavEvent> {
        if let Some((last, last_source, at)) = &self.last_nav
            && *last == nav
            && *last_source != source
            && at.elapsed() < CROSS_DEVICE_DEDUP_WINDOW
        {
            tracing::debug!(
                ?nav,
                ?source,
                "Suppressing cross-device duplicate nav event"
            );
            return None;
        }
        self.last_nav = Some((nav.clone(), source, Instant::now()));
        Some(nav)
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

    fn pad_button(which: u32, button: Button) -> Event {
        Event::ControllerButtonDown {
            timestamp: 0,
            which,
            button,
        }
    }

    fn key_down(keycode: Keycode) -> Event {
        Event::KeyDown {
            timestamp: 0,
            window_id: 0,
            keycode: Some(keycode),
            scancode: None,
            keymod: sdl2::keyboard::Mod::NOMOD,
            repeat: false,
        }
    }

    #[test]
    fn cross_device_echo_is_suppressed() {
        let mut mapper = InputMapper::new();

        // One physical D-pad press, echoed by Steam's virtual pad and
        // its synthesized arrow key: only the first event survives.
        assert_eq!(
            mapper.map(&pad_button(0, Button::DPadDown)),
            Some(NavEvent::Down)
        );
        assert_eq!(mapper.map(&pad_button(1, Button::DPadDown)), None);
        assert_eq!(mapper.map(&key_down(Keycode::Down)), None);

        // B echoed as Escape (Steam desktop layout).
        assert_eq!(mapper.map(&pad_button(0, Button::B)), Some(NavEvent::Back));
        assert_eq!(mapper.map(&key_down(Keycode::Escape)), None);
    }

    #[test]
    fn same_device_presses_pass_through() {
        let mut mapper = InputMapper::new();

        // Two rapid presses on the SAME device are two real inputs.
        assert_eq!(
            mapper.map(&pad_button(0, Button::DPadDown)),
            Some(NavEvent::Down)
        );
        assert_eq!(
            mapper.map(&pad_button(0, Button::DPadDown)),
            Some(NavEvent::Down)
        );

        // A different nav event from another device is not a duplicate.
        assert_eq!(
            mapper.map(&pad_button(1, Button::DPadUp)),
            Some(NavEvent::Up)
        );
    }

    #[test]
    fn echo_after_window_passes_through() {
        let mut mapper = InputMapper::new();
        assert_eq!(
            mapper.map(&pad_button(0, Button::DPadDown)),
            Some(NavEvent::Down)
        );
        std::thread::sleep(CROSS_DEVICE_DEDUP_WINDOW + Duration::from_millis(10));
        assert_eq!(
            mapper.map(&key_down(Keycode::Down)),
            Some(NavEvent::Down),
            "a press on another device after the window is a real input"
        );
    }
}
