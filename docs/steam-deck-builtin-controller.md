# Steam Deck built-in controller handoff

By default, Deck input reaches the server through Steam Input's virtual
gamepad, which stargaze forwards as an emulated Xbox 360 pad. That works
for many games, but the remote machine never sees a *Steam Deck
Controller*: no gyro, no trackpads, no back paddles, no Steam-native
navigation — and a remote Steam that refuses virtual pads can't be
driven at all.

The **handoff** mode fixes this the same way stargaze forwards Steam
Controller dongles (v1.1.0): the Deck's built-in controller
(`28de:1205`) is tunneled wholesale over USB/IP through the session
connection. The server's kernel attaches it as a real USB device, and
the remote Steam sees an actual Steam Deck Controller with every
feature — remote Steam Input then also handles trackpad-mouse and
on-screen keyboard, exactly as if the Deck were plugged into the server.

## Setup (once, on the Deck)

SteamOS ships the `usbip-host` kernel module but doesn't load it, and
the sysfs knobs are root-only. Copy `scripts/steamos-usbip-setup.sh` to
the Deck and run:

```bash
sudo bash steamos-usbip-setup.sh
```

(Needs a sudo password: set one with `passwd` in desktop mode if you
never have. Everything lands under `/etc`, which survives SteamOS
updates; rerun after a major update if the handoff stops working.)

The server side needs the flake's `nixosModules.usb-server` — already
required for any USB forwarding.

Then enable **Settings → Handoff built-in controller** in the stargaze
launcher.

## What to expect during a session

- The controller *leaves* the Deck: local Steam (buttons, trackpads,
  Steam button, overlay) stops responding until the session ends. This
  is by design — the remote Steam owns the hardware.
- The touchscreen stays local and is forwarded as mouse input.
- **To end the session, hold Volume+ and Volume− together for one
  second.** The volume keys live on a separate local device and keep
  working. (Suspending the Deck with the power button also ends the
  session once the connection times out; the controller returns the
  moment the tunnel closes.)
- If the log shows `cannot bind ... to usbip-host`, the setup script
  hasn't been run (or a SteamOS update reverted it).
