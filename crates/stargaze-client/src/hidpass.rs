//! HID-level controller pass-through: forwards raw HID reports so the
//! server can rebuild the device via `/dev/uhid` and the host attaches
//! the *real* driver.
//!
//! Valve controllers (Steam Controller, Steam Deck) are only supported
//! by hosts through Steam's hidraw stack — an evdev-level clone is
//! invisible to games (no SDL mapping) and to Steam Input. Forwarding
//! at the HID report level sidesteps that: the server-side kernel runs
//! `hid-steam` on the forwarded descriptor and Steam adopts the device
//! as if it were plugged in locally.
//!
//! A scanner enumerates `/sys/class/hidraw`, claims Valve-vendor nodes,
//! reads their report descriptors, and streams input reports to the
//! server. Host-side requests (rumble output reports, and the get/set
//! feature round-trips `hid-steam` performs at attach) arrive over the
//! control stream as [`HidHostRequest`] and are executed on the real
//! device via hidraw ioctls.
//!
//! Opening the hidraw node has a welcome side effect: the client's own
//! hid-steam driver retires its local input node (it defers to hidraw
//! clients), so the controller stops acting on the client machine
//! without any evdev grab. SDL must not fight over the node either —
//! `main` sets `SDL_HIDAPI_IGNORE_DEVICES=0x28de/0x0000` while HID
//! pass-through is active.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use stargaze_core::input::{
    HidDeviceDescriptor, HidHostRequest, HidReplyKind, HidReportType, InputEvent,
    MAX_HID_PASSTHROUGH,
};
use tracing::{debug, info, warn};

use crate::gamepad::SharedGamepads;

/// How often the scanner looks for newly connected HID devices.
const SCAN_INTERVAL: Duration = Duration::from_millis(1000);

/// Valve's USB vendor id (Steam Controller, Steam Deck, dongles).
const VALVE_VENDOR_ID: u16 = 0x28de;

/// `HID_MAX_DESCRIPTOR_SIZE`; also an upper bound on report sizes.
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;

/// How often forwarded input reports are logged (first, then every Nth).
const REPORT_LOG_EVERY: u64 = 512;

// hidraw ioctl encoding (linux/hidraw.h via asm-generic/ioctl.h).
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const fn hidraw_ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'H' as u64) << 8) | nr
}

const HIDIOCGRDESCSIZE: u64 = hidraw_ioc(IOC_READ, 0x01, 4);
const HIDIOCGRDESC: u64 = hidraw_ioc(IOC_READ, 0x02, 4 + HID_MAX_DESCRIPTOR_SIZE);

/// `HIDIOCS*` (write a report) ioctl for a report class.
fn set_report_ioc(rtype: HidReportType, len: usize) -> u64 {
    let nr = match rtype {
        HidReportType::Feature => 0x06,
        HidReportType::Input => 0x09,
        HidReportType::Output => 0x0B,
    };
    hidraw_ioc(IOC_WRITE | IOC_READ, nr, len)
}

/// `HIDIOCG*` (read a report) ioctl for a report class.
fn get_report_ioc(rtype: HidReportType, len: usize) -> u64 {
    let nr = match rtype {
        HidReportType::Feature => 0x07,
        HidReportType::Input => 0x0A,
        HidReportType::Output => 0x0C,
    };
    hidraw_ioc(IOC_WRITE | IOC_READ, nr, len)
}

/// Identity parsed from a hidraw node's sysfs `device/uevent`.
#[derive(Debug, PartialEq, Eq)]
struct HidIdentity {
    name: String,
    phys: String,
    uniq: String,
    bus_type: u32,
    vendor: u16,
    product: u16,
}

/// Parses `HID_ID=0003:000028DE:00001142`-style uevent content.
fn parse_uevent(content: &str) -> Option<HidIdentity> {
    let mut name = String::new();
    let mut phys = String::new();
    let mut uniq = String::new();
    let mut id: Option<(u32, u16, u16)> = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("HID_NAME=") {
            name = v.to_string();
        } else if let Some(v) = line.strip_prefix("HID_PHYS=") {
            phys = v.to_string();
        } else if let Some(v) = line.strip_prefix("HID_UNIQ=") {
            uniq = v.to_string();
        } else if let Some(v) = line.strip_prefix("HID_ID=") {
            let mut parts = v.splitn(3, ':');
            let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
            let vendor = u32::from_str_radix(parts.next()?, 16).ok()?;
            let product = u32::from_str_radix(parts.next()?, 16).ok()?;
            id = Some((
                bus,
                u16::try_from(vendor & 0xffff).ok()?,
                u16::try_from(product & 0xffff).ok()?,
            ));
        }
    }
    let (bus_type, vendor, product) = id?;
    Some(HidIdentity {
        name,
        phys,
        uniq,
        bus_type,
        vendor,
        product,
    })
}

/// Whether a report descriptor describes a vendor-defined interface
/// (first usage page in `0xFF00`-`0xFFFF`).
///
/// Valve controllers expose boot keyboard/mouse interfaces alongside
/// the proprietary ones ("lizard mode"). Only the vendor interfaces
/// carry controller traffic; forwarding a boot interface rebuilds a
/// live keyboard/mouse on the server that shadows the pad.
fn is_vendor_interface(report_descriptor: &[u8]) -> bool {
    // A descriptor opens with its usage page item: `05 pp` for one-byte
    // pages (never vendor) or `06 lo hi` for two-byte pages.
    report_descriptor.len() >= 3 && report_descriptor[0] == 0x06 && report_descriptor[2] == 0xFF
}

/// `struct hidraw_report_descriptor` (linux/hidraw.h).
#[repr(C)]
struct HidrawReportDescriptor {
    size: u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

/// Reads the HID report descriptor via hidraw ioctls.
fn read_report_descriptor(file: &File) -> std::io::Result<Vec<u8>> {
    let fd = file.as_raw_fd();
    let mut size: libc::c_int = 0;
    // SAFETY: HIDIOCGRDESCSIZE writes a single c_int.
    if unsafe { libc::ioctl(fd, HIDIOCGRDESCSIZE as libc::c_ulong, &raw mut size) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut desc = HidrawReportDescriptor {
        size: u32::try_from(size).unwrap_or(0).min(4096),
        value: [0; HID_MAX_DESCRIPTOR_SIZE],
    };
    // SAFETY: HIDIOCGRDESC reads desc.size and fills desc.value.
    if unsafe { libc::ioctl(fd, HIDIOCGRDESC as libc::c_ulong, &raw mut desc) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(desc.value[..desc.size as usize].to_vec())
}

/// Executes a get-report request on the device.
///
/// Returns `(err, data)`; data starts with the report-number byte, as
/// hidraw provides it and uhid expects it.
fn get_report(file: &File, rtype: HidReportType, report_number: u8) -> (bool, Vec<u8>) {
    let mut buf = vec![0u8; HID_MAX_DESCRIPTOR_SIZE];
    buf[0] = report_number;
    let req = get_report_ioc(rtype, buf.len());
    // SAFETY: the ioctl reads buf[0] and writes at most buf.len() bytes.
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), req as libc::c_ulong, buf.as_mut_ptr()) };
    if ret < 0 {
        (true, Vec::new())
    } else {
        buf.truncate(usize::try_from(ret).unwrap_or(0));
        (false, buf)
    }
}

/// Executes a set-report request on the device; returns whether it failed.
fn set_report(file: &File, rtype: HidReportType, data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    let req = set_report_ioc(rtype, data.len());
    // SAFETY: the ioctl reads data.len() bytes from the buffer.
    let ret = unsafe { libc::ioctl(file.as_raw_fd(), req as libc::c_ulong, data.as_ptr()) };
    ret < 0
}

/// Writes an output report to the device (fire-and-forget).
fn write_output(file: &File, data: &[u8]) {
    // SAFETY: plain write(2) of a byte buffer.
    let ret = unsafe {
        libc::write(
            file.as_raw_fd(),
            data.as_ptr().cast::<libc::c_void>(),
            data.len(),
        )
    };
    if ret < 0 {
        debug!(
            "hidraw output write failed: {}",
            std::io::Error::last_os_error()
        );
    }
}

struct HidState {
    /// `slots[i]` holds the sysfs node name occupying HID slot `i`.
    slots: [Option<PathBuf>; MAX_HID_PASSTHROUGH as usize],
    /// Request-execution fds by slot (clones of the reader's fd).
    files: HashMap<u8, File>,
    /// (vendor, product) per slot, for grab-registry cleanup.
    idents: HashMap<u8, (u16, u16)>,
    /// Nodes to leave alone (open failed, non-Valve); pruned when they
    /// disappear so a re-plug is re-evaluated.
    ignored: HashSet<PathBuf>,
}

/// Registry of forwarded HID devices, shared between the scanner, the
/// per-device readers, and the host-request handler.
pub struct HidPassthrough {
    state: Mutex<HidState>,
    shared: Arc<SharedGamepads>,
}

impl HidPassthrough {
    fn new(shared: Arc<SharedGamepads>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(HidState {
                slots: [const { None }; MAX_HID_PASSTHROUGH as usize],
                files: HashMap::new(),
                idents: HashMap::new(),
                ignored: HashSet::new(),
            }),
            shared,
        })
    }

    fn release(&self, hid: u8, node: &Path) {
        let mut state = self.state.lock().unwrap();
        if state.slots[hid as usize].as_deref() == Some(node) {
            state.slots[hid as usize] = None;
        }
        state.files.remove(&hid);
        if let Some((vendor, product)) = state.idents.remove(&hid) {
            // Only clear the SDL-skip registration once no other slot
            // (e.g. another dongle interface) shares the identity.
            if !state.idents.values().any(|&vp| vp == (vendor, product)) {
                drop(state);
                self.shared.unmark_claimed(vendor, product);
            }
        }
    }

    /// Records a node the scanner should not touch again while present.
    fn ignore(&self, node: &Path) {
        self.state
            .lock()
            .unwrap()
            .ignored
            .insert(node.to_path_buf());
    }
}

/// Starts HID pass-through: runs one synchronous scan (so devices
/// present at startup are claimed before SDL initializes), then spawns
/// the periodic scanner and the host-request handler.
///
/// Returns `true` if the initial scan claimed at least one device —
/// the caller then tells SDL to ignore Valve devices at the HIDAPI
/// level before initializing it.
pub fn start(
    shared: Arc<SharedGamepads>,
    input_tx: std::sync::mpsc::Sender<InputEvent>,
    request_rx: tokio::sync::mpsc::Receiver<HidHostRequest>,
) -> bool {
    let registry = HidPassthrough::new(shared);

    let claimed = scan_once(&registry, &input_tx);

    let scan_registry = Arc::clone(&registry);
    let scan_input_tx = input_tx.clone();
    let spawned = std::thread::Builder::new()
        .name("stargaze-hid-scan".into())
        .spawn(move || {
            loop {
                std::thread::sleep(SCAN_INTERVAL);
                scan_once(&scan_registry, &scan_input_tx);
            }
        });
    if let Err(e) = spawned {
        warn!("Failed to spawn HID scanner thread: {e}");
    }

    let spawned = std::thread::Builder::new()
        .name("stargaze-hid-req".into())
        .spawn(move || request_loop(&registry, request_rx, &input_tx));
    if let Err(e) = spawned {
        warn!("Failed to spawn HID request thread: {e}");
    }

    claimed > 0
}

/// One enumeration pass over `/sys/class/hidraw`; returns how many new
/// devices were claimed.
fn scan_once(
    registry: &Arc<HidPassthrough>,
    input_tx: &std::sync::mpsc::Sender<InputEvent>,
) -> u32 {
    let Ok(entries) = std::fs::read_dir("/sys/class/hidraw") else {
        return 0;
    };
    let nodes: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| PathBuf::from(e.file_name())))
        .collect();

    {
        let mut state = registry.state.lock().unwrap();
        state.ignored.retain(|n| nodes.contains(n));
    }

    let mut claimed = 0;
    for node in nodes {
        {
            let state = registry.state.lock().unwrap();
            if state.ignored.contains(&node)
                || state.slots.iter().any(|s| s.as_deref() == Some(&*node))
            {
                continue;
            }
        }
        let uevent_path = Path::new("/sys/class/hidraw")
            .join(&node)
            .join("device/uevent");
        let Ok(content) = std::fs::read_to_string(&uevent_path) else {
            continue;
        };
        let Some(identity) = parse_uevent(&content) else {
            continue;
        };
        if identity.vendor != VALVE_VENDOR_ID {
            registry.state.lock().unwrap().ignored.insert(node);
            continue;
        }
        if claim_device(registry, input_tx, &node, &identity) {
            claimed += 1;
        }
    }
    claimed
}

/// Opens and announces one hidraw node; spawns its reader thread.
fn claim_device(
    registry: &Arc<HidPassthrough>,
    input_tx: &std::sync::mpsc::Sender<InputEvent>,
    node: &Path,
    identity: &HidIdentity,
) -> bool {
    let devnode = Path::new("/dev").join(node);
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&devnode)
    {
        Ok(f) => f,
        Err(e) => {
            info!(
                name = %identity.name,
                path = %devnode.display(),
                "Cannot open HID device for pass-through ({e}); check udev \
                 rules (steam-devices) grant hidraw access"
            );
            registry.ignore(node);
            return false;
        }
    };

    let report_descriptor = match read_report_descriptor(&file) {
        Ok(rd) => rd,
        Err(e) => {
            warn!(name = %identity.name, "Failed to read HID report descriptor: {e}");
            registry.ignore(node);
            return false;
        }
    };

    if !is_vendor_interface(&report_descriptor) {
        // Lizard-mode boot keyboard/mouse interface, not controller
        // traffic — leave it local.
        debug!(
            name = %identity.name,
            node = %node.display(),
            "Skipping non-vendor HID interface (lizard keyboard/mouse)"
        );
        registry.ignore(node);
        return false;
    }

    let hid = {
        let mut state = registry.state.lock().unwrap();
        let Some(free) = state.slots.iter().position(Option::is_none) else {
            warn!(name = %identity.name, "All HID pass-through slots taken, ignoring device");
            return false;
        };
        state.slots[free] = Some(node.to_path_buf());
        let Ok(hid) = u8::try_from(free) else {
            return false;
        };
        match file.try_clone() {
            Ok(clone) => {
                state.files.insert(hid, clone);
            }
            Err(e) => {
                warn!(name = %identity.name, "Failed to clone hidraw fd: {e}");
                state.slots[free] = None;
                return false;
            }
        }
        state
            .idents
            .insert(hid, (identity.vendor, identity.product));
        hid
    };
    registry
        .shared
        .mark_claimed(identity.vendor, identity.product);

    info!(
        name = %identity.name,
        hid,
        node = %node.display(),
        vendor = format!("{:04x}", identity.vendor),
        product = format!("{:04x}", identity.product),
        "HID pass-through active (device rebuilt on the server via uhid)"
    );

    let descriptor = HidDeviceDescriptor {
        name: identity.name.clone(),
        phys: identity.phys.clone(),
        uniq: identity.uniq.clone(),
        bus_type: identity.bus_type,
        vendor: identity.vendor,
        product: identity.product,
        report_descriptor,
    };
    if input_tx
        .send(InputEvent::HidPassthroughConnected { hid, descriptor })
        .is_err()
    {
        return false; // Transport gone; the session is ending.
    }

    let registry = Arc::clone(registry);
    let input_tx = input_tx.clone();
    let node = node.to_path_buf();
    let name = identity.name.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("stargaze-hid{hid}"))
        .spawn(move || read_loop(&registry, &input_tx, &node, file, hid, &name));
    if let Err(e) = spawned {
        warn!("Failed to spawn HID reader thread: {e}");
    }
    true
}

/// Blocking per-device reader: forwards raw input reports until the
/// device disappears or the transport closes.
fn read_loop(
    registry: &Arc<HidPassthrough>,
    input_tx: &std::sync::mpsc::Sender<InputEvent>,
    node: &Path,
    mut file: File,
    hid: u8,
    name: &str,
) {
    let mut buf = [0u8; HID_MAX_DESCRIPTOR_SIZE];
    let mut reports: u64 = 0;
    loop {
        match file.read(&mut buf) {
            Ok(0) => {
                info!(hid, name, "HID device closed");
                break;
            }
            Ok(n) => {
                reports += 1;
                if reports == 1 {
                    debug!(hid, len = n, "First input report read, forwarding");
                } else if reports.is_multiple_of(REPORT_LOG_EVERY) {
                    debug!(hid, count = reports, "Input reports forwarded");
                }
                let report = InputEvent::HidPassthroughReport {
                    hid,
                    data: buf[..n].to_vec(),
                };
                if input_tx.send(report).is_err() {
                    debug!(hid, "Input channel closed, stopping HID reader");
                    break;
                }
            }
            Err(e) => {
                info!(hid, name, "HID device disconnected ({e})");
                break;
            }
        }
    }
    let _ = input_tx.send(InputEvent::HidPassthroughDisconnected { hid });
    registry.release(hid, node);
}

/// Executes host-side requests (rumble, feature get/set) on the real
/// devices and sends replies back through the input channel.
fn request_loop(
    registry: &Arc<HidPassthrough>,
    mut request_rx: tokio::sync::mpsc::Receiver<HidHostRequest>,
    input_tx: &std::sync::mpsc::Sender<InputEvent>,
) {
    while let Some(request) = request_rx.blocking_recv() {
        match request {
            HidHostRequest::Output { hid, data } => {
                debug!(hid, len = data.len(), "Host output report → device");
                let state = registry.state.lock().unwrap();
                if let Some(file) = state.files.get(&hid) {
                    write_output(file, &data);
                }
            }
            HidHostRequest::GetReport {
                hid,
                request,
                report_number,
                report_type,
            } => {
                let (err, data) = {
                    let state = registry.state.lock().unwrap();
                    state.files.get(&hid).map_or((true, Vec::new()), |file| {
                        get_report(file, report_type, report_number)
                    })
                };
                if err {
                    warn!(
                        hid,
                        request,
                        ?report_type,
                        report_number,
                        "get-report failed on device"
                    );
                } else {
                    debug!(
                        hid,
                        request,
                        ?report_type,
                        len = data.len(),
                        "get-report answered"
                    );
                }
                let reply = InputEvent::HidPassthroughReply {
                    hid,
                    request,
                    kind: HidReplyKind::GetReport,
                    err,
                    data,
                };
                if input_tx.send(reply).is_err() {
                    return;
                }
            }
            HidHostRequest::SetReport {
                hid,
                request,
                report_number: _,
                report_type,
                data,
            } => {
                let len = data.len();
                let err = {
                    let state = registry.state.lock().unwrap();
                    state
                        .files
                        .get(&hid)
                        .is_none_or(|file| set_report(file, report_type, &data))
                };
                if err {
                    warn!(
                        hid,
                        request,
                        ?report_type,
                        len,
                        "set-report failed on device"
                    );
                } else {
                    debug!(hid, request, ?report_type, len, "set-report applied");
                }
                let reply = InputEvent::HidPassthroughReply {
                    hid,
                    request,
                    kind: HidReplyKind::SetReport,
                    err,
                    data: Vec::new(),
                };
                if input_tx.send(reply).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_constants_match_kernel_values() {
        // Known-good values from a C program on x86_64.
        assert_eq!(HIDIOCGRDESCSIZE, 0x8004_4801);
        assert_eq!(HIDIOCGRDESC, 0x9004_4802);
        assert_eq!(get_report_ioc(HidReportType::Feature, 4096), 0xD000_4807);
        assert_eq!(set_report_ioc(HidReportType::Feature, 64), 0xC040_4806);
    }

    #[test]
    fn uevent_parses_valve_dongle() {
        let content = "DRIVER=hid-steam\n\
                       HID_ID=0003:000028DE:00001142\n\
                       HID_NAME=Valve Software Wireless Steam Controller\n\
                       HID_PHYS=usb-0000:00:14.0-2/input1\n\
                       HID_UNIQ=12AB34CD\n\
                       MODALIAS=hid:b0003g0001v000028DEp00001142\n";
        let id = parse_uevent(content).unwrap();
        assert_eq!(
            id,
            HidIdentity {
                name: "Valve Software Wireless Steam Controller".to_string(),
                phys: "usb-0000:00:14.0-2/input1".to_string(),
                uniq: "12AB34CD".to_string(),
                bus_type: 3,
                vendor: 0x28de,
                product: 0x1142,
            }
        );
    }

    #[test]
    fn uevent_without_id_is_rejected() {
        assert!(parse_uevent("HID_NAME=Nameless\n").is_none());
    }

    #[test]
    fn vendor_interface_detection() {
        // Valve proprietary interface: Usage Page (Vendor 0xFF00).
        assert!(is_vendor_interface(&[0x06, 0x00, 0xFF, 0x09, 0x01]));
        // Lizard-mode boot keyboard: Usage Page (Generic Desktop).
        assert!(!is_vendor_interface(&[0x05, 0x01, 0x09, 0x06]));
        // Lizard-mode boot mouse.
        assert!(!is_vendor_interface(&[0x05, 0x01, 0x09, 0x02]));
        // Non-vendor two-byte page (e.g. 0x0C00 would be 06 00 0C).
        assert!(!is_vendor_interface(&[0x06, 0x00, 0x0C, 0x09, 0x01]));
        assert!(!is_vendor_interface(&[0x06]));
    }

    #[test]
    fn slots_release_clears_shared_claim_only_when_last() {
        let shared = SharedGamepads::new();
        let registry = HidPassthrough::new(shared.clone());
        {
            let mut state = registry.state.lock().unwrap();
            state.slots[0] = Some(PathBuf::from("hidraw3"));
            state.slots[1] = Some(PathBuf::from("hidraw4"));
            state.idents.insert(0, (0x28de, 0x1142));
            state.idents.insert(1, (0x28de, 0x1142));
        }
        shared.mark_claimed(0x28de, 0x1142);

        registry.release(0, Path::new("hidraw3"));
        assert!(
            shared.is_grabbed(0x28de, 0x1142),
            "still one interface active"
        );

        registry.release(1, Path::new("hidraw4"));
        assert!(!shared.is_grabbed(0x28de, 0x1142));
    }
}
