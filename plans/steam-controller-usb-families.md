# Steam Controller USB families

## Issues to Address

- The new Steam Controller puck on athena (`28de:1304`, product `Steam Ctrl (USB)`) is absent from the USB forwarder's old-controller-only allowlist, so input falls back to Xbox 360 emulation.
- Centralize known controller USB identities so extending support does not require replacing the transport or weakening device selection.
- Correct documentation that implies the Deck controller is forwarded by default or that Xbox fallback is the only Valve path.

## Important Notes

- Athena's receiver has seven interfaces, with controller HID interfaces 2–5; forwarding the entire USB device preserves this topology.
- Athena already has the client USB/IP module, group membership, and writable driver knobs. Do not change its running session or detach hardware merely to gather traces.
- SDL's upstream controller list identifies new-model wired USB as `28de:1302`, Proteus puck as `28de:1304`, and Nereid receiver as `28de:1305`.
- BLE `28de:1303` is not a USB device; never forward the entire Bluetooth adapter to emulate native forwarding.
- Do not match the Valve vendor wholesale. In particular, a `28de:2432` Wi-Fi adapter exists on the development host; VR hardware, bootloaders, virtual controllers, and unknown devices must stay local.
- Preserve the Deck's explicit handoff opt-in and existing USB/IP cleanup behavior.
- Live test on 2026-10-07 (athena → zeus) verified the Proteus puck is forwarded, enumerates as real USB, and is adopted by Steam with the Triton configuration. Graceful shutdown restored the client USB/HID drivers and released the server vhci port; reconnect succeeded. The user subsequently confirmed gyro and rear-button operation on zeus. Haptics were not separately verified; the other new-model connection modes still need hardware verification.
- A pre-existing emulated Xbox pad survives server session disconnect; the test created no new emulated pad. Recorded as a separate follow-up in `docs/roadmap.md`.

## Implementation Strategy

- Extract a named controller USB registry, covering the original controller/receiver, new wired controller/receivers, and opt-in Deck controls.
- Use the registry for sysfs discovery and diagnostic model identification, retaining the generic USB tunnel and server attachment path unchanged.
- Add fixture-based sysfs discovery tests based on athena's actual identity and interface layout.
- Update controller forwarding docs with supported connection modes, setup, fallback limitations, and runtime verification steps.

## Tests

- Registry selects all known USB controllers and enforces the built-in Deck opt-in.
- Exclude BLE IDs, unrelated Valve hardware, virtual pads, bootloaders, foreign vendors, and unknown IDs.
- Fake sysfs verifies new and old device discovery, metadata/speed, interface filtering, malformed attributes, and opt-in Deck discovery without binding real devices.
- Run formatting, workspace check, all-target pedantic clippy, and workspace nextest.
