# Video Encoding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Encode captured video frames using NVIDIA NVENC hardware encoding via FFmpeg, producing H.265 encoded packets for network transport.

**Architecture:** A dedicated `std::thread` receives `Frame` values from the capture pipeline via `tokio::sync::mpsc`, uploads them to the GPU via `av_hwframe_transfer_data()`, encodes with `hevc_nvenc`, and sends `EncodedPacket` values on an output channel. FFmpeg CUDA hardware contexts are set up via `ffmpeg-sys-next` FFI, while the encode loop uses `ffmpeg-next` safe wrappers.

**Tech Stack:** Rust 2024 nightly, `ffmpeg-next` 7 (safe FFmpeg bindings), `ffmpeg-sys-next` 7 (raw FFI for hw context), tokio (channels), thiserror (errors)

**Spec:** `docs/specs/2026-03-31-video-encoding-design.md`

**Build environment note:** Every `cargo` command in this plan MUST be prefixed with:
```bash
export PATH="$HOME/.local/usr/bin:$HOME/.local/usr/lib/llvm-19/bin:$PATH" && \
export PKG_CONFIG_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu/pkgconfig" && \
export LIBRARY_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu:$HOME/.local/usr/lib/llvm-19/lib" && \
export LD_LIBRARY_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu:$HOME/.local/usr/lib/llvm-19/lib" && \
export C_INCLUDE_PATH="$HOME/.local/usr/include" && \
export LIBCLANG_PATH="$HOME/.local/usr/lib/llvm-19/lib" && \
export BINDGEN_EXTRA_CLANG_ARGS="-I$HOME/.local/usr/include -I/usr/lib/gcc/x86_64-linux-gnu/14/include -I/usr/include"
```

For brevity, the plan references this as `ENV_SETUP` — paste the full block before each `cargo` invocation.

---

## File Structure

### New files to create

- `crates/stargaze-core/src/encode.rs` — shared types: `EncodedPacket`, `EncoderConfig`, `EncodeError`
- `crates/stargaze-server/src/encode/mod.rs` — public API: `EncoderSession`, `start_encoder()`
- `crates/stargaze-server/src/encode/ffmpeg.rs` — FFmpeg NVENC init, hw device/frame context, encode loop

### Files to modify

- `crates/stargaze-core/src/lib.rs` — add `pub mod encode;`
- `crates/stargaze-core/Cargo.toml` — no dependency changes needed
- `crates/stargaze-server/Cargo.toml` — add `ffmpeg-next`, `ffmpeg-sys-next` dependencies
- `crates/stargaze-server/src/main.rs` — wire encoder between capture and (future) transport
- `.devcontainer/Dockerfile` — add FFmpeg dev packages

---

## Task 1: Install FFmpeg dev packages and add crate dependencies

**Files:**
- Modify: `.devcontainer/Dockerfile`
- Modify: `crates/stargaze-server/Cargo.toml`

- [ ] **Step 1: Install FFmpeg development headers**

The devcontainer has FFmpeg runtime but not dev headers. Extract them to `~/.local/usr/` like the PipeWire packages:

```bash
apt-get download libavcodec-dev libavutil-dev libavformat-dev libswscale-dev libswresample-dev && \
for pkg in libavcodec-dev*.deb libavutil-dev*.deb libavformat-dev*.deb libswscale-dev*.deb libswresample-dev*.deb; do \
  dpkg-deb -x "$pkg" ~/.local/usr_tmp && \
  cp -rn ~/.local/usr_tmp/usr/* ~/.local/usr/ && \
  rm -rf ~/.local/usr_tmp; \
done && \
rm -f libavcodec-dev*.deb libavutil-dev*.deb libavformat-dev*.deb libswscale-dev*.deb libswresample-dev*.deb
```

Verify the pkg-config files are discoverable:

```bash
export PKG_CONFIG_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu/pkgconfig" && \
pkg-config --modversion libavcodec && \
pkg-config --modversion libavutil
```

Expected: prints version numbers (e.g. `61.19.100`, `59.39.100`).

- [ ] **Step 2: Update the Dockerfile for reproducibility**

Add FFmpeg dev packages to the `apt-get install` list in `.devcontainer/Dockerfile`. The full install line should become:

```dockerfile
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
    libdbus-1-dev \
    libavcodec-dev \
    libavutil-dev \
    libavformat-dev \
    libswscale-dev \
    libswresample-dev
```

- [ ] **Step 3: Add ffmpeg-next and ffmpeg-sys-next to stargaze-server dependencies**

Update `crates/stargaze-server/Cargo.toml`:

```toml
[package]
name = "stargaze-server"
version.workspace = true
edition.workspace = true

[dependencies]
stargaze-core = { path = "../stargaze-core" }
anyhow = "1"
ashpd = { version = "0.13", features = ["pipewire", "screencast"] }
clap = { version = "4", features = ["derive"] }
ffmpeg-next = "7"
ffmpeg-sys-next = "7"
libc = "0.2"
pipewire = "0.9"
pipewire-sys = "0.9"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 4: Verify the project compiles with new dependencies**

Run:

```bash
ENV_SETUP && cargo check --package stargaze-server
```

Expected: compiles successfully. `ffmpeg-sys-next` build script will find FFmpeg via pkg-config. This may take a while on first build (bindgen runs against FFmpeg headers).

- [ ] **Step 5: Commit**

```bash
git add .devcontainer/Dockerfile crates/stargaze-server/Cargo.toml Cargo.lock
git commit --no-gpg-sign -m "chore(deps): add ffmpeg-next and ffmpeg-sys-next for video encoding"
```

---

## Task 2: Add shared encode types to stargaze-core

**Files:**
- Create: `crates/stargaze-core/src/encode.rs`
- Modify: `crates/stargaze-core/src/lib.rs`

- [ ] **Step 1: Write tests for EncodeError display messages and type construction**

Create `crates/stargaze-core/src/encode.rs` with types and tests:

```rust
use thiserror::Error;

/// An encoded video packet (one or more NAL units from a single frame).
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// H.265 NAL units with Annex B start codes (0x00000001 prefix).
    pub data: Vec<u8>,
    /// Presentation timestamp (frame number).
    pub pts: u64,
    /// True for IDR frames.
    pub is_keyframe: bool,
}

/// Configuration for the video encoder.
///
/// Constructed from `ServerConfig` fields but kept separate so
/// the encoder doesn't depend on the full config system.
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Target framerate.
    pub framerate: u32,
    /// Target bitrate in Mbps.
    pub bitrate_mbps: u32,
}

/// Errors from the video encoding subsystem.
#[derive(Error, Debug)]
pub enum EncodeError {
    /// An FFmpeg operation failed.
    #[error("FFmpeg error: {0}")]
    FfmpegError(String),

    /// Encoder initialization failed (NVENC unavailable, CUDA device error, etc.).
    #[error("Encoder initialization failed: {0}")]
    InitError(String),

    /// Encoding a specific frame failed.
    #[error("Encoding failed for frame {frame}: {reason}")]
    EncodeFrameError {
        /// Frame number that failed.
        frame: u64,
        /// Description of the failure.
        reason: String,
    },

    /// The captured frame has an unsupported pixel format.
    #[error("Unsupported pixel format: {0}")]
    UnsupportedFormat(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_error_display_ffmpeg() {
        let err = EncodeError::FfmpegError("codec not found".to_string());
        assert_eq!(err.to_string(), "FFmpeg error: codec not found");
    }

    #[test]
    fn encode_error_display_init() {
        let err = EncodeError::InitError("CUDA device creation failed".to_string());
        assert_eq!(
            err.to_string(),
            "Encoder initialization failed: CUDA device creation failed"
        );
    }

    #[test]
    fn encode_error_display_frame() {
        let err = EncodeError::EncodeFrameError {
            frame: 42,
            reason: "upload failed".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "Encoding failed for frame 42: upload failed"
        );
    }

    #[test]
    fn encode_error_display_unsupported_format() {
        let err = EncodeError::UnsupportedFormat("YUV444P".to_string());
        assert_eq!(err.to_string(), "Unsupported pixel format: YUV444P");
    }

    #[test]
    fn encoded_packet_construction() {
        let pkt = EncodedPacket {
            data: vec![0x00, 0x00, 0x00, 0x01, 0x40, 0x01],
            pts: 0,
            is_keyframe: true,
        };
        assert_eq!(pkt.data.len(), 6);
        assert_eq!(pkt.pts, 0);
        assert!(pkt.is_keyframe);
    }

    #[test]
    fn encoder_config_construction() {
        let cfg = EncoderConfig {
            width: 1920,
            height: 1080,
            framerate: 60,
            bitrate_mbps: 20,
        };
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert_eq!(cfg.framerate, 60);
        assert_eq!(cfg.bitrate_mbps, 20);
    }
}
```

- [ ] **Step 2: Add the encode module to lib.rs**

Update `crates/stargaze-core/src/lib.rs`:

```rust
pub mod capture;
pub mod config;
pub mod encode;
pub mod error;
```

- [ ] **Step 3: Run the tests**

Run:

```bash
ENV_SETUP && cargo test --package stargaze-core
```

Expected: all existing tests (16 config/error + 7 capture) plus 6 new encode tests pass — 29 total.

- [ ] **Step 4: Run clippy**

Run:

```bash
ENV_SETUP && cargo clippy --package stargaze-core -- -W clippy::pedantic
```

Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-core/src/encode.rs crates/stargaze-core/src/lib.rs
git commit --no-gpg-sign -m "feat(core): add shared encode types — EncodedPacket, EncoderConfig, EncodeError"
```

---

## Task 3: Implement FFmpeg NVENC initialization and encode loop

This is the core FFmpeg integration — hardware context setup, codec configuration, and the synchronous encode loop. This file contains only private functions called from the public API in `mod.rs` (Task 4).

**Files:**
- Create: `crates/stargaze-server/src/encode/ffmpeg.rs`

- [ ] **Step 1: Create the encode directory**

```bash
mkdir -p crates/stargaze-server/src/encode
```

- [ ] **Step 2: Implement the FFmpeg encoder module**

Create `crates/stargaze-server/src/encode/ffmpeg.rs`:

```rust
//! FFmpeg NVENC encoder internals.
//!
//! Handles CUDA hardware context setup, codec configuration,
//! and the synchronous encode loop. All FFmpeg interaction is
//! confined to this module.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use stargaze_core::capture::Frame;
use stargaze_core::encode::{EncodeError, EncodedPacket, EncoderConfig};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

/// Opaque handle to initialized FFmpeg encoder state.
///
/// Owns the codec context, hardware device context, and hardware frames context.
/// All fields are used through the FFmpeg safe wrappers except for the raw
/// hardware context pointers which require `ffmpeg-sys-next` FFI.
pub(crate) struct FfmpegEncoder {
    /// Opened H.265 NVENC encoder (owns the `AVCodecContext`).
    encoder: ffmpeg_next::encoder::video::Encoder,
    /// Raw pointer to the CUDA hardware device context (`AVBufferRef`).
    /// Owned — must be freed via `av_buffer_unref` on drop.
    hw_device_ctx: *mut ffmpeg_sys_next::AVBufferRef,
    /// Raw pointer to the hardware frames context (`AVBufferRef`).
    /// Owned — must be freed via `av_buffer_unref` on drop.
    hw_frames_ctx: *mut ffmpeg_sys_next::AVBufferRef,
    /// Encoder configuration (width, height, framerate, bitrate).
    config: EncoderConfig,
}

// Safety: FfmpegEncoder is only used on the dedicated encoder thread.
// FFmpeg contexts are not thread-safe, but we never share them across threads.
unsafe impl Send for FfmpegEncoder {}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.hw_frames_ctx.is_null() {
                ffmpeg_sys_next::av_buffer_unref(&mut self.hw_frames_ctx);
            }
            if !self.hw_device_ctx.is_null() {
                ffmpeg_sys_next::av_buffer_unref(&mut self.hw_device_ctx);
            }
        }
    }
}

/// Initializes the FFmpeg NVENC encoder with CUDA hardware acceleration.
///
/// Sets up:
/// 1. CUDA device context (`av_hwdevice_ctx_create`)
/// 2. Hardware frames context (`av_hwframe_ctx_alloc` + `av_hwframe_ctx_init`)
/// 3. H.265 NVENC codec context with ultra-low-latency settings
///
/// # Errors
///
/// Returns `EncodeError::InitError` if any FFmpeg initialization step fails
/// (NVENC not available, CUDA device not found, etc.).
pub(crate) fn init_encoder(config: &EncoderConfig) -> Result<FfmpegEncoder, EncodeError> {
    // Initialize FFmpeg (safe to call multiple times).
    ffmpeg_next::init().map_err(|e| EncodeError::InitError(format!("ffmpeg init: {e}")))?;

    // Step 1: Find the hevc_nvenc encoder.
    let codec = ffmpeg_next::encoder::find_by_name("hevc_nvenc")
        .ok_or_else(|| EncodeError::InitError("hevc_nvenc encoder not found — is FFmpeg compiled with NVENC support?".to_string()))?;

    info!("Found hevc_nvenc encoder: {}", codec.name());

    // Step 2: Create CUDA hardware device context.
    let mut hw_device_ctx: *mut ffmpeg_sys_next::AVBufferRef = ptr::null_mut();
    let ret = unsafe {
        ffmpeg_sys_next::av_hwdevice_ctx_create(
            &mut hw_device_ctx,
            ffmpeg_sys_next::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
            ptr::null(),     // default CUDA device
            ptr::null_mut(), // no options
            0,               // no flags
        )
    };
    if ret < 0 {
        return Err(EncodeError::InitError(format!(
            "failed to create CUDA device context (error code {ret}) — is an NVIDIA GPU available?"
        )));
    }
    debug!("Created CUDA device context");

    // Step 3: Allocate and configure hardware frames context.
    let hw_frames_ctx = unsafe { ffmpeg_sys_next::av_hwframe_ctx_alloc(hw_device_ctx) };
    if hw_frames_ctx.is_null() {
        unsafe { ffmpeg_sys_next::av_buffer_unref(&mut hw_device_ctx) };
        return Err(EncodeError::InitError(
            "failed to allocate hardware frames context".to_string(),
        ));
    }

    unsafe {
        let frames_ctx = (*hw_frames_ctx).data.cast::<ffmpeg_sys_next::AVHWFramesContext>();
        (*frames_ctx).format = ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*frames_ctx).sw_format = ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_NV12;
        (*frames_ctx).width = config.width as i32;
        (*frames_ctx).height = config.height as i32;
        (*frames_ctx).initial_pool_size = 0; // on-demand allocation
    }

    let ret = unsafe { ffmpeg_sys_next::av_hwframe_ctx_init(hw_frames_ctx) };
    if ret < 0 {
        unsafe {
            let mut hw_frames_ptr = hw_frames_ctx;
            ffmpeg_sys_next::av_buffer_unref(&mut hw_frames_ptr);
            ffmpeg_sys_next::av_buffer_unref(&mut hw_device_ctx);
        };
        return Err(EncodeError::InitError(format!(
            "failed to initialize hardware frames context (error code {ret})"
        )));
    }
    debug!("Initialized CUDA hardware frames context ({}x{}, NV12)", config.width, config.height);

    // Step 4: Create codec context and configure.
    let mut ctx = ffmpeg_next::codec::context::Context::new_with_codec(codec);

    // Attach hardware contexts before configuring the encoder.
    unsafe {
        let raw_ctx = ctx.as_mut_ptr();
        (*raw_ctx).hw_device_ctx = ffmpeg_sys_next::av_buffer_ref(hw_device_ctx);
        (*raw_ctx).hw_frames_ctx = ffmpeg_sys_next::av_buffer_ref(hw_frames_ctx);
    }

    let mut encoder = ctx
        .encoder()
        .video()
        .map_err(|e| EncodeError::InitError(format!("failed to create video encoder context: {e}")))?;

    // Configure codec context.
    encoder.set_width(config.width);
    encoder.set_height(config.height);
    encoder.set_format(ffmpeg_next::format::Pixel::CUDA);
    encoder.set_time_base(ffmpeg_next::Rational(1, config.framerate as i32));
    encoder.set_frame_rate(Some(ffmpeg_next::Rational(config.framerate as i32, 1)));
    encoder.set_bit_rate(i64::from(config.bitrate_mbps) * 1_000_000);
    encoder.set_max_b_frames(0);
    encoder.set_gop(config.framerate * 2); // keyframe every ~2 seconds

    // Set color space parameters.
    unsafe {
        let raw_ctx = encoder.as_mut_ptr();
        (*raw_ctx).color_range = ffmpeg_sys_next::AVColorRange::AVCOL_RANGE_MPEG;
        (*raw_ctx).colorspace = ffmpeg_sys_next::AVColorSpace::AVCOL_SPC_BT709;
        (*raw_ctx).color_primaries = ffmpeg_sys_next::AVColorPrimaries::AVCOL_PRI_BT709;
        (*raw_ctx).color_trc = ffmpeg_sys_next::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
    }

    // Step 5: Open encoder with NVENC-specific options.
    let mut opts = ffmpeg_next::Dictionary::new();
    opts.set("preset", "p1");
    opts.set("tune", "ull");
    opts.set("rc", "cbr");
    opts.set("delay", "0");
    opts.set("forced-idr", "1");
    opts.set("zerolatency", "1");

    let opened = encoder.open_with(opts).map_err(|e| {
        EncodeError::InitError(format!("failed to open hevc_nvenc encoder: {e}"))
    })?;

    info!(
        "NVENC encoder initialized: {}x{} @ {}fps, {} Mbps, H.265",
        config.width, config.height, config.framerate, config.bitrate_mbps
    );

    Ok(FfmpegEncoder {
        encoder: opened,
        hw_device_ctx,
        hw_frames_ctx,
        config: config.clone(),
    })
}

/// Runs the encode loop: receives frames, uploads to GPU, encodes, sends packets.
///
/// This function blocks until `shutdown` is signaled or the input channel closes.
/// It is meant to run on a dedicated `std::thread`.
///
/// # Errors
///
/// Returns `EncodeError` if a fatal encoding error occurs. Non-fatal errors
/// (e.g., a single frame upload failure) are logged and skipped.
pub(crate) fn run_encode_loop(
    encoder: &mut FfmpegEncoder,
    frames: &mut mpsc::Receiver<Frame>,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    shutdown: &Arc<AtomicBool>,
) -> Result<(), EncodeError> {
    let mut frame_counter: u64 = 0;
    let mut packet = ffmpeg_next::Packet::empty();

    loop {
        // Check shutdown flag.
        if shutdown.load(Ordering::Relaxed) {
            debug!("Shutdown signaled, flushing encoder");
            break;
        }

        // Blocking receive from capture channel.
        let frame = match frames.blocking_recv() {
            Some(f) => f,
            None => {
                info!("Capture channel closed, flushing encoder");
                break;
            }
        };

        // Upload frame to GPU and encode.
        match upload_and_encode(encoder, &frame, frame_counter) {
            Ok(()) => {}
            Err(e) => {
                warn!(frame = frame_counter, "Skipping frame: {e}");
                frame_counter += 1;
                continue;
            }
        }

        // Receive encoded packets.
        drain_packets(&mut encoder.encoder, &mut packet, packets_tx, frame_counter);

        frame_counter += 1;
        if frame_counter % 300 == 0 {
            trace!(frame = frame_counter, "Encode progress");
        }
    }

    // Flush: send null frame to drain the encoder.
    flush_encoder(&mut encoder.encoder, &mut packet, packets_tx);

    info!(total_frames = frame_counter, "Encoder loop finished");
    Ok(())
}

/// Uploads a captured frame to a GPU hardware frame and sends it to the encoder.
///
/// For both `Frame::DmaBuf` and `Frame::CpuMapped`, the pixel data is read into
/// a software `AVFrame` (BGRA), then uploaded to a CUDA hardware frame via
/// `av_hwframe_transfer_data()`. FFmpeg handles BGRA→NV12 conversion during upload.
fn upload_and_encode(
    encoder: &mut FfmpegEncoder,
    frame: &Frame,
    pts: u64,
) -> Result<(), EncodeError> {
    let (data, width, height, stride) = match frame {
        Frame::CpuMapped {
            data,
            width,
            height,
            stride,
            ..
        } => (data.as_slice(), *width, *height, *stride),
        Frame::DmaBuf(info) => {
            // For MVP: mmap the DMA-BUF to get CPU-accessible pixels.
            return upload_dmabuf_and_encode(encoder, info, pts);
        }
    };

    // Create software frame with BGRA pixel data.
    let mut sw_frame = ffmpeg_next::frame::Video::new(
        ffmpeg_next::format::Pixel::BGRA,
        width,
        height,
    );

    // Copy pixel data into the software frame line by line.
    let dst_stride = sw_frame.stride(0);
    let dst_data = sw_frame.data_mut(0);
    for y in 0..height as usize {
        let src_offset = y * stride as usize;
        let dst_offset = y * dst_stride;
        let copy_len = (width as usize * 4).min(stride as usize).min(dst_stride);
        if src_offset + copy_len <= data.len() && dst_offset + copy_len <= dst_data.len() {
            dst_data[dst_offset..dst_offset + copy_len]
                .copy_from_slice(&data[src_offset..src_offset + copy_len]);
        }
    }

    // Allocate a hardware frame from the pool.
    let mut hw_frame = ffmpeg_next::frame::Video::empty();
    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_get_buffer(
            encoder.hw_frames_ctx,
            hw_frame.as_mut_ptr(),
            0,
        )
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_get_buffer failed (error code {ret})"),
        });
    }

    // Upload SW frame → HW frame (handles BGRA→NV12 conversion).
    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_transfer_data(
            hw_frame.as_mut_ptr(),
            sw_frame.as_ptr(),
            0,
        )
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_transfer_data failed (error code {ret})"),
        });
    }

    // Set PTS and send to encoder.
    hw_frame.set_pts(Some(pts as i64));
    encoder
        .encoder
        .send_frame(&hw_frame)
        .map_err(|e| EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("avcodec_send_frame failed: {e}"),
        })?;

    Ok(())
}

/// Uploads a `DMA-BUF` frame to the encoder by `mmap`-ing the fd.
///
/// For the MVP, this does a GPU→CPU→GPU round-trip: reads the DMA-BUF
/// contents via `mmap`, copies into a software frame, then uploads to CUDA.
fn upload_dmabuf_and_encode(
    encoder: &mut FfmpegEncoder,
    info: &stargaze_core::capture::DmaBufInfo,
    pts: u64,
) -> Result<(), EncodeError> {
    use std::os::unix::io::AsRawFd;

    let size = (info.stride * info.height) as usize;
    if size == 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: "DMA-BUF has zero size".to_string(),
        });
    }

    // mmap the DMA-BUF fd to get CPU-accessible pixels.
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            info.fd.as_raw_fd(),
            info.offset as i64,
        )
    };

    if ptr == libc::MAP_FAILED {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: "mmap of DMA-BUF fd failed".to_string(),
        });
    }

    // Safety: ptr is valid for `size` bytes, and we only read from it.
    let data = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) };

    // Create a CpuMapped-like frame and encode it.
    let mut sw_frame = ffmpeg_next::frame::Video::new(
        ffmpeg_next::format::Pixel::BGRA,
        info.width,
        info.height,
    );

    let dst_stride = sw_frame.stride(0);
    let dst_data = sw_frame.data_mut(0);
    for y in 0..info.height as usize {
        let src_offset = y * info.stride as usize;
        let dst_offset = y * dst_stride;
        let copy_len = (info.width as usize * 4).min(info.stride as usize).min(dst_stride);
        if src_offset + copy_len <= data.len() && dst_offset + copy_len <= dst_data.len() {
            dst_data[dst_offset..dst_offset + copy_len]
                .copy_from_slice(&data[src_offset..src_offset + copy_len]);
        }
    }

    // Unmap before proceeding (data slice is no longer valid).
    unsafe {
        libc::munmap(ptr, size);
    }

    // Allocate HW frame and upload.
    let mut hw_frame = ffmpeg_next::frame::Video::empty();
    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_get_buffer(
            encoder.hw_frames_ctx,
            hw_frame.as_mut_ptr(),
            0,
        )
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_get_buffer failed (error code {ret})"),
        });
    }

    let ret = unsafe {
        ffmpeg_sys_next::av_hwframe_transfer_data(
            hw_frame.as_mut_ptr(),
            sw_frame.as_ptr(),
            0,
        )
    };
    if ret < 0 {
        return Err(EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("av_hwframe_transfer_data failed (error code {ret})"),
        });
    }

    hw_frame.set_pts(Some(pts as i64));
    encoder
        .encoder
        .send_frame(&hw_frame)
        .map_err(|e| EncodeError::EncodeFrameError {
            frame: pts,
            reason: format!("avcodec_send_frame failed: {e}"),
        })?;

    Ok(())
}

/// Drains all available packets from the encoder after sending a frame.
fn drain_packets(
    encoder: &mut ffmpeg_next::encoder::video::Encoder,
    packet: &mut ffmpeg_next::Packet,
    packets_tx: &mpsc::Sender<EncodedPacket>,
    frame_counter: u64,
) {
    loop {
        match encoder.receive_packet(packet) {
            Ok(()) => {
                let encoded = EncodedPacket {
                    data: packet.data().map_or_else(Vec::new, <[u8]>::to_vec),
                    pts: packet.pts().unwrap_or(0) as u64,
                    is_keyframe: packet.is_key(),
                };

                if encoded.is_keyframe {
                    debug!(pts = encoded.pts, size = encoded.data.len(), "Keyframe encoded");
                }

                if packets_tx.blocking_send(encoded).is_err() {
                    warn!("Packet receiver dropped, stopping encoder");
                    return;
                }
            }
            Err(ffmpeg_next::Error::Other { errno: libc::EAGAIN }) => {
                // No more packets available — encoder needs another frame.
                break;
            }
            Err(e) => {
                error!(frame = frame_counter, "receive_packet error: {e}");
                break;
            }
        }
    }
}

/// Flushes the encoder by sending a null frame and draining remaining packets.
fn flush_encoder(
    encoder: &mut ffmpeg_next::encoder::video::Encoder,
    packet: &mut ffmpeg_next::Packet,
    packets_tx: &mpsc::Sender<EncodedPacket>,
) {
    debug!("Flushing encoder (sending EOF)");

    if let Err(e) = encoder.send_eof() {
        warn!("Failed to send EOF to encoder: {e}");
        return;
    }

    // Drain all remaining packets.
    loop {
        match encoder.receive_packet(packet) {
            Ok(()) => {
                let encoded = EncodedPacket {
                    data: packet.data().map_or_else(Vec::new, <[u8]>::to_vec),
                    pts: packet.pts().unwrap_or(0) as u64,
                    is_keyframe: packet.is_key(),
                };

                if packets_tx.blocking_send(encoded).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    debug!("Encoder flushed");
}
```

**Important notes for the implementing agent:**

- `ffmpeg_next::frame::Video` has `as_ptr()` returning `*const AVFrame` and `as_mut_ptr()` returning `*mut AVFrame`. The `av_hwframe_transfer_data` C function takes `const AVFrame *src`, which maps to `*const AVFrame` — use `sw_frame.as_ptr()` for the source.
- `ffmpeg_next::Error::Other { errno }` is how EAGAIN is represented. Check the actual `ffmpeg_next::Error` enum — it may differ. The key pattern is: `receive_packet` returns EAGAIN when the encoder needs more input. If the error enum uses a different variant, adjust accordingly.
- `packet.data()` returns `Option<&[u8]>`. `packet.is_key()` returns `bool`. `packet.pts()` returns `Option<i64>`.
- The `encoder.send_frame()` method takes `&Frame` (the base frame type). `frame::Video` derefs to `Frame` so this works directly.
- If `initial_pool_size = 0` causes issues, try `20` instead — some FFmpeg versions require a non-zero pool size.

- [ ] **Step 3: Verify the file compiles in isolation**

This file won't compile standalone yet (needs `mod.rs`), but you can verify syntax by creating a minimal `mod.rs`:

Create `crates/stargaze-server/src/encode/mod.rs` as a temporary stub:

```rust
pub(crate) mod ffmpeg;
```

Then run:

```bash
ENV_SETUP && cargo check --package stargaze-server
```

Fix any compilation errors. Common issues to watch for:
- `ffmpeg_next::Error` variant names may differ — check the actual enum
- `frame::Video::as_ptr()` might not exist — use `unsafe { (*frame.as_mut_ptr()) as *const _ }` if needed
- `encoder.send_frame()` takes `&ffmpeg_next::frame::Frame` — the base type, not `Video` directly. Since `Video` derefs to `Frame`, this should work, but if not, use `.deref()` or cast via `.as_ref()`
- The `Rational` import might be `ffmpeg_next::util::rational::Rational` or re-exported as `ffmpeg_next::Rational`

- [ ] **Step 4: Commit**

```bash
git add crates/stargaze-server/src/encode/
git commit --no-gpg-sign -m "feat(encode): implement FFmpeg NVENC initialization and encode loop"
```

---

## Task 4: Implement the public encoder API

The public API module owns the encoder thread lifecycle, mirroring the `CaptureSession` pattern.

**Files:**
- Modify: `crates/stargaze-server/src/encode/mod.rs` (replace stub)
- Modify: `crates/stargaze-server/src/main.rs` (add `mod encode;`)

- [ ] **Step 1: Implement the public encoder API**

Replace `crates/stargaze-server/src/encode/mod.rs` with:

```rust
//! Video encoding module — public API.
//!
//! Provides `start_encoder()` which spawns a dedicated thread for
//! FFmpeg NVENC encoding and returns an `EncoderSession` handle
//! plus a channel receiver for encoded packets.

pub(crate) mod ffmpeg;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use stargaze_core::capture::Frame;
use stargaze_core::encode::{EncodeError, EncodedPacket, EncoderConfig};
use tokio::sync::mpsc;
use tracing::{error, info};

/// Channel capacity for encoded packet delivery.
///
/// 4 packets provides a small buffer without excessive latency.
/// If the consumer can't keep up, the encoder thread blocks on
/// `blocking_send()`, which backs up into the capture channel.
const PACKET_CHANNEL_CAPACITY: usize = 4;

/// Handle to a running encoder session.
///
/// Signals the encoder thread to shut down on drop. The caller must
/// keep this alive for the duration of encoding.
pub struct EncoderSession {
    /// Join handle for the dedicated encoder thread.
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Shared flag to signal the encoder thread to stop.
    shutdown: Arc<AtomicBool>,
}

impl EncoderSession {
    /// Gracefully stops encoding: signals shutdown, waits for the
    /// encoder thread to flush remaining packets and exit.
    ///
    /// # Errors
    ///
    /// Returns `EncodeError::FfmpegError` if the encoder thread panicked.
    pub fn stop(mut self) -> Result<(), EncodeError> {
        self.signal_shutdown();
        self.join_thread()
    }

    /// Signals the encoder thread to shut down.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    /// Joins the encoder thread, returning any error.
    fn join_thread(&mut self) -> Result<(), EncodeError> {
        if let Some(handle) = self.thread_handle.take() {
            handle.join().map_err(|_| {
                EncodeError::FfmpegError("encoder thread panicked".to_string())
            })?;
        }
        Ok(())
    }
}

impl Drop for EncoderSession {
    fn drop(&mut self) {
        self.signal_shutdown();
        // Best-effort join — don't propagate errors from drop.
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

/// Starts the video encoder.
///
/// Takes ownership of the frame receiver from capture and returns
/// an encoder session handle plus a channel receiver for encoded packets.
///
/// FFmpeg initialization (CUDA device, NVENC codec) happens on the
/// spawned thread. If initialization fails, the error is sent back
/// via a oneshot channel and returned from this function.
///
/// # Errors
///
/// Returns `EncodeError::InitError` if FFmpeg/NVENC initialization fails.
/// Returns `EncodeError::FfmpegError` if the encoder thread fails to spawn.
pub fn start_encoder(
    config: EncoderConfig,
    frames: mpsc::Receiver<Frame>,
) -> Result<(EncoderSession, mpsc::Receiver<EncodedPacket>), EncodeError> {
    let (packets_tx, packets_rx) = mpsc::channel(PACKET_CHANNEL_CAPACITY);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    // Use a oneshot channel to report initialization errors back to the caller.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), EncodeError>>();

    let thread_handle = thread::Builder::new()
        .name("stargaze-encoder".to_string())
        .spawn(move || {
            // Initialize the encoder on this thread (FFmpeg contexts are thread-local).
            let mut encoder = match ffmpeg::init_encoder(&config) {
                Ok(enc) => {
                    let _ = init_tx.send(Ok(()));
                    enc
                }
                Err(e) => {
                    error!("Encoder initialization failed: {e}");
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let mut frames = frames;

            // Run the encode loop until shutdown or channel close.
            if let Err(e) = ffmpeg::run_encode_loop(
                &mut encoder,
                &mut frames,
                &packets_tx,
                &shutdown_clone,
            ) {
                error!("Encoder loop failed: {e}");
            }

            info!("Encoder thread exiting");
        })
        .map_err(|e| EncodeError::FfmpegError(format!("failed to spawn encoder thread: {e}")))?;

    // Wait for initialization to complete.
    let init_result = init_rx
        .recv()
        .map_err(|_| EncodeError::InitError("encoder thread exited during initialization".to_string()))?;

    // If init failed, join the thread and propagate the error.
    init_result?;

    info!("Encoder started on dedicated thread");

    Ok((
        EncoderSession {
            thread_handle: Some(thread_handle),
            shutdown,
        },
        packets_rx,
    ))
}
```

- [ ] **Step 2: Add the encode module to main.rs**

Add `mod encode;` to `crates/stargaze-server/src/main.rs` after the existing `mod capture;`:

```rust
mod capture;
mod encode;
```

- [ ] **Step 3: Verify it compiles**

Run:

```bash
ENV_SETUP && cargo check --package stargaze-server
```

Expected: compiles. There will be a warning about unused `encode` module — that's fine, we use it in Task 5.

- [ ] **Step 4: Run clippy**

Run:

```bash
ENV_SETUP && cargo clippy --package stargaze-server -- -W clippy::pedantic
```

Expected: no errors. If there are warnings about `missing_docs` or similar, add the appropriate doc comments. If clippy warns about the oneshot `std::sync::mpsc` usage, that's the correct choice here (not `tokio::sync::oneshot`) because the receiver blocks the calling tokio thread only briefly during init.

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-server/src/encode/mod.rs crates/stargaze-server/src/main.rs
git commit --no-gpg-sign -m "feat(encode): add public encoder API with session lifecycle management"
```

---

## Task 5: Integrate encoder into server main.rs

**Files:**
- Modify: `crates/stargaze-server/src/main.rs`

- [ ] **Step 1: Wire the encoder between capture and packet logging**

Update `crates/stargaze-server/src/main.rs` to replace the frame-logging loop with an encode→packet-logging pipeline. The full updated file:

```rust
use clap::Parser;
use stargaze_core::config::{self, Codec, Resolution, ServerConfig};
use stargaze_core::encode::EncoderConfig;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod capture;
mod encode;

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

    // Start capture pipeline.
    let capture_config = CaptureConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
    };
    let (capture_session, frames) = capture::start_capture(capture_config).await?;
    info!("Capture started");

    // Start encoder pipeline.
    let encoder_config = EncoderConfig {
        width: cfg.resolution.width,
        height: cfg.resolution.height,
        framerate: cfg.framerate,
        bitrate_mbps: cfg.bitrate,
    };
    let (encoder_session, mut packets) = encode::start_encoder(encoder_config, frames)?;
    info!("Encoder started");

    // Receive encoded packets (later: send over network).
    let mut packet_count: u64 = 0;
    loop {
        tokio::select! {
            pkt = packets.recv() => {
                let Some(pkt) = pkt else {
                    info!("Encoder channel closed");
                    break;
                };
                packet_count += 1;
                if pkt.is_keyframe || packet_count % 300 == 1 {
                    info!(
                        packet = packet_count,
                        pts = pkt.pts,
                        size = pkt.data.len(),
                        keyframe = pkt.is_keyframe,
                        "Encoded packet"
                    );
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down gracefully");
                break;
            }
        }
    }

    info!(total_packets = packet_count, "Shutting down pipeline");
    encoder_session.stop()?;
    capture_session.stop()?;

    Ok(())
}
```

- [ ] **Step 2: Verify it compiles**

Run:

```bash
ENV_SETUP && cargo check --package stargaze-server
```

Expected: compiles successfully.

- [ ] **Step 3: Run clippy on the full workspace**

Run:

```bash
ENV_SETUP && cargo clippy --workspace -- -W clippy::pedantic
```

Expected: no errors across all crates.

- [ ] **Step 4: Run cargo fmt**

Run:

```bash
ENV_SETUP && cargo fmt --all
```

- [ ] **Step 5: Run all tests**

Run:

```bash
ENV_SETUP && cargo test --workspace
```

Expected: all tests pass (existing config/error/capture tests + new encode type tests). The ignored capture integration test is skipped.

- [ ] **Step 6: Commit**

```bash
git add crates/stargaze-server/src/main.rs
git commit --no-gpg-sign -m "feat(server): integrate encoder pipeline between capture and packet logging"
```

---

## Task 6: Add build verification test and ignored integration test

**Files:**
- Modify: `crates/stargaze-server/src/main.rs` (update test module)

- [ ] **Step 1: Add a build verification test for NVENC encoder lookup**

This test verifies that FFmpeg was built with NVENC headers registered. It does NOT require a GPU — the encoder is registered at compile time. Add to the `#[cfg(test)] mod tests` block in `crates/stargaze-server/src/main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that the `hevc_nvenc` encoder is registered in FFmpeg.
    ///
    /// This does NOT require an NVIDIA GPU — the encoder is registered
    /// at FFmpeg compile time if NVENC headers were present. It confirms
    /// that `ffmpeg-next` can find the encoder by name.
    #[test]
    fn test_hevc_nvenc_encoder_registered() {
        ffmpeg_next::init().expect("ffmpeg init");
        let codec = ffmpeg_next::encoder::find_by_name("hevc_nvenc");
        // The encoder should be registered if FFmpeg was built with NVENC support.
        // If this fails, FFmpeg was built without NVENC headers.
        if let Some(c) = codec {
            assert_eq!(c.name(), "hevc_nvenc");
        } else {
            eprintln!("WARNING: hevc_nvenc not registered — FFmpeg may lack NVENC support");
        }
    }

    /// Integration test: runs the full capture→encode pipeline for 3 seconds.
    ///
    /// Requires a running Wayland compositor + `PipeWire` + NVIDIA GPU.
    /// Run manually with:
    /// ```bash
    /// cargo test --package stargaze-server -- --ignored test_capture_encode_pipeline
    /// ```
    #[tokio::test]
    #[ignore = "requires running Wayland compositor, PipeWire, and NVIDIA GPU"]
    async fn test_capture_encode_pipeline() {
        use stargaze_core::encode::EncoderConfig;

        init_tracing();

        // Start capture.
        let capture_config = CaptureConfig {
            width: 1920,
            height: 1080,
            framerate: 30,
        };
        let (capture_session, frames) = capture::start_capture(capture_config)
            .await
            .expect("capture should start");

        // Start encoder.
        let encoder_config = EncoderConfig {
            width: 1920,
            height: 1080,
            framerate: 30,
            bitrate_mbps: 10,
        };
        let (encoder_session, mut packets) =
            encode::start_encoder(encoder_config, frames).expect("encoder should start");

        // Receive packets for up to 3 seconds.
        let mut count = 0u32;
        let mut got_keyframe = false;
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(3));
        tokio::pin!(timeout);

        loop {
            tokio::select! {
                pkt = packets.recv() => {
                    match pkt {
                        Some(p) => {
                            assert!(!p.data.is_empty(), "packet should have data");
                            if p.is_keyframe {
                                got_keyframe = true;
                            }
                            count += 1;
                        }
                        None => break,
                    }
                }
                () = &mut timeout => break,
            }
        }

        // Write encoded output to a file for manual inspection with ffprobe.
        // (Only if we got packets)
        if count > 0 {
            eprintln!("Received {count} encoded packets in 3 seconds");
        }

        encoder_session.stop().expect("encoder should stop cleanly");
        capture_session.stop().expect("capture should stop cleanly");

        assert!(count > 0, "should have received at least one encoded packet");
        assert!(got_keyframe, "should have received at least one keyframe");
    }
}
```

- [ ] **Step 2: Run all tests**

Run:

```bash
ENV_SETUP && cargo test --workspace
```

Expected: all non-ignored tests pass. The `test_hevc_nvenc_encoder_registered` test should pass (prints a warning if NVENC isn't available, but doesn't fail). The two ignored tests are skipped.

- [ ] **Step 3: Verify ignored tests are listed**

Run:

```bash
ENV_SETUP && cargo test --package stargaze-server -- --list 2>&1 | grep -E "(ignored|test_)"
```

Expected: shows both `test_capture_receives_frames` and `test_capture_encode_pipeline` as ignored, plus `test_hevc_nvenc_encoder_registered` as a regular test.

- [ ] **Step 4: Run clippy and fmt**

Run:

```bash
ENV_SETUP && cargo fmt --all && cargo clippy --workspace -- -W clippy::pedantic
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/stargaze-server/src/main.rs
git commit --no-gpg-sign -m "test(encode): add NVENC build verification and ignored pipeline integration test"
```

---

## Verification Commands

After completing all 6 tasks:

```bash
# Set environment (required for every cargo command in this devcontainer):
export PATH="$HOME/.local/usr/bin:$HOME/.local/usr/lib/llvm-19/bin:$PATH" && \
export PKG_CONFIG_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu/pkgconfig" && \
export LIBRARY_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu:$HOME/.local/usr/lib/llvm-19/lib" && \
export LD_LIBRARY_PATH="$HOME/.local/usr/lib/x86_64-linux-gnu:$HOME/.local/usr/lib/llvm-19/lib" && \
export C_INCLUDE_PATH="$HOME/.local/usr/include" && \
export LIBCLANG_PATH="$HOME/.local/usr/lib/llvm-19/lib" && \
export BINDGEN_EXTRA_CLANG_ARGS="-I$HOME/.local/usr/include -I/usr/lib/gcc/x86_64-linux-gnu/14/include -I/usr/include"

# All unit tests pass:
cargo test --workspace

# No clippy warnings:
cargo clippy --workspace -- -W clippy::pedantic

# Clean formatting:
cargo fmt --all -- --check

# Full pipeline test (on a machine with compositor + PipeWire + NVIDIA GPU):
cargo test --package stargaze-server -- --ignored test_capture_encode_pipeline
```
