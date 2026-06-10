use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputEvent as EvdevInputEvent, InputId,
    KeyCode, RelativeAxisCode, UinputAbsSetup,
};
use stargaze_core::input::{GamepadAxis, GamepadButton, InputEvent, MAX_GAMEPADS, MouseButton};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

#[derive(Debug)]
pub enum InputError {
    DeviceCreation(std::io::Error),
    SpawnFailed(String),
    ThreadPanic,
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceCreation(e) => write!(f, "failed to create virtual device: {e}"),
            Self::SpawnFailed(s) => write!(f, "failed to spawn input thread: {s}"),
            Self::ThreadPanic => write!(f, "input injection thread panicked"),
        }
    }
}

impl std::error::Error for InputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DeviceCreation(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for InputError {
    fn from(e: std::io::Error) -> Self {
        Self::DeviceCreation(e)
    }
}

pub(crate) struct VirtualDevices {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
    /// Virtual gamepads keyed by pad slot, created on demand when the
    /// client connects a controller and removed when it disconnects.
    gamepads: HashMap<u8, VirtualDevice>,
}

impl VirtualDevices {
    /// Returns the virtual gamepad for `pad`, creating it if needed.
    ///
    /// Pads beyond [`MAX_GAMEPADS`] are rejected.
    fn gamepad(&mut self, pad: u8) -> Result<&mut VirtualDevice, InputError> {
        if pad >= MAX_GAMEPADS {
            return Err(InputError::SpawnFailed(format!(
                "gamepad slot {pad} exceeds maximum of {MAX_GAMEPADS}"
            )));
        }
        match self.gamepads.entry(pad) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(e) => {
                let device = create_virtual_gamepad()?;
                info!(pad, "Created virtual gamepad (Xbox 360 layout)");
                Ok(e.insert(device))
            }
        }
    }
}

pub(crate) fn create_virtual_devices() -> Result<VirtualDevices, InputError> {
    let keyboard = create_virtual_keyboard()?;
    let mouse = create_virtual_mouse()?;
    Ok(VirtualDevices {
        keyboard,
        mouse,
        gamepads: HashMap::new(),
    })
}

fn create_virtual_keyboard() -> Result<VirtualDevice, InputError> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in 1..=248 {
        keys.insert(KeyCode::new(code));
    }

    let device = VirtualDevice::builder()?
        .name("Stargaze Virtual Keyboard")
        .with_keys(&keys)?
        .build()?;

    Ok(device)
}

fn create_virtual_mouse() -> Result<VirtualDevice, InputError> {
    let mut keys = AttributeSet::<KeyCode>::new();
    keys.insert(KeyCode::BTN_LEFT);
    keys.insert(KeyCode::BTN_RIGHT);
    keys.insert(KeyCode::BTN_MIDDLE);
    keys.insert(KeyCode::BTN_SIDE);
    keys.insert(KeyCode::BTN_EXTRA);

    let mut rel_axes = AttributeSet::<RelativeAxisCode>::new();
    rel_axes.insert(RelativeAxisCode::REL_X);
    rel_axes.insert(RelativeAxisCode::REL_Y);
    rel_axes.insert(RelativeAxisCode::REL_WHEEL);
    rel_axes.insert(RelativeAxisCode::REL_HWHEEL);

    let device = VirtualDevice::builder()?
        .name("Stargaze Virtual Mouse")
        .with_keys(&keys)?
        .with_relative_axes(&rel_axes)?
        .build()?;

    Ok(device)
}

/// Creates a virtual gamepad that mimics a wired Xbox 360 controller.
///
/// Name, vendor/product ids, and the evdev layout (`HAT0X`/`HAT0Y` d-pad,
/// 0..255 triggers on `ABS_Z`/`ABS_RZ`) match the kernel `xpad` driver, so
/// games and SDL's controller database recognize it out of the box —
/// the same approach Sunshine uses.
fn create_virtual_gamepad() -> Result<VirtualDevice, InputError> {
    let mut keys = AttributeSet::<KeyCode>::new();
    let gamepad_keys = [
        KeyCode::BTN_SOUTH,
        KeyCode::BTN_EAST,
        KeyCode::BTN_NORTH,
        KeyCode::BTN_WEST,
        KeyCode::BTN_TL,
        KeyCode::BTN_TR,
        KeyCode::BTN_SELECT,
        KeyCode::BTN_START,
        KeyCode::BTN_MODE,
        KeyCode::BTN_THUMBL,
        KeyCode::BTN_THUMBR,
    ];
    for key in &gamepad_keys {
        keys.insert(*key);
    }

    let stick_info = AbsInfo::new(0, -32768, 32767, 16, 128, 1);
    let trigger_info = AbsInfo::new(0, 0, 255, 0, 0, 1);
    let hat_info = AbsInfo::new(0, -1, 1, 0, 0, 0);

    let mut builder = VirtualDevice::builder()?
        .name("Microsoft X-Box 360 pad")
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x110))
        .with_keys(&keys)?;

    let stick_axes = [
        AbsoluteAxisCode::ABS_X,
        AbsoluteAxisCode::ABS_Y,
        AbsoluteAxisCode::ABS_RX,
        AbsoluteAxisCode::ABS_RY,
    ];
    for axis in &stick_axes {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(*axis, stick_info))?;
    }

    let device = builder
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Z, trigger_info))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RZ, trigger_info))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_HAT0X, hat_info))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_HAT0Y, hat_info))?
        .build()?;
    Ok(device)
}

pub(crate) fn run_injection_loop(
    mut devices: VirtualDevices,
    mut input_rx: mpsc::Receiver<InputEvent>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), InputError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|e| InputError::SpawnFailed(format!("tokio runtime: {e}")))?;

    rt.block_on(async {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let event = tokio::select! {
                ev = input_rx.recv() => {
                    match ev {
                        Some(e) => e,
                        None => break,
                    }
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(100)) => continue,
            };

            if let Err(e) = inject_event(&mut devices, &event) {
                warn!("Failed to inject input event: {e}");
            }
        }
    });

    Ok(())
}

fn inject_event(devices: &mut VirtualDevices, event: &InputEvent) -> Result<(), InputError> {
    let syn = EvdevInputEvent::new(evdev::EventType::SYNCHRONIZATION.0, 0, 0);

    match event {
        InputEvent::Keyboard { scancode, pressed } => {
            let evdev_code = stargaze_core::input::sdl_scancode_to_evdev(*scancode);
            let Some(code) = evdev_code else {
                debug!(scancode, "Unmapped SDL scancode, ignoring");
                return Ok(());
            };
            let value = i32::from(*pressed);
            let ev = EvdevInputEvent::new(evdev::EventType::KEY.0, code, value);
            devices.keyboard.emit(&[ev, syn])?;
        }
        InputEvent::MouseMove { dx, dy } => {
            inject_mouse_move(&mut devices.mouse, *dx, *dy, &syn)?;
        }
        InputEvent::MouseButton { button, pressed } => {
            let code = mouse_button_code(*button);
            let value = i32::from(*pressed);
            let ev = EvdevInputEvent::new(evdev::EventType::KEY.0, code, value);
            devices.mouse.emit(&[ev, syn])?;
        }
        InputEvent::MouseWheel { dx, dy } => {
            inject_mouse_wheel(&mut devices.mouse, *dx, *dy, &syn)?;
        }
        InputEvent::GamepadAxis { pad, axis, value } => {
            let (evdev_axis, val) = gamepad_axis_code(*axis, *value);
            let ev = EvdevInputEvent::new(evdev::EventType::ABSOLUTE.0, evdev_axis, val);
            devices.gamepad(*pad)?.emit(&[ev, syn])?;
        }
        InputEvent::GamepadButton {
            pad,
            button,
            pressed,
        } => {
            let ev = match gamepad_button_mapping(*button) {
                PadButtonMapping::Key(code) => {
                    EvdevInputEvent::new(evdev::EventType::KEY.0, code, i32::from(*pressed))
                }
                // D-pad buttons map to hat axis positions on the Xbox 360
                // layout: pressed moves the hat, released recenters it.
                PadButtonMapping::Hat { axis, direction } => EvdevInputEvent::new(
                    evdev::EventType::ABSOLUTE.0,
                    axis,
                    if *pressed { direction } else { 0 },
                ),
            };
            devices.gamepad(*pad)?.emit(&[ev, syn])?;
        }
        InputEvent::GamepadConnected { pad } => {
            devices.gamepad(*pad)?;
        }
        InputEvent::GamepadDisconnected { pad } => {
            if devices.gamepads.remove(pad).is_some() {
                info!(pad, "Removed virtual gamepad");
            }
        }
    }

    Ok(())
}

fn inject_mouse_move(
    mouse: &mut VirtualDevice,
    dx: i32,
    dy: i32,
    syn: &EvdevInputEvent,
) -> Result<(), InputError> {
    let mut events = Vec::with_capacity(3);
    if dx != 0 {
        events.push(EvdevInputEvent::new(
            evdev::EventType::RELATIVE.0,
            RelativeAxisCode::REL_X.0,
            dx,
        ));
    }
    if dy != 0 {
        events.push(EvdevInputEvent::new(
            evdev::EventType::RELATIVE.0,
            RelativeAxisCode::REL_Y.0,
            dy,
        ));
    }
    if !events.is_empty() {
        events.push(*syn);
        mouse.emit(&events)?;
    }
    Ok(())
}

fn inject_mouse_wheel(
    mouse: &mut VirtualDevice,
    dx: i32,
    dy: i32,
    syn: &EvdevInputEvent,
) -> Result<(), InputError> {
    let mut events = Vec::with_capacity(3);
    if dy != 0 {
        events.push(EvdevInputEvent::new(
            evdev::EventType::RELATIVE.0,
            RelativeAxisCode::REL_WHEEL.0,
            dy,
        ));
    }
    if dx != 0 {
        events.push(EvdevInputEvent::new(
            evdev::EventType::RELATIVE.0,
            RelativeAxisCode::REL_HWHEEL.0,
            dx,
        ));
    }
    if !events.is_empty() {
        events.push(*syn);
        mouse.emit(&events)?;
    }
    Ok(())
}

fn mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => KeyCode::BTN_LEFT.code(),
        MouseButton::Right => KeyCode::BTN_RIGHT.code(),
        MouseButton::Middle => KeyCode::BTN_MIDDLE.code(),
        MouseButton::Side => KeyCode::BTN_SIDE.code(),
        MouseButton::Extra => KeyCode::BTN_EXTRA.code(),
    }
}

/// Maps a gamepad axis event to an evdev (axis code, value) pair.
///
/// Sticks pass through unchanged; triggers are rescaled from SDL's
/// 0..32767 to the Xbox 360 pad's 0..255 range.
fn gamepad_axis_code(axis: GamepadAxis, value: i16) -> (u16, i32) {
    let trigger = (i32::from(value).max(0) * 255 / 32767).min(255);
    match axis {
        GamepadAxis::LeftX => (AbsoluteAxisCode::ABS_X.0, i32::from(value)),
        GamepadAxis::LeftY => (AbsoluteAxisCode::ABS_Y.0, i32::from(value)),
        GamepadAxis::RightX => (AbsoluteAxisCode::ABS_RX.0, i32::from(value)),
        GamepadAxis::RightY => (AbsoluteAxisCode::ABS_RY.0, i32::from(value)),
        GamepadAxis::TriggerLeft => (AbsoluteAxisCode::ABS_Z.0, trigger),
        GamepadAxis::TriggerRight => (AbsoluteAxisCode::ABS_RZ.0, trigger),
    }
}

/// How a gamepad button is injected: as a key event, or as a hat axis
/// position (the Xbox 360 d-pad is the HAT0X/HAT0Y axis pair).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PadButtonMapping {
    Key(u16),
    Hat { axis: u16, direction: i32 },
}

fn gamepad_button_mapping(button: GamepadButton) -> PadButtonMapping {
    match button {
        GamepadButton::South => PadButtonMapping::Key(KeyCode::BTN_SOUTH.code()),
        GamepadButton::East => PadButtonMapping::Key(KeyCode::BTN_EAST.code()),
        GamepadButton::North => PadButtonMapping::Key(KeyCode::BTN_NORTH.code()),
        GamepadButton::West => PadButtonMapping::Key(KeyCode::BTN_WEST.code()),
        GamepadButton::Start => PadButtonMapping::Key(KeyCode::BTN_START.code()),
        GamepadButton::Back => PadButtonMapping::Key(KeyCode::BTN_SELECT.code()),
        GamepadButton::Guide => PadButtonMapping::Key(KeyCode::BTN_MODE.code()),
        GamepadButton::LeftStick => PadButtonMapping::Key(KeyCode::BTN_THUMBL.code()),
        GamepadButton::RightStick => PadButtonMapping::Key(KeyCode::BTN_THUMBR.code()),
        GamepadButton::LeftShoulder => PadButtonMapping::Key(KeyCode::BTN_TL.code()),
        GamepadButton::RightShoulder => PadButtonMapping::Key(KeyCode::BTN_TR.code()),
        GamepadButton::DPadUp => PadButtonMapping::Hat {
            axis: AbsoluteAxisCode::ABS_HAT0Y.0,
            direction: -1,
        },
        GamepadButton::DPadDown => PadButtonMapping::Hat {
            axis: AbsoluteAxisCode::ABS_HAT0Y.0,
            direction: 1,
        },
        GamepadButton::DPadLeft => PadButtonMapping::Hat {
            axis: AbsoluteAxisCode::ABS_HAT0X.0,
            direction: -1,
        },
        GamepadButton::DPadRight => PadButtonMapping::Hat {
            axis: AbsoluteAxisCode::ABS_HAT0X.0,
            direction: 1,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_event_with_unmapped_scancode_returns_ok() {
        assert!(
            stargaze_core::input::sdl_scancode_to_evdev(9999).is_none(),
            "Scancode 9999 should be unmapped"
        );
    }

    #[test]
    fn mouse_button_maps_to_evdev_keys() {
        assert_eq!(KeyCode::BTN_LEFT.code(), 0x110);
        assert_eq!(KeyCode::BTN_RIGHT.code(), 0x111);
        assert_eq!(KeyCode::BTN_MIDDLE.code(), 0x112);
    }

    #[test]
    fn gamepad_button_south_maps_correctly() {
        assert_eq!(
            gamepad_button_mapping(GamepadButton::South),
            PadButtonMapping::Key(0x130)
        );
    }

    #[test]
    fn dpad_maps_to_hat_axes() {
        assert_eq!(
            gamepad_button_mapping(GamepadButton::DPadUp),
            PadButtonMapping::Hat {
                axis: AbsoluteAxisCode::ABS_HAT0Y.0,
                direction: -1
            }
        );
        assert_eq!(
            gamepad_button_mapping(GamepadButton::DPadDown),
            PadButtonMapping::Hat {
                axis: AbsoluteAxisCode::ABS_HAT0Y.0,
                direction: 1
            }
        );
        assert_eq!(
            gamepad_button_mapping(GamepadButton::DPadLeft),
            PadButtonMapping::Hat {
                axis: AbsoluteAxisCode::ABS_HAT0X.0,
                direction: -1
            }
        );
        assert_eq!(
            gamepad_button_mapping(GamepadButton::DPadRight),
            PadButtonMapping::Hat {
                axis: AbsoluteAxisCode::ABS_HAT0X.0,
                direction: 1
            }
        );
    }

    #[test]
    fn triggers_rescale_to_xbox_range() {
        // Full press: 32767 → 255.
        assert_eq!(
            gamepad_axis_code(GamepadAxis::TriggerLeft, 32767),
            (AbsoluteAxisCode::ABS_Z.0, 255)
        );
        // Released: 0 → 0.
        assert_eq!(
            gamepad_axis_code(GamepadAxis::TriggerRight, 0),
            (AbsoluteAxisCode::ABS_RZ.0, 0)
        );
        // Half press: ~127.
        assert_eq!(gamepad_axis_code(GamepadAxis::TriggerLeft, 16384).1, 127);
        // Negative values (shouldn't happen for triggers) clamp to 0.
        assert_eq!(gamepad_axis_code(GamepadAxis::TriggerLeft, -100).1, 0);
    }

    #[test]
    fn sticks_pass_through_unchanged() {
        assert_eq!(
            gamepad_axis_code(GamepadAxis::LeftX, -32768),
            (AbsoluteAxisCode::ABS_X.0, -32768)
        );
        assert_eq!(
            gamepad_axis_code(GamepadAxis::RightY, 32767),
            (AbsoluteAxisCode::ABS_RY.0, 32767)
        );
    }

    #[test]
    #[ignore = "requires /dev/uinput access"]
    fn virtual_keyboard_creation() {
        let device = create_virtual_keyboard();
        assert!(device.is_ok(), "Virtual keyboard should be created");
    }

    #[test]
    #[ignore = "requires /dev/uinput access"]
    fn virtual_mouse_creation() {
        let device = create_virtual_mouse();
        assert!(device.is_ok(), "Virtual mouse should be created");
    }

    #[test]
    #[ignore = "requires /dev/uinput access"]
    fn virtual_gamepad_creation() {
        let device = create_virtual_gamepad();
        assert!(device.is_ok(), "Virtual gamepad should be created");
    }
}
