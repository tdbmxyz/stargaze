# stargaze-client

The [Stargaze](../../README.md) streaming client: connects to a server over QUIC, decodes the video/audio streams, renders them with SDL2, and forwards keyboard, mouse, and controller input back to the server.

## Pipeline

```
QUIC datagrams ──► FrameAssembler ──► decode thread ──► render loop (main thread)
                   in-order delivery   FFmpeg H.265       SDL2 IYUV texture
                   gap → IDR request   (VAAPI or sw)      latest-frame-wins
QUIC datagrams ──► Opus decode thread ─────────────────► SDL2 audio queue
SDL2 events ─────► control stream ───────────────────────► server (input)
```

## Modules

| Module | Responsibility |
|---|---|
| `transport` | QUIC connection (self-signed certs accepted — LAN trust model), session handshake, `FrameAssembler`, receive loop, input/IDR sending on the control stream |
| `decode` | Dedicated decode threads. Video: FFmpeg HEVC with VAAPI hardware acceleration when available (probed via `avcodec_get_hw_config`, selected with a custom `get_format` callback), multi-threaded software decode otherwise. Audio: Opus → PCM. |
| `render` | SDL2 window/canvas/texture on the main thread; doubles as the input event pump and audio queue feeder |

## Latency and loss-recovery design

These are deliberate choices — see `AGENTS.md` before changing them:

- **In-order frame delivery**: the `FrameAssembler` reassembles datagram fragments and releases frames strictly in `frame_index` order. A missing frame is skipped only after two complete frames have accumulated past it (tolerates reordering); every skip sends a rate-limited `IdrRequest` so the picture heals in a few frames.
- **Backpressure drops at the receiver, not the decoder**: the transport → decoder channel holds 2 frames; when full, the newest frame is dropped *and an IDR is requested*. The decode loop itself never skips frames — skipping deltas corrupts every frame until the next keyframe.
- **Renderer keeps only the latest decoded frame** (always safe post-decode) and presents without vsync; it blocks on the decoded-frame channel with a ~2 ms timeout, keeping input polling responsive without busy-spinning.
- Hardware (VAAPI) frames are transferred to CPU (NV12) and converted to YUV420P planes for the SDL `IYUV` texture.

## Requirements

- PipeWire/Wayland desktop with SDL2
- FFmpeg with HEVC decode; VAAPI driver (`/dev/dri/renderD128`) for hardware decode, otherwise software decode is used automatically
- A game controller is optional; hotplug is supported

## Running

```bash
stargaze-client --server 192.168.1.10           # defaults: port 9000, fullscreen
stargaze-client --server 192.168.1.10 --fullscreen false
stargaze-client --config /path/to/client.toml
```

Esc or closing the window ends the session. The mouse is captured in relative mode while the window is focused.

Check which decoder engaged at startup (`RUST_LOG=info`): look for `H.265 VAAPI hardware decoder initialized` vs `H.265 multi-threaded software decoder initialized`, and `decoded_format=VAAPI` in the first-frame diagnostics.

## Tests

```bash
cargo test -p stargaze-client
```

Includes unit tests for the `FrameAssembler` (ordering, gap skipping, IDR triggering) and a localhost QUIC integration test exercising fragmentation → reassembly end-to-end.
