# Steam Controllers on the server (built-in USB forwarding)

Both the original and 2026 Steam Controllers use the same whole-device
USB/IP forwarding path. Stargaze selects known controller/receiver IDs,
not all Valve devices; unrelated Wi-Fi, VR, firmware-update, and virtual
controller devices are never automatically exported.

Steam talks to the original Steam Controller through its raw HID protocol, not
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

- No `usbipd` or USB/IP TCP port (3240): the tunnel rides the existing session.
- Session-scoped: the device leaves the client when the session starts
  and **returns automatically** when it ends (or the connection drops).
- On the server it is genuine USB hardware — steam-devices udev rules
  match, Steam adopts it fully (gyro, paddles, per-game configs).

USB forwarding is enabled by default; disable with `--usb-forward false`
(or `usb_forward = false` in `client.toml`). Recognized USB hardware:

| Model / connection | USB VID:PID | Policy |
| --- | --- | --- |
| Original Steam Controller, wired | `28de:1102` | Forward by default |
| Original Steam Controller wireless dongle | `28de:1142` | Forward by default |
| 2026 Steam Controller (Triton), wired USB-C | `28de:1302` | Forward by default |
| 2026 Steam Controller Proteus puck | `28de:1304` | Forward by default |
| 2026 Steam Controller Nereid receiver | `28de:1305` | Forward by default |
| Steam Deck built-in controller | `28de:1205` | **Opt-in only**, via built-in controller handoff |

The new-model IDs are identified in [SDL's controller list](https://github.com/libsdl-org/SDL/blob/main/src/joystick/controller_list.h).
Athena's puck was observed as `28de:1304`, product `Steam Ctrl (USB)`,
with seven USB interfaces; controller HID interfaces 2–5 match
[SDL's Triton driver](https://github.com/libsdl-org/SDL/blob/main/src/joystick/hidapi/SDL_hidapi_steam_triton.c).
The tunnel preserves the whole device, its original identity and interfaces;
no new controller-specific packet translation is needed on the server.
**Live verification (2026-10-07, athena → zeus):** the updated client
forwarded the `28de:1304` puck, the server enumerated it as genuine USB,
and Steam opened its HID interfaces, established the wireless connection,
and loaded the Triton controller configuration. Graceful client exit
released the server's vhci port and restored the receiver's original
USB/HID drivers on athena; reconnect successfully forwarded it again.
The user subsequently confirmed gyro and rear-button operation on zeus.
Haptics were not separately verified during this test. Wired `1302` and
Nereid `1305` selection is unit-tested, not hardware-verified.

**Bluetooth is not native USB forwarding.** For the 2026 controller,
`28de:1303` is its BLE identity, not a USB device to export. Use the puck
or a USB cable for genuine-device forwarding. Do not export the client's
whole Bluetooth adapter: that would take its other paired devices away.
Bluetooth controllers can still use SDL's Xbox 360 emulation fallback,
provided the local SDL/Steam setup exposes a compatible gamepad.

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

## Verifying a new controller

1. On the client, confirm the USB identity in `lsusb` (for the new puck:
   `28de:1304`) and enable **USB forwarding** in Stargaze's settings.
2. Use the one-time client/server setup above. The NixOS permission rules
   are not model-specific; they also cover the new receiver. Keep Steam
   and its controller/hidraw udev rules current on the server so it can
   recognize and open the new model.
3. Start a session with the updated client. Its log should contain
   `USB device forwarded to the server`, the original VID/PID, and the
   named controller model. The server should log `USB device attached
   from the client` with the same VID/PID.
4. Check `lsusb` and Steam's controller settings on the **server**. A
   successful tunnel is not by itself proof that Steam adopted the pad:
   verify Steam names it as a Steam Controller and offers the expected
   gyro, trackpads, and rear-button settings rather than only Xbox controls.
5. End the session and verify the receiver returns to the client. Logs
   should show `USB device detached` (server) and `USB device released
   back to this machine` (client).

If only an Xbox 360 controller appears, inspect USB forwarding errors
first (missing module, sysfs permissions, no free vhci port, or the
wrong connection mode). Adding a device name to an emulated Xbox pad
cannot preserve the real controller's HID features. A receiver and all
controllers paired to it move together, not one controller at a time.

## Manual alternative (standalone usbip)

The classic `usbip` tooling still works without any stargaze
involvement — useful for debugging or for forwarding devices stargaze
doesn't know about: run `usbipd` on the client, `usbip bind -b <busid>`
(client) and `usbip attach -r <client> -b <busid>` (server), with TCP
3240 reachable. Remember that plain usbip is unauthenticated and
unencrypted: keep it on a trusted network. The built-in tunnel doesn't
have this problem (it inherits the session's QUIC/TLS).
