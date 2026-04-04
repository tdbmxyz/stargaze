# Cursor Rendering Implementation Plan

**Date**: 2026-04-04
**Design Spec**: `docs/specs/2026-04-04-cursor-rendering-design.md`

## Issues to Address

The stargaze server captures screen frames with `CursorMode::Hidden`, so users see no cursor during streaming. We need to:

1. Make cursor visibility configurable (default: visible/embedded)
2. Thread the config through the capture pipeline to the portal handshake
3. Future-proof the protocol for eventual client-side cursor overlay

## Important Notes

- `ashpd` (v0.13.9) exposes `CursorMode::Hidden`, `CursorMode::Embedded`, `CursorMode::Metadata` in `ashpd::desktop::screencast::CursorMode`.
- `CursorMode` uses `enumflags2::BitFlags` — it's a bitflag enum, not a simple enum. Must use `BitFlags::from()` or directly pass the variant.
- The portal's `select_sources` takes `SelectSourcesOptions` which has `.set_cursor_mode(CursorMode)`.
- `create_screencast_session()` currently takes no arguments — needs to accept `show_cursor: bool`.
- `start_capture()` takes `CaptureConfig` — add `show_cursor` field there.
- Server `main.rs` already has the pattern for bool CLI flags (see `--mic-forward`).
- Only the server config needs `CursorConfig` — the client doesn't control cursor capture.
- `SessionResponse` already uses `postcard` serde — adding a field is backward-compatible since we use `Serialize`/`Deserialize` derive.

## Implementation Strategy

### Step 1: Core config (`stargaze-core/src/config.rs`)

Add `CursorConfig` struct with `show_cursor: bool` (default `true`). Embed in `ServerConfig`.

### Step 2: Capture pipeline (`stargaze-server/src/capture/`)

- Add `show_cursor: bool` to `CaptureConfig`
- Pass it to `create_screencast_session(show_cursor)`
- In `portal.rs`, use `CursorMode::Embedded` when true, `CursorMode::Hidden` when false

### Step 3: Server CLI (`stargaze-server/src/main.rs`)

- Add `--show-cursor` / `--no-show-cursor` CLI flag (default: follows config, which defaults to true)
- Thread through `build_config()` → `CaptureConfig`

### Step 4: Protocol future-proofing (`stargaze-core/src/transport.rs`)

- Add `cursor_embedded: bool` field to `SessionResponse`
- Update server transport to set it based on config
- Client can ignore it for now but it's available when overlay mode is added

### Step 5: Tests

- Config defaults test (show_cursor = true)
- TOML parsing with cursor section
- `SessionResponse` round-trip with new field
- `CursorMode` mapping test (show_cursor → Embedded/Hidden)

## Tests

**Unit tests** (in `config.rs`):
- `test_cursor_config_defaults` — verify `show_cursor` defaults to `true`
- `test_server_config_with_cursor_toml` — parse `[cursor]\nshow_cursor = false`
- `test_server_config_cursor_default_when_absent` — cursor config uses defaults when TOML section missing

**Unit tests** (in `transport.rs`):
- `test_session_response_with_cursor_round_trip` — serialize/deserialize `SessionResponse` with `cursor_embedded`

**Unit test** (in `portal.rs` or `capture/mod.rs`):
- Not needed — portal code can't be unit tested without D-Bus. The existing `#[ignore]` integration tests cover the capture pipeline.
