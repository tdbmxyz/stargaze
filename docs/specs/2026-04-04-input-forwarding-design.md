# Input Forwarding (Client → Server) — Design Spec

**Sub-project:** 8 of 9 (Input Forwarding)

## Overview

Capture keyboard, mouse, and gamepad input from the SDL2 event loop on the client, serialize input events as `ControlMessage` variants over the existing QUIC reliable control stream, and inject them on the server via Linux uinput virtual devices using the `evdev` crate.

This sub-project reuses the existing bidirectional control stream already used for session handshake, IDR requests, and ping/pong. No new QUIC streams are opened.

## Target Environment

- **Client capture**: SDL2 event pump (already running for video/audio rendering)
- **Network**: Existing QUIC reliable control stream (`ControlMessage` enum, postcard + 4-byte LE length prefix)
- **Server injection**: Linux uinput via `evdev` crate — creates virtual keyboard, mouse, and gamepad devices
- **Permissions**: Server needs `/dev/uinput` access (udev rule, `input` group, or `chmod`)
- **Latency target**: <5ms event capture to uinput injection (LAN only)

## Architecture

### Data Flow

```
SDL2 Event Pump (client main thread)
    ├─ KeyDown/KeyUp → InputEvent::Keyboard { scancode, pressed }
    ├─ MouseMotion → InputEvent::MouseMove { dx, dy }
    ├─ MouseButtonDown/Up → InputEvent::MouseButton { button, pressed }
    ├─ MouseWheel → InputEvent::MouseWheel { dx, dy }
    ├─ ControllerAxisMotion → InputEvent::GamepadAxis { axis, value }
    └─ ControllerButtonDown/Up → InputEvent::GamepadButton { button, pressed }
    ↓
    std::sync::mpsc::Sender<InputEvent>  (unbounded, main thread → async task)
    ↓
    Client input sender task (tokio::spawn)
    ↓
    serialize_control_message(ControlMessage::Input(event))
    ↓
    quinn::SendStream.write_all()  (reliable QUIC control stream)
    ↓ network
    Server handle_control_messages() (reads from recv_stream)
    ↓
    match ControlMessage::Input(event) → tokio::sync::mpsc::Sender<InputEvent>
    ↓
    Input injection thread (std::thread)
    ├─ Virtual keyboard (evdev uinput) ← Keyboard events
    ├─ Virtual mouse (evdev uinput) ← MouseMove/MouseButton/MouseWheel events
    └─ Virtual gamepad (evdev uinput) ← GamepadAxis/GamepadButton events
```

### Module Layout

```
crates/stargaze-core/src/
    input.rs              # NEW: InputEvent enum, scancode mapping, mouse/gamepad types
    transport.rs          # MODIFIED: add ControlMessage::Input(InputEvent)
    lib.rs                # MODIFIED: add pub mod input

crates/stargaze-client/src/
    render/
        sdl.rs            # MODIFIED: capture input events in event loop, send via channel
        mod.rs            # MODIFIED: start_renderer gains input_tx parameter
    transport/
        mod.rs            # MODIFIED: connect() returns input_tx for sending; spawns input sender task
        receiver.rs       # MODIFIED: receive_loop gains input_rx param; sends InputEvent over control stream
    main.rs               # MODIFIED: wire input channel into renderer and transport

crates/stargaze-server/src/
    input/
        mod.rs            # NEW: InputSession, start_input_injection() — public API
        uinput.rs         # NEW: virtual device creation and event injection via evdev
    transport/
        sender.rs         # MODIFIED: handle_control_messages gains input_tx param, dispatches InputEvent
        mod.rs            # MODIFIED: pass input_tx to handle_control_messages, accept from caller
    main.rs               # MODIFIED: start input injection session, pass input_tx to transport
```

## Design Decisions

### 1. InputEvent as a Core Shared Type

Define `InputEvent` in `stargaze-core/src/input.rs` so both client and server share the exact same type. It derives `Serialize`/`Deserialize` for postcard compatibility. The enum is flat (no nesting) for minimal serialization overhead.

### 2. SDL Scancode → Linux evdev Keycode Mapping

SDL2 scancodes use USB HID codes. Linux evdev keycodes use a different numbering. We define a mapping function `sdl_scancode_to_evdev(scancode: u32) -> Option<u16>` in `stargaze-core/src/input.rs` that covers the ~120 most common keys (letters, digits, modifiers, function keys, arrows, numpad). Unknown scancodes are logged and dropped.

The `InputEvent::Keyboard` variant carries the raw SDL scancode as `u32`. The server-side input injection maps this to evdev at injection time, keeping the wire format platform-neutral.

**Update**: On reflection, to keep the wire protocol platform-neutral, we store the SDL scancode in the event. The mapping lives in the server's input module where evdev is available.

### 3. Reuse Existing Control Stream

Input events are sent as `ControlMessage::Input(InputEvent)` over the same reliable QUIC stream used for handshake/IDR/ping. This avoids opening a new stream and reuses the existing serialization infrastructure (`serialize_control_message`/`deserialize_control_message`).

Reliable delivery is appropriate for keyboard and button events (must not be lost). For mouse motion, minor delays are acceptable since the reliable stream on LAN has near-zero RTT.

### 4. Client-Side Channel: std::sync::mpsc (Unbounded)

The SDL2 event loop runs on the main thread (synchronous, no tokio runtime). To bridge to the async transport layer, input events are sent via `std::sync::mpsc::Sender<InputEvent>` from the main thread. An async task receives them with `tokio::sync::mpsc` bridging (the receiver side converts from blocking to async).

We use `std::sync::mpsc::channel()` (unbounded) because:
- The main thread must never block on input event sending (would stall rendering)
- Input events are small (~20 bytes serialized) and infrequent relative to video/audio
- The async sender task drains the channel continuously

### 5. Client Transport Architecture

Two options considered:

**Option A (chosen)**: Give `receive_loop` a new `mpsc::Receiver<InputEvent>` parameter. Inside `receive_loop`, `tokio::select!` between reading datagrams and receiving input events. When an input event arrives, serialize and write to `control_send`.

This keeps all control stream writing in one place (alongside IDR requests), avoids needing a second `SendStream` reference, and is consistent with the existing pattern.

**Option B (rejected)**: Spawn a separate async task that owns a cloned `SendStream`. Rejected because `quinn::SendStream` is not `Clone`, and sharing via `Arc<Mutex>` adds unnecessary complexity.

### 6. Server-Side Input Injection Session

Follows the established session pattern (like `EncoderSession`, `AudioEncoderSession`):
- `InputSession` struct with `thread_handle: Option<JoinHandle<()>>`, `shutdown: Arc<AtomicBool>`
- `stop()` method, `Drop` impl for cleanup
- Dedicated `std::thread` for the blocking injection loop
- Init error reporting via `std::sync::mpsc::channel` oneshot

Three virtual devices are created on the injection thread:
1. **Virtual keyboard**: EV_KEY with all mapped keys registered
2. **Virtual mouse**: EV_REL (X, Y, WHEEL, HWHEEL) + EV_KEY (BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA)
3. **Virtual gamepad**: EV_ABS (X, Y, RX, RY, Z, RZ for sticks/triggers) + EV_KEY (BTN_SOUTH, BTN_EAST, etc.)

### 7. Relative Mouse Mode

The client enables SDL2 relative mouse mode (`sdl.mouse().set_relative_mouse_mode(true)`) when the window has focus. This:
- Hides the cursor
- Confines it to the window
- Provides raw delta values in `MouseMotion` events (`xrel`, `yrel`)

Escape key exits relative mouse mode and breaks the main loop (existing behavior). A toggle key (e.g., `ScrollLock`) could be added later to release the mouse without quitting.

### 8. Gamepad Initialization

SDL2's `GameController` subsystem must be initialized to receive controller events. On startup, the client opens any already-connected controllers. New controllers connected during the session are handled via `ControllerDeviceAdded` events.

For MVP, only one gamepad is supported. The virtual gamepad on the server is always created regardless of whether a physical gamepad is connected on the client.

### 9. Event Batching

No explicit batching for MVP. Each SDL2 event becomes one `ControlMessage::Input(event)` written to the control stream individually. The QUIC reliable stream handles Nagle-like coalescing at the transport layer.

Future optimization: batch all events from one `poll_iter()` cycle into a single `ControlMessage::InputBatch(Vec<InputEvent>)` to reduce framing overhead.

## Input Event Types

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Key pressed or released. `scancode` is the SDL scancode (USB HID code).
    Keyboard { scancode: u32, pressed: bool },
    /// Relative mouse movement (deltas).
    MouseMove { dx: i32, dy: i32 },
    /// Mouse button pressed or released.
    MouseButton { button: MouseButton, pressed: bool },
    /// Mouse scroll wheel.
    MouseWheel { dx: i32, dy: i32 },
    /// Gamepad analog axis movement.
    GamepadAxis { axis: GamepadAxis, value: i16 },
    /// Gamepad button pressed or released.
    GamepadButton { button: GamepadButton, pressed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton { Left, Right, Middle, Side, Extra }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GamepadAxis { LeftX, LeftY, RightX, RightY, TriggerLeft, TriggerRight }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GamepadButton {
    South, East, North, West,
    Start, Back, Guide,
    LeftStick, RightStick,
    LeftShoulder, RightShoulder,
    DPadUp, DPadDown, DPadLeft, DPadRight,
}
```

## ControlMessage Extension

```rust
pub enum ControlMessage {
    // ... existing variants ...
    /// Client -> Server: input event from keyboard, mouse, or gamepad.
    Input(InputEvent),
}
```

## Scancode Mapping

The server maps SDL scancodes (u32) to Linux evdev keycodes (u16). The mapping covers:
- A-Z (SDL 4-29 → evdev 30-38, 44-54)
- 0-9 (SDL 39-48 → evdev 11, 2-10)
- Function keys F1-F12 (SDL 58-69 → evdev 59-68, 87-88)
- Modifiers (LCtrl, LShift, LAlt, LGui, RCtrl, RShift, RAlt, RGui)
- Navigation (Enter, Escape, Backspace, Tab, Space, arrows, Home, End, PgUp, PgDn, Insert, Delete)
- Punctuation and symbols

Unknown scancodes produce a `debug!` log and are dropped (no crash, no error).

## Testing Strategy

### Unit Tests

1. **InputEvent round-trip serialization** — serialize/deserialize each variant through `ControlMessage::Input`, verify equality
2. **Scancode mapping** — verify known SDL→evdev mappings (A, Space, Enter, Escape, F1, arrow keys)
3. **Scancode mapping unknown** — verify unknown scancodes return `None`
4. **MouseButton/GamepadAxis/GamepadButton enums** — verify Display/Debug formatting

### Integration Tests (ignored — need hardware/uinput)

1. **uinput virtual device creation** — create and destroy virtual keyboard/mouse/gamepad (needs /dev/uinput)
2. **Full input pipeline** — client captures events, sends over loopback QUIC, server injects via uinput

## Non-Goals

- Absolute mouse positioning (using relative deltas for game streaming)
- Multi-gamepad support (MVP: one gamepad)
- Touch input or pen tablet support
- Key repeat configuration (OS-level repeat on the server handles this)
- Input event batching optimization
- Mouse cursor visibility management beyond basic relative mode
- Hotkey system for releasing mouse capture (Escape exits for now)
