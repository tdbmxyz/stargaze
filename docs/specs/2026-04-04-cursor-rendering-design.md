# Cursor Rendering Design Spec

**Date**: 2026-04-04
**Status**: Post-MVP #5
**Depends on**: Sub-project 2 (PipeWire capture), Sub-project 3 (video encoding)

## Problem

The stargaze server currently captures screen frames via xdg-desktop-portal with `CursorMode::Hidden`, meaning the cursor is excluded from captured frames. The client renders only the video texture — users see no cursor at all during a streaming session.

## Approach: Compositor-Embedded Cursor (Phase 1)

Use `CursorMode::Embedded` in the portal screencast request. The Wayland compositor composites the cursor into captured frames before PipeWire delivers them. This is the same approach Sunshine uses (`CURSOR_MODE_EMBEDDED = 2` in `portalgrab.cpp`).

### Why This Approach

- **Zero client changes**: cursor flows through the existing capture → encode → transport → decode → render pipeline.
- **Zero protocol changes**: no new messages needed.
- **Proven**: Sunshine ships this as default behavior.
- **Compositors handle it well**: Hyprland, GNOME, KDE all support `CursorMode::Embedded`.

### Tradeoffs

- Cursor quality is tied to video bitrate (slight compression artifacts).
- Cursor movement latency matches video frame latency (no independent cursor rendering).
- Cannot render cursor independently of video (e.g., over client-side UI overlays).

These are acceptable for MVP. Phase 2 (client-side overlay) addresses them.

## Configuration

### New Config: `CursorConfig`

```toml
[cursor]
show_cursor = true  # default: true (embedded in frame)
```

- `show_cursor = true` → `CursorMode::Embedded` (cursor visible in stream)
- `show_cursor = false` → `CursorMode::Hidden` (cursor excluded, current behavior)

### CLI Flag

- `--show-cursor` / `--no-show-cursor` (bool flag, overrides config)

### Why Not Expose `cursor_mode` Directly

The three portal modes (Hidden, Embedded, Metadata) are implementation details. Users care about "do I see a cursor?" — not "which portal API variant." The `show_cursor` boolean maps cleanly:

- Phase 1: `true` → Embedded, `false` → Hidden
- Phase 2: Could be extended with `cursor_mode = "overlay"` when client-side rendering is added

## Future: Client-Side Overlay (Phase 2)

When/if Phase 2 is implemented:

1. Server sends `CursorMode::Metadata` to portal, getting cursor position + image as PipeWire stream metadata (`SPA_META_Cursor`).
2. New `ControlMessage` variants carry cursor data to client:
   - `CursorPosition { x: u32, y: u32, visible: bool }`
   - `CursorShape { width: u16, height: u16, hotspot_x: u16, hotspot_y: u16, data: Vec<u8> }`
3. Client renders cursor as an RGBA SDL texture overlay on top of the video texture.
4. Config extends to `cursor_mode = "embedded" | "overlay" | "hidden"`.

Phase 2 is out of scope for this implementation but the config structure is designed to not require breaking changes.

## Protocol Future-Proofing

Add a `cursor_mode` field to `SessionResponse` so the client knows whether to expect cursor in the video frames or as separate messages. This costs nothing now and avoids a protocol version bump later.

## Data Flow

### Phase 1 (This Implementation)

```
Portal (CursorMode::Embedded)
  → PipeWire stream (frames include cursor pixels)
    → Encoder (NVENC, cursor is part of frame)
      → Transport (QUIC datagrams, unchanged)
        → Decoder (FFmpeg, unchanged)
          → SDL renderer (unchanged — cursor visible in texture)
```

### Phase 2 (Future)

```
Portal (CursorMode::Metadata)
  → PipeWire stream (frames without cursor + cursor metadata)
    → Encoder (cursor-free frames)
    → Cursor extractor (position + image from PipeWire metadata)
      → Transport: video datagrams + cursor ControlMessages
        → Client: video texture + cursor texture overlay
```

## Files Changed

- `crates/stargaze-core/src/config.rs` — add `CursorConfig`, embed in `ServerConfig`
- `crates/stargaze-server/src/capture/portal.rs` — accept `show_cursor` param, set cursor mode
- `crates/stargaze-server/src/capture/mod.rs` — thread `show_cursor` through `CaptureConfig` → portal
- `crates/stargaze-server/src/main.rs` — CLI flag, pass config to capture
- `crates/stargaze-core/src/transport.rs` — add `cursor_embedded` to `SessionResponse`
