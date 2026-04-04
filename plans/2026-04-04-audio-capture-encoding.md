# Audio Capture + Encoding — Implementation Plan

**Sub-project:** 6 of 9
**Design spec:** `docs/specs/2026-04-04-audio-capture-encoding-design.md`

## Issues to Address

1. Server needs to capture system audio output (PipeWire sink monitor) and encode it to Opus
2. Transport sender currently hardcodes `STREAM_TYPE_VIDEO` — needs parameterization for multi-stream
3. Server main.rs needs to start audio pipeline alongside video and pass both to transport

## Important Notes

- `pipewire` crate v0.9 is already a dependency (used for video capture). Reuse it for audio.
- `opus` crate needs `libopus-dev` — installed to `~/.local/` (same workaround as SDL2). The `build.rs` needs to query `pkg-config` for opus `-L` paths.
- `audiopus_sys` (dependency of `opus`) uses `pkg-config` to find libopus. Set `PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:$PKG_CONFIG_PATH"`.
- PipeWire audio capture uses `STREAM_CAPTURE_SINK=true` to capture monitor ports (system audio output, not microphone).
- Opus packets are small (~40-160 bytes per 10ms frame at 128kbps) — typically fit in a single QUIC datagram.
- Audio has no keyframes — `is_keyframe` is always `false` in `EncodedPacket`.
- The PipeWire audio capture thread pattern mirrors video capture: `MainLoop` on dedicated thread, `process` callback sends `AudioFrame` via `blocking_send()`.
- Existing `EncodedPacket` from stargaze-core is reused for Opus packets (same fields: `data`, `pts`, `is_keyframe`).

## Implementation Strategy

### Task 1: Dependencies + Build Configuration

Add the `opus` crate to server's `Cargo.toml`. Update `build.rs` to query `pkg-config` for `opus` link paths (same pattern as FFmpeg and SDL2 queries in client's `build.rs`).

**Files**: `crates/stargaze-server/Cargo.toml`, `crates/stargaze-server/build.rs`
**Commit**: `chore(deps): add opus crate for audio encoding`

### Task 2: Shared Audio Types in stargaze-core

Create `crates/stargaze-core/src/audio.rs` with:
- `AudioFrame` — raw PCM data from capture (f32 interleaved, sample rate, channels, pts)
- `AudioCaptureConfig` — sample rate, channels
- `AudioEncoderConfig` — sample rate, channels, bitrate, application mode
- `AudioApplication` enum — Audio, Voip, LowDelay
- `AudioError` — CaptureInit, CaptureStream, EncoderInit, EncodeFailed, ChannelClosed

Add `pub mod audio;` to `crates/stargaze-core/src/lib.rs`.

Unit tests: type construction, Default impls, Display on errors.

**Files**: `crates/stargaze-core/src/audio.rs`, `crates/stargaze-core/src/lib.rs`
**Commit**: `feat(core): add shared audio types — AudioFrame, AudioError, configs`

### Task 3: PipeWire Audio Capture Module

Create `crates/stargaze-server/src/audio/mod.rs` with:
- `AudioCaptureSession` — thread handle + shutdown flag (same pattern as `CaptureSession`)
- `start_audio_capture()` → `Result<(AudioCaptureSession, mpsc::Receiver<AudioFrame>), AudioError>`
- Channel capacity: 8

Create `crates/stargaze-server/src/audio/pipewire.rs` with:
- `run_audio_capture()` — PipeWire main loop, stream with `STREAM_CAPTURE_SINK=true`
- Process callback: dequeue buffer, read f32 PCM, send `AudioFrame` via `blocking_send()`
- Format negotiation: request f32le, 48kHz, stereo
- Shutdown: check `AtomicBool`, quit PipeWire main loop

Add `mod audio;` to server's `main.rs`.

**Files**: `crates/stargaze-server/src/audio/mod.rs`, `crates/stargaze-server/src/audio/pipewire.rs`
**Commit**: `feat(server): add PipeWire audio capture from sink monitor`

### Task 4: Opus Audio Encoder Module

Create `crates/stargaze-server/src/encode/opus.rs` with:
- `init_opus_encoder(config: &AudioEncoderConfig) → Result<opus::Encoder, AudioError>`
- `run_opus_encode_loop(encoder, frames_rx, packets_tx, shutdown)` — blocking recv/send loop
- Frame size: 480 samples/channel (10ms at 48kHz)
- Encode: `encoder.encode_float()` → wrap in `EncodedPacket`

Create `AudioEncoderSession` in `crates/stargaze-server/src/encode/mod.rs`:
- `start_audio_encoder(config, frames_rx) → Result<(AudioEncoderSession, mpsc::Receiver<EncodedPacket>), AudioError>`
- Same thread + init handshake pattern as video encoder
- Channel capacity: 4

**Files**: `crates/stargaze-server/src/encode/opus.rs`, `crates/stargaze-server/src/encode/mod.rs`
**Commit**: `feat(server): add Opus audio encoder with dedicated thread`

### Task 5: Parameterize Transport for Multi-Stream

Modify `sender::send_packets()` to accept `stream_type: u8` parameter — replace hardcoded `STREAM_TYPE_VIDEO`.

Modify `start_server_transport()` to accept both video and audio packet receivers. Update `run_server_loop()` to spawn two concurrent send tasks, one per stream type.

**Files**: `crates/stargaze-server/src/transport/sender.rs`, `crates/stargaze-server/src/transport/mod.rs`
**Commit**: `refactor(transport): parameterize sender for video + audio streams`

### Task 6: Wire Audio Pipeline into Server

Update `main.rs`:
1. Start audio capture: `audio::start_audio_capture(audio_config).await?`
2. Start audio encoder: `encode::start_audio_encoder(audio_enc_config, audio_frames)?`
3. Pass both video and audio packet receivers to `start_server_transport()`
4. Add audio session shutdown alongside video shutdown

**Files**: `crates/stargaze-server/src/main.rs`
**Commit**: `feat(server): wire audio capture and encoding into streaming pipeline`

### Task 7: Tests + Final Verification

Add tests:
- Core audio types: construction, error Display (in `audio.rs`)
- Opus encoder init: valid config succeeds, invalid sample rate rejected
- Opus encode: silence frame produces valid packet
- Transport: send_packets with audio stream type

Run full verification:
- `cargo test --workspace`
- `cargo clippy --workspace -- -W clippy::pedantic`
- `cargo fmt --check`

**Commit**: `test(server): add audio capture and Opus encoder tests`

## Tests

### Unit Tests (always run)
- `AudioFrame` construction and field access
- `AudioError` Display formatting
- `AudioCaptureConfig` / `AudioEncoderConfig` defaults
- Opus encoder initialization with valid config (48kHz stereo)
- Opus encoder rejects invalid sample rate (e.g., 44100)
- Opus encode silence produces non-empty output
- Transport `send_packets` with `STREAM_TYPE_AUDIO` parameter

### Integration Tests (ignored — need PipeWire daemon)
- Full audio capture → encode pipeline (capture for 1s, verify packets)
