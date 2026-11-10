# HDR-safe desktop streaming

## Issues to Address

- **Actual HDR desktop capture is not implemented.** The native HDR media path accepts explicit HDR producers, but the installed Hyprland/XDPH desktop path exports an sRGB copy.
- Zeus's headless `Main` is now HDR-configured (XBGR2101010, hdr, SDR 0–160), 3440×1440 at 145 Hz, verified after the user applied the authorized NixOS change and restarted it. Real HDR desktop content and preserved capture values remain unverified; the portal still exports SDR.
- Automatic source-driven SDR/HDR selection exists for accepted sources; discovering and consuming an HDR-preserving compositor export is still missing.
- SDR/HDR and geometry transitions currently stop the fixed-mode pipeline. Safe seamless renegotiation/reconnection remains unverified.
- Startup delta frames, decoder backpressure, old-peer rejection diagnostics, zero encode timing statistics and target-rate HDR performance need follow-up.

## Important Notes

- Preserve athena's permanently enabled HDR, SDR mapping 0–160 nits, saturation, disabled automatic profile switching and disabled direct scanout. Do not change calibration or compositor settings to disguise errors.
- Keep port 9002 and its deployment untouched by tests. The user authorized Main's HDR configuration in /etc/nixos and performed its activation/restart; do not make further compositor/system changes without authorization. The graphical client launches locally on athena; preserve the SSH destination exactly as `zeus`.
- HDR requires authoritative source color information, not ten-bit storage, output EDID or an HDR-capable client. Never relabel the existing sRGB portal copy PQ.
- No application tone/gamut mapper exists. `--use-hdr false` / `use_hdr = false` rejects an HDR-only source before encoder creation; it does not truncate precision, relabel PQ or switch either desktop's HDR settings.
- Preserve >=10-bit precision through CPU and DMA-BUF paths, self-contained keyframes, HDR static-SEI repetition and the encoded-frame loss/IDR invariants.
- Nix-wrapped packages avoid bare-binary/mixed-glibc problems. Clear inherited LD_LIBRARY_PATH/LD_PRELOAD when launching a package. Keep packaged Mesa libraries/drivers matched.
- Full audit and runtime evidence: `docs/hdr-streaming.md`. Hardware tests and the user-observed synthetic run are not proof of HDR desktop capture, calibrated luminance or 3440×1440/145-fps HDR throughput.

## Implementation Strategy

1. **Completed media foundation:** explicit color contracts and peer negotiation; full-range RGB BT.2020/PQ → limited P010; precision-preserving CPU/CUDA/RGBA16 DMA-BUF conversion; Main10 NVENC/VUI/static SEI; VAAPI/software ten-bit decode; P010 GL rendering; verified ten-bit EGL and acknowledged Wayland PQ surface/mastering metadata. SDR remains SDR.
2. **Completed handoff re-audit:** tools operate on zeus; running Hyprland 0.57.0 (`4b9ac40d71ceeeb027cfa06992c9212e3c3077b6`), XDPH 1.5.0 (`d1467fe` prefix), portal 1.22.1. Read the installed Nix-store source. wlr-screencopy, toplevel-export and ext-image-copy all share the sRGB export implementation. Binding color-manager does not negotiate HDR capture. No wlr-export-dmabuf global; `Main` is headless, not a physical KMS output.
3. **Virtual output configured; next, compositor export contract:** /etc/nixos commit 2a57615 passes supports_hdr=1, supports_wide_color=1, cm=hdr, bitdepth=10 and SDR 0–160 through Main's remote launcher. The user activated/restarted it. Both the user's report and an independent check show XBGR2101010/hdr at 3440×1440/145. hardwareDetails still describes absent EDID, not these overrides. Preferred surface color, actual HDR application output and preserved pixel values still need verification. A fresh current-branch capture test still selects BGRA8/sRGB → NV12/Main with no SPA color fields; screencopy_force_8b is true and was left unchanged. An HDR output alone does not change the sRGB capture contract. Develop an isolated, pinned compositor/producer stack rather than restart the active desktop. Export before irreversible SDR mapping with a per-session explicit color description, >=10-bit HDR buffers, correct SDR composition/reference white, cursor/privacy masking, and safe buffer/color/geometry transition boundaries. Retain legacy sRGB capture unchanged. A metadata-only patch or globally PQ screenshots is not a solution.
4. **Next, bridge and automatic selection:** expose that opt-in contract through a matching portal/PipeWire producer or direct capture backend. Discover capability automatically; select HDR only for real HDR source values and a supported peer. With HDR disabled, use an explicitly provided genuine SDR rendering or clearly reject. Do not infer an export's color from monitor information alone.
5. **Next, real desktop validation:** establish the isolated server virtual output's actual HDR configuration and use real HDR desktop content. Compare composed SDR and HDR reference content through CPU and DMA-BUF capture on NVIDIA→AMD, then measure the target resolution/frame rate. Preserve athena's settings. Do not deploy until this is demonstrated.
6. **Packaging/policy completed:** normal and portable client derivations explicitly carry wayland-protocols in their overridden buildInputs. Normal/portable client and wrapped server builds succeed without overrides. HDR opt-out defaults on, respects TOML/CLI precedence and fails closed on HDR-only sources.
7. Native media/policy and package changes are checkpointed separately on this feature branch; do not merge/release or delete this plan while desktop capture is incomplete. Signing was resolved using the same identity's working primary OpenPGP key explicitly per commit: the default GPG probe signs with that primary, whereas Git's email selector chooses the signing subkey whose pinentry times out. Signatures are verified good; signing was not disabled and global Git configuration was not changed. No push/CI has been triggered.

## Tests

- Source SDR/HDR × client capability × HDR opt-out; old/new session compatibility and explicit rejection. Test that ten-bit sRGB remains SDR and unsupported transfer/range never downgrades.
- CPU/GPU gradients, black/reference white/highlights, full/limited range, metadata and actual bitstream round trips. Retain hardware tests for true DMA-BUF precision, Main10 VUI/SEI and AMD presentation.
- New compositor/bridge tests must capture an actual composed HDR surface with SDR UI/cursor and verify exported pixel values plus authoritative color, including CPU readback, privacy masks and transitions. Synthetic direct-to-encoder and PipeWire producer tests are not substitutes.
- Required checks in the Nix dev shell: fmt, workspace check, all-target pedantic Clippy, workspace nextest. Current handoff: 282 passed, 28 skipped; no new Clippy diagnostic categories/files against b7904f6 (existing warnings remain).
- Repeated on zeus: NVIDIA native_hdr10_round_trip, hdr_gpu_input_formats, hdr10_dma_buf_paths and live SDR portal capture/encode. After the user's Main HDR activation, test_capture_encode_pipeline passes again (3440×1440 capture, 30-fps test encode); it verifies the current sRGB portal, not HDR capture or 145-fps performance. The normal wrapped server rejects synthetic HDR with --use-hdr false on test port 10401. AMD graphical tests were not rerun in this handoff.
