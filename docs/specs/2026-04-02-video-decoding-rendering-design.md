# Video Decoding + Rendering Design — Sub-project 5

**Date:** 2026-04-02
**Status:** Draft
**Sub-project:** 5 of 9 (Video Decoding + Rendering — Client)

## Overview

Decode H.265 (HEVC) video frames received from the transport layer and render them to a Wayland window with minimal latency. This sub-project takes `ReassembledFrame` values from the client transport (Sub-project 4) and displays them as a live video stream on the client machine.

## Approach

**FFmpeg software decode via `ffmpeg-next` + SDL2 rendering via `sdl2`.**

The `ffmpeg-next` crate provides the H.265 decoder (`hevc` / libavcodec software decoder). SDL2 provides window creation and hardware-accelerated YUV texture rendering with Wayland support. This mirrors the server's use of `ffmpeg-next` for encoding, keeping the FFmpeg dependency consistent across both binaries.

**Why this approach:**

- **Software decode (`hevc`) for MVP:** The target client (AMD CPU, no discrete GPU) may or may not have VAAPI-capable integrated graphics. Software decode is universally available and avoids hardware discovery complexity. VAAPI can be added later as a transparent optimization.
- **vs. `libde265` / `openh265`:** These are standalone H.265 decoders but lack the ecosystem maturity and performance of FFmpeg's decoder. Using `ffmpeg-next` keeps one FFmpeg dependency across the whole project.
- **SDL2 for rendering:** SDL2 handles Wayland window creation, vsync, and hardware-accelerated YUV→RGB texture upload in one library. This avoids the complexity of separate winit + wgpu/OpenGL setup for the MVP.
- **vs. `winit` + `wgpu`:** More control but significantly more code (shader pipeline, texture management, format conversion). Better suited for a future optimization pass.
- **vs. GStreamer:** Adds a massive dependency for something we can do with FFmpeg directly.

**Dedicated decoder thread.** FFmpeg decode is synchronous and CPU-intensive. A dedicated `std::thread` (matching the encoder pattern) receives `ReassembledFrame` values from the transport channel and sends decoded frames to the renderer. This keeps the tokio runtime unblocked.

**SDL2 renderer on main thread.** SDL2 requires the event loop on the main thread (Wayland/X11 constraint). The renderer receives decoded frames from the decoder thread via a channel and presents them in the SDL2 event loop.

## Module Structure

```
crates/stargaze-core/src/
├── lib.rs           # add `pub mod decode;`
├── decode.rs        # NEW — DecodedFrame, DecoderConfig, DecodeError
├── encode.rs        # existing (no changes)
├── transport.rs     # existing (no changes)
├── capture.rs       # existing (no changes)
├── config.rs        # existing (no changes)
└── error.rs         # existing (no changes)

crates/stargaze-client/src/
├── main.rs          # modified — wire decoder+renderer after transport
├── lib.rs           # modified — add pub mod decode; pub mod render;
├── transport/       # existing (no changes)
├── decode/
│   ├── mod.rs       # public API: DecoderSession, start_decoder()
│   └── ffmpeg.rs    # FFmpeg H.265 software decode init + decode loop
└── render/
    ├── mod.rs       # public API: start_renderer()
    └── sdl.rs       # SDL2 window creation, texture management, present loop
```

**Rationale:** Same pattern as server's capture/ and encode/ — shared types in core, implementation split into `mod.rs` (public API) and backend-specific file. Splitting decode and render allows swapping either independently (e.g., VAAPI decoder, wgpu renderer).

## Core Types

### `stargaze-core::decode`

```rust
/// A decoded video frame ready for rendering.
pub struct DecodedFrame {
    /// NV12 pixel data: Y plane followed by interleaved UV plane.
    /// Y plane: width × height bytes.
    /// UV plane: width × (height / 2) bytes (interleaved U, V).
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Presentation timestamp (matches the encoded frame's PTS).
    pub pts: u64,
}

/// Configuration for the video decoder.
pub struct DecoderConfig {
    /// Expected frame width in pixels.
    pub width: u32,
    /// Expected frame height in pixels.
    pub height: u32,
    /// Codec to decode (H265 for MVP).
    pub codec: Codec,
}

/// Errors from the video decoding subsystem.
#[derive(Error, Debug)]
pub enum DecodeError {
    #[error("FFmpeg error: {0}")]
    FfmpegError(String),
    #[error("Decoder initialization failed: {0}")]
    InitError(String),
    #[error("Decoding failed for frame at PTS {pts}: {reason}")]
    DecodeFrameError { pts: u64, reason: String },
    #[error("Unsupported codec: {0}")]
    UnsupportedCodec(String),
}
```

**Design notes:**

- `DecodedFrame` uses NV12 format — this is what FFmpeg's H.265 decoder outputs natively, and SDL2's `YV12`/`IYUV`/`NV12` texture formats accept it directly. No pixel format conversion needed.
- `DecodedFrame::data` stores Y and UV planes contiguously. This is a single allocation containing `width * height * 3/2` bytes. The Y plane occupies `[0..width*height]`, the UV plane occupies `[width*height..width*height*3/2]`.
- `DecoderConfig` is constructed from the `SessionResponse` parameters received during transport handshake.
- `DecodeError` follows the same `String`-payload pattern as `EncodeError` and `CaptureError`.

## Data Flow & Threading Model

```
┌──────────────────────────────────────────────────────────────┐
│                      Client main()                            │
│                     (tokio runtime)                            │
│                                                               │
│   1. transport::connect(config) → (ClientTransport, Rx<RF>)   │
│   2. start_decoder(config, rf_rx) → (DecoderSession, Rx<DF>)  │
│   3. start_renderer(config, df_rx) — takes over main thread    │
└────────────────┬─────────────────────┬──────────────────────┘
                 │                     │
                 ▼                     ▼
┌───────────────────────┐    ┌──────────────────────────┐
│  Transport task        │ RF │  Decoder thread           │
│  (tokio task)          │───→│  (std::thread)            │
│                        │    │                            │
│  - QUIC datagrams      │    │  - FFmpeg H.265 decode     │
│  - FrameAssembler      │    │  - SW decode (no HW ctx)   │
│  - IDR request send    │    │  - NV12 frame output       │
│                        │    │  - blocking_recv on Rx<RF>  │
└────────────────────────┘    │  - blocking_send on Tx<DF>  │
                              └──────────┬─────────────────┘
                                         │ DF
                                         ▼
                              ┌──────────────────────────┐
                              │  Renderer (main thread)    │
                              │                            │
                              │  - SDL2 window + event loop│
                              │  - YUV texture update      │
                              │  - try_recv decoded frames  │
                              │  - vsync presentation       │
                              └────────────────────────────┘

RF = ReassembledFrame (tokio mpsc, capacity 16)
DF = DecodedFrame (std mpsc, unbounded → backpressure via SDL vsync)
```

**Channel design:**

- **Transport → Decoder:** `tokio::sync::mpsc::Receiver<ReassembledFrame>` (same channel from `transport::connect()`, capacity 16). The decoder thread calls `blocking_recv()` to bridge tokio→std.
- **Decoder → Renderer:** `std::sync::mpsc::Receiver<DecodedFrame>` (standard library channel). The renderer calls `try_recv()` non-blocking in its SDL event loop — if no frame is ready, it re-presents the last frame. This keeps the event loop responsive to window events (resize, close, input).

**Why `std::sync::mpsc` for decoder→renderer:**

The renderer runs on the main thread outside the tokio runtime (SDL2's event loop is blocking and not async-compatible). Using `std::sync::mpsc` avoids pulling tokio into the renderer and lets it do non-blocking `try_recv()` in the event loop.

**Frame dropping strategy:**

If the decoder produces frames faster than the renderer consumes them (unlikely with vsync), the decoder→renderer channel will buffer. The renderer always renders the *latest* available frame by draining the channel with `try_recv()` in a loop and keeping only the last frame. This prevents accumulating latency.

## Decoder Implementation

### Initialization

1. Find the `hevc` decoder via `ffmpeg_next::decoder::find(codec::Id::H265)`
2. Create a codec context and open it — no hardware device context needed for software decode
3. The decoder will auto-detect the input pixel format from the bitstream (SPS/PPS NAL units in the first keyframe)

### Decode Loop

```
loop {
    frame = blocking_recv(rf_rx)    // ReassembledFrame from transport
    packet = AVPacket from frame.data (Annex-B H.265)
    packet.set_pts(frame.pts)
    decoder.send_packet(&packet)
    while decoder.receive_frame(&mut video_frame) == Ok {
        convert frame to NV12 if needed (scaler)
        send DecodedFrame to renderer
    }
}
// On channel close: send EOF, drain remaining frames
```

**Input format:** `ReassembledFrame.data` contains H.265 NAL units with Annex-B start codes (0x00000001 prefix). FFmpeg's H.265 parser/decoder accepts this directly — no conversion to AVCC format needed.

**PTS mapping:** `ReassembledFrame.pts` is a monotonically increasing frame counter (u64). Map it directly to the AVPacket PTS. The decoder preserves PTS through to the output frame.

**Error handling:** Decode errors for individual frames are logged and skipped — the stream continues. The transport layer's IDR request mechanism will recover from corruption. Fatal errors (decoder crash, OOM) propagate up and stop the session.

## Renderer Implementation

### Initialization

1. `SDL_Init(VIDEO)` — initialize SDL2 video subsystem
2. Create window at session resolution (or fullscreen if configured)
3. Create SDL renderer (hardware-accelerated, vsync)
4. Create YUV texture (`SDL_PIXELFORMAT_NV12`, streaming access)

### Event Loop

```
loop {
    poll SDL events (window close, key events, etc.)
    drain decoded frames channel (try_recv loop, keep latest)
    if new frame available:
        update YUV texture with frame.data
        copy texture to renderer
        present
    else:
        sleep briefly or rely on vsync to throttle
}
```

**Wayland compatibility:** SDL2 handles Wayland natively via its Wayland video driver. Set `SDL_VIDEODRIVER=wayland` environment variable to force Wayland (otherwise SDL2 may fall back to XWayland).

**Texture format:** `SDL_PIXELFORMAT_NV12` matches the decoder output directly. SDL2 uploads this to the GPU and converts to RGB in hardware.

## Future Optimizations (Not MVP)

- **VAAPI hardware decode:** Add `set_hw_device_ctx()` for VAAPI on AMD APUs. Zero code change in the renderer — just different pixel data source.
- **DMA-BUF zero-copy:** Decoder outputs VAAPI surfaces → directly import into Wayland buffer via `linux-dmabuf-unstable-v1` protocol. Skips CPU entirely.
- **winit + wgpu renderer:** Custom shader pipeline for pixel format conversion, lower latency than SDL2's built-in renderer.
- **Frame pacing:** PTS-based presentation timing to smooth out decode jitter.
- **Adaptive quality:** Signal the server to reduce bitrate/resolution when the decoder can't keep up.

## Dependencies

### New crate dependencies

| Crate | `stargaze-core` | `stargaze-client` |
|---|---|---|
| `ffmpeg-next` 7 | — | yes |
| `ffmpeg-sys-next` 7 | — | yes (for raw AVPacket/frame access if needed) |
| `sdl2` 0.37 | — | yes |
| `libc` 0.2 | — | yes (EAGAIN constant) |

### New system packages (Dockerfile)

- `libsdl2-dev` — SDL2 development headers and libraries

### Build notes

The client crate needs the same FFmpeg link fixup as the server. A `build.rs` script identical to the server's will be added to query `pkg-config --libs` for transitive FFmpeg dependencies.

## Testing Strategy

### Unit tests

- **Decoder input handling:** Feed synthetic H.265 Annex-B data (minimal valid SPS/PPS/IDR) to the decoder and verify it produces a frame with correct dimensions.
- **DecodedFrame construction:** Verify NV12 data layout (Y plane size, UV plane size).
- **DecodeError display:** Verify error message formatting (matching existing pattern).

### Integration tests

- **Decode loop:** Create a decoder with a mock channel, send real H.265 packets (generated by encoding a synthetic test pattern), verify decoded frames have correct PTS and dimensions.

### Ignored tests (require hardware/display)

- **Full pipeline:** Server encodes test pattern → QUIC transport → client decodes and renders. Requires GPU (encoder), Wayland compositor (renderer), and PipeWire (capture).
- **SDL2 rendering:** Window creation and texture upload require a display server.

## Success Criteria

- `cargo build --workspace` succeeds with the new decoder and renderer modules
- `cargo test --workspace` passes all non-ignored tests (existing 47 + new decode tests)
- `cargo clippy --workspace -- -W clippy::pedantic` is clean
- `cargo fmt --check` is clean
- Client binary starts, connects to server, decodes frames, and renders to a window (manual test with actual server)
