# HDR-safe desktop streaming: deployment validation

## Issues to Address

- The direct, color-described Wayland backend is now implemented and merged
  into `feature/hdr-safe-streaming`. The installed legacy portal still exports
  an SDR copy; changing package pins alone does not select direct capture.
- Prepare NixOS pins and launch settings for the personal Hyprland fork's `hdr`
  branch and this integrated Stargaze branch. Activation and real-session
  restarts remain separately authorized operations.
- Validate capture → transport → AMD client in real conditions, loss/reconnect
  behavior, native-camera/game content, and hours-long buffer-lifetime stability.

## Important Notes

- Preserve athena's permanent HDR, SDR mapping 0–160, saturation, disabled
  automatic profile switching and disabled direct scanout. Do not change
  calibration to disguise errors.
- No activation or service restart is authorized merely by preparing pins.
  Keep port 9002 untouched until the user explicitly approves real-session tests.
- Source color is explicit, not inferred from storage depth, EDID or the peer.
  The default portal remains SDR. Direct `--use-hdr false` requests genuine
  compositor-rendered sRGB; strict HDR-only producers are rejected instead of
  silently tone-mapped or relabelled.
- Retain independent DMA-BUF ownership, precision, VUI/static SEI, self-contained
  IDRs, and encoded-frame loss recovery. Production intentionally uses an
  infinite GOP; test retention must be bounded.
- Keep Nix runtime Mesa/driver libraries matched and NVRTC explicitly at CUDA
  13.2 for the deployed NVIDIA driver. Do not align unrelated nixpkgs pins as a
  substitute for runtime compatibility.
- New direct-capture commits have been re-signed. Publishing personal-fork/HDR
  branches is authorized; no upstream issues/PRs or automated conversations.

## Implementation Strategy

- Completed: HDR media path and protocol policy; private Hyprland compositor
  export; direct Wayland backend and CLI; isolated real GPU pixel comparisons,
  FP16 Vulkan photographic playback, mixed SDR/HDR composition, source changes,
  and minute-scale capture-to-Main10 measurements.
- Completed direct-backend plan is removed after integration. Durable contract,
  reproduction, evidence and limitations live in `docs/hdr-streaming.md` and
  the Hyprland fork's HDR docs/validation record.
- Next: evaluate/build the prepared NixOS configuration, then obtain explicit
  activation approval. Verify the extension's advertised capability, selected
  output/socket, owned DMA-BUF capture, P010/Main10 encoding, peer negotiation,
  AMD decode/presentation, and reconnect behavior from actual runtime evidence.
- Preserve tested exact locks before later rebasing/signing or deployment.

## Tests

- Required Nix-shell fmt/check, all-target pedantic Clippy and nextest: last
  integrated baseline 287 passed, 34 hardware-dependent skips; existing warnings
  remain, with no new capture/build diagnostics.
- Isolated native 3440×1440 capture → Main10: 144.99 fps over 120 seconds,
  stable FDs and <1 MiB RSS growth. SHM is correct but slower (~51.49 fps).
- FP16 Vulkan photographic playback and SDR window: exact capture composition
  and SHM/DMA agreement, ~59 dB Main10 YUV comparison. Authored demo variants are
  synthetic HDR/SDR, not native-camera/game content. Source output transitions
  terminate safely. These are not network/display latency measurements.
- Retain native HDR bitstream/metadata, RGB10 DMA-BUF, explicit SDR, old/new peer,
  capability rejection, protocol lifetime, and loss/IDR tests. Repeat AMD client
  and real-session tests only with the user's approval for physical output.
