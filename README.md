# Stargaze

A Rust-native, low-latency desktop and game streaming system for Linux/Wayland.

Stargaze streams a Wayland desktop from a host machine (the **server**) to another machine on the LAN (the **client**) with hardware video encoding, Opus audio, and full keyboard/mouse/gamepad forwarding. It is inspired by the [Sunshine](https://github.com/LizardByte/Sunshine) + [Moonlight](https://github.com/moonlight-stream/moonlight-qt) ecosystem, rebuilt as a deliberately simple two-binary architecture in Rust.

## Features

- **Video**: PipeWire screen capture (DMA-BUF zero-copy or CPU path) → NVENC H.265 → QUIC → VAAPI or multi-threaded software decode → SDL2
- **Audio**: PipeWire capture → Opus (stereo, 48 kHz) → SDL2 playback
- **Input**: keyboard, mouse, and game controller events forwarded from the client and injected on the server via uinput
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

## Usage

On the host machine:

```bash
stargaze-server --resolution 2560x1440 --framerate 60 --bitrate 20
# A portal dialog asks which screen to share on first run.
```

On the client machine:

```bash
stargaze-client --server 192.168.1.10
# Esc or closing the window ends the session.
```

Both binaries accept `--help` for the full flag list and read an optional TOML config file (CLI flags override it):

```toml
# ~/.config/stargaze/server.toml
bind_address = "0.0.0.0"
port = 9000
framerate = 60
bitrate = 20            # Mbps
codec = "h265"

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
# ~/.config/stargaze/client.toml
server_address = "192.168.1.10"
port = 9000
fullscreen = true
```

### Diagnostics

Logging uses `tracing` with `RUST_LOG` (default `info`). The first few frames of every pipeline stage log their negotiated formats, strides, and hardware-acceleration status — start there when the picture is wrong:

```bash
RUST_LOG=debug stargaze-client --server 192.168.1.10
```

## Project status

All MVP milestones (capture, encode, transport, decode, render, audio, input, mic forwarding, cursor) are implemented. See [`docs/roadmap.md`](docs/roadmap.md) for follow-up work and known issues, and [`AGENTS.md`](AGENTS.md) for architecture invariants and development conventions.
