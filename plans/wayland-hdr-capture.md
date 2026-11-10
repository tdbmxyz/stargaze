# Direct color-described Wayland capture

## Issues to Address

- The deployed portal exports SDR even on an HDR-configured output.
- The private Hyprland fork now offers per-frame color-managed image-copy capture.
- A direct backend must preserve both capture color and DMA-BUF ownership through encoding, without changing the existing portal path.

## Important Notes

- This worktree stacks on `feature/hdr-safe-streaming`; do not switch or alter the other session's worktree.
- Keep the active remote desktop/port 9002 and system configuration untouched. Deployment/restarts require separate approval.
- Pin the experimental XML identically to `/projects/Hyprland/protocols/xx-image-capture-color-management-v1.xml` (wayland-protocols aa62366fb800a0689f4e9de83811ff33f6a91f44).
- Select color from the explicit frame contract, never storage depth or a streaming peer. A preferred description is only a hint for choosing a requested encoding.
- Stop on geometry/preferred-color/source loss; the current streaming pipeline is fixed-mode.
- Initially allocate independent DMA-BUF storage per delivered frame. An owned exported FD keeps storage alive after Wayland/GBM wrappers are destroyed; never reuse storage while the encoder may read it. Pooling needs a separate lease contract and measurement.
- SHM fallback is explicit and retains RGB10 precision. No silent SDR fallback for an HDR contract.

## Implementation Strategy

- Add a Wayland C bridge following the client's existing generated-protocol bridge pattern, with bounded dispatch and no GL context on the capture thread.
- Add a dedicated-thread Rust backend producing existing color-described `CapturedFrame` values. Keep PipeWire/portal behavior unchanged when direct capture is not requested.
- Expose an explicit output selector and SHM diagnostic option. Choose PQ/BT.2020 only when supported and requested by policy; otherwise request genuine sRGB capture.
- Validate the direct backend against the isolated native RGB10 fixture, then exercise the existing Main10 encoder with those actual desktop frames.

## Current validation

- Server build generates all referenced protocol code (including foreign-toplevel
  list), compiles the C bridge, and links Wayland/GBM/libdrm. Nix server package
  builds without dependency overrides.
- Fresh isolated Hyprland package passes 868 unit tests; its native RGB10 fixture
  has exact full-frame SHM/real DMA-BUF agreement.
- Direct capture tests pass for both transports: all neutral/primary SHM codes,
  retained-buffer lifetime past shutdown, actual NVENC Main10 software decode,
  explicit SDR, full-channel shutdown, and unsupported-compositor rejection.
- Workspace fmt/check and all-target pedantic Clippy have no new capture/build
  diagnostics; nextest passes 287 tests with 34 hardware-dependent skips.
- Exact local Git packages build: Hyprland compositor `b9df23ef7`, server `42036cd`.
  Commits are unsigned by user approval, pending rebase/signing.
- Two-minute 3440×1440 DMA-BUF capture → Main10: 144.99 fps, stable FDs, <1 MiB
  RSS growth. Phase-anchored pacing removes wake-up drift; no catch-up bursts.
  SHM is correct but only 51.49 fps at that resolution.
- FP16 Vulkan mpv photographic playback plus an independent SDR window: exact
  raw composition/SHM-DMA agreement; ~59 dB YUV Main10 comparison; minute-long
  dynamic 3440×1440 capture/encode at 144.67 fps. Its application content is
  25 fps and the demo labels its HDR/SDR variants synthetic, not native-camera
  HDR. Resize/color/removal terminate capture; SDR window movement is validated.
- Evidence and reproduction: `docs/hdr-streaming.md` and the Hyprland lab README.
  All labs are stopped; no deployed service or system configuration was changed.

## Remaining work

- Native-camera HDR/game content, device migration, and hours-long capture
  shutdown/lifetime stress. Minute-scale fixtures/application playback are not
  production validation.
- Target-rate transport/loss/reconnect testing and AMD client presentation.
- Rebase/sign the authorized unsigned commits and rebuild final exact pins
  before any separately approved deployment. Do not reuse DMA-BUF storage
  without a lease contract.

## Tests

- CLI conflicts and source-selection policy; metadata/layout compatibility and safe startup failure.
- Ignored live SHM/DMA-BUF capture of the native ten-bit fixture, precision and descriptor checks.
- Capture-to-NVENC integration, shutdown and unsupported-compositor behavior.
- Workspace fmt/check, all-target pedantic Clippy and nextest in the Nix shell. Do not treat software tests as hardware verification.
