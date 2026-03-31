# Video Encoding Design — Sub-project 3

**Date:** 2026-03-31
**Status:** Draft
**Sub-project:** 3 of 9 (Video Encoding — Server)

## Overview

Encode captured video frames using NVIDIA NVENC hardware encoding via FFmpeg, producing H.265 (HEVC) encoded packets for network transport. This sub-project takes `Frame` values from the capture pipeline (Sub-project 2) and outputs `EncodedPacket` values ready for the transport layer (Sub-project 4).

## Approach

**FFmpeg `hevc_nvenc` via `ffmpeg-next` + `ffmpeg-sys-next`.** The high-level `ffmpeg-next` crate handles codec lookup, context creation, and the send/receive encode loop. The raw `ffmpeg-sys-next` crate provides FFI access to `AVHWDeviceContext` and `AVHWFramesContext` for CUDA hardware frame setup.

**Why this approach:**

- **vs. Direct NVENC SDK (`nvidia-video-codec-sdk`):** Requires CUDA toolkit at build time, raw bindgen output, must reimplement buffer management / NV12 conversion / NAL packaging that FFmpeg already handles. Much more code, can't compile without CUDA.
- **vs. `rsmpeg`:** Viable alternative but 20x fewer downloads than `ffmpeg-next`, smaller community. The advantage (tighter HW frame wrapping) doesn't justify the ecosystem trade-off for the MVP.

**CPU upload path for MVP.** Both `Frame::DmaBuf` and `Frame::CpuMapped` frames are uploaded to the GPU via `av_hwframe_transfer_data()`. For DMA-BUF frames this means a GPU→CPU→GPU round-trip (~2-3ms at 1080p) — acceptable for the MVP. Zero-copy DMA-BUF→CUDA import via EGL/GL interop (as Sunshine does) is a future optimization.

## Module Structure

```
crates/stargaze-core/src/
├── lib.rs           # add `pub mod encode;`
├── encode.rs        # NEW — EncodedPacket, EncoderConfig, EncodeError
├── capture.rs       # existing (no changes)
├── config.rs        # existing (no changes)
└── error.rs         # existing (no changes)

crates/stargaze-server/src/
├── main.rs          # modified — wire encoder between capture and (future) transport
├── capture/         # existing (no changes)
└── encode/
    ├── mod.rs       # public API: EncoderSession, start_encoder()
    └── ffmpeg.rs    # FFmpeg NVENC init, hw device/frame context, encode loop
```

**Rationale:** Same pattern as capture — shared types in core, implementation in server. Splitting `mod.rs` (public API) from `ffmpeg.rs` (FFmpeg internals) isolates the FFmpeg FFI. If the encoder backend is ever swapped, only `ffmpeg.rs` changes.

## Core Types

All in `stargaze-core::encode`:

```rust
/// An encoded video packet (one or more NAL units from a single frame).
pub struct EncodedPacket {
    pub data: Vec<u8>,       // H.265 NAL units with Annex B start codes
    pub pts: u64,            // Presentation timestamp (frame number)
    pub is_keyframe: bool,   // True for IDR frames
}

/// Configuration for the video encoder.
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_mbps: u32,   // Target bitrate in Mbps
}

/// Errors from the video encoding subsystem.
#[derive(Error, Debug)]
pub enum EncodeError {
    #[error("FFmpeg error: {0}")]
    FfmpegError(String),
    #[error("Encoder initialization failed: {0}")]
    InitError(String),
    #[error("Encoding failed for frame {frame}: {reason}")]
    EncodeFrameError { frame: u64, reason: String },
    #[error("Unsupported pixel format: {0}")]
    UnsupportedFormat(String),
}
```

**Design notes:**

- `EncodedPacket` owns its data (`Vec<u8>`). Packets are typically 10-100 KB — allocation cost is negligible.
- `EncodedPacket::data` contains raw H.265 NAL units with Annex B start codes (0x00000001 prefix). This is what FFmpeg's `hevc_nvenc` produces by default and what the transport layer will packetize.
- `EncoderConfig` is constructed from `ServerConfig` fields but kept separate so the encoder doesn't depend on the full config system.
- `EncodeError` uses `String` payloads rather than wrapping FFmpeg error types — same pattern as `CaptureError`, avoids leaking library types into the core crate's public API.

## Data Flow & Threading Model

```
┌──────────────────────────────────────────────────────────┐
│                      Server main()                        │
│                     (tokio runtime)                        │
│                                                           │
│   1. start_capture(config) → (CaptureSession, Rx<Frame>)  │
│   2. start_encoder(config, frame_rx)                       │
│        → (EncoderSession, Rx<EncodedPacket>)               │
│   3. Receive encoded packets from Rx<EncodedPacket>        │
│   4. (Later: send packets over the network)                │
└──────────────┬──────────────────────┬────────────────────┘
               │                      │
               ▼                      ▼
┌──────────────────┐        ┌──────────────────────┐
│  PipeWire thread  │ Frame  │  Encoder thread       │
│  (existing)       │─(mpsc)→│  (std::thread)        │
│                   │ cap=2  │                       │
└──────────────────┘        │  1. Init FFmpeg CUDA   │
                             │     hw device + frames │
                             │  2. Loop:              │
                             │     recv Frame         │
                             │     upload to GPU      │
                             │     convert BGRA→NV12  │
                             │     send to encoder    │
                             │     recv packet        │
                             │     send on channel    │
                             │  3. On shutdown:       │
                             │     flush encoder      │
                             │     exit               │
                             └──────────┬─────────────┘
                                        │ EncodedPacket
                                        │ (mpsc, cap=4)
                                        ▼
                             ┌──────────────────────┐
                             │  main() recv loop     │
                             │  (later: network)     │
                             └──────────────────────┘
```

**Why a dedicated std::thread?** FFmpeg's encode loop is synchronous and blocking. `avcodec_send_frame()` + `avcodec_receive_packet()` can block for the duration of a frame encode (~1-5ms on NVENC). Running this on tokio would starve other tasks.

**Channel design:**

- **Input:** The encoder thread takes ownership of the `mpsc::Receiver<Frame>` from capture. It calls `blocking_recv()` to get frames.
- **Output:** A new `tokio::sync::mpsc::channel<EncodedPacket>` (capacity 4) bridges encoded packets back to the tokio world. Capacity 4 gives a small buffer without excessive latency.
- **Backpressure:** If the consumer can't keep up, the encoder thread blocks on `blocking_send()`, which backs up into the capture channel, naturally throttling the whole pipeline.

**Frame upload path (MVP):** For `Frame::CpuMapped`, the pixel data is already in CPU memory — it is copied into a software `AVFrame` and uploaded to the GPU via `av_hwframe_transfer_data()`. For `Frame::DmaBuf`, the encoder `mmap()`s the DMA-BUF fd to get CPU-accessible pixels, copies them into the software `AVFrame`, then uploads the same way. This GPU→CPU→GPU round-trip adds ~2-3ms at 1080p — acceptable for the MVP. Zero-copy DMA-BUF→CUDA import (via EGL/GL interop, as Sunshine does) is a future optimization.

**Color space conversion:** FFmpeg handles BGRA→NV12 conversion internally when uploading via `av_hwframe_transfer_data()` with `sw_format = AV_PIX_FMT_NV12`.

**Encoder session lifecycle:** Mirrors `CaptureSession` — owns the thread handle and a shutdown flag. `stop()` signals shutdown, the encoder thread flushes remaining packets (sends null frame to FFmpeg to drain), then exits. Drop impl signals shutdown if `stop()` wasn't called.

## FFmpeg NVENC Configuration

The encoder initialization follows Sunshine's proven pattern for ultra-low-latency streaming.

**Encoder selection:** `avcodec_find_encoder_by_name("hevc_nvenc")`.

**Hardware context setup (via `ffmpeg-sys-next` FFI):**

1. Create CUDA device context: `av_hwdevice_ctx_create(&buf, AV_HWDEVICE_TYPE_CUDA, null, null, 0)`
2. Allocate HW frames context: `av_hwframe_ctx_alloc(device_ctx)`
3. Configure: `format = AV_PIX_FMT_CUDA`, `sw_format = AV_PIX_FMT_NV12`, width/height, `initial_pool_size = 0`
4. Init: `av_hwframe_ctx_init(frame_ref)`
5. Attach to codec context: `ctx.hw_frames_ctx = av_buffer_ref(frame_ref)`

**Codec context settings:**

| Setting | Value | Rationale |
|---|---|---|
| `width`, `height` | From `EncoderConfig` | Match capture resolution |
| `time_base` | `1/framerate` | Frame timing |
| `bit_rate` | `bitrate_mbps * 1_000_000` | Target bitrate |
| `pix_fmt` | `AV_PIX_FMT_CUDA` | Hardware pixel format |
| `max_b_frames` | `0` | B-frames add latency |
| `gop_size` | `framerate * 2` | Keyframe every ~2 seconds |
| `color_range` | `MPEG` | Limited range (standard for streaming) |
| `colorspace` | `BT709` | HD color matrix |
| `color_primaries` | `BT709` | HD primaries |
| `color_trc` | `BT709` | HD transfer characteristics |

**NVENC-specific options (via `av_opt_set`):**

| Option | Value | Rationale |
|---|---|---|
| `preset` | `p1` | Fastest NVENC preset |
| `tune` | `ull` | Ultra-low-latency tuning |
| `rc` | `cbr` | Constant bitrate — predictable bandwidth |
| `delay` | `0` | No output reordering delay |
| `forced-idr` | `1` | Allow forcing IDR frames on demand |
| `zerolatency` | `1` | Don't buffer frames |

**Encode loop:**

1. `blocking_recv()` a `Frame` from the capture channel
2. Allocate a software frame (`AVFrame`) with BGRA pixel data from the `Frame`
3. Get a hardware frame: `av_hwframe_get_buffer(hw_frames_ctx, hw_frame, 0)`
4. Upload: `av_hwframe_transfer_data(hw_frame, sw_frame, 0)` — handles BGRA→NV12 + CPU→GPU
5. Set `hw_frame.pts = frame_counter`
6. `avcodec_send_frame(ctx, hw_frame)`
7. Loop `avcodec_receive_packet(ctx, pkt)` until `EAGAIN` — for each packet, construct `EncodedPacket` and `blocking_send()` on the output channel

**Flush on shutdown:**

1. `avcodec_send_frame(ctx, null)` — signal end of stream
2. Drain remaining packets via `avcodec_receive_packet()` until `EOF`

## Public API

```rust
// crates/stargaze-server/src/encode/mod.rs

/// Handle to a running encoder session. Signals shutdown on drop.
pub struct EncoderSession { /* JoinHandle, shutdown signal */ }

impl EncoderSession {
    /// Gracefully stop encoding: signal shutdown, flush remaining
    /// packets, and wait for the encoder thread to exit.
    pub fn stop(self) -> Result<(), EncodeError>;
}

impl Drop for EncoderSession {
    // Signals shutdown if stop() wasn't called
}

/// Start the video encoder.
///
/// Takes ownership of the frame receiver from capture and returns
/// an encoder session handle plus a channel receiver for encoded packets.
/// FFmpeg initialization happens on the spawned thread; if it fails,
/// the error is returned via a oneshot channel.
pub fn start_encoder(
    config: EncoderConfig,
    frames: tokio::sync::mpsc::Receiver<Frame>,
) -> Result<(EncoderSession, tokio::sync::mpsc::Receiver<EncodedPacket>), EncodeError>;
```

**Server `main.rs` integration:**

```rust
let (capture_session, frames) = capture::start_capture(capture_config).await?;

let encoder_config = EncoderConfig {
    width: cfg.resolution.width,
    height: cfg.resolution.height,
    framerate: cfg.framerate,
    bitrate_mbps: cfg.bitrate,
};
let (encoder_session, mut packets) = encode::start_encoder(encoder_config, frames)?;

loop {
    tokio::select! {
        pkt = packets.recv() => {
            let Some(pkt) = pkt else { break; };
            // log / send over network
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down");
            break;
        }
    }
}

encoder_session.stop()?;
capture_session.stop()?;
```

## Error Handling

**Errors during initialization** (FFmpeg not found, NVENC not available, CUDA device creation failed) are returned from `start_encoder()` via `Result`. For the MVP, the server exits on init failure.

**Errors during encoding** (frame upload failure, encoder internal error) happen inside the encoder thread:

1. Log at `error!` level
2. Drop the `Sender<EncodedPacket>`, causing `packets.recv()` to return `None`
3. Exit the encoder thread

The caller interprets a closed channel as "encoder stopped" — same pattern as capture.

**Graceful shutdown:** When the capture channel closes (PipeWire thread exited or `CaptureSession` dropped), the encoder thread's `blocking_recv()` returns `None`. It flushes the encoder (null frame + drain), sends remaining packets, then exits cleanly.

## Testing Strategy

**Unit tests (run anywhere, `cargo test`):**

- `EncodedPacket` construction and field access
- `EncoderConfig` construction from `ServerConfig` values
- `EncodeError` display messages

**Build verification test (no GPU needed):**

- Verify FFmpeg encoder lookup: `avcodec_find_encoder_by_name("hevc_nvenc")` returns non-null. Confirms FFmpeg was compiled with NVENC headers. Does not require a GPU — the encoder is registered at compile time. Skipped (not failed) if NVENC is not available.

**Integration test (`#[ignore]`, requires NVIDIA GPU):**

- Feed synthetic BGRA frames (solid color buffers) into `start_encoder()` via an `mpsc` channel
- Verify encoded packets come out with `is_keyframe = true` for the first packet
- Write encoded output to a `.h265` file, verify with `ffprobe` that it's valid HEVC
- Run manually on a machine with an NVIDIA GPU

**What we do NOT test:**

- No mocking of FFmpeg — the FFI boundary is thin, mocking adds complexity without value
- No latency benchmarks — those come with the full pipeline
- No visual quality tests — bitstream validity is sufficient for the MVP

## Dependencies

**`stargaze-server/Cargo.toml` (new):**

- `ffmpeg-next = "7"` — safe FFmpeg bindings
- `ffmpeg-sys-next = "7"` — raw FFmpeg FFI for hardware context setup

**`stargaze-core/Cargo.toml`:** No new dependencies.

**System libraries (build-time):**

- `libavcodec-dev`, `libavutil-dev`, `libavformat-dev`, `libswscale-dev`, `libswresample-dev` — FFmpeg development headers
- `pkg-config` — for `ffmpeg-sys-next` to locate FFmpeg (already installed to `~/.local/usr/`)
- `clang` — already installed for PipeWire bindgen

**Runtime (target server only):**

- NVIDIA driver 535+ (provides `libnvidia-encode.so`, `libcuda.so`)
- FFmpeg 7.x runtime libs with NVENC compiled in

**Explicitly avoided:**

- No CUDA toolkit — FFmpeg dynamically loads `libnvidia-encode.so` at runtime
- No `nvidia-video-codec-sdk` crate — FFmpeg wraps NVENC for us
- No `image` crate — test frame generation uses raw byte arrays

## Future Optimizations (Not in MVP)

- **Zero-copy DMA-BUF→CUDA import:** Use EGL/GL interop to import DMA-BUF fds directly into CUDA memory, eliminating the CPU round-trip. Requires `libEGL`, `libGL`, and CUDA interop APIs.
- **AV1 encoding:** Change encoder name to `av1_nvenc`. Requires RTX 4000+ series GPU.
- **Adaptive bitrate:** Adjust bitrate based on network conditions (requires feedback from transport layer).
- **IDR frame requests:** Allow the client to request keyframes (requires control channel from Sub-project 4).
