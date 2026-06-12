# Stargaze — Roadmap

Follow-up tasks after the MVP implementation. Ordered roughly by priority.

## 1. Code Quality & Documentation Cleanup

- [ ] **CLI help text**: Remove manually written `[default: X]` from clap doc comments where `default_value_t` already causes clap to display the default. Uniformize the pattern across both server and client CLI structs.
- [ ] **Rustdoc coverage**: Audit public API surface for missing or incomplete doc comments (`# Arguments`, `# Returns`, `# Errors` sections).

## 2. End-to-End Testing & Hardening

- [ ] **Graceful shutdown**: Verify all pipeline stages (capture, encode, transport, decode, render, input, audio) shut down cleanly on Ctrl+C without hanging threads or leaked resources.
- [ ] **Error recovery**: Handle transient failures (PipeWire disconnects, encoder stalls, QUIC timeouts) with reconnection or clean error reporting instead of panics.
- [ ] **Frame loss resilience**: Test behavior under packet loss — verify IDR-on-loss strategy recovers video within a few frames.
- [ ] **Audio/video sync**: Measure and correct drift between audio and video streams over long sessions (>10 min).

## Known Issues & Workarounds

### Zero-copy rendering and nvidia-vaapi-driver

The client's zero-copy path (VAAPI decode → DRM PRIME dma-buf export → EGL
import → GL render) is enabled automatically for hardware decoding on
Mesa drivers (AMD/Intel). On NVIDIA's VAAPI shim (nvidia-vaapi-driver) it
is blocklisted: exporting a surface poisons the decoder (every subsequent
`vaBeginPicture` fails with `MAX_NUM_EXCEEDED`) and the exported planes
read as zeros (verified on driver 595.71.05). `STARGAZE_FORCE_ZERO_COPY=1`
re-enables it for testing newer drivers; `STARGAZE_NO_ZERO_COPY=1` forces
the CPU path anywhere. If the path misbehaves at runtime, the client
detects the failure, recreates the decoder, requests an IDR, and continues
with CPU frames.

### 10-bit Display Formats

Systems running 10-bit color depth (e.g. Hyprland with `misc:screen_bit_depth = 10`) expose 10-bit DRM formats (`xBGR2101010`, `ABGR2101010`) through the portal. Stargaze accepts these formats and converts to 8-bit in the encode pipeline.

If capture still fails with `no more input formats`, check your compositor:
- **Hyprland ≥ v0.43**: `misc:screencopy_force_8b = true` (enabled by default) forces the portal to expose 8-bit formats only.
- **Older Hyprland**: Set `misc:screen_bit_depth = 8` in `hyprland.conf`.

## 3. Latency Measurement

- [ ] **Pipeline instrumentation**: Add timestamps at each pipeline stage (capture → encode → transport → decode → render) to measure per-stage latency.
- [ ] **End-to-end latency reporting**: Calculate and log total glass-to-glass latency.
- [ ] **Latency optimization**: Identify and reduce bottlenecks based on measurement data.

## 4. Client-Side Cursor Overlay

- [ ] **CursorMode::Metadata support**: Server requests cursor metadata from the portal instead of embedding it in frames.
- [ ] **Cursor transport**: New `ControlMessage` variants for cursor position and shape data.
- [ ] **SDL overlay rendering**: Client renders cursor as an independent RGBA texture layer, reducing perceived cursor latency.
- [ ] **Config extension**: `cursor_mode = "embedded" | "overlay" | "hidden"` replacing the current boolean.

## 5. Nix Build & Packaging

- [ ] **Nix package**: `flake.nix` outputs a buildable package (not just devShell) for easy installation.
- [ ] **NixOS module**: Optional NixOS service module for running the server as a systemd unit.

## 6. Future Features (Post-MVP)

- [x] **VAAPI decode**: Hardware-accelerated decoding on AMD/Intel clients (with zero-copy dma-buf rendering; see Known Issues for the NVIDIA shim caveat).
- [ ] **AV1 encoding**: Alternative to H.265 for better quality-per-bit.
- [ ] **Adaptive bitrate**: Dynamically adjust encoding bitrate based on network conditions.
- [ ] **Multi-monitor**: Support capturing and streaming individual monitors or regions.
- [ ] **NAT traversal**: STUN/TURN or hole-punching for streaming over the internet.
- [ ] **Authentication**: Token-based or PIN pairing for secure connections.
