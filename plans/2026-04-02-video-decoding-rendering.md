# Video Decoding + Rendering Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decode H.265 video frames received via QUIC transport and render them to a Wayland window using SDL2, completing the client-side video pipeline.

**Architecture:** A dedicated `std::thread` receives `ReassembledFrame` values from the transport channel, decodes H.265 Annex-B NAL units via FFmpeg's software decoder, and sends `DecodedFrame` values to the SDL2 renderer on the main thread. The renderer creates a Wayland window with a YUV texture and presents decoded frames at vsync rate.

**Tech Stack:** Rust 2024 nightly, `ffmpeg-next` 7 (safe FFmpeg bindings), `ffmpeg-sys-next` 7 (raw FFI for AVPacket access), `sdl2` 0.37, tokio (channels), thiserror (errors)

**Spec:** `docs/specs/2026-04-02-video-decoding-rendering-design.md`

**Build environment note:** Every `cargo` command in this plan MUST be prefixed with:
```bash
export LIBCLANG_PATH="/usr/lib/llvm-19/lib"
```

For brevity, the plan references this as `ENV_SETUP` — paste the block before each `cargo` invocation.

---

## File Structure

### New files to create

- `crates/stargaze-core/src/decode.rs` — shared types: `DecodedFrame`, `DecoderConfig`, `DecodeError`
- `crates/stargaze-client/src/decode/mod.rs` — public API: `DecoderSession`, `start_decoder()`
- `crates/stargaze-client/src/decode/ffmpeg.rs` — FFmpeg H.265 software decode init + decode loop
- `crates/stargaze-client/src/render/mod.rs` — public API: `start_renderer()`
- `crates/stargaze-client/src/render/sdl.rs` — SDL2 window, texture management, present loop
- `crates/stargaze-client/build.rs` — pkg-config link fixup for FFmpeg transitive deps

### Files to modify

- `crates/stargaze-core/src/lib.rs` — add `pub mod decode;`
- `crates/stargaze-client/src/lib.rs` — add `pub mod decode; pub mod render;`
- `crates/stargaze-client/Cargo.toml` — add `ffmpeg-next`, `ffmpeg-sys-next`, `sdl2`, `libc` dependencies
- `crates/stargaze-client/src/main.rs` — wire decoder + renderer into the transport pipeline
- `.devcontainer/Dockerfile` — add `libsdl2-dev` system package

---

## Task 1: Add crate dependencies and system packages

**Files:**
- Modify: `crates/stargaze-client/Cargo.toml`
- Modify: `.devcontainer/Dockerfile`
- Create: `crates/stargaze-client/build.rs`

- [ ] **Step 1: Add SDL2 dev package to Dockerfile**

Add `libsdl2-dev` to the `apt-get install` list in `.devcontainer/Dockerfile`. The install line in the `base` stage should include:

```dockerfile
libsdl2-dev \
```

after the existing FFmpeg dev packages. This is needed for the `sdl2` crate to link against.

For the current running container (without rebuild), download and extract the dev package:

```bash
cd /tmp && \
apt-get download libsdl2-dev 2>/dev/null && \
dpkg-deb -x libsdl2-dev*.deb /tmp/sdl2-extract && \
sudo cp -rn /tmp/sdl2-extract/usr/* /usr/ && \
rm -rf /tmp/sdl2-extract /tmp/libsdl2-dev*.deb
```

Verify:
```bash
pkg-config --modversion sdl2
```

Expected: `2.32.4` or similar version number.

- [ ] **Step 2: Add FFmpeg and SDL2 dependencies to client Cargo.toml**

```bash
ENV_SETUP && cargo add ffmpeg-next@7 ffmpeg-sys-next@7 sdl2 libc --package stargaze-client
```

The `[dependencies]` section should now include:
```toml
ffmpeg-next = "7"
ffmpeg-sys-next = "7"
sdl2 = "0.37"
libc = "0.2"
```

- [ ] **Step 3: Create client build.rs for FFmpeg link fixup**

Create `crates/stargaze-client/build.rs` — identical purpose to the server's `build.rs`. Queries `pkg-config --libs` for FFmpeg transitive dependencies:

```rust
/// Build script for `stargaze-client`.
///
/// Same purpose as stargaze-server's build.rs: re-queries `pkg-config`
/// to obtain transitive linker flags for FFmpeg shared libraries.
fn main() {
    let ld_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();

    let Ok(output) = std::process::Command::new("pkg-config")
        .args(["--libs", "libavcodec", "libavutil", "libswscale"])
        .env(
            "PKG_CONFIG_PATH",
            std::env::var("PKG_CONFIG_PATH").unwrap_or_default(),
        )
        .env("LD_LIBRARY_PATH", &ld_path)
        .output()
    else {
        return;
    };

    if !output.status.success() {
        return;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for token in stdout.split_whitespace() {
        if let Some(lib) = token.strip_prefix("-l") {
            if !matches!(
                lib,
                "avcodec"
                    | "avformat"
                    | "avutil"
                    | "avfilter"
                    | "avdevice"
                    | "swscale"
                    | "swresample"
                    | "postproc"
            ) {
                println!("cargo:rustc-link-lib={lib}");
            }
        } else if let Some(path) = token.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        }
    }
}
```

- [ ] **Step 4: Verify the workspace compiles**

```bash
ENV_SETUP && cargo check --workspace
```

Expected: compiles with zero errors. There may be unused-dependency warnings since we haven't written the code yet.

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-client/Cargo.toml crates/stargaze-client/build.rs .devcontainer/Dockerfile Cargo.lock && \
git commit --no-gpg-sign -m "chore(deps): add ffmpeg-next, sdl2, libc for video decoding and rendering"
```

---

## Task 2: Shared decode types in stargaze-core

**Files:**
- Create: `crates/stargaze-core/src/decode.rs`
- Modify: `crates/stargaze-core/src/lib.rs`

This task creates the shared types that the client decoder and renderer use: `DecodedFrame`, `DecoderConfig`, `DecodeError`.

- [ ] **Step 1: Create decode.rs with types and tests**

Create `crates/stargaze-core/src/decode.rs`:

```rust
//! Shared types for video decoding.
//!
//! Defines decoded frame data, decoder configuration, and error types
//! used by the client decoder and renderer.

use crate::config::Codec;
use thiserror::Error;

/// A decoded video frame ready for rendering.
///
/// Contains raw pixel data in NV12 format: a Y (luma) plane followed
/// by an interleaved UV (chroma) plane at half vertical resolution.
///
/// Total data size: `width * height * 3 / 2` bytes.
/// - Y plane:  `data[0 .. width * height]`
/// - UV plane: `data[width * height .. width * height * 3 / 2]`
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    /// NV12 pixel data (Y plane followed by interleaved UV plane).
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Presentation timestamp (matches the encoded frame's PTS).
    pub pts: u64,
}

/// Configuration for the video decoder.
///
/// Constructed from session parameters received during the transport
/// handshake — describes the expected stream properties.
#[derive(Debug, Clone)]
pub struct DecoderConfig {
    /// Expected frame width in pixels.
    pub width: u32,
    /// Expected frame height in pixels.
    pub height: u32,
    /// Codec to decode.
    pub codec: Codec,
}

/// Errors from the video decoding subsystem.
#[derive(Error, Debug)]
pub enum DecodeError {
    /// An FFmpeg operation failed.
    #[error("FFmpeg error: {0}")]
    FfmpegError(String),

    /// Decoder initialization failed (codec unavailable, etc.).
    #[error("Decoder initialization failed: {0}")]
    InitError(String),

    /// Decoding a specific frame failed.
    #[error("Decoding failed for frame at PTS {pts}: {reason}")]
    DecodeFrameError {
        /// PTS of the frame that failed.
        pts: u64,
        /// Description of the failure.
        reason: String,
    },

    /// The requested codec is not supported.
    #[error("Unsupported codec: {0}")]
    UnsupportedCodec(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_error_display_ffmpeg() {
        let err = DecodeError::FfmpegError("packet rejected".to_string());
        assert_eq!(err.to_string(), "FFmpeg error: packet rejected");
    }

    #[test]
    fn decode_error_display_init() {
        let err = DecodeError::InitError("hevc decoder not found".to_string());
        assert_eq!(
            err.to_string(),
            "Decoder initialization failed: hevc decoder not found"
        );
    }

    #[test]
    fn decode_error_display_frame() {
        let err = DecodeError::DecodeFrameError {
            pts: 42,
            reason: "corrupt NAL unit".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Decoding failed for frame at PTS 42: corrupt NAL unit"
        );
    }

    #[test]
    fn decode_error_display_unsupported_codec() {
        let err = DecodeError::UnsupportedCodec("VP9".to_string());
        assert_eq!(err.to_string(), "Unsupported codec: VP9");
    }

    #[test]
    fn decoded_frame_construction() {
        let width: u32 = 1920;
        let height: u32 = 1080;
        let nv12_size = (width * height * 3 / 2) as usize;
        let frame = DecodedFrame {
            data: vec![128; nv12_size],
            width,
            height,
            pts: 0,
        };
        assert_eq!(frame.data.len(), nv12_size);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.pts, 0);
    }

    #[test]
    fn decoded_frame_nv12_plane_sizes() {
        let width: u32 = 640;
        let height: u32 = 480;
        let y_size = (width * height) as usize;
        let uv_size = (width * height / 2) as usize;
        let total = y_size + uv_size;

        let frame = DecodedFrame {
            data: vec![0; total],
            width,
            height,
            pts: 100,
        };

        // Y plane: first width*height bytes.
        assert_eq!(y_size, 307_200);
        // UV plane: next width*height/2 bytes.
        assert_eq!(uv_size, 153_600);
        assert_eq!(frame.data.len(), y_size + uv_size);
    }

    #[test]
    fn decoder_config_construction() {
        let cfg = DecoderConfig {
            width: 1920,
            height: 1080,
            codec: Codec::H265,
        };
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert!(matches!(cfg.codec, Codec::H265));
    }
}
```

- [ ] **Step 2: Add the module to lib.rs**

Modify `crates/stargaze-core/src/lib.rs` to add the decode module:

```rust
pub mod capture;
pub mod config;
pub mod decode;
pub mod encode;
pub mod error;
pub mod transport;
```

- [ ] **Step 3: Run tests**

```bash
ENV_SETUP && cargo test --package stargaze-core -- decode
```

Expected: all 7 decode tests pass.

- [ ] **Step 4: Run clippy**

```bash
ENV_SETUP && cargo clippy --package stargaze-core -- -W clippy::pedantic
```

Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-core/src/decode.rs crates/stargaze-core/src/lib.rs && \
git commit --no-gpg-sign -m "feat(core): add shared decode types — DecodedFrame, DecoderConfig, DecodeError"
```

---

## Task 3: FFmpeg H.265 software decoder

**Files:**
- Create: `crates/stargaze-client/src/decode/mod.rs`
- Create: `crates/stargaze-client/src/decode/ffmpeg.rs`
- Modify: `crates/stargaze-client/src/lib.rs`

This task implements the core decoder: FFmpeg initialization, the synchronous decode loop on a dedicated thread, and the `DecoderSession` lifecycle API.

- [ ] **Step 1: Create the decode module structure**

Create `crates/stargaze-client/src/decode/mod.rs`:

This file provides the public API: `DecoderSession` (handle with `stop()` and `Drop`) and `start_decoder()` (spawns the decoder thread, returns session + decoded frame receiver). Follow the exact pattern from `stargaze-server/src/encode/mod.rs`:

- `DecoderSession` struct with `thread_handle: Option<thread::JoinHandle<()>>` and `shutdown: Arc<AtomicBool>`
- `stop(self) -> Result<(), DecodeError>` method
- `Drop` impl that signals shutdown and joins
- `start_decoder(config, frames_rx)` that:
  1. Creates `std::sync::mpsc::channel::<DecodedFrame>()` (decoder→renderer channel)
  2. Spawns a named thread (`stargaze-decoder`)
  3. Uses a `std::sync::mpsc::channel` for init error reporting (same pattern as encoder)
  4. On the thread: calls `ffmpeg::init_decoder(&config)`, then `ffmpeg::run_decode_loop()`
  5. Returns `(DecoderSession, std::sync::mpsc::Receiver<DecodedFrame>)`

- [ ] **Step 2: Create the FFmpeg decode implementation**

Create `crates/stargaze-client/src/decode/ffmpeg.rs`:

This file implements:

**`init_decoder(config: &DecoderConfig) -> Result<FfmpegDecoder, DecodeError>`:**
1. `ffmpeg_next::init()` — initialize FFmpeg
2. Find the `hevc` decoder: `ffmpeg_next::decoder::find(codec::Id::H265)`
3. Create codec context: `codec::context::Context::new_with_codec(codec)`
4. Open the decoder: `context.decoder().video()`
5. Return `FfmpegDecoder { decoder, scaler: None }` — scaler is lazily created on first frame when we know the output format

**`run_decode_loop(decoder, frames_rx, decoded_tx, shutdown) -> Result<(), DecodeError>`:**
1. Loop on `frames_rx.blocking_recv()`:
   - Create `ffmpeg_next::Packet` from `ReassembledFrame.data`
   - Set packet PTS from `frame.pts`
   - Call `decoder.send_packet(&packet)`
   - Drain frames with `decoder.receive_frame(&mut video_frame)` in a loop:
     - If frame format is not NV12, lazily create/use a software scaler (`ffmpeg_next::software::scaling::Context`) to convert to NV12
     - Extract Y and UV plane data from the decoded frame
     - Construct `DecodedFrame` and send via `decoded_tx`
   - On EAGAIN, continue to next packet
   - On decode errors, log warning and continue (skip corrupt frames)
2. On channel close: `decoder.send_eof()`, drain remaining frames
3. Check `shutdown` flag each iteration

**Important details:**
- FFmpeg's H.265 decoder outputs YUV420P by default, not NV12. Use `ffmpeg_next::software::scaling::Context` to convert YUV420P → NV12.
- The scaler is created lazily after receiving the first frame (when decoder output format is known).
- Packet creation: `ffmpeg_next::Packet::copy(&frame.data)` or construct from raw bytes.

- [ ] **Step 3: Export the decode module from lib.rs**

Modify `crates/stargaze-client/src/lib.rs`:

```rust
pub mod decode;
pub mod transport;
```

- [ ] **Step 4: Verify compilation**

```bash
ENV_SETUP && cargo check --package stargaze-client
```

Expected: compiles with zero errors. Dead-code warnings are fine since the decoder isn't wired into main.rs yet.

- [ ] **Step 5: Run clippy**

```bash
ENV_SETUP && cargo clippy --package stargaze-client -- -W clippy::pedantic
```

Expected: no warnings (or only dead-code warnings).

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-client/src/decode/ crates/stargaze-client/src/lib.rs && \
git commit --no-gpg-sign -m "feat(client): add FFmpeg H.265 software decoder with dedicated thread"
```

---

## Task 4: SDL2 video renderer

**Files:**
- Create: `crates/stargaze-client/src/render/mod.rs`
- Create: `crates/stargaze-client/src/render/sdl.rs`

This task implements the SDL2 renderer: window creation, YUV texture management, and the main-thread event loop that presents decoded frames.

- [ ] **Step 1: Create the render module structure**

Create `crates/stargaze-client/src/render/mod.rs`:

Public API: `start_renderer(config, decoded_rx)` — this function **does not return** until the window is closed. It takes over the main thread to run the SDL2 event loop.

```rust
/// Starts the video renderer.
///
/// Takes over the calling thread to run the SDL2 event loop.
/// Returns when the window is closed or an error occurs.
///
/// # Arguments
///
/// * `config` — Decoder config (width, height for window sizing)
/// * `decoded_rx` — Receiver for decoded NV12 frames
/// * `fullscreen` — Whether to create a fullscreen window
pub fn start_renderer(
    config: &DecoderConfig,
    decoded_rx: std::sync::mpsc::Receiver<DecodedFrame>,
    fullscreen: bool,
) -> Result<(), anyhow::Error>
```

- [ ] **Step 2: Create the SDL2 renderer implementation**

Create `crates/stargaze-client/src/render/sdl.rs`:

**`run_sdl_loop(config, decoded_rx, fullscreen) -> Result<(), anyhow::Error>`:**

1. **Init SDL2:**
   - `sdl2::init()?.video()?`
   - Create window: `video.window("Stargaze", width, height)` with `.position_centered()` and optional `.fullscreen_desktop()`
   - Create SDL renderer: `window.into_canvas().accelerated().present_vsync().build()?`
   - Create YUV texture: `texture_creator.create_texture_streaming(PixelFormatEnum::NV12, width, height)?`

2. **Event loop:**
   ```
   let mut event_pump = sdl.event_pump()?;
   'main: loop {
       // Handle SDL events
       for event in event_pump.poll_iter() {
           match event {
               Event::Quit { .. } => break 'main,
               Event::KeyDown { keycode: Some(Keycode::Escape), .. } => break 'main,
               _ => {}
           }
       }

       // Drain decoded frames, keep latest
       let mut latest_frame = None;
       while let Ok(frame) = decoded_rx.try_recv() {
           latest_frame = Some(frame);
       }

       // Update texture and present
       if let Some(frame) = latest_frame {
           texture.update_yuv(
               None,
               &frame.data[..y_size],        // Y plane
               width as usize,                // Y pitch
               &frame.data[y_size..],         // UV plane (NV12: interleaved)
               width as usize,                // UV pitch
           )?;
           // Note: for NV12, use update() with the full NV12 data, not update_yuv()
           // SDL2's update_yuv is for planar YUV (I420). For NV12 use update() directly.
       }

       canvas.copy(&texture, None, None)?;
       canvas.present();
   }
   ```

**NV12 texture update detail:**
SDL2's `Texture::update()` accepts raw bytes for NV12 format. The pitch is `width` for the Y plane. The data layout must match what the decoder produces: Y plane (`width*height` bytes) followed by UV plane (`width*height/2` bytes interleaved).

- [ ] **Step 3: Export the render module from lib.rs**

Modify `crates/stargaze-client/src/lib.rs`:

```rust
pub mod decode;
pub mod render;
pub mod transport;
```

- [ ] **Step 4: Verify compilation**

```bash
ENV_SETUP && cargo check --package stargaze-client
```

Expected: compiles with zero errors.

- [ ] **Step 5: Run clippy**

```bash
ENV_SETUP && cargo clippy --package stargaze-client -- -W clippy::pedantic
```

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-client/src/render/ crates/stargaze-client/src/lib.rs && \
git commit --no-gpg-sign -m "feat(client): add SDL2 video renderer with NV12 texture presentation"
```

---

## Task 5: Wire decoder + renderer into client pipeline

**Files:**
- Modify: `crates/stargaze-client/src/main.rs`

This task connects the three stages: transport → decoder → renderer.

- [ ] **Step 1: Update main.rs**

Modify the `main()` function in `crates/stargaze-client/src/main.rs`:

1. After `transport::connect()` returns `(client_transport, frames_rx)`:
2. Create `DecoderConfig` from the session parameters (width/height/codec)
3. Call `decode::start_decoder(config, frames_rx)` → `(decoder_session, decoded_rx)`
4. Call `render::start_renderer(config, decoded_rx, cfg.fullscreen)` — this blocks on the main thread running the SDL2 event loop
5. After renderer returns (window closed): stop decoder, abort transport
6. Clean shutdown with proper ordering

The structure should be:

```rust
// Connect to server
let (client_transport, frames_rx) = transport::connect(&cfg, session_request).await?;

// Start decoder
let decoder_config = DecoderConfig { width: 1920, height: 1080, codec: Codec::H265 };
let (decoder_session, decoded_rx) = decode::start_decoder(decoder_config.clone(), frames_rx)?;

// Run renderer (blocks until window close)
// Note: SDL2 event loop must run on main thread, so we need to drop out of
// the async context. Use tokio::task::block_in_place or spawn the tokio
// runtime on a separate thread.
render::start_renderer(&decoder_config, decoded_rx, cfg.fullscreen)?;

// Cleanup
decoder_session.stop()?;
client_transport.abort();
```

**Threading consideration:** `main()` runs inside `#[tokio::main]` which means we're on the tokio runtime. The SDL2 event loop is blocking and must run on the main OS thread. Options:
- Use `tokio::task::block_in_place()` to allow blocking in the tokio context
- Or restructure: init tokio runtime manually, spawn transport+decoder, run SDL on the calling thread

The simpler approach is `block_in_place()`.

- [ ] **Step 2: Handle Ctrl+C gracefully**

Integrate signal handling: the existing `tokio::signal::ctrl_c()` select should still work. When the renderer's event loop returns (user closes window OR Ctrl+C), clean up all resources.

- [ ] **Step 3: Verify compilation**

```bash
ENV_SETUP && cargo check --package stargaze-client
```

- [ ] **Step 4: Run full workspace tests**

```bash
ENV_SETUP && cargo test --workspace
```

Expected: existing 47 tests pass + new decode type tests pass.

- [ ] **Step 5: Run clippy**

```bash
ENV_SETUP && cargo clippy --workspace -- -W clippy::pedantic
```

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-client/src/main.rs && \
git commit --no-gpg-sign -m "feat(client): wire decoder and renderer into transport pipeline"
```

---

## Task 6: Tests

**Files:**
- Tests in `crates/stargaze-core/src/decode.rs` (already written in Task 2)
- Tests in `crates/stargaze-client/src/decode/ffmpeg.rs`

- [ ] **Step 1: Add decoder unit tests**

In `crates/stargaze-client/src/decode/ffmpeg.rs`, add a `#[cfg(test)]` module:

- `test_decoder_init`: Verify `init_decoder()` succeeds with H.265 config (FFmpeg's software `hevc` decoder should be available in any FFmpeg build).
- `test_decoder_init_rejects_unknown`: Verify graceful error for unsupported codec (if applicable).

Note: Testing actual decode of H.265 packets requires a valid encoded bitstream. The simplest approach is an `#[ignore]` test that encodes a synthetic frame with the server's NVENC encoder and then decodes it — but this requires GPU. For now, init tests are sufficient.

- [ ] **Step 2: Verify all tests pass**

```bash
ENV_SETUP && cargo test --workspace
```

Expected: existing 47 + new tests all pass.

- [ ] **Step 3: Final clippy and fmt check**

```bash
ENV_SETUP && cargo clippy --workspace -- -W clippy::pedantic && cargo fmt --check
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add -A && \
git commit --no-gpg-sign -m "test(client): add decoder initialization and decode type tests"
```

---

## Important Notes

### FFmpeg Decoder Output Format
FFmpeg's software H.265 decoder (`hevc`) outputs **YUV420P** (3 separate planes), not NV12. A software scaler (`sws_scale` via `ffmpeg_next::software::scaling::Context`) is needed to convert YUV420P → NV12 before sending to the renderer. This is a CPU operation but fast (< 1ms at 1080p).

### SDL2 NV12 Texture
SDL2's `SDL_PIXELFORMAT_NV12` expects the Y plane followed by interleaved UV data. Verify the `update()` call uses the correct pitch values. For NV12:
- Y pitch = width
- UV pitch = width (each UV row has width bytes: width/2 U-V pairs × 2 bytes)

### SDL2 on Wayland
SDL2 supports Wayland natively. Set `SDL_VIDEODRIVER=wayland` in the environment if SDL2 defaults to X11/XWayland. The devcontainer has no display server, so SDL2 tests that create windows must be `#[ignore]`.

### Blocking in Tokio
The SDL2 event loop blocks the main thread. Since `main()` uses `#[tokio::main]`, use `tokio::task::block_in_place()` to run the renderer without starving the tokio runtime's thread pool. Alternatively, restructure to spawn the tokio runtime on a background thread and keep the main thread for SDL2.

### Error Recovery
The decoder should be resilient to corrupt/missing frames:
- `send_packet` errors for individual packets: log warning, skip, continue
- `receive_frame` EAGAIN: normal, means decoder needs more input
- The transport layer already handles IDR requests when frames are lost
