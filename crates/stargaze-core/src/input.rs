//! Input event types for keyboard, mouse, and gamepad forwarding.
//!
//! Defines the wire format for input events sent from the client to
//! the server over the QUIC control stream. All types derive
//! `Serialize`/`Deserialize` for postcard compatibility.

use serde::{Deserialize, Serialize};

/// An input event captured on the client and forwarded to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Key pressed or released.
    ///
    /// `scancode` is the SDL scancode value (USB HID usage code).
    /// The server maps this to a Linux evdev keycode for injection.
    Keyboard {
        /// SDL scancode (USB HID code).
        scancode: u32,
        /// `true` = key pressed, `false` = key released.
        pressed: bool,
    },
    /// Relative mouse movement (deltas from SDL2 relative mode).
    MouseMove {
        /// Horizontal delta (positive = right).
        dx: i32,
        /// Vertical delta (positive = down).
        dy: i32,
    },
    /// Mouse button pressed or released.
    MouseButton {
        /// Which button.
        button: MouseButton,
        /// `true` = pressed, `false` = released.
        pressed: bool,
    },
    /// Mouse scroll wheel movement.
    MouseWheel {
        /// Horizontal scroll (positive = right).
        dx: i32,
        /// Vertical scroll (positive = up, matching SDL2 convention).
        dy: i32,
    },
    /// Gamepad analog axis movement.
    GamepadAxis {
        /// Which axis.
        axis: GamepadAxis,
        /// Axis value (-32768..32767 for sticks, 0..32767 for triggers).
        value: i16,
    },
    /// Gamepad button pressed or released.
    GamepadButton {
        /// Which button.
        button: GamepadButton,
        /// `true` = pressed, `false` = released.
        pressed: bool,
    },
}

/// Mouse button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    /// Left mouse button.
    Left,
    /// Right mouse button.
    Right,
    /// Middle mouse button (scroll wheel click).
    Middle,
    /// Side button (typically "back").
    Side,
    /// Extra button (typically "forward").
    Extra,
}

/// Gamepad analog axis identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GamepadAxis {
    /// Left stick horizontal.
    LeftX,
    /// Left stick vertical.
    LeftY,
    /// Right stick horizontal.
    RightX,
    /// Right stick vertical.
    RightY,
    /// Left trigger (analog).
    TriggerLeft,
    /// Right trigger (analog).
    TriggerRight,
}

/// Gamepad button identifiers.
///
/// Uses cardinal directions (South/East/North/West) rather than
/// vendor-specific labels (A/B/X/Y) for platform neutrality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GamepadButton {
    /// Bottom face button (Xbox A, PS ×).
    South,
    /// Right face button (Xbox B, PS ○).
    East,
    /// Top face button (Xbox Y, PS △).
    North,
    /// Left face button (Xbox X, PS □).
    West,
    /// Start / Menu / Options.
    Start,
    /// Back / View / Share.
    Back,
    /// Guide / Home / PS button.
    Guide,
    /// Left stick press (L3).
    LeftStick,
    /// Right stick press (R3).
    RightStick,
    /// Left shoulder bumper (LB / L1).
    LeftShoulder,
    /// Right shoulder bumper (RB / R1).
    RightShoulder,
    /// D-pad up.
    DPadUp,
    /// D-pad down.
    DPadDown,
    /// D-pad left.
    DPadLeft,
    /// D-pad right.
    DPadRight,
}

/// Maps an SDL scancode (USB HID usage code) to a Linux evdev keycode.
///
/// Returns `None` for unmapped scancodes. Covers letters, digits,
/// function keys, modifiers, navigation, and common punctuation.
///
/// SDL scancode values are defined in the SDL2 `Scancode` enum and
/// correspond to USB HID usage table values. Linux evdev keycodes
/// are defined in `linux/input-event-codes.h`.
#[must_use]
pub fn sdl_scancode_to_evdev(scancode: u32) -> Option<u16> {
    map_alphanumeric(scancode).or_else(|| map_special_keys(scancode))
}

/// Maps SDL scancodes for letters (A-Z), digits (0-9), and common typing keys
/// to their Linux evdev equivalents.
fn map_alphanumeric(scancode: u32) -> Option<u16> {
    match scancode {
        // Letters A-Z (SDL 4-29 → evdev)
        4 => Some(30),  // A → KEY_A
        5 => Some(48),  // B → KEY_B
        6 => Some(46),  // C → KEY_C
        7 => Some(32),  // D → KEY_D
        8 => Some(18),  // E → KEY_E
        9 => Some(33),  // F → KEY_F
        10 => Some(34), // G → KEY_G
        11 => Some(35), // H → KEY_H
        12 => Some(23), // I → KEY_I
        13 => Some(36), // J → KEY_J
        14 => Some(37), // K → KEY_K
        15 => Some(38), // L → KEY_L
        16 => Some(50), // M → KEY_M
        17 => Some(49), // N → KEY_N
        18 => Some(24), // O → KEY_O
        19 => Some(25), // P → KEY_P
        20 => Some(16), // Q → KEY_Q
        21 => Some(19), // R → KEY_R
        22 => Some(31), // S → KEY_S
        23 => Some(20), // T → KEY_T
        24 => Some(22), // U → KEY_U
        25 => Some(47), // V → KEY_V
        26 => Some(17), // W → KEY_W
        27 => Some(45), // X → KEY_X
        28 => Some(21), // Y → KEY_Y
        29 => Some(44), // Z → KEY_Z

        // Digits 1-9, 0 (SDL 30-39 → evdev)
        30 => Some(2),  // 1 → KEY_1
        31 => Some(3),  // 2 → KEY_2
        32 => Some(4),  // 3 → KEY_3
        33 => Some(5),  // 4 → KEY_4
        34 => Some(6),  // 5 → KEY_5
        35 => Some(7),  // 6 → KEY_6
        36 => Some(8),  // 7 → KEY_7
        37 => Some(9),  // 8 → KEY_8
        38 => Some(10), // 9 → KEY_9
        39 => Some(11), // 0 → KEY_0

        // Common keys
        40 => Some(28), // Return → KEY_ENTER
        41 => Some(1),  // Escape → KEY_ESC
        42 => Some(14), // Backspace → KEY_BACKSPACE
        43 => Some(15), // Tab → KEY_TAB
        44 => Some(57), // Space → KEY_SPACE

        // Symbols
        45 => Some(12), // Minus → KEY_MINUS
        46 => Some(13), // Equals → KEY_EQUAL
        47 => Some(26), // LeftBracket → KEY_LEFTBRACE
        48 => Some(27), // RightBracket → KEY_RIGHTBRACE
        49 => Some(43), // Backslash → KEY_BACKSLASH
        51 => Some(39), // Semicolon → KEY_SEMICOLON
        52 => Some(40), // Apostrophe → KEY_APOSTROPHE
        53 => Some(41), // Grave → KEY_GRAVE
        54 => Some(51), // Comma → KEY_COMMA
        55 => Some(52), // Period → KEY_DOT
        56 => Some(53), // Slash → KEY_SLASH

        // Caps Lock
        57 => Some(58), // CapsLock → KEY_CAPSLOCK

        _ => None,
    }
}

/// Maps SDL scancodes for function keys, navigation, keypad, and modifiers
/// to their Linux evdev equivalents.
#[allow(clippy::match_same_arms)]
fn map_special_keys(scancode: u32) -> Option<u16> {
    match scancode {
        // Function keys F1-F12 (SDL 58-69 → evdev)
        58 => Some(59), // F1 → KEY_F1
        59 => Some(60), // F2 → KEY_F2
        60 => Some(61), // F3 → KEY_F3
        61 => Some(62), // F4 → KEY_F4
        62 => Some(63), // F5 → KEY_F5
        63 => Some(64), // F6 → KEY_F6
        64 => Some(65), // F7 → KEY_F7
        65 => Some(66), // F8 → KEY_F8
        66 => Some(67), // F9 → KEY_F9
        67 => Some(68), // F10 → KEY_F10
        68 => Some(87), // F11 → KEY_F11
        69 => Some(88), // F12 → KEY_F12

        // Navigation keys
        70 => Some(99),  // PrintScreen → KEY_SYSRQ
        71 => Some(70),  // ScrollLock → KEY_SCROLLLOCK
        72 => Some(119), // Pause → KEY_PAUSE
        73 => Some(110), // Insert → KEY_INSERT
        74 => Some(102), // Home → KEY_HOME
        75 => Some(104), // PageUp → KEY_PAGEUP
        76 => Some(111), // Delete → KEY_DELETE
        77 => Some(107), // End → KEY_END
        78 => Some(109), // PageDown → KEY_PAGEDOWN
        79 => Some(106), // Right → KEY_RIGHT
        80 => Some(105), // Left → KEY_LEFT
        81 => Some(108), // Down → KEY_DOWN
        82 => Some(103), // Up → KEY_UP

        // Keypad
        83 => Some(69), // NumLock → KEY_NUMLOCK
        84 => Some(98), // KP Divide → KEY_KPSLASH
        85 => Some(55), // KP Multiply → KEY_KPASTERISK
        86 => Some(74), // KP Minus → KEY_KPMINUS
        87 => Some(78), // KP Plus → KEY_KPPLUS
        88 => Some(96), // KP Enter → KEY_KPENTER
        89 => Some(79), // KP 1 → KEY_KP1
        90 => Some(80), // KP 2 → KEY_KP2
        91 => Some(81), // KP 3 → KEY_KP3
        92 => Some(75), // KP 4 → KEY_KP4
        93 => Some(76), // KP 5 → KEY_KP5
        94 => Some(77), // KP 6 → KEY_KP6
        95 => Some(71), // KP 7 → KEY_KP7
        96 => Some(72), // KP 8 → KEY_KP8
        97 => Some(73), // KP 9 → KEY_KP9
        98 => Some(82), // KP 0 → KEY_KP0
        99 => Some(83), // KP Period → KEY_KPDOT

        // Modifiers (SDL 224-231 → evdev)
        224 => Some(29),  // LCtrl → KEY_LEFTCTRL
        225 => Some(42),  // LShift → KEY_LEFTSHIFT
        226 => Some(56),  // LAlt → KEY_LEFTALT
        227 => Some(125), // LGui → KEY_LEFTMETA
        228 => Some(97),  // RCtrl → KEY_RIGHTCTRL
        229 => Some(54),  // RShift → KEY_RIGHTSHIFT
        230 => Some(100), // RAlt → KEY_RIGHTALT
        231 => Some(126), // RGui → KEY_RIGHTMETA

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scancode_mapping_letters() {
        // A (SDL 4 → evdev 30)
        assert_eq!(sdl_scancode_to_evdev(4), Some(30));
        // Z (SDL 29 → evdev 44)
        assert_eq!(sdl_scancode_to_evdev(29), Some(44));
        // Q (SDL 20 → evdev 16)
        assert_eq!(sdl_scancode_to_evdev(20), Some(16));
    }

    #[test]
    fn scancode_mapping_digits() {
        // 1 (SDL 30 → evdev 2)
        assert_eq!(sdl_scancode_to_evdev(30), Some(2));
        // 0 (SDL 39 → evdev 11)
        assert_eq!(sdl_scancode_to_evdev(39), Some(11));
    }

    #[test]
    fn scancode_mapping_common_keys() {
        // Space (SDL 44 → evdev 57)
        assert_eq!(sdl_scancode_to_evdev(44), Some(57));
        // Enter (SDL 40 → evdev 28)
        assert_eq!(sdl_scancode_to_evdev(40), Some(28));
        // Escape (SDL 41 → evdev 1)
        assert_eq!(sdl_scancode_to_evdev(41), Some(1));
        // Backspace (SDL 42 → evdev 14)
        assert_eq!(sdl_scancode_to_evdev(42), Some(14));
        // Tab (SDL 43 → evdev 15)
        assert_eq!(sdl_scancode_to_evdev(43), Some(15));
    }

    #[test]
    fn scancode_mapping_function_keys() {
        // F1 (SDL 58 → evdev 59)
        assert_eq!(sdl_scancode_to_evdev(58), Some(59));
        // F11 (SDL 68 → evdev 87)
        assert_eq!(sdl_scancode_to_evdev(68), Some(87));
        // F12 (SDL 69 → evdev 88)
        assert_eq!(sdl_scancode_to_evdev(69), Some(88));
    }

    #[test]
    fn scancode_mapping_arrows() {
        // Right (SDL 79 → evdev 106)
        assert_eq!(sdl_scancode_to_evdev(79), Some(106));
        // Left (SDL 80 → evdev 105)
        assert_eq!(sdl_scancode_to_evdev(80), Some(105));
        // Down (SDL 81 → evdev 108)
        assert_eq!(sdl_scancode_to_evdev(81), Some(108));
        // Up (SDL 82 → evdev 103)
        assert_eq!(sdl_scancode_to_evdev(82), Some(103));
    }

    #[test]
    fn scancode_mapping_modifiers() {
        // LCtrl (SDL 224 → evdev 29)
        assert_eq!(sdl_scancode_to_evdev(224), Some(29));
        // LShift (SDL 225 → evdev 42)
        assert_eq!(sdl_scancode_to_evdev(225), Some(42));
        // LAlt (SDL 226 → evdev 56)
        assert_eq!(sdl_scancode_to_evdev(226), Some(56));
        // RGui (SDL 231 → evdev 126)
        assert_eq!(sdl_scancode_to_evdev(231), Some(126));
    }

    #[test]
    fn scancode_mapping_unknown_returns_none() {
        assert_eq!(sdl_scancode_to_evdev(999), None);
        assert_eq!(sdl_scancode_to_evdev(50), None); // SDL 50 is unused gap
        assert_eq!(sdl_scancode_to_evdev(0), None);
    }

    #[test]
    fn input_event_keyboard_construction() {
        let event = InputEvent::Keyboard {
            scancode: 4,
            pressed: true,
        };
        assert_eq!(
            event,
            InputEvent::Keyboard {
                scancode: 4,
                pressed: true,
            }
        );
    }

    #[test]
    fn input_event_mouse_move_construction() {
        let event = InputEvent::MouseMove { dx: -10, dy: 5 };
        assert_eq!(event, InputEvent::MouseMove { dx: -10, dy: 5 });
    }

    #[test]
    fn input_event_mouse_button_construction() {
        let event = InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        };
        assert_eq!(
            event,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            }
        );
    }

    #[test]
    fn input_event_mouse_wheel_construction() {
        let event = InputEvent::MouseWheel { dx: 0, dy: 3 };
        assert_eq!(event, InputEvent::MouseWheel { dx: 0, dy: 3 });
    }

    #[test]
    fn input_event_gamepad_axis_construction() {
        let event = InputEvent::GamepadAxis {
            axis: GamepadAxis::LeftX,
            value: -16000,
        };
        assert_eq!(
            event,
            InputEvent::GamepadAxis {
                axis: GamepadAxis::LeftX,
                value: -16000,
            }
        );
    }

    #[test]
    fn input_event_gamepad_button_construction() {
        let event = InputEvent::GamepadButton {
            button: GamepadButton::South,
            pressed: true,
        };
        assert_eq!(
            event,
            InputEvent::GamepadButton {
                button: GamepadButton::South,
                pressed: true,
            }
        );
    }

    #[test]
    fn mouse_button_variants() {
        let buttons = [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Side,
            MouseButton::Extra,
        ];
        // Each variant should be distinct.
        for (i, a) in buttons.iter().enumerate() {
            for (j, b) in buttons.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn gamepad_axis_variants() {
        let axes = [
            GamepadAxis::LeftX,
            GamepadAxis::LeftY,
            GamepadAxis::RightX,
            GamepadAxis::RightY,
            GamepadAxis::TriggerLeft,
            GamepadAxis::TriggerRight,
        ];
        for (i, a) in axes.iter().enumerate() {
            for (j, b) in axes.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn gamepad_button_variants() {
        let buttons = [
            GamepadButton::South,
            GamepadButton::East,
            GamepadButton::North,
            GamepadButton::West,
            GamepadButton::Start,
            GamepadButton::Back,
            GamepadButton::Guide,
            GamepadButton::LeftStick,
            GamepadButton::RightStick,
            GamepadButton::LeftShoulder,
            GamepadButton::RightShoulder,
            GamepadButton::DPadUp,
            GamepadButton::DPadDown,
            GamepadButton::DPadLeft,
            GamepadButton::DPadRight,
        ];
        for (i, a) in buttons.iter().enumerate() {
            for (j, b) in buttons.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }
}
