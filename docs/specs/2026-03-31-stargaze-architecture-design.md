# Stargaze — Architecture Design

## Overview

**Stargaze** is a Rust-native low-latency desktop/game streaming system consisting of two binaries (server and client) that share a common library crate. It reimplements the functionality of [Sunshine](https://github.com/LizardByte/Sunshine) (server) + [Moonlight](https://github.com/moonlight-stream/moonlight-qt) (client) in Rust, following [rsonance](https://github.com/algora-io/rsonance)'s approach of a simple two-binary architecture.

The goal is **not** feature-completeness with Sunshine+Moonlight. The MVP targets a single environment: streaming from a Linux Wayland (Hyprland) headless server with an NVIDIA GPU to a Linux Wayland (Hyprland) laptop with an AMD CPU and no discrete GPU, over LAN.

## Architecture

### Workspace Structure

Cargo workspace with 3 crates:

- **`stargaze-core`** — shared library: config types (serde + TOML), common error types (thiserror), protocol message definitions, constants (default ports, codec identifiers). No runtime logic — just types and utilities.
- **`stargaze-server`** — binary: captures screen (Wayland/KMS) and audio (PipeWire), encodes (NVENC + Opus), streams over UDP to the client. Listens for input events from the client. Emulates input via uinput/evdev.
- **`stargaze-client`** — binary: connects to the server, receives and decodes video (FFmpeg/VAAPI) and audio (Opus), renders to a Wayland window, captures local keyboard/mouse/gamepad input and sends it back to the server.

### Connection Model

Simple IP + port. Server binds and listens. Client connects with `--server <addr> --port <port>`. No discovery, no pairing, no authentication for MVP.

### Config Layering

TOML config files (`~/.config/stargaze/server.toml`, `~/.config/stargaze/client.toml`) provide defaults. CLI arguments override any config file value. A missing config file is silently ignored — everything has sensible defaults.

### Mic Forwarding

Microphone forwarding is delegated to **rsonance** as an external companion tool, not reimplemented. The client can optionally launch rsonance's transmitter to send mic audio back to the server, where rsonance's receiver creates a virtual PulseAudio microphone. This keeps stargaze focused on game/desktop streaming.

## MVP Target Environment

- **Server**: Linux Wayland (Hyprland), headless, NVIDIA GPU (modern — NVENC-capable)
- **Client**: Linux Wayland (Hyprland), AMD CPU, no discrete GPU (software or VAAPI decoding)
- **Video**: Low-latency encoding/decoding using H.265 or AV1; no need to support old hardware
- **Audio**: Opus codec, stereo at minimum
- **Input**: Keyboard, mouse, and gamepad forwarded from client to server
- **Network**: LAN streaming only (no NAT traversal or internet streaming)

## Sub-project Breakdown

Each sub-project gets its own plan and implementation cycle. They are built in this order, each producing something testable on its own:

### 1. Scaffolding

Cargo workspace, binary crates, shared core, CLI (clap), config (serde + toml), logging (tracing). No streaming functionality — just the skeleton that compiles and runs `--help`.

### 2. Video Capture (Server)

Wayland screen capture on the server using PipeWire or wlr-screencopy/ext-screencopy protocols. Outputs raw frames. Testable standalone by dumping frames to a file.

### 3. Video Encoding (Server)

Takes raw frames from capture, encodes with NVENC (H.265 or AV1). Outputs encoded NAL units. Testable by writing an encoded file and verifying with ffprobe.

### 4. Network Transport

The protocol layer: UDP streaming with RTP-like framing for video/audio packets, plus a TCP control channel for session setup and input events. Testable with synthetic data (no real capture needed).

### 5. Video Decoding + Rendering (Client)

Client-side FFmpeg/VAAPI decode of the encoded stream, rendered into a Wayland window. End-to-end test: server sends encoded test pattern, client decodes and displays it.

### 6. Audio Capture + Encoding (Server)

PipeWire audio capture, Opus encoding, sent over the transport layer.

### 7. Audio Decoding + Playback (Client)

Opus decode, audio output via PipeWire/ALSA.

### 8. Input Forwarding (Client -> Server)

Keyboard, mouse, and gamepad capture on the client (evdev or libinput), sent over the control channel, injected on the server via uinput.

### 9. Mic Forwarding

Integration with rsonance — documented as a companion tool or optionally launched as a subprocess. Not a core stargaze feature.

## Scaffolding Spec (Sub-project 1)

### Workspace Layout

```
stargaze/
├── Cargo.toml                    # Workspace manifest
├── crates/
│   ├── stargaze-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs            # Re-exports
│   │       ├── config.rs         # ServerConfig, ClientConfig, shared defaults
│   │       └── error.rs          # Common error types
│   ├── stargaze-server/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs           # CLI parsing, config loading, tokio runtime, logging init
│   └── stargaze-client/
│       ├── Cargo.toml
│       └── src/
│           └── main.rs           # CLI parsing, config loading, tokio runtime, logging init
├── data/                         # Reference examples (gitignored)
├── docs/                         # Design specs
├── plans/                        # Implementation plans
├── tests/                        # Integration tests (empty for now)
```

### Dependencies

| Crate | `stargaze-core` | `stargaze-server` | `stargaze-client` |
|---|---|---|---|
| `serde` (with derive) | yes | — | — |
| `toml` | yes | — | — |
| `thiserror` | yes | — | — |
| `anyhow` | — | yes | yes |
| `clap` (with derive) | — | yes | yes |
| `tokio` (full) | — | yes | yes |
| `tracing` | yes | yes | yes |
| `tracing-subscriber` | — | yes | yes |
| `directories` | yes | — | — |
| `stargaze-core` | — | yes (path) | yes (path) |

### Config

**`ServerConfig`** fields (all with defaults):
- `bind_address`: `String` — default `"0.0.0.0"`
- `port`: `u16` — default `9000`
- `resolution`: `Resolution` (width, height) — default `1920x1080`
- `framerate`: `u32` — default `60`
- `bitrate`: `u32` (Mbps) — default `20`
- `codec`: `Codec` enum (H265, Av1) — default `H265`

**`ClientConfig`** fields:
- `server_address`: `String` — required (no default)
- `port`: `u16` — default `9000`
- `fullscreen`: `bool` — default `true`

Config file locations use `directories` crate: `~/.config/stargaze/server.toml` and `~/.config/stargaze/client.toml`. CLI args override config values. Missing file is silently ignored.

### CLI

```
stargaze-server [OPTIONS]
  --bind <ADDR>        Bind address [default: 0.0.0.0]
  --port <PORT>        Port [default: 9000]
  --resolution <WxH>   Resolution [default: 1920x1080]
  --framerate <FPS>    Framerate [default: 60]
  --bitrate <BPS>      Bitrate in Mbps [default: 20]
  --codec <CODEC>      Video codec: h265, av1 [default: h265]
  --config <PATH>      Config file path override

stargaze-client [OPTIONS]
  --server <ADDR>      Server address (required)
  --port <PORT>        Port [default: 9000]
  --fullscreen         Fullscreen mode [default: true]
  --config <PATH>      Config file path override
```

### Logging

`tracing` with `tracing-subscriber` using `EnvFilter`. Default level: `info`. Override with `RUST_LOG` environment variable. Format: human-readable for terminal with timestamps.

### Success Criteria

- `cargo build` succeeds for the whole workspace
- `cargo test` passes
- `stargaze-server --help` and `stargaze-client --help` print usage
- `stargaze-server` starts, logs "Starting stargaze server on 0.0.0.0:9000", and exits (nothing to do yet)
- `stargaze-client --server 127.0.0.1` starts, logs "Connecting to 127.0.0.1:9000", and exits
- Config loading from TOML works, CLI overrides work
- `cargo clippy -W clippy::pedantic` is clean
- `cargo fmt` is clean
