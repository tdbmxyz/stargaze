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

### Stale virtual gamepads after session disconnect

The server can retain an emulated Xbox 360 pad after its client session
ends: gamepad removal currently relies on a `GamepadDisconnected` event,
not connection teardown. Observed on zeus during the 2026-10-07 Steam
Controller live test: an Xbox pad from the earlier session was still
present before the new USB-forwarding client connected, and no new Xbox
pad was created during that test. Do not confuse this leftover device
with the genuinely forwarded Steam Controller. Follow-up: clear
session-owned gamepads and held input when a connection ends, including
abrupt disconnects.

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

Bit depth does not identify HDR or color space. The single-user NVIDIA→AMD path
now supports native Main10/P010 BT.2020/PQ streaming and an explicitly described
10-bit Wayland surface, including precision-preserving DMA-BUF and CPU paths.
Hyprland's ordinary portal still renders an sRGB sharing copy; its known SDR
contract works by default, while explicit PipeWire node capture requires
metadata. Native desktop capture is implemented with `--wayland-output Main`
and the personal Hyprland fork's opt-in experimental color-management extension.
The NVIDIA → AMD desktop stream has been tested in a real session; isolated
capture-to-Main10 sustained 144.99 fps at 3440×1440 for two minutes.

See [HDR usage, source limitations and hardware evidence](hdr-streaming.md).
Remaining work:

- Hours-long capture/transport/decode/presentation soaks, controlled native HDR
  game/camera comparisons, loss/reconnect stress and device migration. The short
  user session and isolated throughput result are not panel-luminance or
  glass-to-glass latency measurements.
- Seamless source SDR/HDR and geometry changes. The current immutable contract
  stops capture safely for a supervisor restart and fresh negotiation.
- Startup keyframe readiness and decoder-backpressure/recovery diagnostics;
  clearer HDR rejection messages for older SDR-only peers.
- Correct encode timing statistics before drawing performance conclusions from
  near-zero reported encode time.
- An optional real application HDR→SDR tone/gamut mapper. Direct capture can
  request genuine compositor-rendered sRGB; HDR-only producer input is otherwise
  rejected, never truncated or relabelled. Keep desktop HDR/calibration unchanged.

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
