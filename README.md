# Stargaze

A Rust-native, low-latency desktop and game streaming system for Linux/Wayland.

Stargaze streams a Wayland desktop from a host machine (the **server**) to another machine on the LAN (the **client**) with hardware video encoding, Opus audio, and full keyboard/mouse/gamepad forwarding. It is **largely inspired by** the [Sunshine](https://github.com/LizardByte/Sunshine) + [Moonlight](https://github.com/moonlight-stream/moonlight-qt) ecosystem, rebuilt as a deliberately simple two-binary architecture in Rust.

> [!IMPORTANT]
> **Status: personal project, "vibe-coded".** Stargaze is, for now, nearly fully vibe-coded — written largely through AI-assisted, exploratory iteration rather than a rigorous engineering process. It was built **specifically for the owner's own hardware** (a particular Hyprland + NVIDIA host and AMD client; see [Requirements](#requirements)) and there is **no plan to make it work on more devices for now**. Expect rough edges. Use at your own risk — see [License & disclaimer](#license--disclaimer).

## Features

- **Video**: PipeWire screen capture (DMA-BUF zero-copy or CPU path) → NVENC H.265 → QUIC → VAAPI or multi-threaded software decode → SDL2
- **Audio**: PipeWire capture → Opus (stereo by default; mono/5.1/7.1 selectable, 48 kHz) → SDL2 playback. Surround is opt-in on the server (`audio_channels`) and advertised to the client in the handshake — see [docs/surround-audio.md](docs/surround-audio.md)
- **Input**: keyboard, mouse, and game controller events forwarded from the client and injected on the server via uinput. Controllers are passed through at the evdev level — the host sees the real device (name, vendor/product ids, exact button/axis layout) — with automatic per-device fallback to Xbox 360 pad emulation (`--gamepad-passthrough false` forces emulation). Valve controller hardware (Steam Controller, dongle, Steam Deck) is forwarded wholesale as a USB device — USB/IP tunneled through the session connection — because Steam only accepts it with its real USB topology; needs a one-time permission setup, see [docs/steam-controller-usbip.md](docs/steam-controller-usbip.md)
- **Mic forwarding** (optional): client microphone streamed back to the server via an [rsonance](https://github.com/tdbmxyz/rsonance) subprocess
- **Loss recovery**: unreliable QUIC datagrams for media with in-order frame reassembly; lost frames trigger rate-limited IDR keyframe requests so the picture recovers in a few frames instead of seconds
- **Low latency by design**: no vsync blocking in the render path, bounded channels with drop-oldest backpressure, IDR-on-drop

## Architecture

```
SERVER                                              CLIENT
PipeWire capture (DMA-BUF / MemFd)                  SDL2 render + input event pump
  → FFmpeg NVENC H.265 encode                         ↑ latest decoded frame
    (DMA-BUF: EGL → GL → CUDA interop)              FFmpeg H.265 decode (VAAPI / software)
  → fragmentation ────── QUIC datagrams ──────→     FrameAssembler (in-order, gap → IDR req)
PipeWire audio → Opus ── QUIC datagrams ──────→     Opus decode → SDL2 audio queue
uinput injection ←────── QUIC control stream ←───── input events, IDR requests, handshake
```

Two binaries, one shared library:

| Crate | Role |
|---|---|
| [`stargaze-server`](crates/stargaze-server/) | Capture, encode, send; inject client input |
| [`stargaze-client`](crates/stargaze-client/) | Receive, decode, render; capture and forward input |
| [`stargaze-core`](crates/stargaze-core/) | Shared config, wire protocol, input/event types |

## Requirements

**Server**
- Linux with Wayland and a screencast-capable portal (tested: Hyprland + `xdg-desktop-portal-hyprland`)
- PipeWire (video and audio)
- NVIDIA GPU with NVENC, proprietary driver, CUDA runtime
- FFmpeg with `hevc_nvenc`

**Client**
- Linux with PipeWire/Wayland
- FFmpeg (VAAPI hardware decode used when available, otherwise multi-threaded software decode)
- SDL2
- `/dev/dri/renderD128` access for VAAPI

**Network**: LAN. There is no NAT traversal, encryption is QUIC/TLS with a self-signed certificate, and there is **no authentication yet** — do not expose the server port to untrusted networks.

## Building

With Nix (recommended — pins the Rust nightly toolchain and all native dependencies):

```bash
nix develop          # dev shell (use `nix develop .#cuda` on the NVIDIA host)
cargo build --release
```

Or build the packaged binaries directly:

```bash
nix build .#stargaze-server
nix build .#stargaze-client
```

A `.devcontainer/` (Debian Trixie + CUDA) is provided as an alternative to Nix.

### Prebuilt client (AppImage)

Each release on GitHub ships `stargaze-client` as a self-contained x86_64 Linux AppImage (FFmpeg, SDL2, and Mesa bundled) for machines without Nix — e.g. a Steam Deck:

```bash
chmod +x stargaze-client-*.AppImage
./stargaze-client-*.AppImage --server 192.168.1.10
```

The server is not published as a prebuilt binary: it hard-requires NVIDIA CUDA/NVENC and is built from the flake on the host (`nix build .#stargaze-server`).

#### Adding to Steam (Steam Deck)

Add the AppImage as a non-Steam game. Two things matter:

- **Compatibility tool must be "None"** (game Properties → Compatibility → leave "Force the use of a specific Steam Play compatibility tool" unchecked). Proton and the Steam Linux Runtime run the game inside a container that breaks AppImages.
- Steam injects its own runtime libraries via `LD_LIBRARY_PATH`/`LD_PRELOAD`; the client strips those automatically at startup (since v1.2.3, hardened in v1.2.6: the stripping now runs in a host-shell stage before any bundled binary loads — Steam’s library path could previously crash the wrapper itself).
- Gaming mode’s compositor (gamescope) only displays windows that come in through XWayland, so the client prefers SDL’s `x11` video driver whenever an X display is available (since v1.2.5; the v1.2.4 gamescope-detection approach did not fire under Steam). Set `SDL_VIDEODRIVER` yourself to override. Steam Input mirrors one button press onto several devices; the launcher de-duplicates those, so navigation moves one step per press.

If the client still fails to start from Steam, check `~/.config/stargaze/client.log` — the client mirrors its stderr output there precisely because Steam swallows it.

#### Steam Deck controls

- **Real Deck controller on the remote (recommended)**: enable the built-in controller handoff — the remote Steam sees an actual Steam Deck Controller (gyro, trackpads, paddles) instead of an emulated Xbox 360 pad. One-time setup: [docs/steam-deck-builtin-controller.md](docs/steam-deck-builtin-controller.md). While a session runs the local controls belong to the remote; hold **Vol+ and Vol− together for one second** to end the session.

- **Ending a session**: hold **View (Select) + Menu (Start)** together for one second — the controller equivalent of `Ctrl+Alt+Shift+Q`. Works for both pass-through and emulated controllers; the launcher itself is fully D-pad/touch navigable.
- **Mouse on a remote desktop**: in gaming mode every Deck input goes through Steam Input, and the default gamepad layout generates *no mouse events at all* — the remote desktop looks unresponsive even though the controller is forwarded fine. Open the controller settings for stargaze in Steam and pick the official **"Gamepad with Mouse Trackpad"** layout (or map the right trackpad to Mouse yourself). Trackpad motion then arrives as real mouse events, which stargaze forwards to the remote session; the rest of the controller keeps working as a gamepad for remote games.

## Usage

On the host machine:

```bash
stargaze-server --resolution 2560x1440 --framerate 60 --bitrate 20
# A portal dialog asks which screen to share on first run.
# Headless host (no display to approve the dialog)? See
# docs/headless-screencast.md for an auto-approving portal setup.
```

On the client machine, just launch `stargaze-client` (from the desktop
menu or a terminal): a gamepad/touch-friendly launcher opens where you
save hosts (name, address, port, per-host resolution/framerate/codec),
tweak toggles, and connect. Sessions return to the launcher when they
end. For scripts, `--server` skips the launcher and connects directly
as before:

```bash
stargaze-client --server 192.168.1.10
# Esc or closing the window ends the session and exits.
```

Both binaries accept `--help` for the full flag list and read an optional TOML config file (CLI flags override it):

```toml
# ~/.config/stargaze/server.toml
bind_address = "0.0.0.0"
port = 9000
framerate = 60
bitrate = 20            # Mbps
codec = "h265"
audio_channels = 2      # 1 = mono, 2 = stereo, 6 = 5.1, 8 = 7.1

[resolution]
width = 2560
height = 1440

[cursor]
show_cursor = true

[mic_forward]
enabled = false
port = 9001
```

```toml
# ~/.config/stargaze/client.toml — managed by the launcher UI, hand-editable
fullscreen = true

[[hosts]]
name = "zeus"
address = "192.168.1.10"   # IP or DNS name
port = 9000
framerate = 60
bitrate = 20            # Mbps; 0 = server default (WiFi clients: 15-25)
codec = "h265"

[hosts.resolution]
width = 1920
height = 1080
```

(The legacy single `server_address`/`port` form still works and is
migrated into a host entry the first time the launcher saves.)

### Diagnostics

Logging uses `tracing` with `RUST_LOG` (default `info`). The first few frames of every pipeline stage log their negotiated formats, strides, and hardware-acceleration status — start there when the picture is wrong:

```bash
RUST_LOG=debug stargaze-client --server 192.168.1.10
```

FFmpeg's own diagnostics are routed through `tracing` under the `ffmpeg` target (filter with e.g. `RUST_LOG=info,ffmpeg=off`). Decoder warnings like `Could not find ref with POC N` are expected after network loss: a frame was dropped, the picture shows artifacts, and the client has already requested an IDR keyframe that clears them — the receiver logs the loss and the recovery alongside.

## Project status

All MVP milestones (capture, encode, transport, decode, render, audio, input, mic forwarding, cursor) are implemented. See [`docs/roadmap.md`](docs/roadmap.md) for follow-up work and known issues, and [`AGENTS.md`](AGENTS.md) for architecture invariants and development conventions.

This is a personal, hardware-specific project (see the note at the top). It is largely vibe-coded and targets only the owner's own setup; portability to other hardware, distributions, or compositors is explicitly out of scope for now.

## License & disclaimer

Stargaze is licensed under the **GNU Affero General Public License v3.0 or later** (AGPL-3.0-or-later). See [`LICENSE`](LICENSE) for the full text.

The client embeds the DejaVu Sans typeface for its launcher UI; DejaVu is distributed under the Bitstream Vera license (free to embed and redistribute), reproduced at [`assets/fonts/LICENSE-DejaVu`](assets/fonts/LICENSE-DejaVu).

The AGPL-3.0 was chosen deliberately for **compatibility with the GPL-3.0** under which [Sunshine](https://github.com/LizardByte/Sunshine) and [Moonlight](https://github.com/moonlight-stream/moonlight-qt) are distributed — as a precaution, in case any code, patterns, or protocol details turn out to have been derived or copied from those projects. Licensing under a GPL-3.0-compatible copyleft license keeps Stargaze in the clear with respect to their terms.

**No warranty.** This software is provided **"as is", without warranty of any kind**, express or implied, including but not limited to the warranties of merchantability, fitness for a particular purpose, and non-infringement, as set out in sections 15 and 16 of the AGPL-3.0.

**No liability.** The owner/author cannot be held responsible or liable for anything arising from the use of this software — including any damage, data loss, or other consequences. **Only the end user is responsible** for how they build, run, and use it. By using Stargaze you accept all risk.
