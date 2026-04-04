# Audio Capture + Encoding — Design Spec

**Sub-project:** 6 of 9 (Audio Capture + Encoding — Server)

## Overview

Capture desktop/system audio output via PipeWire monitor, encode it to Opus, and feed the encoded packets into the existing QUIC transport layer alongside video. The transport already defines `STREAM_TYPE_AUDIO = 1` and `DatagramHeader` supports multiple stream types — this sub-project produces the audio packets that use them.

## Target Environment

- **Audio server**: PipeWire (standard on modern Linux Wayland desktops)
- **Capture mode**: Sink monitor (system audio output, not microphone)
- **Codec**: Opus via `libopus` (the `opus` Rust crate)
- **Format**: 48 kHz, stereo, f32le PCM from PipeWire → Opus encoded packets
- **Frame size**: 10ms (480 samples per channel, 960 total stereo samples)
- **Bitrate**: 128 kbps (configurable)
- **Latency target**: <20ms capture-to-packet

## Architecture

### Data Flow

```
PipeWire sink monitor
    ↓ f32le PCM (48kHz stereo, 10ms frames)
AudioCaptureSession (dedicated thread, PipeWire main loop)
    ↓ tokio::sync::mpsc (capacity 8)
    ↓ AudioFrame { data: Vec<f32>, sample_rate, channels, pts }
AudioEncoderSession (dedicated thread)
    ↓ Opus encode (10ms frames → ~40-160 byte packets)
    ↓ tokio::sync::mpsc (capacity 4)
    ↓ EncodedPacket { data, pts, is_keyframe: false }
Transport sender (existing, parameterized with STREAM_TYPE_AUDIO)
    ↓ QUIC unreliable datagrams
```

### Module Layout

```
crates/stargaze-core/src/
    audio.rs              # NEW: AudioFrame, AudioCaptureConfig, AudioEncoderConfig, AudioError

crates/stargaze-server/src/
    audio/
        mod.rs            # NEW: AudioCaptureSession, start_audio_capture()
        pipewire.rs       # NEW: PipeWire monitor capture thread
    encode/
        mod.rs            # MODIFIED: add pub(crate) mod opus;
        opus.rs           # NEW: Opus encoder thread, init + encode loop
    transport/
        sender.rs         # MODIFIED: send_packets() takes stream_type parameter
        mod.rs            # MODIFIED: spawn two send tasks (video + audio)
    main.rs               # MODIFIED: start audio pipeline, pass to transport
```

## Design Decisions

### 1. PipeWire Direct (not cpal)

PipeWire is already a dependency for video capture. Using the `pipewire` crate directly (not cpal) gives us:
- Access to `STREAM_CAPTURE_SINK` property for monitor audio (system output)
- Same threading pattern as video capture (PipeWire main loop on dedicated thread)
- No extra dependencies

cpal would add a dependency and may not cleanly expose PipeWire monitor capture.

### 2. Opus via `opus` Crate (not FFmpeg)

Using the `opus` crate (safe Rust bindings to `libopus`) instead of FFmpeg's Opus encoder because:
- Simpler API — no FFmpeg codec context overhead for audio
- Direct control over frame size and application mode
- Lightweight — Opus encoding is CPU-cheap, doesn't need GPU
- `libopus` is the reference encoder and produces optimal output

### 3. Separate Capture + Encoder Threads

Same pattern as video: capture on one thread (PipeWire main loop), encoder on another (Opus encode loop), connected by `tokio::sync::mpsc`. This keeps the PipeWire callback fast (just copy PCM data into channel) and lets the encoder work independently.

### 4. Transport Integration

The sender's `send_packets()` currently hardcodes `STREAM_TYPE_VIDEO`. Change it to accept a `stream_type: u8` parameter. Then spawn two send tasks in `run_server_loop()` — one for video packets, one for audio packets. Each maintains its own `frame_index` counter.

### 5. No IDR Equivalent for Audio

Audio doesn't have keyframes. Opus packets are self-contained (decoder can resync at any packet boundary with minimal artifact). No need for an IDR-like request mechanism for audio.

### 6. Frame Size: 10ms

10ms (480 samples at 48kHz) balances latency vs overhead:
- 5ms = lower latency but more packets/sec and higher overhead
- 20ms = fewer packets but adds 10ms to pipeline latency
- 10ms = ~100 packets/sec, each ~40-160 bytes, fits in single datagram

### 7. Channel Capacities

- Capture → Encoder: capacity 8 (audio frames are small, ~3.8KB each for f32 stereo 10ms)
- Encoder → Transport: capacity 4 (same as video, Opus packets are tiny)

## Shared Types (stargaze-core)

```rust
/// Raw audio frame from capture.
pub struct AudioFrame {
    /// Interleaved f32 PCM samples (L0, R0, L1, R1, ...).
    pub data: Vec<f32>,
    /// Sample rate in Hz (expected: 48000).
    pub sample_rate: u32,
    /// Number of channels (expected: 2).
    pub channels: u16,
    /// Presentation timestamp (monotonic frame counter).
    pub pts: u64,
}

/// Configuration for audio capture.
pub struct AudioCaptureConfig {
    /// Target sample rate (48000).
    pub sample_rate: u32,
    /// Number of channels (2 for stereo).
    pub channels: u16,
}

/// Configuration for the Opus audio encoder.
pub struct AudioEncoderConfig {
    /// Sample rate in Hz (must be 48000 for Opus).
    pub sample_rate: u32,
    /// Number of channels (1 or 2).
    pub channels: u16,
    /// Target bitrate in bits per second (e.g., 128000).
    pub bitrate: u32,
    /// Opus application mode.
    pub application: AudioApplication,
}

pub enum AudioApplication {
    /// General audio (music, game sounds, mixed content).
    Audio,
    /// Voice-optimized (speech-heavy content).
    Voip,
    /// Ultra-low latency (sacrifices quality for speed).
    LowDelay,
}

pub enum AudioError {
    CaptureInit(String),
    CaptureStream(String),
    EncoderInit(String),
    EncodeFailed(String),
    ChannelClosed(String),
}
```

## PipeWire Audio Capture

### Stream Properties

```
MEDIA_TYPE = "Audio"
MEDIA_CATEGORY = "Capture"
MEDIA_ROLE = "Game"
STREAM_CAPTURE_SINK = "true"   ← captures system audio output
```

### Negotiated Format

Request f32le at the configured sample rate and channels. PipeWire will convert from the sink's native format if needed.

### Buffer Handling

In the `process` callback:
1. Dequeue buffer from stream
2. Read f32 PCM data from the SPA buffer
3. Copy into `AudioFrame`
4. `blocking_send()` into the capture channel

### Shutdown

Same as video capture: `Arc<AtomicBool>` flag checked periodically, PipeWire main loop quit via raw pointer.

## Opus Encoding

### Encoder Settings

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| Sample rate | 48000 Hz | Opus native, game audio standard |
| Channels | Stereo (2) | Game audio is stereo |
| Application | Audio | Mixed content (music + SFX + voice) |
| Bitrate | 128 kbps | Good quality for LAN streaming |
| Frame size | 10ms (480 samples/ch) | Low latency, manageable overhead |
| VBR | Enabled | Better quality-per-bit |
| Complexity | 5 | Balance between quality and CPU |

### Encode Loop

1. `blocking_recv()` AudioFrame from capture channel
2. Verify sample count matches expected frame size (480 * 2 = 960 for stereo)
3. `encoder.encode_float(&frame.data, &mut output_buf)` → Opus packet bytes
4. Wrap in `EncodedPacket { data, pts, is_keyframe: false }`
5. `blocking_send()` into encoder output channel
6. Check shutdown flag

## Transport Changes

### `sender::send_packets()`

Add `stream_type: u8` parameter. Replace hardcoded `STREAM_TYPE_VIDEO` with the parameter in `DatagramHeader` construction. No other changes needed — fragmentation is stream-type agnostic.

### `run_server_loop()`

Accept two packet receivers. Spawn two concurrent send tasks:
```rust
let video_send = sender::send_packets(&connection, &mut video_packets, STREAM_TYPE_VIDEO);
let audio_send = sender::send_packets(&connection, &mut audio_packets, STREAM_TYPE_AUDIO);
tokio::select! { ... }
```

## Testing Strategy

### Unit Tests

1. **Audio type construction** — verify `AudioFrame`, config types work correctly
2. **Opus encoder init** — verify encoder initializes with valid config
3. **Opus encode round-trip** — encode silence, verify output is valid Opus packet (non-empty, reasonable size)
4. **Opus rejects invalid config** — verify error on unsupported sample rate

### Integration Tests (ignored — need PipeWire daemon)

1. **Capture → encode pipeline** — capture system audio for 1s, verify Opus packets produced

## Non-Goals

- Microphone forwarding (that's sub-project 9 / rsonance)
- Audio format negotiation (fixed at 48kHz stereo Opus for MVP)
- Multiple audio streams or surround sound
- Audio mixing or volume control
