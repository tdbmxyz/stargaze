# Audio Decoding + Playback — Design Spec

**Sub-project:** 7 of 9 (Audio Decoding + Playback — Client)

## Overview

Receive Opus-encoded audio packets from the server via QUIC unreliable datagrams, decode them to f32 PCM, and play them through SDL2's audio queue. This is the client-side counterpart to Sub-project 6 (Audio Capture + Encoding on the server).

The client transport currently sends all reassembled frames through a single channel regardless of stream type. This sub-project also fixes a critical bug in the `FrameAssembler` where audio and video frame indices can collide, and adds proper stream demuxing so video and audio frames reach their respective decoders.

## Target Environment

- **Decoder**: Opus via `libopus` (the `opus` Rust crate) — same library as the server encoder
- **Playback**: SDL2 `AudioQueue<f32>` — SDL2 is already a dependency for video rendering
- **Format**: 48 kHz, stereo, f32 PCM output from Opus decoder
- **Frame size**: 10ms (480 samples per channel, 960 total stereo samples per decode call)
- **Latency target**: <20ms decode-to-playback

## Architecture

### Data Flow

```
QUIC unreliable datagrams
    ↓
FrameAssembler (keyed by (stream_type, frame_index) — fixed)
    ├─ video_rx (tokio::sync::mpsc, capacity 16)
    │   ↓ ReassembledFrame { stream_type: 0 }
    │   FFmpeg H.265 decoder thread (existing)
    │   ↓ DecodedFrame (NV12)
    │   SDL2 video renderer (main thread, existing)
    │
    └─ audio_rx (tokio::sync::mpsc, capacity 16)
        ↓ ReassembledFrame { stream_type: 1 }
        Opus decoder thread (new, dedicated std::thread)
        ↓ f32 PCM (interleaved stereo)
        SDL2 AudioQueue<f32> (thread-safe, queue from decoder thread)
```

### Module Layout

```
crates/stargaze-core/src/
    audio.rs              # MODIFIED: add AudioDecoderConfig, DecoderInit/DecodeFailed errors

crates/stargaze-client/src/
    transport/
        receiver.rs       # MODIFIED: fix FrameAssembler keying, demux in receive_loop
        mod.rs            # MODIFIED: connect() returns separate video_rx + audio_rx
    decode/
        mod.rs            # MODIFIED: add AudioDecoderSession, start_audio_decoder()
        ffmpeg.rs         # UNCHANGED
        opus_dec.rs       # NEW: Opus decoder thread, init + decode loop
    render/
        mod.rs            # MODIFIED: add start_audio_renderer() public API
        sdl.rs            # UNCHANGED
        audio.rs          # NEW: SDL2 AudioQueue playback
    main.rs               # MODIFIED: wire audio pipeline
```

## Design Decisions

### 1. FrameAssembler Bug Fix — Per-Stream Keying

**Problem**: `FrameAssembler.pending` is `HashMap<u32, PendingFrame>` keyed by `frame_index` alone. Since video and audio each maintain independent `frame_index` counters starting at 0 on the server, audio fragments for frame 0 will collide with video fragments for frame 0, corrupting reassembly.

**Fix**: Change the key to `(u8, u32)` — `(stream_type, frame_index)`. Also change `next_frame: u32` to `next_frame: HashMap<u8, u32>` so in-order delivery tracking is per-stream. IDR request logic only applies to video frames.

### 2. Transport Demux at `connect()` Level

Change `connect()` to return two receivers: `video_rx` and `audio_rx`. Inside `receive_loop()`, route completed `ReassembledFrame`s to the correct sender based on `frame.stream_type`. This keeps the demux logic in the transport layer where it belongs, and downstream decoders receive only their stream type.

IDR requests are only triggered by video frame loss, not audio. Audio has no keyframes and the decoder can resync at any packet boundary.

### 3. SDL2 AudioQueue (not PipeWire/cpal)

SDL2 is already a dependency for video rendering. `AudioQueue<f32>` provides a simple queue-based API ideal for streaming:
- `queue.queue_audio(&pcm_slice)` is thread-safe (can be called from the decoder thread)
- No callback complexity — just push decoded PCM samples
- SDL2 audio and video coexist fine as separate subsystems from the same `sdl2::init()` context

Alternative considered: PipeWire via `cpal`. Would add a dependency and complexity. SDL2 audio is simpler and already linked.

### 4. Opus via `opus` Crate (mirroring server)

Same crate as the server-side encoder. `decoder.decode_float()` outputs f32 PCM directly — no format conversion needed for SDL2 AudioQueue.

### 5. Dedicated Decoder Thread

Same pattern as the video decoder: a dedicated `std::thread` running a blocking decode loop. The Opus `Decoder` is `Send` but not `Sync`, so confining it to a single thread is the natural approach.

The decoded f32 PCM is pushed directly to `SDL2::AudioQueue::queue_audio()` from the decoder thread (it's thread-safe). No intermediate channel is needed between decoder and audio playback.

### 6. Audio Playback Configuration

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| Sample rate | 48000 Hz | Matches Opus encoder output |
| Channels | 2 (stereo) | Matches encoder configuration |
| Format | f32 | Opus decode_float output, SDL2 AudioQueue<f32> |
| Buffer samples | 512 | ~10.7ms latency, balances smoothness vs latency |

### 7. Channel Capacities

- Transport → audio decoder: capacity 16 (same as video, consistent with existing pattern)
- No decoder → playback channel needed (AudioQueue is called directly from decoder thread)

### 8. No Packet Loss Concealment (MVP)

Opus supports PLC (pass empty input to `decode_float` to generate concealment audio), but for MVP we skip lost packets silently. The brief gap is acceptable for LAN streaming. PLC can be added later by tracking expected PTS and detecting gaps.

## Shared Types (stargaze-core additions)

```rust
/// Configuration for the Opus audio decoder.
pub struct AudioDecoderConfig {
    /// Sample rate in Hz (must be 48000 for Opus).
    pub sample_rate: u32,
    /// Number of channels (1 or 2).
    pub channels: u16,
}

// Additional AudioError variants:
pub enum AudioError {
    // ... existing variants ...
    DecoderInit(String),
    DecodeFailed(String),
}
```

## Opus Decoding

### Decoder Initialization

```rust
opus::Decoder::new(48000, opus::Channels::Stereo)
```

### Decode Loop

1. `blocking_recv()` `ReassembledFrame` from audio transport channel
2. `decoder.decode_float(&frame.data, &mut output_buf, false)` → samples per channel
3. `audio_queue.queue_audio(&output_buf[..samples * channels])` — push to SDL2
4. Check shutdown flag

### Error Handling

- Decode errors on individual packets: warn and skip (Opus can resync at next packet)
- Queue errors: fatal, stop decoder thread
- Channel closed: clean shutdown

## SDL2 Audio Playback

### Initialization

The SDL2 context (`sdl2::init()`) is created on the main thread for the video renderer. The audio subsystem must be initialized from the same SDL context. The `AudioQueue<f32>` handle is then passed to the decoder thread (it's `Send`).

Initialization sequence:
1. Main thread: `sdl2::init()` → get `audio_subsystem`
2. Main thread: `audio_subsystem.open_queue::<f32, _>(None, &desired_spec)` → `AudioQueue<f32>`
3. Main thread: `queue.resume()` — start playback
4. Pass `AudioQueue` to the audio decoder thread

### Buffer Monitoring

`AudioQueue::size()` returns queued bytes. For flow control logging, track queue depth and warn if it grows beyond ~100ms (to detect backpressure).

## Transport Changes

### `FrameAssembler` (receiver.rs)

```rust
// Before (buggy):
pending: HashMap<u32, PendingFrame>,
next_frame: u32,

// After (fixed):
pending: HashMap<(u8, u32), PendingFrame>,
next_frame: HashMap<u8, u32>,
```

All methods updated to use `(stream_type, frame_index)` as the composite key. `deliver_in_order()` operates per-stream. IDR check only triggers for video-stream pending frame excess.

### `receive_loop()` (receiver.rs)

Takes two senders (`video_tx`, `audio_tx`) instead of one. Routes completed frames by `stream_type`:
- `STREAM_TYPE_VIDEO` → `video_tx.send(frame)`
- `STREAM_TYPE_AUDIO` → `audio_tx.send(frame)`
- Unknown → warn and drop

IDR request logic unchanged (only triggered by video frame loss).

### `connect()` (mod.rs)

Returns `(ClientTransport, mpsc::Receiver<ReassembledFrame>, mpsc::Receiver<ReassembledFrame>)` — the second receiver is for video, third for audio.

## Testing Strategy

### Unit Tests

1. **FrameAssembler mixed streams** — interleave video and audio fragments with same `frame_index`, verify no collision and correct reassembly of both
2. **FrameAssembler per-stream ordering** — verify `deliver_in_order()` tracks each stream independently
3. **Opus decoder init** — verify decoder initializes with valid config (48kHz stereo)
4. **Opus decoder rejects invalid config** — verify error on unsupported channel count
5. **Opus decode round-trip** — encode silence with server's encoder, decode with client's decoder, verify output is f32 PCM of expected length
6. **AudioDecoderConfig construction** — verify type works correctly
7. **AudioError decoder variants** — verify Display formatting

### Integration Tests (ignored — need audio device)

1. **Audio playback** — decode and play a short Opus stream through SDL2 AudioQueue

## Non-Goals

- Packet loss concealment (PLC) — skip for MVP
- Audio format negotiation — fixed at 48kHz stereo Opus
- Volume control or mixing
- Multiple audio streams or surround sound
- Audio sync with video (clock sync is a future sub-project concern)
