//! Pure input-handling logic for the SDL render loop.
//!
//! Keeps the testable pieces — shortcut detection and pressed-input
//! tracking — out of the SDL event loop. Gamepad slot allocation lives
//! in [`crate::gamepad::SharedGamepads`], shared with the evdev
//! pass-through threads.

use std::collections::HashSet;

use sdl2::keyboard::{Mod, Scancode};
use stargaze_core::input::{InputEvent, MouseButton};

/// Client-side action triggered by a Ctrl+Alt+Shift shortcut.
///
/// Shortcuts are consumed locally and never forwarded to the server,
/// mirroring Moonlight's Ctrl+Alt+Shift+key scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShortcutAction {
    /// Quit the client (Ctrl+Alt+Shift+Q).
    Quit,
    /// Toggle input capture between "inside" (everything goes to the remote
    /// session) and "outside" (everything stays local) (Ctrl+Alt+Shift+Z).
    ToggleCapture,
    /// Toggle fullscreen (Ctrl+Alt+Shift+X).
    ToggleFullscreen,
    /// Toggle the stats overlay (Ctrl+Alt+Shift+S).
    ToggleStats,
}

/// Returns the shortcut action for a key press, if the Ctrl+Alt+Shift
/// modifier chord is held and the key maps to an action.
pub(super) fn shortcut_action(keymod: Mod, scancode: Scancode) -> Option<ShortcutAction> {
    let ctrl = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);
    let alt = keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
    let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
    if !(ctrl && alt && shift) {
        return None;
    }
    match scancode {
        Scancode::Q => Some(ShortcutAction::Quit),
        Scancode::Z => Some(ShortcutAction::ToggleCapture),
        Scancode::X => Some(ShortcutAction::ToggleFullscreen),
        Scancode::S => Some(ShortcutAction::ToggleStats),
        _ => None,
    }
}

/// Tracks keys and mouse buttons forwarded to the server while captured,
/// so they can all be released when capture ends (or the client quits)
/// instead of staying stuck pressed in the remote session.
pub(super) struct InputTracker {
    keys: HashSet<u32>,
    mouse_buttons: HashSet<MouseButton>,
}

impl InputTracker {
    pub(super) fn new() -> Self {
        Self {
            keys: HashSet::new(),
            mouse_buttons: HashSet::new(),
        }
    }

    pub(super) fn key_down(&mut self, scancode: u32) {
        self.keys.insert(scancode);
    }

    pub(super) fn key_up(&mut self, scancode: u32) {
        self.keys.remove(&scancode);
    }

    pub(super) fn mouse_down(&mut self, button: MouseButton) {
        self.mouse_buttons.insert(button);
    }

    pub(super) fn mouse_up(&mut self, button: MouseButton) {
        self.mouse_buttons.remove(&button);
    }

    /// Drains all tracked pressed inputs as release events for the server.
    pub(super) fn release_all(&mut self) -> Vec<InputEvent> {
        let mut events: Vec<InputEvent> = self
            .keys
            .drain()
            .map(|scancode| InputEvent::Keyboard {
                scancode,
                pressed: false,
            })
            .collect();
        events.extend(
            self.mouse_buttons
                .drain()
                .map(|button| InputEvent::MouseButton {
                    button,
                    pressed: false,
                }),
        );
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHORD: Mod = Mod::LCTRLMOD.union(Mod::LALTMOD).union(Mod::LSHIFTMOD);

    #[test]
    fn shortcut_requires_full_chord() {
        assert_eq!(
            shortcut_action(CHORD, Scancode::Q),
            Some(ShortcutAction::Quit)
        );
        assert_eq!(
            shortcut_action(CHORD, Scancode::Z),
            Some(ShortcutAction::ToggleCapture)
        );
        assert_eq!(
            shortcut_action(CHORD, Scancode::X),
            Some(ShortcutAction::ToggleFullscreen)
        );
        assert_eq!(
            shortcut_action(CHORD, Scancode::S),
            Some(ShortcutAction::ToggleStats)
        );

        // Partial chords must not trigger.
        assert_eq!(
            shortcut_action(Mod::LCTRLMOD | Mod::LALTMOD, Scancode::Q),
            None
        );
        assert_eq!(
            shortcut_action(Mod::LCTRLMOD | Mod::LSHIFTMOD, Scancode::Q),
            None
        );
        assert_eq!(shortcut_action(Mod::NOMOD, Scancode::Q), None);
    }

    #[test]
    fn shortcut_accepts_right_side_modifiers() {
        let right = Mod::RCTRLMOD | Mod::RALTMOD | Mod::RSHIFTMOD;
        assert_eq!(
            shortcut_action(right, Scancode::Q),
            Some(ShortcutAction::Quit)
        );
    }

    #[test]
    fn shortcut_ignores_other_keys() {
        assert_eq!(shortcut_action(CHORD, Scancode::A), None);
        assert_eq!(shortcut_action(CHORD, Scancode::Escape), None);
    }

    #[test]
    fn tracker_releases_everything_pressed() {
        let mut tracker = InputTracker::new();
        tracker.key_down(4);
        tracker.key_down(225); // LShift
        tracker.key_up(4);
        tracker.mouse_down(MouseButton::Left);

        let events = tracker.release_all();
        assert_eq!(events.len(), 2);
        assert!(events.contains(&InputEvent::Keyboard {
            scancode: 225,
            pressed: false
        }));
        assert!(events.contains(&InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: false
        }));

        // Tracker is empty afterwards.
        assert!(tracker.release_all().is_empty());
    }
}
