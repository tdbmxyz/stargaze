# Video Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Capture the server's screen via xdg-desktop-portal + PipeWire, delivering frames as DMA-BUF fds (preferred) or CPU-mapped buffers (fallback) over a tokio mpsc channel.

**Architecture:** Async portal setup (ashpd) obtains a PipeWire fd and node_id, then a dedicated std::thread runs the PipeWire main loop. Frames are sent to the tokio world via a bounded mpsc channel (capacity 2). A `CaptureSession` handle owns the thread and signals shutdown on drop.

**Tech Stack:** Rust 2024 nightly, ashpd 0.13 (xdg-desktop-portal), pipewire 0.9 (PipeWire bindings), tokio (async runtime, mpsc channel), thiserror (errors)

**Spec:** `docs/specs/2026-03-31-video-capture-design.md`

---

## File Structure

### New files to create

- `crates/stargaze-core/src/capture.rs` — shared types: `PixelFormat`, `DmaBufInfo`, `Frame`, `CaptureError`
- `crates/stargaze-server/src/capture/mod.rs` — public API: `CaptureConfig`, `CaptureSession`, `start_capture()`
- `crates/stargaze-server/src/capture/portal.rs` — xdg-desktop-portal screencast session setup
- `crates/stargaze-server/src/capture/pipewire.rs` — PipeWire stream, buffer handling, frame delivery

### Files to modify

- `.devcontainer/Dockerfile` — add `libpipewire-0.3-dev`, `libclang-dev`, `libdbus-1-dev`
- `crates/stargaze-core/Cargo.toml` — no dependency changes (uses only std + existing thiserror)
- `crates/stargaze-core/src/lib.rs` — add `pub mod capture;`
- `crates/stargaze-server/Cargo.toml` — add `ashpd`, `pipewire` dependencies
- `crates/stargaze-server/src/main.rs` — integrate capture into the server startup

---

## Task 1: Install system dependencies and add crate dependencies

**Files:**
- Modify: `.devcontainer/Dockerfile`
- Modify: `crates/stargaze-server/Cargo.toml`

- [ ] **Step 1: Install system libraries needed by pipewire-rs and ashpd**

Run:

```bash
sudo apt-get update && sudo apt-get install -y libpipewire-0.3-dev libclang-dev libdbus-1-dev pkg-config
```

- [ ] **Step 2: Update the Dockerfile so future container builds include these**

In `.devcontainer/Dockerfile`, add to the `apt-get install` list in the base stage (after `ffmpeg`):

```dockerfile
# Base image: Debian Trixie with Rust 1.94
FROM rust:1.94-slim-trixie as base

# Install system dependencies and tools
RUN apt-get update && apt-get install -y \
    git \
    curl \
    wget \
    sudo \
    ssh \
    rsync \
    gpg \
    lsof \
    net-tools \
    tree \
    zip \
    unzip \
    tar \
    gzip \
    bzip2 \
    xz-utils \
    build-essential \
    ca-certificates \
    jq \
    sox \
    ffmpeg \
    pkg-config \
    libpipewire-0.3-dev \
    libclang-dev \
    libdbus-1-dev
```

- [ ] **Step 3: Add ashpd and pipewire to stargaze-server dependencies**

Update `crates/stargaze-server/Cargo.toml`:

```toml
[package]
name = "stargaze-server"
version.workspace = true
edition.workspace = true

[dependencies]
stargaze-core = { path = "../stargaze-core" }
anyhow = "1"
ashpd = { version = "0.13", features = ["pipewire"] }
clap = { version = "4", features = ["derive"] }
pipewire = "0.9"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 4: Verify the project compiles with new dependencies**

Run:

```bash
cargo check --package stargaze-server
```

Expected: compiles successfully (no new code yet, just dependency resolution).

- [ ] **Step 5: Commit**

```bash
git add .devcontainer/Dockerfile crates/stargaze-server/Cargo.toml Cargo.lock
git commit --no-gpg-sign -m "chore(deps): add pipewire and ashpd dependencies for video capture"
```

---

## Task 2: Add shared capture types to stargaze-core

**Files:**
- Create: `crates/stargaze-core/src/capture.rs`
- Modify: `crates/stargaze-core/src/lib.rs`

- [ ] **Step 1: Write tests for CaptureError display messages**

Create `crates/stargaze-core/src/capture.rs` with the test module first:

```rust
use std::fmt;
use std::os::unix::io::OwnedFd;

use thiserror::Error;

/// Pixel format of a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// BGRA 8-bit per channel (DRM_FORMAT_XRGB8888 read as BGRA).
    Bgra8,
    /// RGBA 8-bit per channel (DRM_FORMAT_XBGR8888 read as RGBA).
    Rgba8,
    /// NV12 semi-planar YUV (possible direct from some sources).
    Nv12,
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bgra8 => write!(f, "BGRA8"),
            Self::Rgba8 => write!(f, "RGBA8"),
            Self::Nv12 => write!(f, "NV12"),
        }
    }
}

/// Metadata for a DMA-BUF frame (zero-copy GPU buffer).
///
/// The `fd` is a duped file descriptor owned by this struct.
/// It will be closed when this struct is dropped.
pub struct DmaBufInfo {
    /// DMA-BUF file descriptor (duped, caller-owned).
    pub fd: OwnedFd,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Pixel format.
    pub format: PixelFormat,
    /// DRM format modifier (tiling, compression).
    pub modifier: u64,
    /// Bytes per row.
    pub stride: u32,
    /// Offset into the buffer in bytes.
    pub offset: u32,
}

impl fmt::Debug for DmaBufInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DmaBufInfo")
            .field("fd", &self.fd)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("modifier", &format_args!("0x{:x}", self.modifier))
            .field("stride", &self.stride)
            .field("offset", &self.offset)
            .finish()
    }
}

/// A captured video frame — either zero-copy GPU buffer or CPU-mapped data.
#[derive(Debug)]
pub enum Frame {
    /// Zero-copy DMA-BUF frame. The fd is owned and closed on drop.
    DmaBuf(DmaBufInfo),
    /// CPU-mapped frame with owned pixel data.
    CpuMapped {
        /// Owned pixel data (copied from the PipeWire buffer).
        data: Vec<u8>,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
        /// Bytes per row.
        stride: u32,
        /// Pixel format.
        format: PixelFormat,
    },
}

/// Errors from the video capture subsystem.
#[derive(Error, Debug)]
pub enum CaptureError {
    /// The xdg-desktop-portal session failed.
    #[error("Portal session failed: {0}")]
    PortalError(String),

    /// A PipeWire connection or stream error occurred.
    #[error("PipeWire error: {0}")]
    PipeWireError(String),

    /// Could not negotiate a supported buffer format.
    #[error("Buffer format negotiation failed: {0}")]
    NegotiationError(String),

    /// The capture stream ended unexpectedly.
    #[error("Capture stream ended unexpectedly")]
    StreamEnded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_error_display_portal() {
        let err = CaptureError::PortalError("dbus connection refused".to_string());
        assert_eq!(
            err.to_string(),
            "Portal session failed: dbus connection refused"
        );
    }

    #[test]
    fn test_capture_error_display_pipewire() {
        let err = CaptureError::PipeWireError("failed to connect".to_string());
        assert_eq!(err.to_string(), "PipeWire error: failed to connect");
    }

    #[test]
    fn test_capture_error_display_negotiation() {
        let err = CaptureError::NegotiationError("no supported format".to_string());
        assert_eq!(
            err.to_string(),
            "Buffer format negotiation failed: no supported format"
        );
    }

    #[test]
    fn test_capture_error_display_stream_ended() {
        let err = CaptureError::StreamEnded;
        assert_eq!(err.to_string(), "Capture stream ended unexpectedly");
    }

    #[test]
    fn test_pixel_format_display() {
        assert_eq!(PixelFormat::Bgra8.to_string(), "BGRA8");
        assert_eq!(PixelFormat::Rgba8.to_string(), "RGBA8");
        assert_eq!(PixelFormat::Nv12.to_string(), "NV12");
    }
}
```

- [ ] **Step 2: Add the capture module to lib.rs**

Update `crates/stargaze-core/src/lib.rs`:

```rust
pub mod capture;
pub mod config;
pub mod error;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run:

```bash
cargo test --package stargaze-core
```

Expected: all existing tests plus 5 new capture tests pass.

- [ ] **Step 4: Run clippy**

Run:

```bash
cargo clippy --package stargaze-core -- -W clippy::pedantic
```

Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-core/src/capture.rs crates/stargaze-core/src/lib.rs
git commit --no-gpg-sign -m "feat(core): add shared capture types — Frame, DmaBufInfo, PixelFormat, CaptureError"
```

---

## Task 3: Add frame construction tests using memfd

This task adds tests that verify `Frame` and `DmaBufInfo` can be constructed with real file descriptors and pattern-matched correctly. Uses `memfd_create` to get a synthetic fd without needing a real DMA-BUF.

**Files:**
- Modify: `crates/stargaze-core/src/capture.rs` (add tests)

- [ ] **Step 1: Add frame construction tests to the existing test module**

Append these tests to the `#[cfg(test)] mod tests` block in `crates/stargaze-core/src/capture.rs`:

```rust
    #[test]
    fn test_frame_cpu_mapped_construction() {
        let data = vec![0u8; 1920 * 1080 * 4];
        let frame = Frame::CpuMapped {
            data,
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            format: PixelFormat::Bgra8,
        };

        match &frame {
            Frame::CpuMapped {
                width,
                height,
                stride,
                format,
                data,
            } => {
                assert_eq!(*width, 1920);
                assert_eq!(*height, 1080);
                assert_eq!(*stride, 1920 * 4);
                assert_eq!(*format, PixelFormat::Bgra8);
                assert_eq!(data.len(), 1920 * 1080 * 4);
            }
            Frame::DmaBuf(_) => panic!("expected CpuMapped variant"),
        }
    }

    #[test]
    fn test_frame_dmabuf_construction_with_memfd() {
        use std::os::unix::io::AsRawFd;

        // Create a synthetic fd using memfd_create (no real DMA-BUF needed)
        let name = std::ffi::CString::new("test-dmabuf").unwrap();
        let raw_fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(raw_fd >= 0, "memfd_create failed");

        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // Verify the fd is valid before wrapping
        let raw = fd.as_raw_fd();
        assert!(raw >= 0);

        let frame = Frame::DmaBuf(DmaBufInfo {
            fd,
            width: 1920,
            height: 1080,
            format: PixelFormat::Bgra8,
            modifier: 0,
            stride: 1920 * 4,
            offset: 0,
        });

        match &frame {
            Frame::DmaBuf(info) => {
                assert_eq!(info.width, 1920);
                assert_eq!(info.height, 1080);
                assert_eq!(info.format, PixelFormat::Bgra8);
                assert_eq!(info.modifier, 0);
                assert_eq!(info.stride, 1920 * 4);
                assert_eq!(info.offset, 0);
            }
            Frame::CpuMapped { .. } => panic!("expected DmaBuf variant"),
        }
        // fd is closed when frame is dropped
    }
```

Also add the necessary imports at the top of the test module:

```rust
    use std::os::unix::io::FromRawFd;
```

- [ ] **Step 2: Add libc as a dev-dependency to stargaze-core**

Update `crates/stargaze-core/Cargo.toml`:

```toml
[package]
name = "stargaze-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
toml = "0.8"
thiserror = "2"
tracing = "0.1"
directories = "6"

[dev-dependencies]
libc = "0.2"
```

- [ ] **Step 3: Run tests to verify**

Run:

```bash
cargo test --package stargaze-core
```

Expected: all 23 tests pass (16 existing + 5 from Task 2 + 2 new).

- [ ] **Step 4: Commit**

```bash
git add crates/stargaze-core/src/capture.rs crates/stargaze-core/Cargo.toml Cargo.lock
git commit --no-gpg-sign -m "test(core): add frame construction tests with memfd synthetic fds"
```

---

## Task 4: Implement portal session setup

**Files:**
- Create: `crates/stargaze-server/src/capture/mod.rs`
- Create: `crates/stargaze-server/src/capture/portal.rs`

- [ ] **Step 1: Create the capture module directory and mod.rs**

Create `crates/stargaze-server/src/capture/mod.rs`:

```rust
pub mod pipewire;
pub mod portal;

use std::os::unix::io::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use stargaze_core::capture::{CaptureError, Frame};
use tokio::sync::mpsc;
use tracing::{error, info};

/// Channel capacity for frame delivery (provides backpressure).
const FRAME_CHANNEL_CAPACITY: usize = 2;

/// Configuration for the capture session.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Desired capture width in pixels.
    pub width: u32,
    /// Desired capture height in pixels.
    pub height: u32,
    /// Desired capture framerate.
    pub framerate: u32,
}

/// Handle to a running capture session.
///
/// Signals the PipeWire thread to shut down on drop.
/// The caller must keep this alive for the duration of capture.
pub struct CaptureSession {
    /// Join handle for the dedicated PipeWire thread.
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Shared flag to signal the PipeWire thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl CaptureSession {
    /// Gracefully stops the capture session and waits for the PipeWire thread to exit.
    ///
    /// # Errors
    ///
    /// Returns `CaptureError::PipeWireError` if the PipeWire thread panicked.
    pub fn stop(mut self) -> Result<(), CaptureError> {
        self.signal_shutdown();
        self.join_thread()
    }

    /// Signals the PipeWire thread to shut down.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Joins the PipeWire thread, returning any error.
    fn join_thread(&mut self) -> Result<(), CaptureError> {
        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| {
                CaptureError::PipeWireError("PipeWire thread panicked".to_string())
            })?;
        }
        Ok(())
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        // Best-effort join — don't propagate errors from drop.
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts screen capture via xdg-desktop-portal and PipeWire.
///
/// Performs the portal handshake asynchronously (D-Bus), then spawns a
/// dedicated thread for the PipeWire main loop. Returns a session handle
/// and a channel receiver that yields captured frames.
///
/// # Errors
///
/// Returns `CaptureError::PortalError` if the portal session fails.
/// Returns `CaptureError::PipeWireError` if the PipeWire connection fails.
pub async fn start_capture(
    config: CaptureConfig,
) -> Result<(CaptureSession, mpsc::Receiver<Frame>), CaptureError> {
    // Step 1: Portal handshake (async, runs on tokio).
    let (pw_fd, pw_node_id) = portal::create_screencast_session().await?;

    info!(
        node_id = pw_node_id,
        "Portal session established, starting PipeWire capture"
    );

    // Step 2: Create the frame channel.
    let (tx, rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Step 3: Spawn the dedicated PipeWire thread.
    let thread_handle = thread::Builder::new()
        .name("stargaze-pipewire".to_string())
        .spawn(move || {
            if let Err(e) =
                pipewire::run_capture_stream(pw_fd, pw_node_id, config, tx, shutdown_clone)
            {
                error!("PipeWire capture stream failed: {e}");
            }
        })
        .map_err(|e| CaptureError::PipeWireError(format!("failed to spawn thread: {e}")))?;

    Ok((
        CaptureSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        rx,
    ))
}
```

- [ ] **Step 2: Create the portal session module**

Create `crates/stargaze-server/src/capture/portal.rs`:

```rust
use std::os::unix::io::OwnedFd;

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use stargaze_core::capture::CaptureError;
use tracing::debug;

/// Creates a portal screencast session and returns the PipeWire fd and node id.
///
/// This function:
/// 1. Opens a screencast portal session via D-Bus
/// 2. Requests a monitor source (no cursor overlay)
/// 3. Starts the session (may trigger a user confirmation dialog)
/// 4. Opens the PipeWire remote and returns the fd + node id
///
/// # Errors
///
/// Returns `CaptureError::PortalError` if any portal interaction fails
/// (D-Bus unavailable, user denied access, no monitors found).
pub async fn create_screencast_session() -> Result<(OwnedFd, u32), CaptureError> {
    let screencast = Screencast::new()
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to create screencast proxy: {e}")))?;

    debug!("Creating portal screencast session");
    let session = screencast
        .create_session()
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to create session: {e}")))?;

    debug!("Selecting sources (monitor, no cursor)");
    screencast
        .select_sources(
            &session,
            CursorMode::Hidden,
            SourceType::Monitor,
            false, // multiple: only one monitor
            None,  // restore_token
            ashpd::desktop::screencast::PersistMode::DoNot,
        )
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to select sources: {e}")))?;

    debug!("Starting portal session");
    let response = screencast
        .start(&session, None)
        .await
        .map_err(|e| CaptureError::PortalError(format!("failed to start session: {e}")))?;

    let stream = response
        .streams()
        .first()
        .ok_or_else(|| CaptureError::PortalError("no streams returned by portal".to_string()))?;

    let node_id = stream.pipe_wire_node_id();
    debug!(node_id, "Got PipeWire node from portal");

    let fd = screencast
        .open_pipe_wire_remote(&session)
        .await
        .map_err(|e| {
            CaptureError::PortalError(format!("failed to open PipeWire remote: {e}"))
        })?;

    Ok((fd, node_id))
}
```

- [ ] **Step 3: Add the capture module to main.rs**

Add `mod capture;` to `crates/stargaze-server/src/main.rs` (after the existing imports, before the `Cli` struct):

```rust
use clap::Parser;
use stargaze_core::config::{self, Codec, Resolution, ServerConfig};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod capture;
```

- [ ] **Step 4: Verify it compiles**

Run:

```bash
cargo check --package stargaze-server
```

Expected: compiles. There may be warnings about unused `pipewire` module — that's fine, we create it in the next task. If the compiler errors on the missing `pipewire` module, create a placeholder:

Create `crates/stargaze-server/src/capture/pipewire.rs` as a temporary stub:

```rust
use std::os::unix::io::OwnedFd;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use stargaze_core::capture::{CaptureError, Frame};
use tokio::sync::mpsc;

use super::CaptureConfig;

/// Runs the PipeWire capture stream on the current thread (blocking).
///
/// This function blocks until the shutdown signal is set or an error occurs.
pub fn run_capture_stream(
    _pw_fd: OwnedFd,
    _pw_node_id: u32,
    _config: CaptureConfig,
    _tx: mpsc::Sender<Frame>,
    _shutdown: Arc<AtomicBool>,
) -> Result<(), CaptureError> {
    todo!("PipeWire capture stream — implemented in Task 5")
}
```

- [ ] **Step 5: Run clippy**

Run:

```bash
cargo clippy --package stargaze-server -- -W clippy::pedantic
```

Expected: no errors. Fix any warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-server/src/capture/
git commit --no-gpg-sign -m "feat(capture): add portal session setup and capture module skeleton"
```

---

## Task 5: Implement PipeWire capture stream

This is the core capture logic — PipeWire stream creation, format negotiation, and frame delivery.

**Files:**
- Modify: `crates/stargaze-server/src/capture/pipewire.rs` (replace stub)

- [ ] **Step 1: Implement the PipeWire capture stream**

Replace `crates/stargaze-server/src/capture/pipewire.rs` with:

```rust
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pipewire::context::Context;
use pipewire::main_loop::MainLoop;
use pipewire::properties::properties;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::VideoFormat;
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::Object;
use pipewire::spa::sys::{
    SPA_DATA_DmaBuf, SPA_DATA_MemPtr, SPA_PARAM_BUFFERS, SPA_PARAM_META,
};
use pipewire::spa::utils::{Direction, SpaTypes};
use pipewire::stream::{Stream, StreamFlags, StreamState};
use stargaze_core::capture::{CaptureError, DmaBufInfo, Frame, PixelFormat};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::CaptureConfig;

/// Maps a PipeWire/SPA video format to our `PixelFormat`.
fn spa_format_to_pixel_format(format: VideoFormat) -> Option<PixelFormat> {
    match format {
        VideoFormat::BGRA | VideoFormat::BGRx => Some(PixelFormat::Bgra8),
        VideoFormat::RGBA | VideoFormat::RGBx => Some(PixelFormat::Rgba8),
        VideoFormat::NV12 => Some(PixelFormat::Nv12),
        _ => None,
    }
}

/// Runs the PipeWire capture stream on the current thread (blocking).
///
/// Creates a PipeWire main loop, connects to the PipeWire fd, creates a
/// video stream targeting the given node, and runs the main loop until
/// shutdown is signaled or an error occurs.
///
/// Frames are sent on `tx`. When the receiver is dropped or shutdown is
/// signaled, the main loop exits and this function returns.
///
/// # Errors
///
/// Returns `CaptureError::PipeWireError` if connection or stream setup fails.
/// Returns `CaptureError::NegotiationError` if no compatible format is found.
pub fn run_capture_stream(
    pw_fd: OwnedFd,
    pw_node_id: u32,
    config: CaptureConfig,
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), CaptureError> {
    // Initialize PipeWire (safe to call multiple times).
    pipewire::init();

    let mainloop = MainLoop::new(None)
        .map_err(|e| CaptureError::PipeWireError(format!("failed to create main loop: {e}")))?;

    let context = Context::new(&mainloop)
        .map_err(|e| CaptureError::PipeWireError(format!("failed to create context: {e}")))?;

    let core = context
        .connect_fd(pw_fd, None)
        .map_err(|e| CaptureError::PipeWireError(format!("failed to connect to fd: {e}")))?;

    let stream = Stream::new(
        &core,
        "stargaze-capture",
        properties! {
            "media.type" => "Video",
            "media.category" => "Capture",
            "media.role" => "Screen",
        },
    )
    .map_err(|e| CaptureError::PipeWireError(format!("failed to create stream: {e}")))?;

    // We need to share mutable state with callbacks. Use a Cell/RefCell pattern
    // since PipeWire callbacks run on the same thread as the main loop.
    let negotiated_format: std::cell::Cell<Option<(VideoFormat, u32, u32)>> =
        std::cell::Cell::new(None);

    let mainloop_weak = mainloop.downgrade();
    let shutdown_ref = Arc::clone(&shutdown);

    // Register stream event callbacks.
    let _listener = stream
        .add_local_listener()
        .state_changed(move |_stream, old, new| {
            debug!("PipeWire stream state: {old:?} → {new:?}");
            match new {
                StreamState::Error(msg) => {
                    error!("PipeWire stream error: {msg}");
                    if let Some(ml) = mainloop_weak.upgrade() {
                        ml.quit();
                    }
                }
                StreamState::Paused | StreamState::Streaming => {}
                _ => {}
            }
        })
        .param_changed(move |_stream, id, _user_data, pod| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            if let Some(pod) = pod {
                // Parse the negotiated format to learn width, height, format.
                // This is called when PipeWire and the source agree on parameters.
                if let Some((media_type, media_subtype)) = parse_format(pod) {
                    debug!("Negotiated format: type={media_type:?} subtype={media_subtype:?}");
                }
                // For now, store what we can parse. The actual format details
                // are extracted from the pod properties.
                // Full pod parsing is complex — we'll extract what we need
                // from the buffer metadata in the process callback instead.
            }
        })
        .process(move |stream, _user_data| {
            // Check shutdown flag.
            if shutdown_ref.load(Ordering::Relaxed) {
                return;
            }

            let mut buffer = match stream.dequeue_buffer() {
                Some(buf) => buf,
                None => {
                    warn!("No buffer available in process callback");
                    return;
                }
            };

            let datas = buffer.datas_mut();
            if datas.is_empty() {
                warn!("Buffer has no data planes");
                return;
            }

            let data = &datas[0];
            let chunk = data.chunk();
            let stride = chunk.stride() as u32;
            let size = chunk.size() as usize;
            let offset = chunk.offset() as u32;

            // Determine frame dimensions from chunk and stride.
            // Height = size / stride (for packed formats).
            // Width = stride / bytes_per_pixel.
            // Default to config dimensions if we can't compute.
            let height = if stride > 0 {
                (size as u32) / stride
            } else {
                config.height
            };
            // Assume 4 bytes per pixel for BGRA/RGBA.
            let width = if stride >= 4 { stride / 4 } else { config.width };

            let data_type = data.type_();

            if data_type == SPA_DATA_DmaBuf {
                // DMA-BUF path: dup the fd.
                let raw_fd = data.as_raw();
                if raw_fd < 0 {
                    warn!("DMA-BUF buffer has invalid fd");
                    return;
                }

                // Safety: we dup the fd so we own an independent copy.
                let duped_fd = unsafe { libc::dup(raw_fd) };
                if duped_fd < 0 {
                    warn!("Failed to dup DMA-BUF fd");
                    return;
                }
                let owned_fd = unsafe { OwnedFd::from_raw_fd(duped_fd) };

                let frame = Frame::DmaBuf(DmaBufInfo {
                    fd: owned_fd,
                    width,
                    height,
                    format: PixelFormat::Bgra8, // Most common from compositors.
                    modifier: 0,                // TODO: extract from buffer metadata.
                    stride,
                    offset,
                });

                if tx.blocking_send(frame).is_err() {
                    // Receiver dropped — shutting down.
                    info!("Frame receiver dropped, stopping capture");
                    return;
                }
            } else if data_type == SPA_DATA_MemPtr {
                // CPU-mapped path: copy the data.
                let ptr = data.data();
                if let Some(slice) = ptr {
                    let bytes = if size <= slice.len() {
                        slice[..size].to_vec()
                    } else {
                        slice.to_vec()
                    };

                    let frame = Frame::CpuMapped {
                        data: bytes,
                        width,
                        height,
                        stride,
                        format: PixelFormat::Bgra8,
                    };

                    if tx.blocking_send(frame).is_err() {
                        info!("Frame receiver dropped, stopping capture");
                        return;
                    }
                } else {
                    warn!("MemPtr buffer has null data pointer");
                }
            } else {
                warn!("Unknown buffer data type: {data_type}");
            }
        })
        .register()
        .map_err(|e| CaptureError::PipeWireError(format!("failed to register listener: {e}")))?;

    // Build format parameters: prefer DMA-BUF, fallback to MemPtr.
    // We request BGRA (most common from compositors) at the configured
    // resolution and framerate.
    let params = build_stream_params(&config);

    stream
        .connect(
            Direction::Input,
            Some(pw_node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params.iter().map(|p| p.as_slice()).collect::<Vec<_>>(),
        )
        .map_err(|e| CaptureError::PipeWireError(format!("failed to connect stream: {e}")))?;

    info!("PipeWire stream connected, entering main loop");

    // Run the main loop (blocks until quit is called).
    mainloop.run();

    info!("PipeWire main loop exited");
    Ok(())
}

/// Builds SPA format parameter pods for stream negotiation.
///
/// Returns serialized pod bytes for the stream's `connect()` call.
fn build_stream_params(config: &CaptureConfig) -> Vec<Vec<u8>> {
    // This is a simplified parameter builder. PipeWire's SPA pod system
    // is complex — we build a minimal format request.
    //
    // The actual negotiation happens between PipeWire and the source.
    // We specify our preferred formats and let PipeWire choose.
    //
    // For a production implementation, this would use the spa pod builder
    // API to construct proper format objects. For now, we rely on
    // PipeWire's auto-negotiation with AUTOCONNECT flag.
    vec![]
}
```

**Important note for the implementing agent:** The PipeWire SPA pod builder API in `pipewire-rs` is complex and version-dependent. The exact API for building format parameters may need adjustment based on the `pipewire` 0.9 crate's actual types. The key pattern is:

1. Use `pipewire::spa::pod::serialize::PodSerializer` to build format objects
2. Specify `VideoFormat::BGRA` (and alternatives) as the pixel format
3. Specify the desired width, height, and framerate as ranges
4. The `AUTOCONNECT` flag lets PipeWire handle negotiation even with minimal params

The implementing agent should check `pipewire` 0.9's docs/examples for the exact pod builder API and adjust `build_stream_params` accordingly. The `data/examples/Sunshine/` codebase and the `ashpd` screencast example (`screen_cast_pw.rs`) are references for the SPA pod structure.

- [ ] **Step 2: Add libc dependency to stargaze-server**

Update `crates/stargaze-server/Cargo.toml` to add libc (needed for `dup()` and `memfd_create`):

```toml
[package]
name = "stargaze-server"
version.workspace = true
edition.workspace = true

[dependencies]
stargaze-core = { path = "../stargaze-core" }
anyhow = "1"
ashpd = { version = "0.13", features = ["pipewire"] }
clap = { version = "4", features = ["derive"] }
libc = "0.2"
pipewire = "0.9"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 3: Verify it compiles**

Run:

```bash
cargo check --package stargaze-server
```

Expected: compiles. There will likely be warnings about unused variables/imports in the param_changed callback and `build_stream_params` — these are expected and will resolve once the pod builder is properly implemented.

The implementing agent should fix any actual compilation errors by consulting the `pipewire` 0.9 crate API. Common issues:
- `Stream::add_local_listener()` callback signatures may differ from what's shown
- `data.type_()` might be named differently (check `pipewire::spa::buffer::Data` API)
- `data.as_raw()` for getting the DMA-BUF fd — check the actual method name
- `parse_format` might not exist — may need manual pod parsing

Fix compilation errors while preserving the overall structure and logic.

- [ ] **Step 4: Run clippy**

Run:

```bash
cargo clippy --package stargaze-server -- -W clippy::pedantic
```

Expected: no errors. Fix any warnings (allow specific ones with `#[allow(...)]` if they're in callback signatures imposed by PipeWire).

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-server/src/capture/pipewire.rs crates/stargaze-server/Cargo.toml Cargo.lock
git commit --no-gpg-sign -m "feat(capture): implement PipeWire capture stream with DMA-BUF and CPU fallback"
```

---

## Task 6: Integrate capture into server main.rs

**Files:**
- Modify: `crates/stargaze-server/src/main.rs`

- [ ] **Step 1: Update main.rs to start capture and log frames**

Replace `crates/stargaze-server/src/main.rs` with:

```rust
use clap::Parser;
use stargaze_core::capture::Frame;
use stargaze_core::config::{self, Codec, Resolution, ServerConfig};
use tracing::info;
use tracing_subscriber::EnvFilter;

mod capture;

use capture::CaptureConfig;

/// Stargaze streaming server — captures screen and audio, encodes, and streams to clients.
#[derive(Parser, Debug)]
#[command(name = "stargaze-server", version, about)]
struct Cli {
    /// Address to bind the server to.
    #[arg(long)]
    bind: Option<String>,

    /// Port to listen on.
    #[arg(long)]
    port: Option<u16>,

    /// Video resolution as `WIDTHxHEIGHT` (e.g. 1920x1080).
    #[arg(long)]
    resolution: Option<Resolution>,

    /// Target framerate.
    #[arg(long)]
    framerate: Option<u32>,

    /// Target bitrate in Mbps.
    #[arg(long)]
    bitrate: Option<u32>,

    /// Video codec (h265, av1).
    #[arg(long)]
    codec: Option<Codec>,

    /// Path to config file (default: ~/.config/stargaze/server.toml).
    #[arg(long)]
    config: Option<String>,
}

/// Initializes the tracing subscriber with an env filter.
///
/// Uses the `RUST_LOG` environment variable if set, otherwise defaults to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Builds the final [`ServerConfig`] by loading from file and applying CLI overrides.
///
/// Config resolution order:
/// 1. If `--config` is provided, load from that path.
/// 2. Otherwise, if the default config file exists, load from it.
/// 3. If no file is found, use [`ServerConfig::default()`].
/// 4. Any CLI arguments that are `Some` override the loaded config values.
///
/// # Errors
///
/// Returns an error if the config file exists but cannot be read or parsed.
fn build_config(cli: &Cli) -> anyhow::Result<ServerConfig> {
    let config_path: Option<String> = if let Some(ref path) = cli.config {
        Some(path.clone())
    } else {
        let default_path = config::config_file_path("server");
        if default_path.exists() {
            default_path.to_str().map(String::from)
        } else {
            None
        }
    };

    let mut cfg: ServerConfig = config::load_config(config_path.as_deref())?;

    if let Some(ref bind) = cli.bind {
        cfg.bind_address.clone_from(bind);
    }
    if let Some(port) = cli.port {
        cfg.port = port;
    }
    if let Some(resolution) = cli.resolution {
        cfg.resolution = resolution;
    }
    if let Some(framerate) = cli.framerate {
        cfg.framerate = framerate;
    }
    if let Some(bitrate) = cli.bitrate {
        cfg.bitrate = bitrate;
    }
    if let Some(codec) = cli.codec {
        cfg.codec = codec;
    }

    Ok(cfg)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    let cfg = build_config(&cli)?;

    info!(
        "Starting stargaze server on {}:{} ({}@{}fps, {} Mbps, {})",
        cfg.bind_address, cfg.port, cfg.resolution, cfg.framerate, cfg.bitrate, cfg.codec
    );

    let capture_config = CaptureConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
    };

    let (session, mut frames) = capture::start_capture(capture_config).await?;

    info!("Capture started, receiving frames...");

    let mut frame_count: u64 = 0;
    while let Some(frame) = frames.recv().await {
        frame_count += 1;
        match &frame {
            Frame::DmaBuf(info) => {
                if frame_count % 60 == 1 {
                    info!(
                        frame = frame_count,
                        width = info.width,
                        height = info.height,
                        format = %info.format,
                        "DMA-BUF frame"
                    );
                }
            }
            Frame::CpuMapped {
                width,
                height,
                format,
                ..
            } => {
                if frame_count % 60 == 1 {
                    info!(
                        frame = frame_count,
                        width,
                        height,
                        format = %format,
                        "CPU-mapped frame"
                    );
                }
            }
        }
    }

    info!(total_frames = frame_count, "Capture stream ended");
    session.stop()?;

    Ok(())
}
```

- [ ] **Step 2: Verify it compiles**

Run:

```bash
cargo check --package stargaze-server
```

Expected: compiles successfully.

- [ ] **Step 3: Run clippy on the full workspace**

Run:

```bash
cargo clippy --workspace -- -W clippy::pedantic
```

Expected: no errors across all crates.

- [ ] **Step 4: Run cargo fmt**

Run:

```bash
cargo fmt --all
```

- [ ] **Step 5: Run all tests**

Run:

```bash
cargo test --workspace
```

Expected: all tests pass (existing config/error tests + new capture type tests).

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-server/src/main.rs
git commit --no-gpg-sign -m "feat(server): integrate capture into server startup with frame logging"
```

---

## Task 7: Add integration test (ignored) for manual capture verification

**Files:**
- Create: `crates/stargaze-server/examples/capture_test.rs`

- [ ] **Step 1: Create the capture test example**

Create `crates/stargaze-server/examples/capture_test.rs`:

```rust
//! Manual capture verification tool.
//!
//! Runs the full capture pipeline (portal + PipeWire), receives a few frames,
//! and writes the first CPU-mapped frame to a PPM file for visual inspection.
//!
//! Requires a running Wayland compositor and PipeWire.
//!
//! Usage:
//!     cargo run --package stargaze-server --example capture_test

use stargaze_core::capture::Frame;
use tracing::info;
use tracing_subscriber::EnvFilter;

// The capture module is internal to the server binary, so we can't import it
// from an example. Instead, we duplicate the minimal portal + pipewire setup
// here, OR we restructure to make capture a library.
//
// For now, this example uses ashpd and pipewire directly to test the
// end-to-end flow without depending on the server's internal capture module.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    info!("Starting capture test — this will request screen sharing permission");
    info!("Press Ctrl+C to stop");

    // Use the server's capture module by depending on it as a library.
    // Since the capture module is part of the binary crate, we need to
    // restructure slightly. For the MVP, we test by running the server
    // binary directly and inspecting the log output.
    //
    // A full integration test would be an #[ignore] test in tests/ that
    // imports the capture module. This is deferred to when we restructure
    // the server to expose capture as a library.

    info!("To test capture, run: cargo run --package stargaze-server");
    info!("The server will log frame arrivals. Ctrl+C to stop.");

    Ok(())
}
```

**Note:** Since `capture` is a private module inside the server binary, the example can't directly import it. The primary verification path is running the server binary itself. If we want a proper integration test, we'd need to either:
1. Make the capture module a separate library crate, or
2. Use an `#[ignore]` test inside the server binary's test suite

For the MVP, option 2 is simpler. Add an ignored test to `main.rs`:

- [ ] **Step 2: Add an ignored integration test to the server**

Add at the bottom of `crates/stargaze-server/src/main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Integration test: runs capture for 3 seconds and verifies frames arrive.
    ///
    /// Requires a running Wayland compositor + PipeWire.
    /// Run manually with: cargo test --package stargaze-server -- --ignored test_capture_receives_frames
    #[tokio::test]
    #[ignore = "requires running Wayland compositor and PipeWire"]
    async fn test_capture_receives_frames() {
        init_tracing();

        let config = CaptureConfig {
            width: 1920,
            height: 1080,
            framerate: 30,
        };

        let (session, mut frames) = capture::start_capture(config)
            .await
            .expect("capture should start");

        // Receive a few frames (with timeout).
        let mut count = 0u32;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                frame = frames.recv() => {
                    match frame {
                        Some(Frame::DmaBuf(info)) => {
                            assert!(info.width > 0);
                            assert!(info.height > 0);
                            count += 1;
                        }
                        Some(Frame::CpuMapped { width, height, data, .. }) => {
                            assert!(width > 0);
                            assert!(height > 0);
                            assert!(!data.is_empty());

                            // Write first frame to PPM for visual inspection.
                            if count == 0 {
                                write_ppm("/tmp/stargaze_test_frame.ppm", &data, width, height);
                                eprintln!("Wrote test frame to /tmp/stargaze_test_frame.ppm");
                            }
                            count += 1;
                        }
                        None => break,
                    }
                }
                () = &mut timeout => break,
            }
        }

        session.stop().expect("session should stop cleanly");
        assert!(count > 0, "should have received at least one frame");
        eprintln!("Received {count} frames in 3 seconds");
    }

    /// Writes raw BGRA pixel data as a PPM file (converts BGRA → RGB).
    fn write_ppm(path: &str, data: &[u8], width: u32, height: u32) {
        use std::io::Write;

        let mut file = std::fs::File::create(path).expect("create PPM file");
        write!(file, "P6\n{width} {height}\n255\n").expect("write PPM header");

        // Convert BGRA → RGB, writing pixel by pixel.
        for y in 0..height {
            for x in 0..width {
                let offset = ((y * width + x) * 4) as usize;
                if offset + 2 < data.len() {
                    let b = data[offset];
                    let g = data[offset + 1];
                    let r = data[offset + 2];
                    file.write_all(&[r, g, b]).expect("write pixel");
                }
            }
        }
    }
}
```

- [ ] **Step 3: Remove the placeholder example file**

Delete `crates/stargaze-server/examples/capture_test.rs` (the ignored test in main.rs is the better approach).

- [ ] **Step 4: Verify unit tests still pass**

Run:

```bash
cargo test --workspace
```

Expected: all non-ignored tests pass. The ignored test is skipped.

- [ ] **Step 5: Verify the ignored test is listed**

Run:

```bash
cargo test --package stargaze-server -- --list 2>&1 | grep ignored
```

Expected: shows `test_capture_receives_frames` as ignored.

- [ ] **Step 6: Run clippy and fmt**

Run:

```bash
cargo fmt --all && cargo clippy --workspace -- -W clippy::pedantic
```

Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/stargaze-server/src/main.rs
git commit --no-gpg-sign -m "test(capture): add ignored integration test for manual capture verification"
```

---

## Summary of deliverables

After completing all 7 tasks:

1. **System deps** installed and Dockerfile updated for reproducibility
2. **Core types** in `stargaze-core::capture` — `Frame`, `DmaBufInfo`, `PixelFormat`, `CaptureError` with 7 unit tests
3. **Portal module** — async xdg-desktop-portal session setup via ashpd
4. **PipeWire module** — stream with DMA-BUF preference, CPU fallback, frame delivery via mpsc
5. **Server integration** — main.rs starts capture, logs frames at 1-per-second rate
6. **Integration test** — `#[ignore]` test that captures frames for 3s, writes PPM for visual verification

**Verification commands:**

```bash
# All unit tests pass:
cargo test --workspace

# No clippy warnings:
cargo clippy --workspace -- -W clippy::pedantic

# Clean formatting:
cargo fmt --all -- --check

# Manual capture test (on a machine with compositor + PipeWire):
cargo test --package stargaze-server -- --ignored test_capture_receives_frames
```
