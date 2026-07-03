//! USB device attach (USB/IP tunneled over the session connection).
//!
//! Counterpart of the client's `usb` module: every additional `QUIC`
//! bidirectional stream on a session is expected to start with
//! [`USB_STREAM_MAGIC`] and a [`UsbTunnelHeader`], after which it
//! carries the raw kernel-to-kernel USB/IP byte flow. Each tunneled
//! device is attached to the local `vhci-hcd` virtual host controller,
//! making it a genuine USB device on this host (Steam requires that
//! for Valve controllers — it checks USB interface numbers).
//!
//! Requires write access to the vhci sysfs attributes (normally
//! root-only) — see `docs/steam-controller-usbip.md` and the flake's
//! `nixosModules.usb-server`.

use std::io::Write;
use std::os::fd::AsRawFd;

use stargaze_core::transport::{USB_STREAM_MAGIC, UsbTunnelHeader, deserialize_usb_tunnel_header};
use tracing::{debug, info, warn};

const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";

/// `VDEV_ST_NULL`: a vhci port with nothing attached.
const VDEV_ST_NULL: u32 = 4;

/// Accepts USB tunnel streams on the connection for the lifetime of a
/// session. Never returns while the session is healthy, so it can sit
/// in the session `select!` alongside the senders.
pub(crate) async fn serve_usb_tunnels(connection: &quinn::Connection) {
    let mut tunnels = tokio::task::JoinSet::new();
    loop {
        match connection.accept_bi().await {
            Ok((send, recv)) => {
                tunnels.spawn(handle_tunnel(send, recv));
            }
            Err(e) => {
                debug!("No more incoming streams ({e})");
                // The session ends via the control/sender arms; dropping
                // the JoinSet then aborts the pumps, whose guards detach
                // the vhci ports.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Handles one tunnel stream: header, vhci attach, pump, detach.
async fn handle_tunnel(send: quinn::SendStream, mut recv: quinn::RecvStream) {
    let header = match read_header(&mut recv).await {
        Ok(header) => header,
        Err(e) => {
            warn!("Rejecting unrecognized session stream: {e}");
            return;
        }
    };
    let name = header.name.clone();
    if let Err(e) = attach_and_pump(send, recv, header).await {
        warn!(
            name,
            "USB tunnel failed: {e}; is the vhci-hcd module loaded and \
             are the stargaze USB permissions set up? \
             (nixosModules.usb-server, docs/steam-controller-usbip.md)"
        );
    }
}

/// Reads and validates the tunnel preamble.
async fn read_header(
    recv: &mut quinn::RecvStream,
) -> Result<UsbTunnelHeader, Box<dyn std::error::Error + Send + Sync>> {
    let mut magic = [0u8; 4];
    recv.read_exact(&mut magic).await?;
    if magic != USB_STREAM_MAGIC {
        return Err(format!("bad stream magic {magic:02x?}").into());
    }
    let mut len = [0u8; 2];
    recv.read_exact(&mut len).await?;
    let mut body = vec![0u8; u16::from_le_bytes(len) as usize];
    recv.read_exact(&mut body).await?;
    Ok(deserialize_usb_tunnel_header(&body)?)
}

/// Writes `data` to a sysfs attribute.
fn sysfs_write(path: &str, data: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.write_all(data.as_bytes())
}

/// Detaches the vhci port when the tunnel ends for any reason.
struct PortGuard {
    port: u32,
    name: String,
}

impl Drop for PortGuard {
    fn drop(&mut self) {
        let _ = sysfs_write(&format!("{VHCI}/detach"), &self.port.to_string());
        info!(port = self.port, name = %self.name, "USB device detached");
    }
}

/// Finds a free vhci port for the given device speed by parsing the
/// controller's `status` attribute. Super-speed devices need an `ss`
/// port, everything else an `hs` port.
fn free_port(status: &str, speed: u32) -> Option<u32> {
    let hub = if speed >= 5 { "ss" } else { "hs" };
    for line in status.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(line_hub), Some(port), Some(sta)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if line_hub != hub {
            continue;
        }
        if sta.parse::<u32>().ok() == Some(VDEV_ST_NULL) {
            return port.parse::<u32>().ok();
        }
    }
    None
}

/// Attaches the tunneled device to vhci and pumps bytes until either
/// side closes.
async fn attach_and_pump(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    header: UsbTunnelHeader,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let status = std::fs::read_to_string(format!("{VHCI}/status"))?;
    let port = free_port(&status, header.speed).ok_or("no free vhci port")?;

    // Hand the kernel one end of a local SOCK_STREAM pair; unix pair
    // first, loopback TCP as fallback for stricter kernels.
    let attach = format!("{VHCI}/attach");
    let (kernel_end, ours) = std::os::unix::net::UnixStream::pair()?;
    let line = format!(
        "{port} {} {} {}",
        kernel_end.as_raw_fd(),
        header.devid,
        header.speed
    );
    let local = match sysfs_write(&attach, &line) {
        Ok(()) => {
            drop(kernel_end);
            LocalEnd::Unix(ours)
        }
        Err(e) => {
            debug!("unix socketpair rejected by vhci ({e}), trying TCP loopback");
            drop(kernel_end);
            drop(ours);
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
            let ours = std::net::TcpStream::connect(listener.local_addr()?)?;
            let (kernel_end, _) = listener.accept()?;
            kernel_end.set_nodelay(true)?;
            ours.set_nodelay(true)?;
            let line = format!(
                "{port} {} {} {}",
                kernel_end.as_raw_fd(),
                header.devid,
                header.speed
            );
            sysfs_write(&attach, &line)?;
            LocalEnd::Tcp(ours)
        }
    };
    let guard = PortGuard {
        port,
        name: header.name.clone(),
    };

    info!(
        port,
        name = %header.name,
        vendor = format_args!("{:04x}", header.vendor),
        product = format_args!("{:04x}", header.product),
        "USB device attached from the client (genuine USB device on this host)"
    );

    let mut quic = tokio::io::join(recv, send);
    let copied = match local {
        LocalEnd::Unix(ours) => {
            ours.set_nonblocking(true)?;
            let mut ours = tokio::net::UnixStream::from_std(ours)?;
            tokio::io::copy_bidirectional(&mut ours, &mut quic).await
        }
        LocalEnd::Tcp(ours) => {
            ours.set_nonblocking(true)?;
            let mut ours = tokio::net::TcpStream::from_std(ours)?;
            tokio::io::copy_bidirectional(&mut ours, &mut quic).await
        }
    };
    match copied {
        Ok((to_client, from_client)) => {
            debug!(port, to_client, from_client, "USB tunnel closed");
        }
        Err(e) => debug!(port, "USB tunnel ended: {e}"),
    }
    drop(guard);
    Ok(())
}

/// One end of the local socket pair whose other end the kernel owns.
enum LocalEnd {
    Unix(std::os::unix::net::UnixStream),
    Tcp(std::net::TcpStream),
}

#[cfg(test)]
mod tests {
    use super::*;

    // Column layout of /sys/devices/platform/vhci_hcd.0/status.
    const STATUS: &str = "\
hub port sta spd dev      sockfd local_busid
hs  0000 004 000 00000000 000000 0-0
hs  0001 006 002 00010002 000003 3-1
ss  0002 004 000 00000000 000000 0-0
ss  0003 004 000 00000000 000000 0-0
";

    #[test]
    fn free_port_picks_hub_by_speed() {
        // Full speed (2) → first free hs port.
        assert_eq!(free_port(STATUS, 2), Some(0));
        // Super speed (5) → first free ss port.
        assert_eq!(free_port(STATUS, 5), Some(2));
    }

    #[test]
    fn free_port_skips_occupied_ports() {
        // Both hs ports in use → no port for a high-speed device.
        let status = STATUS.replace("hs  0000 004", "hs  0000 006");
        assert_eq!(free_port(&status, 3), None);
    }
}
