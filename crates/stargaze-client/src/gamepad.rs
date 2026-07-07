//! Gamepad pass-through: forwards physical controllers at the evdev
//! level so the server clones the real device instead of emulating an
//! Xbox 360 pad.
//!
//! A background scanner enumerates `/dev/input/event*`, and for every
//! gamepad-capable node it can open it reads the device's identity
//! (name, bus/vendor/product/version) and capabilities (buttons, axes
//! with ranges), grabs the node exclusively (`EVIOCGRAB`), and streams
//! raw events to the server. The server rebuilds an identical uinput
//! device, so the host sees e.g. a real "Steam Controller" or
//! "Steam Deck" instead of "Microsoft X-Box 360 pad".
//!
//! Fallback: any device that cannot be opened or grabbed (permissions,
//! exotic nodes) is left untouched — SDL still sees it, and the
//! existing SDL → Xbox-360-emulation path picks it up. The SDL loop
//! consults [`SharedGamepads::is_grabbed`] to skip devices this module
//! already owns, and both paths allocate pad slots from the same table
//! so the server never sees two controllers on one slot.
//!
//! Valve controllers (Steam Controller, Steam Deck) are deliberately
//! excluded from pass-through: hosts only drive them through Steam's
//! hidraw stack, and SDL's controller database has no mapping for
//! their evdev nodes — an identity clone on the server enumerates but
//! is invisible to games. Grabbing them is also fragile: hid-steam
//! removes its evdev node whenever anything (Steam, SDL's HIDAPI)
//! opens the hidraw side. They take the SDL → Xbox 360 emulation path;
//! true identity pass-through would require hidraw/uhid forwarding.

use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evdev::{Device, KeyCode};
use stargaze_core::input::{
    AbsAxisSpec, GamepadDescriptor, InputEvent, MAX_GAMEPADS, RawGamepadEvent,
};
use tracing::{debug, info, warn};

/// How often the scanner looks for newly connected controllers.
const SCAN_INTERVAL: Duration = Duration::from_millis(1000);

/// Poll timeout for reader threads; bounds how long a stop request
/// waits for a reader blocked on a quiet device.
const READER_POLL_TIMEOUT_MS: i32 = 250;

/// Valve's USB vendor id (Steam Controller, Steam Deck, dongles).
const VALVE_VENDOR_ID: u16 = 0x28de;

/// How long Select+Start must be held together to end the session —
/// the controller-only equivalent of Ctrl+Alt+Shift+Q, for devices
/// without a keyboard (Steam Deck).
pub const QUIT_CHORD_HOLD: Duration = Duration::from_secs(1);

/// Tracks the Select+Start "end session" chord on one input device.
///
/// The presses themselves are still forwarded to the server (they
/// can't be retracted once the chord completes); the remote side sees
/// a Select+Start tap before the session ends.
#[derive(Debug, Default)]
pub struct QuitChord {
    select_since: Option<std::time::Instant>,
    start_since: Option<std::time::Instant>,
}

impl QuitChord {
    pub fn set_select(&mut self, pressed: bool) {
        Self::set(&mut self.select_since, pressed);
    }

    pub fn set_start(&mut self, pressed: bool) {
        Self::set(&mut self.start_since, pressed);
    }

    fn set(slot: &mut Option<std::time::Instant>, pressed: bool) {
        if pressed {
            if slot.is_none() {
                *slot = Some(std::time::Instant::now());
            }
        } else {
            *slot = None;
        }
    }

    /// True once both buttons have been held together for
    /// [`QUIT_CHORD_HOLD`].
    #[must_use]
    pub fn fired(&self) -> bool {
        match (self.select_since, self.start_since) {
            (Some(a), Some(b)) => a.max(b).elapsed() >= QUIT_CHORD_HOLD,
            _ => false,
        }
    }
}

/// Identifies who owns a pad slot: the SDL emulation path (keyed by SDL
/// joystick instance id) or the evdev pass-through path (keyed by the
/// device node path).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PadKey {
    /// SDL joystick instance id (emulated Xbox 360 path).
    Sdl(u32),
    /// Evdev device node (pass-through path).
    Evdev(PathBuf),
}

struct SharedState {
    /// `slots[i]` holds the key occupying slot `i`.
    slots: [Option<PadKey>; MAX_GAMEPADS as usize],
    /// (vendor, product) pairs of devices currently grabbed by the
    /// pass-through path; the SDL loop skips these.
    grabbed: HashSet<(u16, u16)>,
    /// Device nodes currently owned by a pass-through reader thread.
    active_nodes: HashSet<PathBuf>,
    /// Nodes the scanner decided to leave alone (Valve devices, grab
    /// failures), so the decision is logged once instead of every scan.
    /// Pruned when the node disappears, giving re-plugged devices a
    /// fresh chance.
    ignored_nodes: HashSet<PathBuf>,
    /// (vendor, product) pairs whose pass-through ended because the
    /// device node vanished. The SDL loop uses this to warn loudly when
    /// such a device resurfaces on the emulation path instead of being
    /// silently downgraded to an Xbox 360 pad.
    lost: HashSet<(u16, u16)>,
}

/// Pad slot table and grab registry shared between the SDL event loop
/// and the pass-through scanner/reader threads.
pub struct SharedGamepads {
    state: Mutex<SharedState>,
    /// Bumped whenever the grabbed set changes, so the SDL loop can
    /// cheaply detect that a device it emulates was taken over.
    generation: AtomicU64,
    /// Set when a pass-through reader sees the Select+Start quit chord;
    /// the render loop polls it and ends the session.
    quit_requested: AtomicBool,
}

impl SharedGamepads {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SharedState {
                slots: [const { None }; MAX_GAMEPADS as usize],
                grabbed: HashSet::new(),
                active_nodes: HashSet::new(),
                ignored_nodes: HashSet::new(),
                lost: HashSet::new(),
            }),
            generation: AtomicU64::new(0),
            quit_requested: AtomicBool::new(false),
        })
    }

    /// Signals that the user asked to end the session via the
    /// controller quit chord.
    pub fn request_quit(&self) {
        self.quit_requested.store(true, Ordering::Relaxed);
    }

    /// True when the controller quit chord fired on any device.
    #[must_use]
    pub fn quit_requested(&self) -> bool {
        self.quit_requested.load(Ordering::Relaxed)
    }

    /// Assigns the lowest free slot to `key` and returns it.
    ///
    /// Returns the existing slot if the key is already registered, or
    /// `None` if all slots are taken.
    ///
    /// # Panics
    ///
    /// Panics if the slot table lock is poisoned.
    pub fn allocate(&self, key: &PadKey) -> Option<u8> {
        let mut state = self.state.lock().unwrap();
        if let Some(slot) = Self::find(&state, key) {
            return Some(slot);
        }
        let free = state.slots.iter().position(Option::is_none)?;
        state.slots[free] = Some(key.clone());
        u8::try_from(free).ok()
    }

    /// Frees the slot held by `key`, returning it.
    ///
    /// # Panics
    ///
    /// Panics if the slot table lock is poisoned.
    pub fn release(&self, key: &PadKey) -> Option<u8> {
        let mut state = self.state.lock().unwrap();
        let slot = Self::find(&state, key)?;
        state.slots[slot as usize] = None;
        Some(slot)
    }

    /// Returns the slot held by `key`, if any.
    ///
    /// # Panics
    ///
    /// Panics if the slot table lock is poisoned.
    pub fn get(&self, key: &PadKey) -> Option<u8> {
        Self::find(&self.state.lock().unwrap(), key)
    }

    /// Whether a device with this (vendor, product) is grabbed by the
    /// pass-through path. The SDL loop must not emulate such devices.
    ///
    /// # Panics
    ///
    /// Panics if the slot table lock is poisoned.
    pub fn is_grabbed(&self, vendor: u16, product: u16) -> bool {
        self.state
            .lock()
            .unwrap()
            .grabbed
            .contains(&(vendor, product))
    }

    /// Monotonic counter, bumped whenever the grabbed set changes.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn find(state: &SharedState, key: &PadKey) -> Option<u8> {
        state
            .slots
            .iter()
            .position(|s| s.as_ref() == Some(key))
            .and_then(|i| u8::try_from(i).ok())
    }

    /// Whether pass-through for this (vendor, product) previously ended
    /// with the device node vanishing (e.g. another process opened the
    /// controller's hidraw node and the kernel dropped the evdev one).
    ///
    /// # Panics
    ///
    /// Panics if the slot table lock is poisoned.
    pub fn passthrough_was_lost(&self, vendor: u16, product: u16) -> bool {
        self.state.lock().unwrap().lost.contains(&(vendor, product))
    }

    fn mark_grabbed(&self, node: PathBuf, vendor: u16, product: u16) {
        let mut state = self.state.lock().unwrap();
        state.grabbed.insert((vendor, product));
        state.active_nodes.insert(node);
        // The device is passed through (again); clear any stale loss.
        state.lost.remove(&(vendor, product));
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn mark_lost(&self, vendor: u16, product: u16) {
        self.state.lock().unwrap().lost.insert((vendor, product));
    }

    fn unmark_grabbed(&self, node: &PathBuf, vendor: u16, product: u16) {
        let mut state = self.state.lock().unwrap();
        state.grabbed.remove(&(vendor, product));
        state.active_nodes.remove(node);
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn is_active_node(&self, node: &PathBuf) -> bool {
        self.state.lock().unwrap().active_nodes.contains(node)
    }

    fn active_node_count(&self) -> usize {
        self.state.lock().unwrap().active_nodes.len()
    }

    fn mark_ignored(&self, node: PathBuf) {
        self.state.lock().unwrap().ignored_nodes.insert(node);
    }

    fn is_ignored(&self, node: &PathBuf) -> bool {
        self.state.lock().unwrap().ignored_nodes.contains(node)
    }

    /// Drops ignore records for nodes that no longer exist, so a device
    /// re-plugged at a recycled path is evaluated again.
    fn prune_ignored<'a>(&self, existing: impl Iterator<Item = &'a PathBuf>) {
        let existing: HashSet<&PathBuf> = existing.collect();
        self.state
            .lock()
            .unwrap()
            .ignored_nodes
            .retain(|node| existing.contains(node));
    }
}

/// Extracts (vendor, product) from an SDL joystick GUID.
///
/// SDL encodes the USB ids little-endian at fixed offsets: bytes 4-5
/// hold the vendor id and bytes 8-9 the product id.
#[must_use]
pub fn guid_vendor_product(guid: &[u8; 16]) -> (u16, u16) {
    let vendor = u16::from_le_bytes([guid[4], guid[5]]);
    let product = u16::from_le_bytes([guid[8], guid[9]]);
    (vendor, product)
}

/// Whether an evdev key set describes a gamepad.
///
/// The kernel convention (Documentation/input/gamepad.rst) is that
/// gamepads report `BTN_GAMEPAD` (= `BTN_SOUTH`).
fn is_gamepad(keys: &evdev::AttributeSetRef<KeyCode>) -> bool {
    keys.contains(KeyCode::BTN_SOUTH)
}

/// Builds the wire descriptor from an opened evdev device.
fn build_descriptor(device: &Device) -> GamepadDescriptor {
    let id = device.input_id();
    let keys = device
        .supported_keys()
        .map(|set| set.iter().map(KeyCode::code).collect())
        .unwrap_or_default();
    let abs_axes = device
        .get_absinfo()
        .map(|iter| {
            iter.map(|(code, info)| AbsAxisSpec {
                code: code.0,
                value: info.value(),
                minimum: info.minimum(),
                maximum: info.maximum(),
                fuzz: info.fuzz(),
                flat: info.flat(),
                resolution: info.resolution(),
            })
            .collect()
        })
        .unwrap_or_default();

    GamepadDescriptor {
        name: device.name().unwrap_or("Unknown Gamepad").to_string(),
        bus_type: id.bus_type().0,
        vendor: id.vendor(),
        product: id.product(),
        version: id.version(),
        keys,
        abs_axes,
    }
}

/// Stops the pass-through scanner and its reader threads, releasing
/// every grabbed device back to the rest of the system (so e.g. a
/// launcher menu shown after the session sees the controllers again).
pub struct PassthroughHandle {
    stop: Arc<AtomicBool>,
    shared: Arc<SharedGamepads>,
}

impl PassthroughHandle {
    /// Signals all pass-through threads to exit and ungrab, then waits
    /// (bounded) for the readers to actually release their grabs — the
    /// next session's initial scan must not race a dying reader's
    /// grab, or the device would be marked ignored (EBUSY) and fall
    /// back to Xbox 360 emulation for that whole session.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // Readers poll every READER_POLL_TIMEOUT_MS; give them a few
        // rounds before giving up (a vanished device errors out of its
        // reader on its own).
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        while self.shared.active_node_count() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if self.shared.active_node_count() > 0 {
            warn!("Some pass-through gamepads did not release within 1s");
        }
    }
}

/// Watches the local volume buttons for the session-end chord: both
/// volume keys held together for [`QUIT_CHORD_HOLD`].
///
/// The escape hatch for the built-in-controller handoff
/// (`forward_builtin_controller`): with the Deck's controller tunneled
/// to the server there is no local gamepad left to carry Select+Start,
/// but the volume keys live on a separate local input device. The
/// device is read WITHOUT grabbing, so volume control keeps working.
///
/// Returns a stop flag; set it at session teardown (the thread exits
/// within one poll interval).
pub fn start_volume_quit_watch(shared: Arc<SharedGamepads>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("stargaze-volume-quit".into())
        .spawn(move || {
            let Some((path, mut device)) = evdev::enumerate().find(|(_, d)| {
                d.supported_keys().is_some_and(|keys| {
                    keys.contains(KeyCode::KEY_VOLUMEUP) && keys.contains(KeyCode::KEY_VOLUMEDOWN)
                })
            }) else {
                warn!(
                    "No volume-key device found; the volume-chord session \
                     exit is unavailable (use the server side or suspend \
                     to end the session)"
                );
                return;
            };
            info!(
                device = %path.display(),
                "Watching volume keys: hold Vol+ and Vol- together to end the session"
            );
            let mut chord = QuitChord::default();
            loop {
                if thread_stop.load(Ordering::Relaxed) {
                    return;
                }
                if chord.fired() && !shared.quit_requested() {
                    info!("Volume keys held: requesting session end");
                    shared.request_quit();
                }
                if !wait_readable(&device) {
                    continue;
                }
                match device.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            if ev.event_type() == evdev::EventType::KEY {
                                if ev.code() == KeyCode::KEY_VOLUMEDOWN.code() {
                                    chord.set_select(ev.value() != 0);
                                } else if ev.code() == KeyCode::KEY_VOLUMEUP.code() {
                                    chord.set_start(ev.value() != 0);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Volume-key device read failed ({e}); chord watcher exiting");
                        return;
                    }
                }
            }
        });
    if let Err(e) = spawned {
        warn!("Failed to spawn volume-chord watcher: {e}");
    }
    stop
}

/// Starts the pass-through scanner thread.
///
/// The initial scan runs synchronously before this returns, so devices
/// present at startup are already grabbed by the time the SDL event
/// loop starts delivering `ControllerDeviceAdded` events.
pub fn start_passthrough(
    shared: Arc<SharedGamepads>,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
) -> PassthroughHandle {
    let stop = Arc::new(AtomicBool::new(false));
    scan_once(&shared, &input_tx, &stop);
    let handle = PassthroughHandle {
        stop: Arc::clone(&stop),
        shared: Arc::clone(&shared),
    };
    let scan_stop = Arc::clone(&stop);
    let spawned = std::thread::Builder::new()
        .name("stargaze-gamepad-scan".into())
        .spawn(move || {
            loop {
                // Sleep in short slices so a stop request doesn't wait
                // out a full scan interval.
                let deadline = std::time::Instant::now() + SCAN_INTERVAL;
                while std::time::Instant::now() < deadline {
                    if scan_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                scan_once(&shared, &input_tx, &scan_stop);
            }
        });
    if let Err(e) = spawned {
        warn!("Failed to spawn gamepad scanner thread: {e}");
    }
    handle
}

/// One enumeration pass: claim every new gamepad node we can grab.
fn scan_once(
    shared: &Arc<SharedGamepads>,
    input_tx: &std::sync::mpsc::Sender<InputEvent>,
    stop: &Arc<AtomicBool>,
) {
    let nodes: Vec<(PathBuf, Device)> = evdev::enumerate().collect();
    shared.prune_ignored(nodes.iter().map(|(path, _)| path));
    for (path, device) in nodes {
        // A stop can arrive mid-scan (session ending); don't grab
        // devices for a session that is already going away.
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if shared.is_active_node(&path) || shared.is_ignored(&path) {
            continue;
        }
        let Some(keys) = device.supported_keys() else {
            continue;
        };
        if !is_gamepad(keys) {
            continue;
        }
        claim_device(shared, input_tx, path, device, stop);
    }
}

/// Grabs a gamepad node, announces it to the server, and spawns its
/// reader thread. On any failure the device is left to the SDL
/// emulation path.
fn claim_device(
    shared: &Arc<SharedGamepads>,
    input_tx: &std::sync::mpsc::Sender<InputEvent>,
    path: PathBuf,
    mut device: Device,
    stop: &Arc<AtomicBool>,
) {
    let descriptor = build_descriptor(&device);

    if descriptor.vendor == VALVE_VENDOR_ID {
        // A server-side evdev clone of a Valve controller is invisible
        // to games (no SDL mapping; hosts drive these via Steam/hidraw),
        // and grabbing it races hid-steam's node removal. See module docs.
        info!(
            name = %descriptor.name,
            "Valve controller: skipping evdev pass-through (hosts only \
             support these via Steam/hidraw); using Xbox 360 emulation"
        );
        shared.mark_ignored(path);
        return;
    }

    if let Err(e) = device.grab() {
        // Something else holds the device (or we lack permissions).
        // SDL will emulate it instead.
        info!(
            name = %descriptor.name,
            path = %path.display(),
            "Cannot grab gamepad for pass-through ({e}); \
             falling back to Xbox 360 emulation"
        );
        shared.mark_ignored(path);
        return;
    }

    let key = PadKey::Evdev(path.clone());
    let Some(pad) = shared.allocate(&key) else {
        warn!(
            name = %descriptor.name,
            "All gamepad slots taken, ignoring controller"
        );
        let _ = device.ungrab();
        return;
    };

    info!(
        name = %descriptor.name,
        pad,
        vendor = format!("{:04x}", descriptor.vendor),
        product = format!("{:04x}", descriptor.product),
        "Gamepad pass-through active (device cloned on the server)"
    );
    shared.mark_grabbed(path.clone(), descriptor.vendor, descriptor.product);

    let announce = InputEvent::GamepadPassthroughConnected {
        pad,
        descriptor: descriptor.clone(),
    };
    if input_tx.send(announce).is_err() {
        return; // Transport gone; the session is ending.
    }

    let shared = Arc::clone(shared);
    let input_tx = input_tx.clone();
    let stop = Arc::clone(stop);
    let spawned = std::thread::Builder::new()
        .name(format!("stargaze-pad{pad}"))
        .spawn(move || read_loop(&shared, &input_tx, &path, device, pad, &descriptor, &stop));
    if let Err(e) = spawned {
        warn!("Failed to spawn gamepad reader thread: {e}");
    }
}

/// Waits until the device has readable events or the timeout elapses.
/// Returns `false` on timeout (caller should re-check the stop flag).
fn wait_readable(device: &Device) -> bool {
    let mut pfd = libc::pollfd {
        fd: device.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pfd points to a valid pollfd for the duration of the call.
    let ret = unsafe { libc::poll(&raw mut pfd, 1, READER_POLL_TIMEOUT_MS) };
    // Errors (e.g. EINTR) count as "not readable": the caller re-checks
    // the stop flag and retries instead of entering a blocking read.
    ret > 0
}

/// Blocking per-device reader: forwards raw events until the device
/// disappears or the transport closes.
#[allow(clippy::too_many_arguments)]
fn read_loop(
    shared: &Arc<SharedGamepads>,
    input_tx: &std::sync::mpsc::Sender<InputEvent>,
    path: &PathBuf,
    mut device: Device,
    pad: u8,
    descriptor: &GamepadDescriptor,
    stop: &Arc<AtomicBool>,
) {
    let mut quit_chord = QuitChord::default();
    loop {
        if stop.load(Ordering::Relaxed) {
            info!(
                pad,
                name = %descriptor.name,
                "Session ended, releasing pass-through gamepad"
            );
            let _ = device.ungrab();
            break;
        }
        // Checked every iteration (events or poll timeout) so a held
        // chord fires within one poll interval of the hold elapsing.
        if quit_chord.fired() && !shared.quit_requested() {
            info!(
                pad,
                name = %descriptor.name,
                "Select+Start held: requesting session end"
            );
            shared.request_quit();
        }
        if !wait_readable(&device) {
            continue;
        }
        match device.fetch_events() {
            Ok(events) => {
                let batch: Vec<RawGamepadEvent> = events
                    .filter(|ev| {
                        matches!(
                            ev.event_type(),
                            evdev::EventType::KEY | evdev::EventType::ABSOLUTE
                        )
                    })
                    .map(|ev| RawGamepadEvent {
                        event_type: ev.event_type().0,
                        code: ev.code(),
                        value: ev.value(),
                    })
                    .collect();
                if batch.is_empty() {
                    continue;
                }
                for ev in &batch {
                    if ev.event_type == evdev::EventType::KEY.0 {
                        if ev.code == KeyCode::BTN_SELECT.code() {
                            quit_chord.set_select(ev.value != 0);
                        } else if ev.code == KeyCode::BTN_START.code() {
                            quit_chord.set_start(ev.value != 0);
                        }
                    }
                }
                if input_tx
                    .send(InputEvent::GamepadPassthroughEvents { pad, events: batch })
                    .is_err()
                {
                    debug!(pad, "Input channel closed, stopping gamepad reader");
                    break;
                }
            }
            Err(e) => {
                info!(
                    pad,
                    name = %descriptor.name,
                    "Gamepad disconnected ({e})"
                );
                shared.mark_lost(descriptor.vendor, descriptor.product);
                let _ = input_tx.send(InputEvent::GamepadDisconnected { pad });
                break;
            }
        }
    }
    shared.release(&PadKey::Evdev(path.clone()));
    shared.unmark_grabbed(path, descriptor.vendor, descriptor.product);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_chord_requires_both_buttons_held() {
        let mut chord = QuitChord::default();
        assert!(!chord.fired());

        chord.set_select(true);
        chord.set_start(true);
        assert!(!chord.fired(), "hold time not elapsed yet");

        // Backdate both presses past the hold requirement.
        let past = std::time::Instant::now() - QUIT_CHORD_HOLD * 2;
        chord.select_since = Some(past);
        chord.start_since = Some(past);
        assert!(chord.fired());

        // Releasing either button cancels the chord.
        chord.set_start(false);
        assert!(!chord.fired());

        // Re-pressing restarts the hold from now.
        chord.set_start(true);
        assert!(!chord.fired());
    }

    #[test]
    fn quit_flag_starts_clear_and_latches() {
        let shared = SharedGamepads::new();
        assert!(!shared.quit_requested());
        shared.request_quit();
        assert!(shared.quit_requested());
    }

    #[test]
    fn slots_allocate_lowest_free_and_reuse() {
        let shared = SharedGamepads::new();
        let a = PadKey::Sdl(100);
        let b = PadKey::Evdev(PathBuf::from("/dev/input/event7"));
        let c = PadKey::Sdl(300);

        assert_eq!(shared.allocate(&a), Some(0));
        assert_eq!(shared.allocate(&b), Some(1));
        assert_eq!(shared.allocate(&c), Some(2));

        // Re-allocating an existing key returns its slot.
        assert_eq!(shared.allocate(&b), Some(1));

        // Releasing frees the slot for the next controller.
        assert_eq!(shared.release(&b), Some(1));
        assert_eq!(shared.get(&b), None);
        assert_eq!(shared.allocate(&PadKey::Sdl(400)), Some(1));
    }

    #[test]
    fn slots_full_returns_none() {
        let shared = SharedGamepads::new();
        for id in 0..u32::from(MAX_GAMEPADS) {
            assert!(shared.allocate(&PadKey::Sdl(id)).is_some());
        }
        assert_eq!(shared.allocate(&PadKey::Sdl(99)), None);
    }

    #[test]
    fn release_unknown_returns_none() {
        let shared = SharedGamepads::new();
        assert_eq!(shared.release(&PadKey::Sdl(42)), None);
    }

    #[test]
    fn grab_registry_tracks_generation() {
        let shared = SharedGamepads::new();
        assert!(!shared.is_grabbed(0x28de, 0x1102));
        let gen0 = shared.generation();

        shared.mark_grabbed(PathBuf::from("/dev/input/event9"), 0x28de, 0x1102);
        assert!(shared.is_grabbed(0x28de, 0x1102));
        assert!(shared.generation() > gen0);

        shared.unmark_grabbed(&PathBuf::from("/dev/input/event9"), 0x28de, 0x1102);
        assert!(!shared.is_grabbed(0x28de, 0x1102));
    }

    #[test]
    fn ignored_nodes_pruned_when_device_disappears() {
        let shared = SharedGamepads::new();
        let node = PathBuf::from("/dev/input/event3");
        shared.mark_ignored(node.clone());
        assert!(shared.is_ignored(&node));

        // Node still enumerated: the ignore record is kept.
        shared.prune_ignored(std::iter::once(&node));
        assert!(shared.is_ignored(&node));

        // Node gone: the record is dropped so a re-plug is re-evaluated.
        shared.prune_ignored(std::iter::empty());
        assert!(!shared.is_ignored(&node));
    }

    #[test]
    fn lost_passthrough_tracked_until_regrabbed() {
        let shared = SharedGamepads::new();
        assert!(!shared.passthrough_was_lost(0x28de, 0x1142));

        shared.mark_lost(0x28de, 0x1142);
        assert!(shared.passthrough_was_lost(0x28de, 0x1142));

        // Re-establishing pass-through clears the loss record.
        shared.mark_grabbed(PathBuf::from("/dev/input/event9"), 0x28de, 0x1142);
        assert!(!shared.passthrough_was_lost(0x28de, 0x1142));
    }

    #[test]
    fn guid_extracts_vendor_product() {
        // SDL GUID layout: bus(0-1) crc(2-3) vendor(4-5) 0(6-7)
        // product(8-9) 0(10-11) version(12-13) driver-specific(14-15).
        let mut guid = [0u8; 16];
        guid[4] = 0xde;
        guid[5] = 0x28; // 0x28de little-endian (Valve)
        guid[8] = 0x02;
        guid[9] = 0x11; // 0x1102 little-endian (Steam Controller)
        assert_eq!(guid_vendor_product(&guid), (0x28de, 0x1102));
    }
}
