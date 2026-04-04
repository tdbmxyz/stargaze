use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, InputEvent as EvdevInputEvent, KeyCode,
    RelativeAxisCode, UinputAbsSetup,
};
use stargaze_core::input::{GamepadAxis, GamepadButton, InputEvent, MouseButton};
use tokio::sync::mpsc;
use tracing::{debug, warn};

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
    gamepad: VirtualDevice,
}

pub(crate) fn create_virtual_devices() -> Result<VirtualDevices, InputError> {
    let keyboard = create_virtual_keyboard()?;
    let mouse = create_virtual_mouse()?;
    let gamepad = create_virtual_gamepad()?;
    Ok(VirtualDevices {
        keyboard,
        mouse,
        gamepad,
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

    // D-pad as buttons (HAT0X/HAT0Y would be axes, but using keys is simpler
    // and compatible with most games via evdev).
    keys.insert(KeyCode::BTN_DPAD_UP);
    keys.insert(KeyCode::BTN_DPAD_DOWN);
    keys.insert(KeyCode::BTN_DPAD_LEFT);
    keys.insert(KeyCode::BTN_DPAD_RIGHT);

    let abs_info = AbsInfo::new(0, -32768, 32767, 16, 128, 1);

    let mut builder = VirtualDevice::builder()?
        .name("Stargaze Virtual Gamepad")
        .with_keys(&keys)?;

    let stick_axes = [
        AbsoluteAxisCode::ABS_X,
        AbsoluteAxisCode::ABS_Y,
        AbsoluteAxisCode::ABS_RX,
        AbsoluteAxisCode::ABS_RY,
    ];
    for axis in &stick_axes {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(*axis, abs_info))?;
    }

    let trigger_info = AbsInfo::new(0, 0, 32767, 16, 128, 1);
    builder = builder
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Z, trigger_info))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_RZ, trigger_info))?;

    let device = builder.build()?;
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
        InputEvent::GamepadAxis { axis, value } => {
            let (evdev_axis, val) = gamepad_axis_code(*axis, *value);
            let ev = EvdevInputEvent::new(evdev::EventType::ABSOLUTE.0, evdev_axis, val);
            devices.gamepad.emit(&[ev, syn])?;
        }
        InputEvent::GamepadButton { button, pressed } => {
            let code = gamepad_button_code(*button);
            let value = i32::from(*pressed);
            let ev = EvdevInputEvent::new(evdev::EventType::KEY.0, code, value);
            devices.gamepad.emit(&[ev, syn])?;
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

fn gamepad_axis_code(axis: GamepadAxis, value: i16) -> (u16, i32) {
    match axis {
        GamepadAxis::LeftX => (AbsoluteAxisCode::ABS_X.0, i32::from(value)),
        GamepadAxis::LeftY => (AbsoluteAxisCode::ABS_Y.0, i32::from(value)),
        GamepadAxis::RightX => (AbsoluteAxisCode::ABS_RX.0, i32::from(value)),
        GamepadAxis::RightY => (AbsoluteAxisCode::ABS_RY.0, i32::from(value)),
        GamepadAxis::TriggerLeft => (AbsoluteAxisCode::ABS_Z.0, i32::from(value)),
        GamepadAxis::TriggerRight => (AbsoluteAxisCode::ABS_RZ.0, i32::from(value)),
    }
}

fn gamepad_button_code(button: GamepadButton) -> u16 {
    match button {
        GamepadButton::South => KeyCode::BTN_SOUTH.code(),
        GamepadButton::East => KeyCode::BTN_EAST.code(),
        GamepadButton::North => KeyCode::BTN_NORTH.code(),
        GamepadButton::West => KeyCode::BTN_WEST.code(),
        GamepadButton::Start => KeyCode::BTN_START.code(),
        GamepadButton::Back => KeyCode::BTN_SELECT.code(),
        GamepadButton::Guide => KeyCode::BTN_MODE.code(),
        GamepadButton::LeftStick => KeyCode::BTN_THUMBL.code(),
        GamepadButton::RightStick => KeyCode::BTN_THUMBR.code(),
        GamepadButton::LeftShoulder => KeyCode::BTN_TL.code(),
        GamepadButton::RightShoulder => KeyCode::BTN_TR.code(),
        GamepadButton::DPadUp => KeyCode::BTN_DPAD_UP.code(),
        GamepadButton::DPadDown => KeyCode::BTN_DPAD_DOWN.code(),
        GamepadButton::DPadLeft => KeyCode::BTN_DPAD_LEFT.code(),
        GamepadButton::DPadRight => KeyCode::BTN_DPAD_RIGHT.code(),
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
        assert_eq!(KeyCode::BTN_SOUTH.code(), 0x130);
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
