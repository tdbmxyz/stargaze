# Audio Decoding + Playback — Implementation Plan

**Sub-project:** 7 of 9
**Design spec:** `docs/specs/2026-04-04-audio-decoding-playback-design.md`

## Issues to Address

1. Client transport's `FrameAssembler` keys pending frames by `frame_index` alone — audio and video frame indices collide, corrupting reassembly
2. Client transport sends all reassembled frames through a single channel — no stream demux
3. Client has no audio decoding — Opus packets from the server are ignored
4. Client has no audio playback — decoded PCM has nowhere to go

## Important Notes

- `opus` crate needs `libopus-dev` — installed to `~/.local/` (same workaround as SDL2 and FFmpeg). The `build.rs` needs to query `pkg-config` for opus `-L` paths (same pattern as server's `build.rs`).
- `opus::Decoder` is `Send` but NOT `Sync` — must live on a single thread. The dedicated decoder thread pattern already used for FFmpeg video is the correct approach.
- `SDL2::AudioQueue::queue_audio()` is thread-safe — can push decoded PCM from the decoder thread. No intermediate channel between decoder and playback is needed.
- The SDL2 context (`sdl2::init()`) must be created on the main thread. The audio subsystem and `AudioQueue` are obtained from it, then the `AudioQueue` handle is moved to the decoder thread.
- Existing `FrameAssembler` tests use only `STREAM_TYPE_VIDEO`. New tests must verify mixed stream behavior.
- The `receive_loop()` function signature changes (two senders instead of one), which also changes `connect()` return type. `main.rs` must be updated to match.
- `ReassembledFrame` already has a `stream_type: u8` field — no changes needed to the frame type itself.

## Implementation Strategy

### Task 1: Shared Audio Decoder Types

Add `AudioDecoderConfig` and decoder error variants to `crates/stargaze-core/src/audio.rs`:
- `AudioDecoderConfig { sample_rate: u32, channels: u16 }` — mirrors `AudioEncoderConfig` but without bitrate/application
- `AudioError::DecoderInit(String)` — decoder creation failure
- `AudioError::DecodeFailed(String)` — per-packet decode failure
- Unit tests for new types

**Files**: `crates/stargaze-core/src/audio.rs`
**Commit**: `feat(core): add audio decoder config and error variants`

### Task 2: Client Dependencies + Build Configuration

Add `opus = "0.3"` to client's `Cargo.toml`. Add `emit_opus_link_paths()` to client's `build.rs` (copy pattern from server's `build.rs`).

**Files**: `crates/stargaze-client/Cargo.toml`, `crates/stargaze-client/build.rs`
**Commit**: `chore(deps): add opus crate to client for audio decoding`

### Task 3: Fix FrameAssembler Per-Stream Keying

Change `FrameAssembler`:
- `pending: HashMap<u32, PendingFrame>` → `HashMap<(u8, u32), PendingFrame>`
- `next_frame: u32` → `HashMap<u8, u32>`
- Update `process_datagram()`: use `(header.stream_type, header.frame_index)` as key
- Update `assemble_frame()`: takes `(u8, u32)` key
- Update `deliver_in_order()`: iterate per-stream using `next_frame` map
- IDR check: only count video pending frames for IDR trigger, clear only video entries on IDR

Add tests:
- Mixed stream fragments with same `frame_index` don't collide
- Per-stream in-order delivery works independently

**Files**: `crates/stargaze-client/src/transport/receiver.rs`
**Commit**: `fix(transport): key FrameAssembler by (stream_type, frame_index) to prevent collisions`

### Task 4: Transport Stream Demux

Modify `receive_loop()`:
- Accept `video_tx: mpsc::Sender<ReassembledFrame>` and `audio_tx: mpsc::Sender<ReassembledFrame>` instead of single `frames_tx`
- Route completed frames: `STREAM_TYPE_VIDEO` → `video_tx`, `STREAM_TYPE_AUDIO` → `audio_tx`, unknown → warn + drop
- IDR request only sent for video frame loss

Modify `connect()`:
- Create two channels: `(video_tx, video_rx)` and `(audio_tx, audio_rx)`, both capacity 16
- Return `(ClientTransport, mpsc::Receiver<ReassembledFrame>, mpsc::Receiver<ReassembledFrame>)` — video then audio

Update integration test to work with new API (test still uses video-only frames, but API changed).

**Files**: `crates/stargaze-client/src/transport/receiver.rs`, `crates/stargaze-client/src/transport/mod.rs`, `crates/stargaze-client/tests/transport_integration.rs`
**Commit**: `refactor(transport): demux video and audio into separate receiver channels`

### Task 5: Opus Decoder Module

Create `crates/stargaze-client/src/decode/opus_dec.rs`:
- `init_opus_decoder(config: &AudioDecoderConfig) → Result<opus::Decoder, AudioError>` — create decoder with 48kHz stereo
- `run_opus_decode_loop(decoder, frames_rx, audio_queue, shutdown)` — blocking recv, decode_float, queue_audio

Create `AudioDecoderSession` in `crates/stargaze-client/src/decode/mod.rs`:
- Same pattern as `DecoderSession`: thread handle + shutdown flag + Drop impl
- `start_audio_decoder(config, frames_rx, audio_queue) → Result<AudioDecoderSession, AudioError>`
- Init handshake via `std::sync::mpsc` oneshot

Unit tests:
- Opus decoder init with valid config succeeds
- Opus decoder rejects invalid channel count
- Opus encode→decode round-trip produces correct-length output

**Files**: `crates/stargaze-client/src/decode/opus_dec.rs`, `crates/stargaze-client/src/decode/mod.rs`
**Commit**: `feat(client): add Opus audio decoder with dedicated thread`

### Task 6: SDL2 Audio Playback Module

Create `crates/stargaze-client/src/render/audio.rs`:
- `create_audio_queue(sdl: &sdl2::Sdl) → Result<AudioQueue<f32>, anyhow::Error>` — init audio subsystem, open queue with 48kHz stereo f32, resume
- The `AudioQueue<f32>` is passed to the decoder thread

Add `start_audio_renderer()` to `crates/stargaze-client/src/render/mod.rs`:
- Public function that creates the audio queue and returns it
- Called from main.rs before spawning the audio decoder

**Files**: `crates/stargaze-client/src/render/audio.rs`, `crates/stargaze-client/src/render/mod.rs`
**Commit**: `feat(client): add SDL2 audio queue playback`

### Task 7: Wire Audio Pipeline in Client main.rs

Update `main.rs`:
1. Receive `(client_transport, video_rx, audio_rx)` from `transport::connect()`
2. Pass `video_rx` to existing `decode::start_decoder()` (video-only, unchanged)
3. Create SDL2 context on main thread, obtain audio queue via `render::start_audio_renderer()`
4. Start audio decoder: `decode::start_audio_decoder(audio_config, audio_rx, audio_queue)?`
5. Pass SDL2 context to `render::start_renderer()` (adjust if SDL init moves to main.rs)
6. Shutdown: stop audio decoder session alongside video decoder

**Files**: `crates/stargaze-client/src/main.rs`, `crates/stargaze-client/src/render/mod.rs`, `crates/stargaze-client/src/render/sdl.rs`
**Commit**: `feat(client): wire audio decoding and playback into main pipeline`

### Task 8: Final Tests + Verification

Run full verification:
- `cargo test --workspace`
- `cargo clippy --workspace -- -W clippy::pedantic`
- `cargo fmt --check`

Fix any issues found.

**Commit**: test commits as needed

## Tests

### Unit Tests (always run)
- `AudioDecoderConfig` construction and field access
- `AudioError::DecoderInit` / `AudioError::DecodeFailed` Display formatting
- `FrameAssembler` mixed stream fragments with colliding frame_index don't corrupt each other
- `FrameAssembler` per-stream in-order delivery is independent
- Opus decoder initialization with valid config (48kHz stereo)
- Opus decoder rejects invalid channel count (e.g., 3)
- Opus encode→decode round-trip produces f32 PCM of expected length

### Integration Tests (ignored — need audio device)
- Full audio decode→playback pipeline (decode Opus and play through SDL2 AudioQueue)
