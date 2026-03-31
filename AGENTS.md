# AGENTS.md

This file provides guidance to LLM Code Assistants when working with code in this repository.

## Environment

- **We are in a devcontainer** - based on `rust:1.94-slim-trixie`. Don't be overly cautious about breaking things. The environment is isolated and easily reset.
- **Already in the correct directory** - do NOT prefix commands with `cd workspace-folder` or similar. All commands run from the project root by default.
- **No need to summarize** - do NOT provide summaries of what you did at the end of your responses. Just complete the task.
- **Local-only Git possible** - The repository in this environment may be a local-only Git (no remote/origin configured). Do not assume a remote exists; perform merges locally and only push when a valid remote is available or when the user provides one.
- **System packages available** - The devcontainer includes `ffmpeg`, `sox`, `build-essential`, and standard Unix tools. Install additional system libraries as needed with `apt-get`.

## Tool Calling

- ALWAYS USE PARALLEL TOOLS WHEN APPLICABLE. Here is an example illustrating how to execute 3 parallel file reads in this chat environment:

```json
{
    "recipient_name": "multi_tool_use.parallel",
    "parameters": {
        "tool_uses": [
            {
                "recipient_name": "functions.read",
                "parameters": {
                    "filePath": "path/to/file.rs"
                }
            },
            {
                "recipient_name": "functions.read",
                "parameters": {
                    "filePath": "Cargo.toml"
                }
            },
            {
                "recipient_name": "functions.read",
                "parameters": {
                    "filePath": "path/to/file.md"
                }
            }
        ]
    }
}
```

## Project Overview

**Stargaze** is a Rust-native low-latency desktop/game streaming system. It consists of two binaries:

- **Server** — captures screen and audio on the host machine, encodes them, and streams to the client over the network.
- **Client** — receives and decodes video/audio streams, renders them locally, and forwards keyboard, mouse, and controller input back to the server.

The project is inspired by the [Sunshine](https://github.com/LizardByte/Sunshine) (server) + [Moonlight](https://github.com/moonlight-stream/moonlight-qt) (client) ecosystem, reimplemented in Rust. It follows [rsonance](https://github.com/algora-io/rsonance)'s approach of a simple two-binary architecture rather than Sunshine/Moonlight's complex multi-protocol stack.

The goal is **not** to be feature-complete with Sunshine+Moonlight. The MVP targets a single specific use case (see below).

## MVP Target Environment

- **Server**: Linux Wayland (Hyprland), headless, NVIDIA GPU (modern — NVENC-capable)
- **Client**: Linux Wayland (Hyprland), AMD CPU, no discrete GPU (software or VAAPI decoding)
- **Video**: Low-latency encoding/decoding using modern codecs (H.265 or AV1); no need to support old hardware
- **Audio**: Opus codec, stereo at minimum
- **Input**: Keyboard, mouse, and gamepad forwarded from client to server
- **Network**: LAN streaming (no NAT traversal or internet streaming required for MVP)

## Reference Examples

Example projects are available in `data/examples/` (gitignored — not part of the repo history):

| Example | What it is | What to study |
|---|---|---|
| `Sunshine/` | C++ streaming server (with submodules) | Screen capture (KMS/DRM, Wayland, PipeWire), video encoding (NVENC, VAAPI, FFmpeg), audio capture, input emulation (uinput/evdev), streaming protocol (RTP/RTSP) |
| `moonlight-qt/` | C++/Qt streaming client (with submodules) | Video decoding (FFmpeg, VAAPI, VDPAU, DRM), audio playback (SDL2, Opus), input capture and forwarding, session management |
| `rsonance/` | Small Rust CLI audio streamer | Simple two-binary architecture (transmitter + receiver), Rust async patterns with tokio, cpal audio capture, clean CLI with clap |

## Rust Coding Standards

### Edition and Toolchain

- **Edition**: 2024
- **Toolchain**: Rust nightly (`rustup default nightly` or `+nightly` flag)

### Error Handling

Use `thiserror` for library/domain errors and `anyhow` for application-level errors. Avoid `.unwrap()` in production code; prefer the `?` operator or explicit error handling.

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CaptureError {
    #[error("Failed to initialize display capture: {0}")]
    InitError(#[from] std::io::Error),
    #[error("No suitable capture method available")]
    NoCaptureMethod,
}

pub fn init_capture() -> Result<CaptureSession, CaptureError> {
    let display = find_display()?;
    // ...
}
```

### Async Runtime

Use **tokio** as the async runtime. Use `#[tokio::main]` for binary entry points and `#[tokio::test]` for async tests.

### Documentation

Use rustdoc-style documentation:

```rust
/// Initializes the video encoder with the given configuration.
///
/// # Arguments
///
/// * `config` - Encoder configuration (codec, resolution, bitrate)
///
/// # Returns
///
/// An initialized `Encoder` ready to accept frames.
///
/// # Errors
///
/// Returns `EncoderError::UnsupportedCodec` if the requested codec is not available.
pub fn init_encoder(config: &EncoderConfig) -> Result<Encoder, EncoderError> {
    // ...
}
```

### Testing Strategy

- Use `cargo test` for all tests
- Place unit tests in the same file using `#[cfg(test)]` module
- Place integration tests in the `tests/` directory
- Use `#[tokio::test]` for async tests
- Mock external dependencies when needed

### Code Quality

- **Formatting**: `cargo fmt` before every commit
- **Linting**: `cargo clippy -W clippy::pedantic`
- **Checking**: `cargo check` for fast feedback

## Project Structure

```
stargaze/
├── src/
│   └── main.rs           # Placeholder (will become workspace or multi-binary)
├── data/
│   └── examples/         # Reference projects (gitignored)
│       ├── Sunshine/
│       ├── moonlight-qt/
│       └── rsonance/
├── tests/                # Integration tests
├── docs/                 # Design specs and plans
├── plans/                # Implementation plans
├── Cargo.toml
└── Cargo.lock
```

As the project grows, this will likely become a Cargo workspace with separate crates for the server and client binaries, and shared library crates for common functionality (protocol, codecs, etc.).

## Package Management

This project uses **Cargo** with nightly Rust.

### Common Commands

```bash
# Build the project
cargo build

# Run tests
cargo test

# Check for issues without building
cargo check

# Format code
cargo fmt

# Run clippy lints
cargo clippy -W clippy::pedantic
```

### Adding Dependencies

Edit `Cargo.toml` directly or use:

```bash
cargo add package-name
cargo add --dev package-name  # For dev dependencies
```

## Git Workflow

- Use feature branches for new work, e.g. `feature/wayland-capture`, `fix/audio-sync-drift`
- **Commit every change at an atomic level** — each commit should represent a single logical change that compiles and (where applicable) passes tests
- Use **conventional commits** with scope:
  - `feat(capture): add KMS/DRM screen capture`
  - `feat(encode): integrate NVENC H.265 encoding`
  - `fix(audio): correct Opus frame timing`
  - `refactor(protocol): extract RTP packet builder`
  - `chore(deps): update tokio to 1.x`
  - `docs(readme): add build instructions`
  - `test(decode): add VAAPI decoder unit tests`
- Run `cargo fmt && cargo clippy` before commits

## Creating a Plan

When creating a plan in the `plans` directory, make sure to include at least these elements:

**Issues to Address**
What the change is meant to do.

**Important Notes**
Things you come across in your research that are important to the implementation.

**Implementation Strategy**
How you are going to make the changes happen. High level approach.

**Tests**
What unit, integration, and end-to-end tests you plan to write to verify the correct behavior. Don't over-test. Usually, a given change only needs one type of test.

Do NOT include these: *Timeline*, *Rollback plan*. This is a minimal list - feel free to include more.

Do NOT write code as part of your plan. Keep it high level. You can reference certain files or functions though.

Before writing your plan, make sure to do research. Explore the relevant sections in the codebase.

## Active Technologies

- Rust nightly (edition 2024), tokio, serde
