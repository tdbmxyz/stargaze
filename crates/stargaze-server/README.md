# stargaze-server

The [Stargaze](../../README.md) streaming server: captures the Wayland desktop and audio, encodes them, streams to a client over QUIC, and injects the client's input events into the host.

## Pipeline

```
xdg-desktop-portal (ScreenCast) ──► PipeWire stream ──► encode thread ──► QUIC datagrams
                                    DMA-BUF or MemFd     NVENC H.265
PipeWire audio capture ───────────► Opus encoder ──────► QUIC datagrams
client control stream ────────────► IDR requests ──────► encoder (forced keyframe)
                                  └► input events ─────► uinput virtual devices
```

## Modules

| Module | Responsibility |
|---|---|
| `capture` | ScreenCast session via `ashpd` (portal) + PipeWire stream; negotiates DMA-BUF with modifiers, falls back to CPU buffers (MemFd buffers are mmap'd manually) |
| `encode` | FFmpeg `hevc_nvenc` low-latency encode. DMA-BUF frames are imported zero-copy via `egl_cuda` (EGL image → `GL_TEXTURE_EXTERNAL_OES` → CUDA-GL interop → CUDA device memory); CPU frames are uploaded through a software frame. Opus audio encoding with frame buffering. |
| `encode/egl_cuda` | Headless EGL device context for the DMA-BUF → CUDA bridge (NVIDIA's GL driver cannot bind linear DMA-BUFs to `GL_TEXTURE_2D`, hence the external-OES texture path) |
| `audio` | PipeWire audio capture node, sample buffering for the Opus encoder |
| `input` | Virtual keyboard/mouse/gamepad devices via `evdev`/uinput; maps `InputEvent`s from the wire |
| `transport` | QUIC endpoint (self-signed cert via `rcgen`), session handshake, frame fragmentation into datagrams, control-stream handling (IDR requests are forwarded to the encoder through a `watch` channel) |

Encoded keyframes are sent with `extradata` (VPS/SPS/PPS) prepended, so a client can join or recover mid-stream from any keyframe. The GOP is 2 s; recovery between GOPs relies on client IDR requests, which the encoder honors by forcing the next frame to `AV_PICTURE_TYPE_I`.

## Requirements

- Wayland compositor with a working ScreenCast portal (tested with Hyprland)
- PipeWire ≥ 0.3.33
- NVIDIA GPU + proprietary driver, CUDA runtime, FFmpeg with `hevc_nvenc`
- `/dev/uinput` access for input injection (typically the `input` group or a udev rule)

## Running

```bash
stargaze-server                          # defaults: 0.0.0.0:9000, 1920x1080@60, 20 Mbps, h265
stargaze-server --resolution 2560x1440 --framerate 120 --bitrate 40
stargaze-server --config /path/to/server.toml
```

Configuration file: `~/.config/stargaze/server.toml` (CLI flags override file values; see the [root README](../../README.md#usage) for the format).

## Tests

```bash
cargo test -p stargaze-server
```

GPU- and uinput-dependent tests are `#[ignore]`d; run them manually on real hardware, e.g.:

```bash
cargo test -p stargaze-server -- --ignored test_nvenc_encode_synthetic_frames
```
