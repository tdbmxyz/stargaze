# Mic Forwarding (Client → Server) — Implementation Plan

**Sub-project:** 9 of 9
**Design spec:** `docs/specs/2026-04-04-mic-forwarding-design.md`

## Issues to Address

1. No mic forwarding capability exists — the architecture spec designates it as a rsonance subprocess integration
2. No subprocess management code exists in stargaze
3. `ServerConfig` and `ClientConfig` have no mic forwarding fields
4. No CLI flags for enabling/configuring mic forwarding

## Important Notes

- `rsonance` binary is **not** available in this devcontainer. All tests that spawn rsonance must be `#[ignore]` with a descriptive reason.
- Rsonance receiver is synchronous (blocks on `TcpListener::accept`), transmitter is async (tokio). Both are run as separate OS processes, so this doesn't affect stargaze's async runtime.
- Rsonance's wire format is fixed: S16LE, 44100 Hz, stereo over raw TCP. No configuration needed.
- The FIFO path `/tmp/stargaze_mic_pipe` differs from rsonance's default `/tmp/rsonance_audio_pipe` to avoid conflicts with standalone rsonance instances.
- `tokio::process::Command` is used instead of `std::process::Command` because both server and client `main()` are already `#[tokio::main]` async contexts, and we need async `child.kill()` / `child.wait()` in the shutdown sequence.
- Rsonance virtual mic name uses `stargaze_virtual_microphone` (not rsonance's default) for the same conflict-avoidance reason.

## Implementation Strategy

### Task 1: Config + CLI Flags

Add `MicForwardConfig` to `crates/stargaze-core/src/config.rs`:
- `MicForwardConfig` struct with `enabled: bool` (default false), `port: u16` (default 9001), `rsonance_binary: String` (default "rsonance")
- Embed as `pub mic_forward: MicForwardConfig` in both `ServerConfig` and `ClientConfig`
- Update `Default` impls for both config structs

Add CLI flags to both binaries:
- `--mic-forward` (boolean flag, sets `mic_forward.enabled = true`)
- `--mic-forward-port <PORT>` (optional, overrides `mic_forward.port`)

Update `build_config()` in both `main.rs` files to apply CLI overrides to `mic_forward`.

Unit tests:
- `MicForwardConfig` defaults (enabled=false, port=9001, binary="rsonance")
- `ServerConfig` / `ClientConfig` TOML round-trip with `[mic_forward]` section
- Partial TOML (no `[mic_forward]` section) still works with defaults

**Files**: `crates/stargaze-core/src/config.rs`, `crates/stargaze-server/src/main.rs`, `crates/stargaze-client/src/main.rs`
**Commit**: `feat(core): add mic forwarding config and CLI flags`

### Task 2: Subprocess Manager Module

Add `crates/stargaze-core/src/mic_forward.rs`:

```
pub async fn spawn_rsonance_receiver(config: &MicForwardConfig) -> anyhow::Result<tokio::process::Child>
pub async fn spawn_rsonance_transmitter(config: &MicForwardConfig, server_address: &str) -> anyhow::Result<tokio::process::Child>
pub async fn stop_rsonance(child: &mut tokio::process::Child)
```

`spawn_rsonance_receiver()`:
- Runs: `<rsonance_binary> receiver --host 0.0.0.0 --port <port> --fifo-path /tmp/stargaze_mic_pipe --microphone-name stargaze_virtual_microphone`
- Returns the `Child` handle for later cleanup
- Logs info on spawn, warns on failure

`spawn_rsonance_transmitter()`:
- Runs: `<rsonance_binary> transmitter --host <server_address> --port <port>`
- Returns the `Child` handle
- Logs info on spawn, warns on failure

`stop_rsonance()`:
- Calls `child.kill().await` then `child.wait().await` for clean process reaping
- Logs the process exit status

Add `pub mod mic_forward;` to `crates/stargaze-core/src/lib.rs`.

Unit tests:
- `spawn_rsonance_receiver` with a non-existent binary returns an error
- `spawn_rsonance_transmitter` with a non-existent binary returns an error
- `stop_rsonance` on an already-exited process doesn't panic

**Files**: `crates/stargaze-core/src/mic_forward.rs`, `crates/stargaze-core/src/lib.rs`
**Commit**: `feat(core): add rsonance subprocess manager for mic forwarding`

### Task 3: Server Integration

In `crates/stargaze-server/src/main.rs`:

After audio encoder starts and before `start_server_transport()`:
- If `cfg.mic_forward.enabled`, call `mic_forward::spawn_rsonance_receiver(&cfg.mic_forward).await`
- Store the `Option<Child>` handle

In shutdown sequence (after `tokio::select!`):
- If rsonance child exists, call `mic_forward::stop_rsonance(&mut child).await`
- Do this before stopping audio encoder (receiver should stop first)

**Files**: `crates/stargaze-server/src/main.rs`
**Commit**: `feat(server): spawn and manage rsonance receiver for mic forwarding`

### Task 4: Client Integration

In `crates/stargaze-client/src/main.rs`:

After `transport::connect()` and before `start_renderer()`:
- If `cfg.mic_forward.enabled`, call `mic_forward::spawn_rsonance_transmitter(&cfg.mic_forward, &cfg.server_address).await`
- Store the `Option<Child>` handle

After renderer returns (in shutdown sequence):
- If rsonance child exists, call `mic_forward::stop_rsonance(&mut child).await`
- Do this alongside bridge/decoder/transport cleanup

**Files**: `crates/stargaze-client/src/main.rs`
**Commit**: `feat(client): spawn and manage rsonance transmitter for mic forwarding`

### Task 5: Docs

Commit the design spec and implementation plan.

**Files**: `docs/specs/2026-04-04-mic-forwarding-design.md`, `plans/2026-04-04-mic-forwarding.md`
**Commit**: `docs: add mic forwarding design spec and implementation plan`

## Tests

### Unit Tests (cargo test)
- `MicForwardConfig` default values
- `ServerConfig` / `ClientConfig` TOML serialization with `[mic_forward]` section
- Partial TOML without `[mic_forward]` uses defaults
- `spawn_rsonance_receiver` with non-existent binary → error
- `spawn_rsonance_transmitter` with non-existent binary → error
- `stop_rsonance` on already-exited child doesn't panic

### Integration Tests (ignored — require rsonance + PulseAudio)
- Full server spawn + kill lifecycle
- Full client spawn + kill lifecycle
- Both spawned simultaneously with loopback connection
