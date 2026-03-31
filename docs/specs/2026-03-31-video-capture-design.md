# Video Capture Design — Sub-project 2

**Date:** 2026-03-31
**Status:** Draft
**Sub-project:** 2 of 9 (Video Capture — Server)

## Overview

Capture the server's screen via xdg-desktop-portal and PipeWire, delivering frames to the rest of the pipeline as either zero-copy DMA-BUF file descriptors (preferred) or CPU-mapped byte buffers (fallback). This sub-project covers capture only — encoding is Sub-project 3.

## Approach

**DMA-BUF-first with CPU fallback.** The PipeWire stream negotiates DMA-BUF buffers as the preferred format, falling back to MemPtr (shared memory / CPU-mapped) if the compositor doesn't support DMA-BUF export. Frames are delivered as an enum — either a DMA-BUF fd with metadata or an owned byte buffer.

**Why this approach over alternatives:**

- **vs. CPU-only (MAP_BUFFERS):** CPU-only forces a GPU→CPU→GPU round-trip per frame. At 1080p60 that's ~500 MB/s of unnecessary copies. Not viable for low-latency streaming. Would need to be reworked when encoding arrives.
- **vs. DMA-BUF only (no fallback):** Too brittle — can't test without a compositor that supports DMA-BUF export, and fails hard on compositors that don't. The CPU fallback costs ~50 lines and enables frame dump verification.

## Module Structure

```
crates/stargaze-core/src/
├── lib.rs           # add `pub mod capture;`
├── capture.rs       # shared types: Frame, DmaBufInfo, PixelFormat, CaptureError
├── config.rs        # existing (no changes)
└── error.rs         # existing (no changes)

crates/stargaze-server/src/
├── main.rs          # calls capture::start_capture(), receives frames
└── capture/
    ├── mod.rs       # public API: CaptureSession, CaptureConfig, start_capture()
    ├── portal.rs    # xdg-desktop-portal session setup (ashpd)
    └── pipewire.rs  # PipeWire stream setup, buffer handling, frame delivery
```

**Rationale:** Shared types (`Frame`, `DmaBufInfo`, `PixelFormat`, `CaptureError`) live in `stargaze-core` because the client will eventually need frame metadata for decoding. The capture implementation is server-only. Splitting portal vs pipewire keeps each file focused on one responsibility.

## Core Types

All in `stargaze-core::capture`:

```rust
/// Pixel format of a captured frame.
pub enum PixelFormat {
    Bgra8,      // DRM_FORMAT_XRGB8888 read as BGRA (most common)
    Rgba8,      // DRM_FORMAT_XBGR8888 read as RGBA
    Nv12,       // possible direct NV12 from some sources
}

/// Metadata for a DMA-BUF frame (zero-copy GPU buffer).
pub struct DmaBufInfo {
    pub fd: OwnedFd,       // DMA-BUF file descriptor (duped, caller-owned)
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub modifier: u64,     // DRM format modifier (tiling, compression)
    pub stride: u32,       // bytes per row
    pub offset: u32,       // offset into the buffer
}

/// A captured video frame.
pub enum Frame {
    DmaBuf(DmaBufInfo),
    CpuMapped {
        data: Vec<u8>,     // owned pixel data (copied from PipeWire buffer)
        width: u32,
        height: u32,
        stride: u32,
        format: PixelFormat,
    },
}

/// Errors from the capture subsystem.
#[derive(Error, Debug)]
pub enum CaptureError {
    #[error("Portal session failed: {0}")]
    PortalError(String),
    #[error("PipeWire error: {0}")]
    PipeWireError(String),
    #[error("Buffer format negotiation failed: {0}")]
    NegotiationError(String),
    #[error("Capture stream ended unexpectedly")]
    StreamEnded,
}
```

**Design notes:**

- `DmaBufInfo` holds a single plane. Compositors export screen capture as single-plane RGB, so multi-plane is unnecessary for this sub-project.
- `Frame::CpuMapped` owns its data (`Vec<u8>`) rather than borrowing. Keeps lifetimes simple across the channel boundary. The copy cost is acceptable for a fallback/test path.
- `CaptureError` uses `String` payloads rather than wrapping `ashpd::Error` / `pipewire::Error` directly, avoiding library types in the core crate's public API.

## Data Flow & Threading Model

```
┌─────────────────────────────────────────────────────┐
│                    Server main()                     │
│                   (tokio runtime)                    │
│                                                     │
│   1. Call capture::start_capture(config).await       │
│   2. Receive frames from mpsc::Receiver<Frame>       │
│   3. (Later: pass frames to encoder)                 │
└──────────────────────┬──────────────────────────────┘
                       │ returns (CaptureSession, Receiver<Frame>)
                       │
┌──────────────────────▼──────────────────────────────┐
│              CaptureSession                          │
│                                                     │
│   Owns:                                              │
│   - JoinHandle for the PipeWire thread               │
│   - Shutdown signal (oneshot or atomic flag)          │
│                                                     │
│   Methods:                                           │
│   - stop(self) → signals shutdown, joins thread      │
│                                                     │
│   Drop: signals shutdown if stop() wasn't called     │
└──────────────────────┬──────────────────────────────┘
                       │ spawns
                       ▼
┌─────────────────────────────────────────────────────┐
│           Dedicated PipeWire thread                  │
│           (std::thread, NOT tokio)                   │
│                                                     │
│   Receives fd + node_id from start_capture()         │
│                                                     │
│   1. pipewire::run_capture_stream(fd, node_id, tx)   │
│      - Create PipeWire MainLoop + Context            │
│      - Connect to fd via context.connect_fd()        │
│      - Create Stream, negotiate format params        │
│        (prefer DMA-BUF, fallback MemPtr)             │
│      - process callback:                             │
│        - Dequeue buffer                              │
│        - Wrap as Frame::DmaBuf or Frame::CpuMapped   │
│        - Send on mpsc::Sender<Frame>                 │
│      - MainLoop.run() (blocks this thread)           │
│                                                     │
│   2. On shutdown signal → MainLoop.quit()            │
└─────────────────────────────────────────────────────┘
```

**Why a dedicated std::thread?** PipeWire has its own event loop (`MainLoop`) that calls `loop.run()` and blocks indefinitely. It drives all PipeWire callbacks including frame delivery. It cannot run inside tokio because it would block the executor, and `MainLoop` may be `!Send` on some configurations.

**Channel bridge:** A bounded `tokio::sync::mpsc::channel<Frame>` (capacity 2) bridges the PipeWire thread to the tokio world. The PipeWire `process` callback calls `tx.blocking_send()`. The tokio side calls `rx.recv().await`. The small bound provides backpressure — if the consumer isn't keeping up, the PipeWire thread blocks, naturally throttling capture to match consumption.

**Portal setup threading:** `ashpd` is async (uses `zbus`). The portal setup (create session, select sources, start, open PipeWire remote) runs as part of the async `start_capture()` function on the tokio runtime. Once the PipeWire fd and node_id are obtained, they are moved into the dedicated PipeWire thread which only runs the PipeWire main loop.

## PipeWire Buffer Negotiation

When creating the PipeWire stream, we attach SPA format parameters in priority order:

1. **First:** video/raw with `SPA_DATA_DmaBuf` buffer type (preferred)
2. **Second:** video/raw with `SPA_DATA_MemPtr` buffer type (fallback)

PipeWire and the compositor negotiate and pick the best match.

**Requested format parameters (both paths):**

- Pixel format: `SPA_VIDEO_FORMAT_BGRA` (preferred), with `BGRx`, `RGBA`, `RGBx` as alternatives
- Size: match `CaptureConfig` resolution, or accept compositor's native resolution
- Framerate: match `CaptureConfig::framerate`

**DMA-BUF process callback:**

1. Check `buffer.datas[0].type_ == SPA_DATA_DmaBuf`
2. `dup()` the fd (buffer is returned to PipeWire after callback)
3. Read stride, offset from `buffer.datas[0].chunk`
4. Construct `Frame::DmaBuf(DmaBufInfo { ... })`
5. Send on channel via `tx.blocking_send()`

**MemPtr process callback:**

1. Check `buffer.datas[0].type_ == SPA_DATA_MemPtr`
2. Copy bytes from mapped pointer into `Vec<u8>` (buffer is reclaimed after callback)
3. Construct `Frame::CpuMapped { ... }`
4. Send on channel

**fd ownership:** For DMA-BUF, we `dup()` the file descriptor before the callback returns. The `Frame::DmaBuf` variant owns the duped fd via `OwnedFd`, which closes it on drop. Each frame carries an independent fd the consumer can use at its own pace.

## Public API

```rust
// crates/stargaze-server/src/capture/mod.rs

/// Configuration for the capture session.
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
}

/// Handle to a running capture session. Signals shutdown on drop.
pub struct CaptureSession { /* JoinHandle, shutdown signal */ }

impl CaptureSession {
    /// Gracefully stop capture and wait for the PipeWire thread to exit.
    pub fn stop(self) -> Result<(), CaptureError>;
}

impl Drop for CaptureSession {
    // Signals shutdown if stop() wasn't called
}

/// Start screen capture via xdg-desktop-portal + PipeWire.
///
/// Returns a session handle and a channel receiver yielding frames.
/// The caller must keep CaptureSession alive — dropping it stops capture.
pub async fn start_capture(
    config: CaptureConfig,
) -> Result<(CaptureSession, tokio::sync::mpsc::Receiver<Frame>), CaptureError>;
```

**Server main.rs integration:**

```rust
let capture_config = CaptureConfig {
    width: cfg.resolution.width(),
    height: cfg.resolution.height(),
    framerate: cfg.framerate,
};

let (session, mut frames) = capture::start_capture(capture_config).await?;

while let Some(frame) = frames.recv().await {
    match &frame {
        Frame::DmaBuf(info) => info!("DMA-BUF frame: {}x{}", info.width, info.height),
        Frame::CpuMapped { width, height, .. } => info!("CPU frame: {width}x{height}"),
    }
}

session.stop()?;
```

Ctrl+C breaks the loop, `session` drops, PipeWire thread shuts down cleanly.

## Error Handling

**Errors during setup** (portal, PipeWire connection) are returned from `start_capture()` via `Result`. The caller decides whether to retry or exit.

**Errors during streaming** (format negotiation failure, stream disconnect, compositor crash) happen inside the PipeWire thread after `start_capture()` has returned. These are handled by:

1. Logging the error at `error!` level
2. Dropping the `Sender<Frame>`, which causes `rx.recv().await` to return `None`
3. Exiting the PipeWire thread

The caller interprets a closed channel as "capture stopped" and can decide whether to retry or exit. For the MVP, it exits.

**Backpressure / shutdown:** If `tx.blocking_send()` fails because the receiver was dropped (server shutting down), the PipeWire thread logs and exits. This is normal shutdown, not an error.

## Testing Strategy

**Unit tests (run anywhere, `cargo test`):**

- Frame type construction and pattern matching (using synthetic fd from `memfd_create`)
- `CaptureConfig` construction from config values
- `PixelFormat` mapping helpers (DRM format codes → `PixelFormat` variants)
- `CaptureError` display messages

**Integration test (requires compositor + PipeWire, `#[ignore]`):**

A test or cargo example that runs the full pipeline — portal session, PipeWire stream, receive N frames — and writes the first CPU-mapped frame to a PPM file for visual inspection:

```bash
# Run manually on a machine with a compositor:
cargo test --package stargaze-server -- --ignored test_capture_dumps_frame
# Or as a cargo example:
cargo run --package stargaze-server --example capture_test
```

**Manual smoke test:**

Run the server binary, verify it logs frame arrivals at the expected framerate, Ctrl+C shuts down cleanly.

**What we do NOT test:**

- No mocking of PipeWire or ashpd — the abstractions are thin, mocking adds complexity without value.
- No performance benchmarks — latency measurement comes with the encoder in Sub-project 3.

## Dependencies

**`stargaze-server/Cargo.toml` (new):**

- `ashpd = { version = "0.13", features = ["pipewire"] }` — xdg-desktop-portal bindings
- `pipewire = "0.9"` — PipeWire Rust bindings

**`stargaze-core/Cargo.toml`:** No changes. Frame types use only `std` types plus existing `thiserror`.

**System libraries (build-time):**

- `libpipewire-0.3-dev` — required by `pipewire-rs`
- `libclang-dev` — required by `pipewire-rs` for bindgen
- `libdbus-1-dev` — required by `ashpd`/`zbus`

**Explicitly avoided:**

- No `image` crate — PPM frame dump is trivial (3-line header + raw bytes)
- No `drm`/`gbm` crates — we pass DMA-BUF fds through, not allocate them
- No `nix` crate — `OwnedFd` and `dup()` are in `std::os::unix::io`
