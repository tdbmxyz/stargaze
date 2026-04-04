# AGENTS.md

This file provides guidance to LLM Code Assistants when working with code in this repository.

## Environment

- **NixOS** — The development environment is provided by `flake.nix` using [fenix](https://github.com/nix-community/fenix) for the Rust nightly toolchain. Enter the dev shell with `nix develop`.
- **Devcontainer alternative** — A `.devcontainer/` setup exists for Docker/GPU environments (Debian Trixie + CUDA). Either environment works.
- **Already in the correct directory** — do NOT prefix commands with `cd workspace-folder` or similar. All commands run from the project root by default.
- **No need to summarize** — do NOT provide summaries of what you did at the end of your responses. Just complete the task.
- **Local-only Git possible** — The repository may be a local-only Git (no remote/origin configured). Do not assume a remote exists; perform merges locally and only push when a valid remote is available or when the user provides one.

## Project Overview

**Stargaze** is a Rust-native low-latency desktop/game streaming system. It consists of two binaries:

- **Server** — captures screen and audio on the host machine, encodes them, and streams to the client over the network.
- **Client** — receives and decodes video/audio streams, renders them locally, and forwards keyboard, mouse, and controller input back to the server.

The project is inspired by the [Sunshine](https://github.com/LizardByte/Sunshine) (server) + [Moonlight](https://github.com/moonlight-stream/moonlight-qt) (client) ecosystem, reimplemented in Rust. It follows [rsonance](https://github.com/tdbmxyz/rsonance)'s approach of a simple two-binary architecture rather than Sunshine/Moonlight's complex multi-protocol stack.

The goal is **not** to be feature-complete with Sunshine+Moonlight. The MVP targets a single specific use case (see below).

## MVP Target Environment

- **Server**: Linux Wayland (Hyprland), headless, NVIDIA GPU (modern — NVENC-capable)
- **Client**: Linux Wayland (Hyprland), AMD CPU, no discrete GPU (software or VAAPI decoding)
- **Video**: Low-latency H.265 encoding (NVENC) / software decoding (FFmpeg)
- **Audio**: Opus codec, stereo, 48 kHz
- **Input**: Keyboard, mouse, and gamepad forwarded from client to server (evdev/uinput)
- **Mic**: Optional mic forwarding via rsonance subprocess
- **Cursor**: Compositor-embedded cursor (configurable)
- **Network**: LAN streaming (no NAT traversal or internet streaming required for MVP)

## Project Status

All 9 MVP sub-projects are implemented, plus cursor rendering (post-MVP #5):

| # | Sub-project | Status |
|---|-------------|--------|
| 1 | Scaffolding (workspace, CLI, config, logging) | Done |
| 2 | Video Capture (PipeWire + portal, DMA-BUF + CPU) | Done |
| 3 | Video Encoding (NVENC H.265 via FFmpeg) | Done |
| 4 | Network Transport (QUIC via quinn, datagrams) | Done |
| 5 | Video Decoding + Rendering (FFmpeg + SDL2) | Done |
| 6 | Audio Capture + Encoding (PipeWire + Opus) | Done |
| 7 | Audio Decoding + Playback (Opus + SDL2 queue) | Done |
| 8 | Input Forwarding (SDL → evdev/uinput) | Done |
| 9 | Mic Forwarding (rsonance subprocess) | Done |
| — | Cursor Rendering (compositor-embedded) | Done |

See `docs/roadmap.md` for remaining follow-up tasks.

## Rust Coding Standards

### Edition and Toolchain

- **Edition**: 2024
- **Toolchain**: Rust nightly (managed by fenix via `flake.nix`)

### Error Handling

Use `thiserror` for library/domain errors and `anyhow` for application-level errors. Avoid `.unwrap()` in production code; prefer the `?` operator or explicit error handling.

### Async Runtime

Use **tokio** as the async runtime. Use `#[tokio::main]` for binary entry points and `#[tokio::test]` for async tests.

### Documentation

Use rustdoc-style documentation (`///` with `# Arguments`, `# Returns`, `# Errors` sections).

### Code Quality

- **Formatting**: `cargo fmt` before every commit
- **Linting**: `cargo clippy --workspace -- -W clippy::pedantic`
- **Testing**: `cargo test --workspace`
- **Checking**: `cargo check --workspace` for fast feedback

### CLI Help Text

Let clap handle default value display. Do **not** manually write `[default: X]` in doc comments for args that use `default_value_t` — clap appends it automatically.

## Project Structure

```
stargaze/
├── crates/
│   ├── stargaze-core/       # Shared types: config, capture, encode, transport, input, audio, error
│   ├── stargaze-server/     # Server binary: capture → encode → transport, input injection
│   │   ├── capture/         # PipeWire screen capture via xdg-desktop-portal
│   │   ├── encode/          # FFmpeg NVENC H.265 + Opus audio encoding
│   │   ├── audio/           # PipeWire audio capture
│   │   ├── input/           # evdev/uinput input injection
│   │   └── transport/       # QUIC server endpoint, frame fragmentation, sender
│   └── stargaze-client/     # Client binary: transport → decode → render, input capture
│       ├── decode/          # FFmpeg H.265 software decode + Opus audio decode
│       ├── render/          # SDL2 window, texture, audio playback
│       └── transport/       # QUIC client, frame assembler, receiver
├── docs/
│   └── roadmap.md           # Follow-up tasks and future work
├── plans/                   # Implementation plans (for active work only)
├── data/
│   └── examples/            # Reference projects (gitignored)
├── .devcontainer/           # Docker devcontainer (alternative to Nix)
├── flake.nix                # Nix dev environment
├── Cargo.toml               # Workspace manifest
└── Cargo.lock
```

## Package Management

This project uses **Cargo** with nightly Rust. Enter the dev shell first:

```bash
nix develop   # Provides rustc, cargo, clippy, rustfmt, and all native deps

cargo build
cargo test --workspace
cargo clippy --workspace -- -W clippy::pedantic
cargo fmt --check
```

### Adding Dependencies

```bash
cargo add package-name
cargo add --dev package-name
```

## Git Workflow

- Use feature branches for new work, e.g. `feature/wayland-capture`, `fix/audio-sync-drift`
- **Commit every change at an atomic level** — each commit should represent a single logical change that compiles and (where applicable) passes tests
- Use **conventional commits** with scope:
  - `feat(capture): add KMS/DRM screen capture`
  - `fix(audio): correct Opus frame timing`
  - `refactor(protocol): extract RTP packet builder`
  - `chore(deps): update tokio to 1.x`
  - `docs(readme): add build instructions`
  - `test(decode): add VAAPI decoder unit tests`
- Run `cargo fmt && cargo clippy --workspace -- -W clippy::pedantic` before commits

## Creating a Plan

When creating a plan in the `plans/` directory, include at least:

- **Issues to Address** — What the change is meant to do.
- **Important Notes** — Things important to the implementation.
- **Implementation Strategy** — High level approach. No code.
- **Tests** — What tests to write. Don't over-test.

Do NOT include: *Timeline*, *Rollback plan*.

## Active Technologies

- Rust nightly (edition 2024), tokio, serde, thiserror, anyhow
- FFmpeg (NVENC H.265), PipeWire, ashpd (xdg-desktop-portal)
- SDL2, Opus, quinn (QUIC), postcard (serialization)
- evdev (input injection), rsonance (mic forwarding)
