# Mic Forwarding (Client → Server) — Design Spec

**Sub-project:** 9 of 9 (Mic Forwarding via rsonance)

## Overview

Forward the client's microphone audio to the server by launching [rsonance](https://github.com/tdbmxyz/rsonance) as a subprocess on both sides. The client runs `rsonance transmitter` (captures mic, sends raw PCM over TCP) and the server runs `rsonance receiver` (creates a PulseAudio virtual microphone via FIFO pipe). This is an optional feature — disabled by default.

Mic forwarding is **not** a core stargaze feature. It delegates entirely to rsonance and manages it as a subprocess lifecycle concern.

## Target Environment

- **Client**: Linux with a microphone and `rsonance` binary in `$PATH`
- **Server**: Linux with PulseAudio or PipeWire (PulseAudio compat layer), `pactl`, `mkfifo`, and `rsonance` binary in `$PATH`
- **Network**: TCP (separate from QUIC streams — rsonance manages its own TCP connection)
- **Audio format**: S16LE, 44100 Hz, stereo (rsonance's fixed wire format)
- **Prerequisite**: `rsonance` must be installed separately. Stargaze does not bundle it.

## Architecture

### Data Flow

```
Client machine                                    Server machine
┌─────────────────────┐    TCP (S16LE PCM)    ┌─────────────────────┐
│  rsonance transmitter │ ──────────────────> │  rsonance receiver    │
│  (subprocess)         │    port 9001         │  (subprocess)         │
│                       │                      │                       │
│  cpal mic capture     │                      │  TCP → FIFO pipe      │
│  → S16LE conversion   │                      │  → PulseAudio         │
│  → TCP stream         │                      │    module-pipe-source  │
└─────────────────────┘                      │    (virtual mic)       │
                                              └─────────────────────┘

Stargaze client main()                        Stargaze server main()
  1. Connect transport                          1. Start pipelines
  2. If mic_forward enabled:                    2. If mic_forward enabled:
     spawn `rsonance transmitter`                  spawn `rsonance receiver`
  3. Start renderer                             3. Start transport
  4. On shutdown: kill transmitter               4. On shutdown: kill receiver
```

### Port Allocation

Rsonance uses a **separate TCP port** from stargaze's QUIC transport (default 9000). The default rsonance port is `9001` (not rsonance's own default of 8080, to avoid common conflicts). Both sides must agree on the port — it's part of `MicForwardConfig`.

### Subprocess Management

Both binaries use `tokio::process::Command` to spawn rsonance as a child process:
- **Stdout/stderr** are inherited (logged alongside stargaze output via tracing)
- **Shutdown**: `child.kill()` on graceful shutdown or `ctrl_c`
- **Failure handling**: If rsonance fails to start (binary not found, port in use), log a warning and continue — mic forwarding is optional and should not prevent streaming

### Config & CLI

New `MicForwardConfig` struct in `stargaze-core/src/config.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MicForwardConfig {
    pub enabled: bool,          // default: false
    pub port: u16,              // default: 9001
    pub rsonance_binary: String, // default: "rsonance"
}
```

Embedded in both `ServerConfig` and `ClientConfig` as `pub mic_forward: MicForwardConfig`.

CLI flags (both binaries):
- `--mic-forward` — enable mic forwarding (sets `mic_forward.enabled = true`)
- `--mic-forward-port <PORT>` — rsonance TCP port (default 9001)

Server-specific rsonance receiver args (derived from config):
- `--host 0.0.0.0 --port <mic_forward.port> --fifo-path /tmp/stargaze_mic_pipe --microphone-name stargaze_virtual_microphone`

Client-specific rsonance transmitter args:
- `--host <server_address> --port <mic_forward.port>`

### Module Layout

```
crates/stargaze-core/src/
    config.rs             # MODIFIED: add MicForwardConfig, embed in ServerConfig + ClientConfig
    mic_forward.rs        # NEW: spawn_rsonance_receiver(), spawn_rsonance_transmitter(), 
                          #      stop_rsonance() — shared subprocess management
    lib.rs                # MODIFIED: add pub mod mic_forward

crates/stargaze-server/src/
    main.rs               # MODIFIED: add --mic-forward CLI flags, spawn/kill receiver

crates/stargaze-client/src/
    main.rs               # MODIFIED: add --mic-forward CLI flags, spawn/kill transmitter
```

## Design Decisions

### Subprocess vs Library Dependency

**Chose subprocess.** Embedding rsonance as a library would couple stargaze to rsonance's `cpal`, `signal-hook`, and `env_logger` dependencies, potentially conflicting with stargaze's own `tracing` and audio stack. Subprocess keeps the boundary clean.

### Why Not a New QUIC Stream?

Rsonance already has a working TCP-based audio pipeline. Re-implementing mic capture, S16LE conversion, FIFO writing, and PulseAudio virtual mic management inside stargaze would be significant effort for little gain over just launching the existing tool.

### Failure Tolerance

Mic forwarding is optional and best-effort. If `rsonance` is not installed, the feature logs a warning and streaming proceeds normally. This matches the architecture spec: "Not a core stargaze feature."

### FIFO Path Uniqueness

The FIFO path is hardcoded to `/tmp/stargaze_mic_pipe` (not `/tmp/rsonance_audio_pipe`) to avoid conflicts if a standalone rsonance instance is also running. A future enhancement could use a unique path per session.

## Success Criteria

- `--mic-forward` flag exists on both `stargaze-server` and `stargaze-client`
- With `--mic-forward`, server spawns `rsonance receiver` and client spawns `rsonance transmitter`
- Without `--mic-forward`, no rsonance processes are spawned (default behavior unchanged)
- If `rsonance` binary is not found, a warning is logged and streaming continues
- On shutdown (Ctrl+C or renderer close), rsonance subprocesses are killed
- `cargo test --workspace`, `cargo clippy --workspace -- -W clippy::pedantic`, and `cargo fmt --check` all pass
- Config is serializable to/from TOML with `[mic_forward]` section
