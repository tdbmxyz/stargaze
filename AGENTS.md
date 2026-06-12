# AGENTS.md

This file provides guidance to LLM Code Assistants when working with code in this repository.

## Environment

- **NixOS** — The development environment is provided by `flake.nix` using [fenix](https://github.com/nix-community/fenix) for the Rust nightly toolchain. `cargo`/`rustc` are **not** on the host `PATH`: run all Rust commands through the dev shell, either interactively (`nix develop`) or one-shot (`nix develop -c cargo test --workspace`).
- **CUDA dev shell** — `nix develop .#cuda` extends the default shell with a CUDA-enabled FFmpeg and the CUDA toolkit, needed for NVENC-related tests on an NVIDIA host.
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

The project is **licensed AGPL-3.0-or-later** (see `LICENSE`), chosen for GPL-3.0 compatibility with Sunshine/Moonlight as a precaution against derived/copied code. It is a personal, hardware-specific, largely vibe-coded project provided **as-is, with no warranty and no liability** on the author's part — portability beyond the owner's own setup is out of scope for now. Preserve SPDX/license metadata and do not relicense without explicit instruction.

## MVP Target Environment

- **Server**: Linux Wayland (Hyprland), headless, NVIDIA GPU (modern — NVENC-capable)
- **Client**: Linux Wayland (Hyprland), AMD CPU, no discrete GPU (software or VAAPI decoding)
- **Video**: Low-latency H.265 encoding (NVENC) / VAAPI or software decoding (FFmpeg)
- **Audio**: Opus codec, stereo, 48 kHz
- **Input**: Keyboard, mouse, and gamepad forwarded from client to server (evdev/uinput)
- **Mic**: Optional mic forwarding via rsonance subprocess
- **Cursor**: Compositor-embedded cursor (configurable)
- **Network**: LAN streaming (no NAT traversal or internet streaming required for MVP)

## Architecture

### Pipeline

```
SERVER                                              CLIENT
PipeWire capture (DMA-BUF or MemFd)                 SDL2 render (IYUV texture)
  → FFmpeg NVENC H.265 encode                         ↑ renderer keeps latest decoded frame
    (DMA-BUF path: EGL → GL → CUDA interop)         FFmpeg H.265 decode (VAAPI, sw fallback)
  → fragment into QUIC datagrams ──── QUIC ────→      ↑ bounded channel, capacity 2
PipeWire audio → Opus encode      ──── QUIC ────→   FrameAssembler (in-order reassembly)
uinput injection ←──── control stream (reliable) ←─ SDL input events, IDR requests
```

- **Transport**: `quinn` QUIC. Media frames travel as **unreliable datagrams**, fragmented to the connection MTU, with a `postcard`-serialized `DatagramHeader` (stream type, frame index, fragment index/count, pts, keyframe flag). Control messages (session handshake, input events, IDR requests) travel on a **reliable bidirectional stream**, length-prefixed.
- **Threading model (client)**: tokio runtime for transport; a dedicated `std::thread` each for video decode and audio decode (FFmpeg/Opus are blocking); the SDL render + event loop runs on the **main thread** (SDL requirement) via `block_in_place`. Bridges between sync and async land use channels — never block a tokio worker with a sync `recv()`; use `spawn_blocking`.
- **Threading model (server)**: tokio for transport/portal; dedicated threads for the PipeWire capture loop and the FFmpeg encode loop.

### Loss-recovery invariants (do not re-break these)

1. **Frames reach the decoder in `frame_index` order, with no gaps — or an IDR is requested.** The `FrameAssembler` (client `transport/receiver.rs`) delivers strictly in order. A frame is declared lost and skipped only after the stream advances two complete frames past it (tolerates datagram reordering); skipping triggers a rate-limited `IdrRequest` on the control stream.
2. **Never silently drop an encoded delta frame.** Dropping one breaks decoder references and corrupts output until the next keyframe (~2 s GOP). Frames may be dropped in exactly two places: at the receiver when the decode channel is full (**must** request an IDR), and at the renderer **after** decoding (always safe).
3. **The decoder decodes everything it receives.** Catch-up logic belongs at the receiver (drop + IDR) and renderer (keep latest), not in the decode loop.

## Project Status

All 9 MVP sub-projects plus cursor rendering are implemented. **"Implemented" means the code path exists and unit tests pass — not that it has been verified end-to-end on target hardware.** History shows several features that compiled and "worked" while silently degraded (VAAPI decode falling back to software, IDR-on-loss never firing). When touching capture/encode/decode/render, verify at runtime: the first frames of each pipeline stage emit diagnostic `info!` logs (formats, strides, hw-accel status) — read them.

See `docs/roadmap.md` for follow-up tasks and known issues (e.g. 10-bit display formats).

## Rust Coding Standards

### Edition and Toolchain

- **Edition**: 2024
- **Toolchain**: Rust nightly (managed by fenix via `flake.nix`)

### Error Handling

Use `thiserror` for library/domain errors and `anyhow` for application-level errors. Avoid `.unwrap()` in production code; prefer the `?` operator or explicit error handling.

### Async Runtime

Use **tokio** as the async runtime. Use `#[tokio::main]` for binary entry points and `#[tokio::test]` for async tests. Anything that blocks (FFmpeg, SDL, PipeWire, sync channel `recv`) runs on a dedicated `std::thread` or `spawn_blocking` — never inside an async task.

### Documentation

Use rustdoc (`///`) on public items. Write a one-line summary in prose; add `# Errors`, `# Panics`, and `# Safety` sections where they apply (clippy pedantic enforces these). Do **not** add boilerplate `# Arguments` / `# Returns` sections to functions whose signature already says it.

### Code Quality

Run all of these inside the dev shell before considering any change done:

- **Formatting**: `cargo fmt`
- **Linting**: `cargo clippy --workspace --all-targets -- -W clippy::pedantic` — `--all-targets` is required: tests and integration tests have repeatedly accumulated lint errors that the default lib/bin-only run never sees. Fix new warnings; don't let them ride.
- **Testing**: `cargo test --workspace`
- **Checking**: `cargo check --workspace` for fast feedback

Hardware-dependent tests (`NVENC`, `uinput`, live compositor) are `#[ignore]`d and listed in each test's doc comment with the command to run them manually. They do not run in CI/sandboxes — do not interpret a green suite as hardware verification.

### CLI Help Text

Let clap handle default value display. Do **not** manually write `[default: X]` in doc comments for args that use `default_value_t` — clap appends it automatically.

## Known Pitfalls

Hard-won, non-obvious constraints. Check this list before "simplifying" anything that looks redundant.

- **FFmpeg hardware decode needs a `get_format` callback.** Attaching `hw_device_ctx` to a decoder context is *not* enough: FFmpeg's default `get_format` skips hardware pixel formats and silently decodes in software. The callback must explicitly select `AV_PIX_FMT_VAAPI` (see client `decode/ffmpeg.rs::select_vaapi_format`). Probe support first with `avcodec_get_hw_config`.
- **SDL2 must be initialized and pumped on the main thread.** The render loop doubles as the input event pump; don't move it to a worker.
- **No vsync, but no busy-spin either.** The render loop deliberately avoids `present_vsync()` (adds up to a frame of input latency) and instead blocks on the decoded-frame channel with a ~2 ms timeout. Don't reintroduce either extreme.
- **NVIDIA GL driver can't bind linear DMA-BUF EGL images to `GL_TEXTURE_2D`** — use `GL_TEXTURE_EXTERNAL_OES` (see `encode/egl_cuda.rs`).
- **nvidia-vaapi-driver cannot export decode surfaces as dma-bufs.** `vaExportSurfaceHandle` "succeeds" but the exported planes read as zeros, and the export poisons the decoder — every later `vaBeginPicture` fails with `MAX_NUM_EXCEEDED` until the decoder is re-created (verified on 595.71.05). The client blocklists it for zero-copy rendering (`decode/mod.rs::zero_copy_allowed`); local zero-copy testing on the NVIDIA box needs `STARGAZE_FORCE_ZERO_COPY=1` and exercises only the failure/recovery path.
- **PipeWire MemFd buffers must be mmap'd manually**; `MAP_BUFFERS` cannot be relied on (commit `c9c8764`).
- **Keyframes are self-contained on the wire**: the encoder prepends `extradata` (VPS/SPS/PPS) to every keyframe so a client can join or recover mid-stream. Keep it that way.
- **10-bit compositor formats** (`xBGR2101010` etc.) reach the portal on some setups; see `docs/roadmap.md` for workarounds.
- **rustls needs a crypto provider installed** before any quinn/TLS call: `rustls::crypto::ring::default_provider().install_default()`.

## Project Structure

```
stargaze/
├── crates/
│   ├── stargaze-core/       # Shared library: config, transport protocol, input events, errors
│   ├── stargaze-server/     # Server binary: capture → encode → transport, input injection
│   │   ├── capture/         # PipeWire screen capture via xdg-desktop-portal
│   │   ├── encode/          # FFmpeg NVENC H.265 (+ EGL→GL→CUDA DMA-BUF import), Opus
│   │   ├── audio/           # PipeWire audio capture
│   │   ├── input/           # evdev/uinput input injection
│   │   └── transport/       # QUIC server endpoint, frame fragmentation, control stream
│   └── stargaze-client/     # Client binary: transport → decode → render, input capture
│       ├── decode/          # FFmpeg H.265 decode (VAAPI + sw fallback), Opus decode
│       ├── render/          # SDL2 window, IYUV texture, audio playback, event pump
│       └── transport/       # QUIC client, FrameAssembler, receiver, IDR requests
├── docs/
│   └── roadmap.md           # Follow-up tasks, known issues
├── plans/                   # Implementation plans — ACTIVE work only, delete when merged
├── data/examples/           # Reference projects (gitignored)
├── .devcontainer/           # Docker devcontainer (alternative to Nix)
├── flake.nix                # Nix dev shells (default, .#cuda) and packages
├── Cargo.toml               # Workspace manifest
└── README.md                # User-facing docs (each crate also has one)
```

## Package Management

This project uses **Cargo** with nightly Rust. Enter the dev shell first:

```bash
nix develop   # Provides rustc, cargo, clippy, rustfmt, and all native deps

cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -W clippy::pedantic
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
- Run `cargo fmt && cargo clippy --workspace --all-targets -- -W clippy::pedantic` before commits
- A commit message must describe what the change *verifiably does*, not what it aspires to do. Don't write `feat(decode): add VAAPI hw decode` if the hardware path was never observed working — that hides the gap from future readers.

## Creating a Plan

When creating a plan in the `plans/` directory, include at least:

- **Issues to Address** — What the change is meant to do.
- **Important Notes** — Things important to the implementation.
- **Implementation Strategy** — High level approach. No code.
- **Tests** — What tests to write. Don't over-test.

Do NOT include: *Timeline*, *Rollback plan*.

**Delete the plan once the work is merged** — `plans/` is for active work only; completed plans live in git history. Promote anything worth keeping (pitfalls, workarounds) to this file or `docs/roadmap.md` first.

## Active Technologies

- Rust nightly (edition 2024), tokio, serde, thiserror, anyhow
- FFmpeg (NVENC H.265, VAAPI decode), PipeWire, ashpd (xdg-desktop-portal)
- cudarc + khronos-egl + gl (DMA-BUF → CUDA import on the server)
- SDL2, Opus, quinn (QUIC), rustls/ring, postcard (serialization)
- evdev (input injection), rsonance (mic forwarding)
