//! USB device forwarding (USB/IP tunneled over the session connection).
//!
//! Some devices can't be usefully recreated on the server as virtual
//! devices — Steam only accepts Valve controllers with their real USB
//! topology (it checks USB interface numbers, which uhid/uinput devices
//! don't have). For those, the whole USB device is forwarded: it is
//! bound to the kernel's `usbip-host` stub here, the server attaches it
//! to its `vhci-hcd` virtual host controller, and the kernel-to-kernel
//! USB/IP byte flow rides a dedicated `QUIC` bidirectional stream of
//! the existing session — no `usbipd`, no extra open port.
//!
//! Both kernel stubs take a plain `SOCK_STREAM` socket fd via sysfs, so
//! each side hands its kernel one end of a local socket pair and pumps
//! the other end into the `QUIC` stream.
//!
//! Requires write access to a few sysfs attributes (normally root-only)
//! — see `docs/steam-controller-usbip.md` and the flake's
//! `nixosModules.usb-client` for the persistent permission setup.

use std::collections::HashSet;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use stargaze_core::transport::{UsbTunnelHeader, serialize_usb_tunnel_header};
use tracing::{debug, info, warn};

/// Devices worth forwarding wholesale: Valve controller hardware, which
/// Steam refuses to see through virtual recreations.
/// (wired Steam Controller, wireless dongle, Steam Deck)
const FORWARDED_DEVICES: [(u16, u16); 3] = [(0x28de, 0x1102), (0x28de, 0x1142), (0x28de, 0x1205)];

/// How often to rescan for forwardable devices (hotplug support).
const SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Backoff before a device whose tunnel ended may be re-exported.
const REEXPORT_BACKOFF: Duration = Duration::from_secs(3);

/// A tunnel that dies faster than this never really worked (missing
/// permissions on either end, no free vhci port, ...). Retrying such a
/// device would detach it from its local driver over and over, so it
/// is skipped for the rest of the session instead.
const MIN_HEALTHY_TUNNEL: Duration = Duration::from_secs(5);

const USB_DEVICES: &str = "/sys/bus/usb/devices";
const USBIP_HOST: &str = "/sys/bus/usb/drivers/usbip-host";
const DRIVERS_PROBE: &str = "/sys/bus/usb/drivers_probe";

/// One forwardable USB device found in sysfs.
#[derive(Debug, Clone)]
struct UsbDevice {
    busid: String,
    vendor: u16,
    product: u16,
    devid: u32,
    speed: u32,
    name: String,
}

/// Starts the USB forwarder: scans for matching devices and tunnels
/// each one to the server for as long as the connection lives. Devices
/// return to this machine when their tunnel ends.
pub fn start(connection: quinn::Connection) {
    tokio::spawn(async move {
        let mut exported: HashSet<String> = HashSet::new();
        let mut skipped: HashSet<String> = HashSet::new();
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
        loop {
            if connection.close_reason().is_some() {
                debug!("Connection closed, USB forwarder exiting");
                return;
            }
            while let Ok((busid, gave_up)) = done_rx.try_recv() {
                exported.remove(&busid);
                if gave_up {
                    skipped.insert(busid);
                }
            }
            for device in scan_devices(Path::new(USB_DEVICES)) {
                if exported.contains(&device.busid) || skipped.contains(&device.busid) {
                    continue;
                }
                exported.insert(device.busid.clone());
                let connection = connection.clone();
                let done_tx = done_tx.clone();
                tokio::spawn(async move {
                    let busid = device.busid.clone();
                    let started = std::time::Instant::now();
                    let failed = match export_device(&connection, device).await {
                        Err(e) => {
                            warn!(busid, "USB forwarding failed: {e}");
                            true
                        }
                        // A tunnel that barely lived never worked (e.g.
                        // the server lacks vhci permissions).
                        Ok(()) => started.elapsed() < MIN_HEALTHY_TUNNEL,
                    };
                    if failed {
                        warn!(
                            busid,
                            "Not retrying this device for the rest of the session \
                             (fix the setup and reconnect)"
                        );
                    } else {
                        tokio::time::sleep(REEXPORT_BACKOFF).await;
                    }
                    let _ = done_tx.send((busid, failed));
                });
            }
            tokio::time::sleep(SCAN_INTERVAL).await;
        }
    });
}

/// Scans sysfs for devices matching [`FORWARDED_DEVICES`].
fn scan_devices(root: &Path) -> Vec<UsbDevice> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let busid = entry.file_name().to_string_lossy().into_owned();
        if !is_device_busid(&busid) {
            continue;
        }
        if let Some(device) = read_device(&entry.path(), &busid) {
            devices.push(device);
        }
    }
    devices
}

/// Whether a sysfs entry name is a USB *device* busid (e.g. `3-1.3`) as
/// opposed to an interface (`3-1.3:1.0`), a root hub (`usb3`), or a bus
/// attribute.
fn is_device_busid(name: &str) -> bool {
    !name.contains(':')
        && name.contains('-')
        && name
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == '.')
}

/// Reads one sysfs device directory; returns it if it matches the
/// forward list.
fn read_device(path: &Path, busid: &str) -> Option<UsbDevice> {
    let vendor = read_hex_u16(&path.join("idVendor"))?;
    let product = read_hex_u16(&path.join("idProduct"))?;
    if !FORWARDED_DEVICES.contains(&(vendor, product)) {
        return None;
    }
    let busnum: u32 = read_trimmed(&path.join("busnum"))?.parse().ok()?;
    let devnum: u32 = read_trimmed(&path.join("devnum"))?.parse().ok()?;
    let speed = speed_code(&read_trimmed(&path.join("speed"))?);
    let name = read_trimmed(&path.join("product")).unwrap_or_else(|| "USB device".to_string());
    Some(UsbDevice {
        busid: busid.to_string(),
        vendor,
        product,
        devid: (busnum << 16) | devnum,
        speed,
        name,
    })
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn read_hex_u16(path: &Path) -> Option<u16> {
    u16::from_str_radix(&read_trimmed(path)?, 16).ok()
}

/// Maps the sysfs `speed` attribute (Mbps string) to the kernel
/// `usb_device_speed` enum value expected by the vhci attach interface.
fn speed_code(speed: &str) -> u32 {
    match speed {
        "1.5" => 1,   // USB_SPEED_LOW
        "12" => 2,    // USB_SPEED_FULL
        "480" => 3,   // USB_SPEED_HIGH
        "5000" => 5,  // USB_SPEED_SUPER
        "10000" => 6, // USB_SPEED_SUPER_PLUS
        other => {
            debug!(speed = other, "Unknown USB speed, assuming high speed");
            3
        }
    }
}

/// Writes `data` to a sysfs attribute.
fn sysfs_write(path: &str, data: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.write_all(data.as_bytes())
}

/// Rebinds the device to its regular driver when the tunnel ends, so
/// it becomes usable on this machine again.
struct StubGuard {
    busid: String,
}

impl Drop for StubGuard {
    fn drop(&mut self) {
        let busid = &self.busid;
        let _ = sysfs_write(&format!("{USBIP_HOST}/unbind"), busid);
        let _ = sysfs_write(
            &format!("{USBIP_HOST}/match_busid"),
            &format!("del {busid}"),
        );
        let _ = sysfs_write(DRIVERS_PROBE, busid);
        info!(busid, "USB device released back to this machine");
    }
}

/// Binds the device to the `usbip-host` stub driver.
fn bind_to_stub(busid: &str) -> std::io::Result<StubGuard> {
    sysfs_write(
        &format!("{USBIP_HOST}/match_busid"),
        &format!("add {busid}"),
    )?;
    let guard = StubGuard {
        busid: busid.to_string(),
    };
    // Detach the regular driver (usually `usb`), then bind the stub
    // explicitly via its `bind` attribute — a bus re-probe would just
    // hand the device back to the generic driver (probe order), which
    // is why the usbip tool also writes to `bind` directly.
    if let Ok(driver) = std::fs::read_link(format!("{USB_DEVICES}/{busid}/driver"))
        && let Some(name) = driver.file_name().and_then(|n| n.to_str())
    {
        let _ = sysfs_write(&format!("/sys/bus/usb/drivers/{name}/unbind"), busid);
    }
    sysfs_write(&format!("{USBIP_HOST}/bind"), busid)?;

    let bound: PathBuf = std::fs::read_link(format!("{USB_DEVICES}/{busid}/driver"))
        .map_err(|e| std::io::Error::other(format!("no driver after probe: {e}")))?;
    if bound.file_name().and_then(|n| n.to_str()) != Some("usbip-host") {
        return Err(std::io::Error::other(format!(
            "device bound to {} instead of usbip-host",
            bound.display()
        )));
    }
    Ok(guard)
}

/// A local `SOCK_STREAM` pair: one end for the kernel stub, one for us.
enum LocalPair {
    Unix(std::os::unix::net::UnixStream),
    Tcp(std::net::TcpStream),
}

/// Hands the kernel one end of a socket pair via `usbip_sockfd` and
/// returns our end. Tries a unix socketpair first (the kernel only
/// checks for `SOCK_STREAM`); falls back to a loopback TCP pair if the
/// running kernel is stricter.
fn attach_kernel_socket(busid: &str) -> std::io::Result<LocalPair> {
    let sockfd_path = format!("{USB_DEVICES}/{busid}/usbip_sockfd");

    let (kernel_end, ours) = std::os::unix::net::UnixStream::pair()?;
    match sysfs_write(&sockfd_path, &kernel_end.as_raw_fd().to_string()) {
        Ok(()) => return Ok(LocalPair::Unix(ours)),
        Err(e) => debug!(
            busid,
            "unix socketpair rejected by stub ({e}), trying TCP loopback"
        ),
    }
    drop(kernel_end);

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let ours = std::net::TcpStream::connect(listener.local_addr()?)?;
    let (kernel_end, _) = listener.accept()?;
    kernel_end.set_nodelay(true)?;
    ours.set_nodelay(true)?;
    sysfs_write(&sockfd_path, &kernel_end.as_raw_fd().to_string())?;
    Ok(LocalPair::Tcp(ours))
}

/// Exports one device: stub bind, kernel socket, tunnel stream, pump.
/// Returns when the tunnel ends for any reason; the [`StubGuard`] then
/// gives the device back to this machine.
async fn export_device(
    connection: &quinn::Connection,
    device: UsbDevice,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let busid = device.busid.clone();
    let guard = bind_to_stub(&busid).map_err(|e| {
        format!(
            "cannot bind {busid} to usbip-host: {e}; is the usbip_host \
             module loaded and are the stargaze USB permissions set up? \
             (nixosModules.usb-client, docs/steam-controller-usbip.md)"
        )
    })?;

    let local = attach_kernel_socket(&busid)?;

    let (mut send, recv) = connection.open_bi().await?;
    let header = UsbTunnelHeader {
        vendor: device.vendor,
        product: device.product,
        devid: device.devid,
        speed: device.speed,
        name: device.name.clone(),
    };
    send.write_all(&serialize_usb_tunnel_header(&header)?)
        .await?;

    info!(
        busid,
        name = %device.name,
        vendor = format_args!("{:04x}", device.vendor),
        product = format_args!("{:04x}", device.product),
        "USB device forwarded to the server (usable there until the session ends)"
    );

    let mut quic = tokio::io::join(recv, send);
    let copied = match local {
        LocalPair::Unix(ours) => {
            ours.set_nonblocking(true)?;
            let mut ours = tokio::net::UnixStream::from_std(ours)?;
            tokio::io::copy_bidirectional(&mut ours, &mut quic).await
        }
        LocalPair::Tcp(ours) => {
            ours.set_nonblocking(true)?;
            let mut ours = tokio::net::TcpStream::from_std(ours)?;
            tokio::io::copy_bidirectional(&mut ours, &mut quic).await
        }
    };
    match copied {
        Ok((to_server, from_server)) => {
            debug!(busid, to_server, from_server, "USB tunnel closed");
        }
        Err(e) => debug!(busid, "USB tunnel ended: {e}"),
    }
    drop(guard);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_busid_filter() {
        assert!(is_device_busid("3-1"));
        assert!(is_device_busid("3-1.3.2"));
        assert!(!is_device_busid("3-1.3:1.0")); // interface
        assert!(!is_device_busid("usb3")); // root hub
        assert!(!is_device_busid("3-0:1.0"));
    }

    #[test]
    fn speed_codes_match_kernel_enum() {
        assert_eq!(speed_code("1.5"), 1);
        assert_eq!(speed_code("12"), 2);
        assert_eq!(speed_code("480"), 3);
        assert_eq!(speed_code("5000"), 5);
        assert_eq!(speed_code("10000"), 6);
        assert_eq!(speed_code("unknown"), 3);
    }

    #[test]
    fn devid_packs_bus_and_device_number() {
        // Matches the USB/IP convention: busnum << 16 | devnum.
        let busnum = 3u32;
        let devnum = 7u32;
        assert_eq!((busnum << 16) | devnum, 0x0003_0007);
    }
}
