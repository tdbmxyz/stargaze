# Steam Controller on the server (built-in USB forwarding)

Steam talks to the Steam Controller through its raw HID protocol, not
through evdev, and it identifies the hardware by its **USB topology**:
interface numbers 1–4 on the `28de:1142` wireless dongle, interface 2
on the wired `28de:1102` (see `SDL_hidapi_steam.c` for the public
mirror of this logic). Virtual recreations (uinput or uhid) have no USB
parent, so hidapi reports `Interface: -1` and Steam enumerates them but
never adopts them. This was verified end to end: a uhid recreation gets
the kernel `hid-steam` driver and a working evdev device, and Steam
still ignores it.

Stargaze therefore forwards Valve controller hardware **as a USB
device**, using the kernel's USB/IP drivers with the byte flow tunneled
through the session's own QUIC connection:

```
CLIENT                                       SERVER
dongle → usbip-host stub                     vhci-hcd virtual host controller
   ↑ socket pair end                            ↑ socket pair end
stargaze-client ══ QUIC bidirectional stream ══ stargaze-server
```

- No `usbipd`, no open TCP port: the tunnel rides the existing session.
- Session-scoped: the device leaves the client when the session starts
  and **returns automatically** when it ends (or the connection drops).
- On the server it is genuine USB hardware — steam-devices udev rules
  match, Steam adopts it fully (gyro, paddles, per-game configs).

Enabled by default; disable with `--usb-forward false` (or
`usb_forward = false` in `client.toml`). Forwarded hardware: wired
Steam Controller (`28de:1102`), wireless dongle (`28de:1142`), Steam
Deck controller (`28de:1205`).

## One-time system setup

The kernel interfaces involved (binding a device to the `usbip-host`
stub, attaching to `vhci-hcd`) are root-only sysfs attributes. The
flake ships NixOS modules that load the kernel modules and make those
attributes writable by a `stargaze-usb` group — the same pattern
Sunshine uses for `/dev/uinput` — so the binaries run unprivileged.

On the **client** machine:

```nix
# flake input: stargaze.url = "github:tdbmxyz/stargaze";
imports = [inputs.stargaze.nixosModules.usb-client];
services.stargaze.usbClient = {
  enable = true;
  users = ["yourname"];
};
```

On the **server**:

```nix
imports = [inputs.stargaze.nixosModules.usb-server];
services.stargaze.usbServer = {
  enable = true;
  users = ["yourname"];
};
```

Non-NixOS equivalent: load `usbip_host` (client) / `vhci-hcd` (server)
at boot, and make these sysfs attributes group-writable for the user
running stargaze — `/sys/bus/usb/drivers/usbip-host/{match_busid,bind,unbind,rebind}`,
`/sys/bus/usb/drivers/usb/unbind`, `/sys/bus/usb/drivers_probe`, and
per-device `usbip_sockfd` on the client (a udev rule keeps up with
hotplug); `/sys/devices/platform/vhci_hcd.*/{attach,detach}` on the
server.

## Behavior notes

- While a session is up, the controller drives the **server**; the
  client machine doesn't see it at all (that's the point — no double
  input, no lizard-mode leakage through the local cursor).
- If the session dies abruptly, both sides clean up on their own: the
  server detaches the vhci port, the client rebinds the device to its
  regular driver.
- The client rescans every 2 seconds, so plugging the dongle in
  mid-session forwards it without a restart.

## Manual alternative (standalone usbip)

The classic `usbip` tooling still works without any stargaze
involvement — useful for debugging or for forwarding devices stargaze
doesn't know about: run `usbipd` on the client, `usbip bind -b <busid>`
(client) and `usbip attach -r <client> -b <busid>` (server), with TCP
3240 reachable. Remember that plain usbip is unauthenticated and
unencrypted: keep it on a trusted network. The built-in tunnel doesn't
have this problem (it inherits the session's QUIC/TLS).
