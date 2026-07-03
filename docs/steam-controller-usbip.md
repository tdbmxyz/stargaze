# Steam Controller on the server via USB/IP

Stargaze's controller pass-through recreates the client's pad on the
server as a virtual device. That works for most controllers, but **not
for the Steam Controller when the server runs Steam**: Valve's
controller handling only accepts the real USB topology.

## Why virtual recreations don't work for Valve pads

Steam talks to the Steam Controller through its raw HID protocol, not
through evdev, and it identifies the dongle's controller slots by **USB
interface number** (interfaces 1–4 on the `28de:1142` wireless dongle,
interface 2 on the wired `28de:1102` — see `SDL_hidapi_steam.c` for the
public mirror of this logic). Virtual devices (uinput or uhid) have no
USB parent, so hidapi reports `Interface: -1` and Steam enumerates them
(`Local Device Found` in `logs/controller.txt`) but never adopts them.
This was verified end to end: a uhid recreation gets the kernel
`hid-steam` driver, registers a "Wireless Steam Controller" evdev, and
Steam still ignores it.

The fix is to forward the **USB device itself** with USB/IP: the dongle
detaches from the client machine and shows up on the server as genuine
USB hardware — real interface numbers, steam-devices udev rules apply,
Steam adopts it exactly as if it were plugged in locally (gyro,
paddles, per-game configs included).

## One-time setup

**Client machine** (the one with the dongle) — exports USB devices:

```nix
# NixOS
boot.kernelModules = ["usbip_host"];
environment.systemPackages = [config.boot.kernelPackages.usbip];
# USB/IP has no authentication: LAN only, ideally restrict to the
# server's address with an iptables/nftables rule instead.
networking.firewall.allowedTCPPorts = [3240];
systemd.services.usbipd = {
  description = "USB/IP export daemon";
  wantedBy = ["multi-user.target"];
  serviceConfig.ExecStart = "${config.boot.kernelPackages.usbip}/bin/usbipd";
};
```

(Non-NixOS: install `usbip`/`linux-tools`, `modprobe usbip_host`, run
`usbipd -D`, open TCP 3240 on the LAN.)

**Server** — attaches remote USB devices:

```nix
# NixOS
boot.kernelModules = ["vhci-hcd"];
environment.systemPackages = [config.boot.kernelPackages.usbip];
```

## Per-session flow

On the **client**, find and export the dongle:

```sh
usbip list -l                 # find the busid of 28de:1142
sudo usbip bind -b 3-1.3      # replace with your busid
```

On the **server**, attach it:

```sh
usbip list -r <client-ip>     # sanity check: the dongle is exported
sudo usbip attach -r <client-ip> -b 3-1.3
```

The dongle now disappears from the client and appears on the server
(`lsusb`, then Steam detects it within seconds). Stargaze needs no
configuration: the client never sees the pad, and its input reaches
the server through the dongle directly instead of the stream's input
channel.

To give the pad back to the client:

```sh
# server: find the vhci port, then detach
usbip port
sudo usbip detach -p 00
# client: stop exporting
sudo usbip unbind -b 3-1.3
```

## Caveats

- While attached, the controller drives the **server** even outside a
  stargaze session — power it off when not streaming.
- USB/IP runs over its own TCP connection (port 3240), outside
  stargaze's QUIC transport, with **no authentication or encryption**:
  keep it strictly on the trusted LAN.
- Input latency over a wired/solid LAN is negligible (single-digit
  milliseconds); flaky Wi-Fi will affect the controller as much as the
  stream.
