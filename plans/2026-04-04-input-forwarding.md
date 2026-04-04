# Input Forwarding (Client → Server) — Implementation Plan

**Sub-project:** 8 of 9
**Design spec:** `docs/specs/2026-04-04-input-forwarding-design.md`

## Issues to Address

1. Client SDL2 event loop only handles Quit and Escape — no keyboard, mouse, or gamepad input capture
2. No input event types exist in the shared core crate
3. Server has no input injection capability (no uinput/evdev integration)
4. Transport control channel only handles IDR requests, Ping, Pong — no input events

## Important Notes

- `evdev` crate on the server requires `/dev/uinput` access. In the devcontainer this may require `chmod 666 /dev/uinput` or running as root. Tests that create virtual devices should be `#[ignore]`.
- SDL2 scancodes use USB HID numbering (e.g., A=4). Linux evdev keycodes use different numbering (e.g., A=30). The mapping function lives in the core crate but is only used server-side.
- The client's `receive_loop` currently owns `control_send: quinn::SendStream` exclusively. Adding input sending requires passing input events into that loop via a new channel parameter and using `tokio::select!` to multiplex datagram reading with input event receiving.
- `std::sync::mpsc::Sender` is used from the SDL event loop (main thread, synchronous) because the SDL loop doesn't run in a tokio context. The async bridge uses `tokio::sync::mpsc` between the sync sender and the transport task.
- The `evdev` crate's `VirtualDevice::emit()` method writes to `/dev/uinput` and is blocking. It must run on a dedicated thread, not in async context.
- Every `emit()` call must include an `EV_SYN/SYN_REPORT` event at the end to flush the event batch.

## Implementation Strategy

### Task 1: Core Input Types + Scancode Mapping

Add `crates/stargaze-core/src/input.rs`:
- `InputEvent` enum with variants: `Keyboard`, `MouseMove`, `MouseButton`, `MouseWheel`, `GamepadAxis`, `GamepadButton`
- Supporting enums: `MouseButton`, `GamepadAxis`, `GamepadButton`
- `sdl_scancode_to_evdev(scancode: u32) -> Option<u16>` mapping function covering ~120 keys
- All types derive `Debug, Clone, PartialEq, Eq, Serialize, Deserialize`

Add `ControlMessage::Input(InputEvent)` variant to `crates/stargaze-core/src/transport.rs`.

Add `pub mod input` to `crates/stargaze-core/src/lib.rs`.

Unit tests:
- `InputEvent` round-trip through `ControlMessage::Input` serialization
- Known scancode mappings (A, Space, Enter, Escape, F1, arrows)
- Unknown scancodes return `None`

**Files**: `crates/stargaze-core/src/input.rs`, `crates/stargaze-core/src/transport.rs`, `crates/stargaze-core/src/lib.rs`
**Commit**: `feat(core): add input event types and SDL-to-evdev scancode mapping`

### Task 2: Server Dependencies

Add `evdev = "0.13"` to `crates/stargaze-server/Cargo.toml`.

**Files**: `crates/stargaze-server/Cargo.toml`
**Commit**: `chore(deps): add evdev crate to server for uinput input injection`

### Task 3: Client Input Capture

Modify `crates/stargaze-client/src/render/sdl.rs` — `run_sdl_loop()`:
- Accept new parameter: `input_tx: std::sync::mpsc::Sender<InputEvent>`
- Enable relative mouse mode: `sdl.mouse().set_relative_mouse_mode(true)`
- Initialize SDL2 GameController subsystem; open any connected controllers
- In the event loop match arms, handle:
  - `KeyDown { scancode, .. }` → send `InputEvent::Keyboard { scancode, pressed: true }` (skip if Escape — still breaks main loop)
  - `KeyUp { scancode, .. }` → send `InputEvent::Keyboard { scancode, pressed: false }`
  - `MouseMotion { xrel, yrel, .. }` → send `InputEvent::MouseMove { dx: xrel, dy: yrel }`
  - `MouseButtonDown { mouse_btn, .. }` → send `InputEvent::MouseButton { button, pressed: true }`
  - `MouseButtonUp { mouse_btn, .. }` → send `InputEvent::MouseButton { button, pressed: false }`
  - `MouseWheel { x, y, .. }` → send `InputEvent::MouseWheel { dx: x, dy: y }`
  - `ControllerAxisMotion { axis, value, .. }` → send `InputEvent::GamepadAxis { axis, value }`
  - `ControllerButtonDown { button, .. }` → send `InputEvent::GamepadButton { button, pressed: true }`
  - `ControllerButtonUp { button, .. }` → send `InputEvent::GamepadButton { button, pressed: false }`
  - `ControllerDeviceAdded { which, .. }` → open the controller
- Map SDL2 `MouseButton` enum to `input::MouseButton`, SDL2 `Axis` to `input::GamepadAxis`, SDL2 `Button` to `input::GamepadButton` via helper functions
- Use `input_tx.send(event).ok()` — if channel is closed, silently drop (transport shutting down)

Modify `crates/stargaze-client/src/render/mod.rs` — `start_renderer()`:
- Add `input_tx: std::sync::mpsc::Sender<InputEvent>` parameter
- Pass through to `run_sdl_loop`

**Files**: `crates/stargaze-client/src/render/sdl.rs`, `crates/stargaze-client/src/render/mod.rs`
**Commit**: `feat(client): capture keyboard, mouse, and gamepad input in SDL event loop`

### Task 4: Client Transport — Input Sending

Modify `crates/stargaze-client/src/transport/receiver.rs` — `receive_loop()`:
- Add parameter: `input_rx: tokio::sync::mpsc::Receiver<InputEvent>`
- Change the main loop to `tokio::select!`:
  - Branch 1: `connection.read_datagram()` — existing datagram handling
  - Branch 2: `input_rx.recv()` — serialize as `ControlMessage::Input(event)`, write to `control_send`
  - Branch 3: input_rx closed → log and continue (renderer shut down, but datagrams may still arrive)

Modify `crates/stargaze-client/src/transport/mod.rs` — `connect()`:
- Create `tokio::sync::mpsc::channel::<InputEvent>(64)` — bounded, 64 capacity
- Pass `input_rx` into `receive_loop()`
- Return 4-tuple: `(ClientTransport, video_rx, audio_rx, input_tx)`

**Files**: `crates/stargaze-client/src/transport/receiver.rs`, `crates/stargaze-client/src/transport/mod.rs`
**Commit**: `feat(client): send input events over QUIC control stream`

### Task 5: Server Input Injection Module

Create `crates/stargaze-server/src/input/mod.rs`:
- `InputSession` struct: `thread_handle: Option<JoinHandle<()>>`, `shutdown: Arc<AtomicBool>`
- `stop()` method, `Drop` impl (same pattern as `EncoderSession`)
- `start_input_injection(input_rx: mpsc::Receiver<InputEvent>) -> Result<InputSession, InputError>` — spawns dedicated thread
- Init error reporting via `std::sync::mpsc::channel` oneshot

Create `crates/stargaze-server/src/input/uinput.rs`:
- `create_virtual_keyboard() -> Result<VirtualDevice, InputError>` — register all mapped evdev keys
- `create_virtual_mouse() -> Result<VirtualDevice, InputError>` — register REL_X, REL_Y, REL_WHEEL, REL_HWHEEL + BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA
- `create_virtual_gamepad() -> Result<VirtualDevice, InputError>` — register ABS axes (X, Y, RX, RY, Z, RZ) with ranges (-32768..32767 for sticks, 0..255 for triggers) + gamepad buttons
- `run_injection_loop(keyboard, mouse, gamepad, input_rx, shutdown)` — blocking loop: `input_rx.blocking_recv()`, match on `InputEvent` variant, call appropriate `device.emit(&[events, SYN_REPORT])`

Add `InputError` to `crates/stargaze-core/src/error.rs` or define locally in the input module using `thiserror`.

**Files**: `crates/stargaze-server/src/input/mod.rs`, `crates/stargaze-server/src/input/uinput.rs`
**Commit**: `feat(server): add uinput virtual device input injection`

### Task 6: Server Transport — InputEvent Dispatch

Modify `crates/stargaze-server/src/transport/sender.rs` — `handle_control_messages()`:
- Add parameter: `input_tx: &mpsc::Sender<InputEvent>`
- Add match arm: `ControlMessage::Input(event) => input_tx.send(event).await` (log warning if channel full)

Modify `crates/stargaze-server/src/transport/mod.rs`:
- `start_server_transport()`: accept `input_tx: mpsc::Sender<InputEvent>` parameter
- Pass `input_tx` through `run_server_loop()` → `handle_control_messages()`

**Files**: `crates/stargaze-server/src/transport/sender.rs`, `crates/stargaze-server/src/transport/mod.rs`
**Commit**: `feat(server): dispatch input events from control stream to injection pipeline`

### Task 7: Wire Input Pipeline

Client `main.rs`:
- Receive `(client_transport, video_frames, audio_frames, input_tx)` from `transport::connect()`
- Create `std::sync::mpsc::channel::<InputEvent>()` for SDL→async bridge
- Spawn a tokio task that reads from `std::sync::mpsc::Receiver` and forwards to `input_tx` (the `tokio::sync::mpsc::Sender`)
- Pass `sdl_input_tx` (the `std::sync::mpsc::Sender`) into `render::start_renderer()`

Server `main.rs`:
- Create `tokio::sync::mpsc::channel::<InputEvent>(64)` — `(input_tx, input_rx)`
- Start input injection: `input::start_input_injection(input_rx)?`
- Pass `input_tx` to `transport::start_server_transport()`
- On shutdown, stop `input_session` alongside other sessions

**Files**: `crates/stargaze-client/src/main.rs`, `crates/stargaze-server/src/main.rs`
**Commit**: `feat: wire input forwarding pipeline in client and server`

### Task 8: Tests + Final Verification

Run full verification:
- `cargo test --workspace`
- `cargo clippy --workspace -- -W clippy::pedantic`
- `cargo fmt --check`

Fix any issues found.

**Commit**: docs + test commits as needed
**Final commit**: `docs: add input forwarding design spec and implementation plan`

## Tests

### Unit Tests (always run)
- `InputEvent::Keyboard` round-trip through `ControlMessage::Input` postcard serialization
- `InputEvent::MouseMove` round-trip serialization
- `InputEvent::MouseButton` round-trip serialization
- `InputEvent::MouseWheel` round-trip serialization
- `InputEvent::GamepadAxis` round-trip serialization
- `InputEvent::GamepadButton` round-trip serialization
- `sdl_scancode_to_evdev` known mappings (A=4→30, Space=44→57, Enter=40→28, Escape=41→1)
- `sdl_scancode_to_evdev` unknown scancode returns `None`
- `MouseButton`, `GamepadAxis`, `GamepadButton` enum coverage

### Integration Tests (ignored — need /dev/uinput)
- Virtual keyboard creation + key injection
- Virtual mouse creation + relative movement injection
- Virtual gamepad creation + axis/button injection
