# HDR-safe streaming

## Issues to Address

- Capture precision is not colorimetry. Current capture discards SPA color fields; actual zeus XDPH format pods have none.
- NVENC is NV12/8-bit with hardcoded 709 metadata. CUDA/CPU fast converters are limited-range 709; swscale fallback uses default coefficients and truncates 10-bit without tone mapping.
- Client assumes limited 709 in every presentation path; decoded color metadata is discarded, and arbitrary decoded formats are downconverted.
- SDL2 GLES surface has no HDR image description. Nix sdl2-compat using SDL3 internally does not add an HDR application API.
- Existing postcard session compatibility requires preserving legacy enum tags and prefixes; malformed extensions must not be interpreted as old SDR.

## Important Notes

- Preserve athena's permanently enabled desktop HDR, 0–160 SDR mapping, saturation, disabled automatic switching and direct scanout. Do not edit compositor configuration.
- Read-only hardware audit: zeus Main is headless 3440x1440/145, XRGB8888, srgb; athena DP-2 is 3440x1440/174.962, XRGB2101010, hdr. Live XDPH and capture Format contain BGRA/modifier/size/framerate, no color description or mastering/content light metadata. XDPH build_format/fixate_format confirms this omission.
- Audit conclusion: absent metadata cannot guarantee correct interpretation at ANY bit depth. Default to rejecting untagged capture. An explicit --allow-untagged-sdr assertion preserves historical 8-bit RGB for a known SDR source, with a warning and a documented inability to detect unannounced transitions. Untagged 10-bit and YUV remain rejected. Explicit contradictory metadata must always fail closed.
- No HDR-to-SDR fallback exists. Reject PQ/HLG/wide-gamut/unsupported modes, including transitions, before conversion/export.
- Native HDR10 cannot be advertised by this backend: no trustworthy capture contract, no precision-preserving EGL staging, no HDR Wayland surface. Keep that limitation explicit instead of claiming hardware support from NVENC/VAAPI availability.

## Implementation Strategy

1. Audit and document complete pipeline, upstream contracts, observed deployment and missing evidence.
2. Add shared color descriptions/capabilities and backward-compatible session extensions. Negotiate only implemented SDR, explicitly reject HDR10 selection or unsupported descriptions. Preserve old SDR peers and validate extension boundaries strictly.
3. Parse/log raw SPA color metadata on every format callback. Validate before buffers can leave capture; stop on unsafe changes. Preserve supported SDR fast paths. Correct swscale matrix/range and cache invalidation; reject unsafe fallback formats.
4. Validate decoded color metadata/precision before VAAPI export or CPU transfer and reject unsafe transitions rather than silently scale. Keep presentation as SDR sRGB/default Wayland, with exact digital limited-range expansion and no double gamma/saturation conversion.
5. Regression tests and hardware validation on available zeus/athena without changing calibration or replacing the deployed service. Document what remains unverified.
6. Follow-up (not advertised as implemented): native HDR requires a color-described capture protocol, P010 CPU/GPU conversion, Main10 VUI+SEI, VAAPI P010 import and a color-managed >=10-bit Wayland surface (likely SDL3/native Wayland migration). Negotiate it only after all stages and output-change handling exist. Real tone/gamut mapping is an independent fallback project.

## Tests

- Legacy/current session prefixes, missing vs malformed extension, unsupported HDR selection.
- Missing/partial/explicit SPA metadata, HDR independent of bit depth, full/limited range, transitions.
- Limited-range black/white/neutral gradients and color vectors; swscale vs direct converter agreement.
- Decoded PQ/HLG/2020/full-range/10-bit rejection before either export or scaling.
- Existing workspace fmt/check/pedantic all-targets clippy/nextest; targeted NVIDIA encode round-trips and available AMD renderer tests. Hardware tests do not prove HDR fidelity or panel luminance.

## Implementation State

- Audit and safety stages 1–5 implemented on this branch; full evidence and deployment migration are in `docs/hdr-streaming.md`.
- Default capture policy deliberately rejects missing metadata; --allow-untagged-sdr is a user assertion for a known SDR source, not inferred SDR detection.
- Workspace: 268 passed, 22 ignored; no added clippy diagnostic categories/file occurrences against baseline. NVIDIA CUDA gradient agreement, NVENC color/VUI round trips and live portal capture pass. AMD VAAPI SDR export/transfer and HDR rejection, plus GL neutral/color tests, pass with desktop HDR unchanged and packaged Mesa paths.
- Stage 6 (native HDR10 and real tone/gamut mapping) remains unimplemented and unadvertised. Current source/presentation contracts do not support it safely; every unsupported mode fails closed. Keep this plan active until that follow-up is resolved; do not describe native HDR10 as implemented.
- No deployed service, desktop calibration, or NixOS pin changed. No push/CI triggered.
