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

- [ ] **VAAPI decode**: Hardware-accelerated decoding on AMD/Intel clients.
- [ ] **AV1 encoding**: Alternative to H.265 for better quality-per-bit.
- [ ] **Adaptive bitrate**: Dynamically adjust encoding bitrate based on network conditions.
- [ ] **Multi-monitor**: Support capturing and streaming individual monitors or regions.
- [ ] **NAT traversal**: STUN/TURN or hole-punching for streaming over the internet.
- [ ] **Authentication**: Token-based or PIN pairing for secure connections.
