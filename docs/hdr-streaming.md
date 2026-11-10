# HDR streaming — single-user NVIDIA → AMD target

Stargaze now has a native **HEVC Main10 / P010 / BT.2020 / PQ** path, including
10-bit color-managed Wayland presentation. It targets zeus's NVIDIA/NVENC encoder
and athena's AMD/VAAPI decoder, not a portable matrix of hardware backends.

**Output HDR and capture HDR are independent.** Zeus's headless `Main` now runs
at 3440×1440/145 with XBGR2101010 buffers, the `hdr` preset and SDR mapping 0–160.
However, the audited Hyprland 0.57 portal path still renders an **sRGB sharing
copy**, confirmed again after enabling that HDR output. Normal desktop capture
therefore still streams SDR. Relabeling this copy PQ would be wrong; native HDR
requires a preserving source/export, described below.

## Use it

### Current portal: SDR capture, permanently HDR client

Launch normally; no override or desktop switching is needed:

```sh
# zeus
stargaze-server --resolution 3440x1440 --framerate 145 --bitrate 80
# athena
stargaze-client --server zeus
```

The single-user default recognizes the **Hyprland/XDPH sRGB-copy contract**, even
when the portal omits color fields. This is not a deduction from 8-/10-bit storage
or the client's display. `--strict-capture-color` rejects incomplete metadata for
other producers. The previous `--allow-untagged-sdr` flag remains a hidden
compatibility alias but is no longer required for the target setup.

Athena's HDR output, SDR 0–160 luminance mapping, saturation 1, disabled automatic
switching, and disabled direct scanout remain unchanged. SDR is presented as SDR;
Hyprland applies the existing SDR output mapping exactly once.

HDR selection is automatic **only for the color-described sources this backend
can actually capture**. To opt out, use `stargaze-server --use-hdr false` or set
`use_hdr = false` in `server.toml`. The default is true; an explicit CLI value
overrides the file. This does not change either machine's compositor settings.
An HDR producer with HDR disabled is rejected before encoder creation, not
truncated or relabeled as SDR. The existing Hyprland portal remains a genuinely
SDR source with either setting.

### Native HDR diagnostic stream

This is an explicit HDR source, **not desktop capture**. It contains absolute-PQ
luminance bars (including black, 100/160/203-nit whites and 1000/4000/10000-nit
highlights), a ten-bit gradient, and BT.2020 primary colors:

```sh
# zeus; use another port if the regular server is running
stargaze-server --hdr-test-pattern --port 9003 \
  --resolution 3440x1440 --framerate 145 --bitrate 80
# athena; windowed is convenient for initial visual inspection
stargaze-client --server zeus --port 9003 --fullscreen false --usb-forward false
```

The pattern contains bright HDR highlights; no test launches visible full-screen
patterns automatically. Its mastering/content-light metadata describes the
synthetic source, not the MSI monitor. The advertised frame rate is an upper
bound, not a guaranteed hardware throughput measurement.

### A real color-described HDR producer

The portal cannot recover HDR highlights already mapped into its SDR sharing
copy. To ingest a producer that actually preserves HDR on the user's PipeWire
instance, select its node explicitly:

```sh
stargaze-server --pipewire-node NODE_ID --framerate 145 --bitrate 80
```

This path **always requires authoritative SPA metadata**. Supported HDR input is
packed RGB10, full-range RGB matrix, BT.2020 primaries and ST2084/PQ transfer.
SPA enums are range=1, matrix=1, transfer=14, primaries=7; these are not CICP codes.
No flag forces untagged PQ, converts sRGB into pretend HDR, or bypasses these
checks. P010/HLG/YUV capture is not currently an input format; P010 is the encoded
and decoded stream layout. Capture resolution comes from the first real buffer,
not the requested dimensions or an initial, unfixated format callback.

If the producer has authoritative static mastering/content-light information
outside SPA, supply it with `--hdr-metadata /path/to/source.toml`. It does not
select HDR and is rejected on an SDR source. Both records are optional:

```toml
# Example source values ONLY; do not copy monitor calibration into this file.
[mastering]
primaries = [[35400, 14600], [8500, 39850], [6550, 2300]] # R,G,B xy / 50000
white_point = [15635, 16450]
min_luminance = 1          # 0.0001 cd/m²
max_luminance = 10000000   # 1000 cd/m² (units are 1/10000)

[content_light]
max_cll = 1000             # cd/m²; 0 = unspecified
max_fall = 400
```

When unavailable, metadata stays absent. Stargaze does not invent mastering
primaries, peak brightness, MaxCLL or MaxFALL from pixel format or display EDID.

## Implementation

### Capture and conversion

- Source color and component precision are separate from storage format.
  Every format callback logs original SPA values, including missing fields.
- Native HDR accepts only fully described full-range BT.2020/PQ RGB at >=10 bits.
  Unsupported transfer, gamut or range is rejected; there is no implicit
  application HDR-to-SDR tone/gamut mapper.
- CUDA converts packed RGB10 or RGBA16 to **limited-range P010 BT.2020 NCL**.
  This matrix operates on existing PQ code values; it does not apply another
  transfer curve, saturation adjustment, or tone mapper.
- DMA-BUF import uses **RGBA16** staging for HDR, retaining precision through
  EGL/GL/CUDA. GPU conversion and the CPU fallback both retain >=10 bits until
  P010 quantization. The fallback never reads HDR into RGBA8.
- SDR keeps the existing low-latency NV12 paths, corrected limited 709 matrix and
  exact neutral chroma. Known sRGB 10-bit capture can be reduced to SDR 8-bit;
  this is precision reduction, not HDR tone mapping.

### Encode and metadata

NVENC uses Main10/P010 for HDR and Main/NV12 for SDR, with no B frames, added
lookahead, vsync, or media buffering. Context and submitted-frame VUI agree:
HDR is limited BT.2020 NCL / BT.2020 / PQ; SDR is limited 709 matrix/primaries with
sRGB transfer (matrix conversion does not apply a BT.709 OETF).

FFmpeg 7's NVENC path does **not** emit static mastering/content-light SEI from
AVFrame side data. Stargaze therefore serializes HEVC prefix SEI types 137/144
itself, with GBR wire ordering and emulation prevention. It repeats available
metadata on every keyframe alongside the existing self-contained parameter-set
handling. Metadata changes force an IDR; bitrate-triggered encoder recreation
reapplies the source metadata. Hardware tests decode and compare the actual
VUI, bit depth, mastering coordinates and light levels.

### Negotiation and decode

The trailing versioned `SGCL` session extension retains SDR compatibility with
old peers. New clients probe Wayland parametric PQ/BT.2020, luminance, mastering
and rendering-intent support before advertising HDR. EGL and decoder creation
must still succeed; a protocol capability alone is not proof of hardware support.

The server selects color from the **source**, never the client's HDR output.
An HDR source is rejected for an SDR-only/older peer, with a reliable
`SessionRejected` reason. It is not silently sent as SDR. Malformed color
extensions cannot downgrade to a legacy interpretation.

Before either DMA-BUF export or CPU transfer, the decoder validates actual color
and software layout of each hardware frame. HDR must be P010/YUV420P10LE,
limited, BT.2020 NCL, BT.2020, PQ. Full-range YUV, HLG, mismatched precision,
changed color, or invalid/incomplete static metadata is rejected. Only historical
SDR peers retain their unspecified/BT.709-transfer compatibility interpretation.

VAAPI P010 stays ten-bit on GPU or after CPU transfer. Software Main10 output is
repacked losslessly from planar ten-bit words into P010. Frame properties and
HDR side data survive both export and transfer/extraction.

### Presentation

The existing SDL input/event loop stays on the main thread. A small C bridge
adds **color-management-v1 on SDL's native wl_surface**, using an isolated event
queue on the same libwayland connection. SDL's GL backend creates surface
feedback, not an existing color surface, so this does not duplicate an SDL-owned
color-management object.

For HDR, Stargaze requests **and checks** actual >=10-bit EGL R/G/B buffers,
creates a compositor-acknowledged BT.2020/PQ image description, and commits it
with EGL pixels. The surface describes an absolute PQ signal, zero black and
203-nit reference white; source mastering/content-light metadata refines the
separate target color volume when available. Metadata description changes are
acknowledged only on change, not once per video frame.

Shaders convert limited BT.2020 YUV back into PQ RGB code values without a
second transfer conversion. P010 DMA-BUF R16/GR1616 textures use explicit
**highp samplers**—Mesa's default lowp samplers produced magenta output during
hardware testing. CPU uploads use GLES3 integer textures with explicit bilinear
sampling, matching normalized DMA-BUF filtering without depending on
`EXT_texture_norm16`. A failed DMA-BUF import can fall back to CPU **P010 on the
same HDR surface**, never to an 8-bit SDR canvas. Failure to create an HDR surface
is a clear session error.

The HDR stats panel is opaque to avoid incorrect alpha blending in PQ space;
its sRGB UI colors are converted to BT.2020/PQ at 160-nit white. This changes
neither desktop calibration nor the video's absolute luminance. SDR rendering
and overlays keep their original presentation path.

EGL YUV import hints describe an input image, not a Wayland surface. Requesting
ten bits alone would not implement HDR; the explicit surface description is
essential. This implementation works with packaged sdl2-compat/SDL3 internally
without replacing the application's SDL2 input and audio APIs.

## Reconnects and changes

Each handshake renegotiates capabilities; decoder recreation retains its color
contract. Each capture format invalidates the previous interpretation before
validation. A source SDR↔HDR or resolution change stops the fixed-mode pipeline
for a process restart and fresh handshake, rather than sending stale VUI or
reusing the wrong conversion path. An external service supervisor can restart
it; a client reconnect alone cannot resurrect a dead capture pipeline.

Moving a color-described surface between outputs leaves output conversion to
Hyprland. Stargaze does not toggle desktop HDR, direct scanout, calibration,
content profiles or output refresh rate. The existing frame-order/IDR recovery,
bounded media channels, decode-every-frame and render-latest policies remain.

## Audit evidence and upstream limit

Baseline: `8316a84`, following the deployed revision
`1628f7cc600a8180047c10670287b14ff7f07226`.

Before the user enabled the virtual HDR output, read-only checks found zeus's
headless `Main` output at 3440×1440/145, XRGB8888,
sRGB, on an RTX 3090/NVIDIA 595.112.07. Athena's MSI `DP-2` was 3440×1440/174.962,
XRGB2101010/HDR, with SDR 0–160 and saturation 1. The live XDPH/consumer format
pods contained BGRA/modifier/size/rates, **no color or HDR mastering fields**.

The deeper implementation audit establishes why: Hyprland 0.57's
`ScreenshareFrame::beginCopy` and shared-memory copy explicitly assign
`DEFAULT_SRGB_IMAGE_DESCRIPTION`; XDPH `build_format`/`fixate_format` does not
forward SPA color fields. The wlr-screencopy protocol itself has no HDR color
contract. This branch does not patch/restart Hyprland or falsely call that SDR
copy native HDR capture. A future compositor/producer preserving HDR can use the
strict node input; the native media/presentation pipeline is already implemented.

### Installed-stack re-audit during the handoff

Read-only checks on zeus confirmed the running binaries, not just upstream
protocol documentation:

- Hyprland **0.57.0**, commit
  `4b9ac40d71ceeeb027cfa06992c9212e3c3077b6` (Nix build marked dirty).
- XDPH **1.5.0**, revision prefix `d1467fe`; xdg-desktop-portal **1.22.1**.
- Initially, `hyprctl -i 0 monitors -j`: headless `Main`, 3440×1440 at 145 Hz,
  XRGB8888, `colorManagementPreset: srgb`, EDID `hdr: false`.
- The live registry offers wlr-screencopy v3, Hyprland toplevel-export v2,
  ext-output-image-capture-source v1, ext-image-copy-capture v1, workspace
  capture and color-manager v3. It does **not** offer wlr-export-dmabuf.
- The exact installed Hyprland source was available in the Nix store.
  `Screencopy.cpp` and `ToplevelExport.cpp` use managed screenshare sessions;
  `ImageCopyCapture.cpp` creates sessions through the same screenshare manager.
  All use `ScreenshareFrame.cpp`: `beginCopy` sets the export framebuffer to
  `DEFAULT_SRGB_IMAGE_DESCRIPTION`; `copyShm` does so for its readback target
  too. Merely binding color-manager changes `cmBackToSRGB` but does not
  negotiate a PQ export framebuffer. Workspace capture additionally fixes its
  scene resources to ARGB8888/sRGB.
- The installed XDPH `ScreencopyShared.cpp` `build_format` and `fixate_format`
  still omit source color fields. A fresh live portal capture negotiated
  BGRA8 at 3440×1440, absent SPA color fields, then concrete NVIDIA DMA-BUF
  modifiers. Its encoder selected NV12/Main SDR, correctly.

**There is no audited supported HDR-preserving desktop export on this installed
stack.** Switching from wlr-screencopy to ext-image-copy, requesting ten bits,
reading output color-management information, or attaching metadata cannot
recover values mapped into the sRGB copy. Headless also rules out a physical
KMS scanout capture workaround for `Main`.

### Runtime-verified virtual HDR output, still SDR capture

The user explicitly authorized configuring `Main` in `/etc/nixos`, applied that
configuration and restarted the remote-desktop service. Configuration commit
`2a57615` on `feature/zeus-headless-hdr` passes per-monitor color settings through
the launcher, with `supports_hdr = 1`, `supports_wide_color = 1`, `cm = "hdr"`,
`bitdepth = 10` and SDR mapping 0–160 for `Main` only. Other presets remain SDR;
athena is unchanged.

The user's output and an independent read-only check agree: `Main` has
**XBGR2101010**, **hdr**, **3440×1440 at 145 Hz**, SDR minimum **0**, maximum
**160**, brightness/saturation **1**, and direct scanout blocked. This verifies
the selected ten-bit output format and active color preset, rather than just
configuration-file intent. `hardwareDetails.hdr/chroma/bt2020` still report false: those fields
read parsed EDID, which a headless output lacks, rather than the forced capability
overrides. Preferred surface color, real HDR application behavior, captured
highlight values and panel luminance still require separate validation.

A fresh `test_capture_encode_pipeline` run from this branch against the restarted
desktop passes but negotiates **BGRA8**, absent SPA range/matrix/transfer/primaries,
and concrete NVIDIA DMA-BUF modifiers. It selects **sRGB → NV12/Main SDR**.
The active older deployment on port 9002 independently logs BGRA8 capture and
the NV12 GPU converter. `misc:screencopy_force_8b` currently reads true; that
explains the eight-bit storage, but disabling it would not undo the sRGB export
contract. It was left unchanged, and the active service was not interrupted by
the test. No PQ labels or invented content mastering metadata were added.

The **source desktop output** is now HDR-configured, but its **capture** copy is
still SDR.
The next capture implementation needs a compositor-side, opt-in color-described
export contract and a matching consumer/portal bridge. It must render/export
before irreversible SDR mapping, negotiate PQ/BT.2020 and >=10-bit storage for
HDR, retain a genuine sRGB export for SDR/opt-out, and associate color/geometry
changes with the correct buffers. A global change making legacy screenshares
PQ is not acceptable. Build and test that stack in an isolated session before
considering replacing zeus's compositor: restarting the current desktop would
interrupt the deployment on port 9002. No compositor patch or HDR desktop
capture implementation is included yet; this remains active work in
`plans/hdr-safe-streaming.md`.

References:

- [Hyprland 0.57 capture framebuffer handling](https://github.com/hyprwm/Hyprland/blob/v0.57.0/src/managers/screenshare/ScreenshareFrame.cpp)
- [XDPH format builders](https://github.com/hyprwm/xdg-desktop-portal-hyprland/blob/master/src/shared/ScreencopyShared.cpp)
- [Wayland color-management-v1](https://wayland.app/protocols/color-management-v1)
- [FFmpeg 7 NVENC SEI handling](https://github.com/FFmpeg/FFmpeg/blob/n7.1/libavcodec/nvenc.c)

## Validation and packaging

The original media-path hardware tests below did not change either desktop's
color configuration or replace the deployed server. The subsequent user-applied
virtual-output change and its capture regression are documented above:

- **zeus:** Main10 VUI and static-SEI round trips; >8-bit gradients and colored
  RGB10/RGBA16 CPU/CUDA agreement; real RGB10 GBM DMA-BUF → RGBA16 → CUDA/P010
  encode and precision-preserving CPU fallback; ordinary SDR portal capture.
- **Real PipeWire HDR input:** `tools/hdr-pipewire-source.c` publishes explicit
  BT.2020/PQ RGB10 buffers. The ignored `hdr_pipewire_source` test captures them
  through normal format negotiation, then verifies Main10/PQ output.
- **athena:** actual Main10 VAAPI decode, P010 DMA-BUF and CPU presentation,
  10-bit EGL verification, acknowledged PQ surface/mastering descriptions,
  HDR gradient and colored-frame CPU/GPU agreement. SDR rendering on the
  permanently HDR desktop is regression-tested as well.
- Unit/network tests cover old/new peers and explicit rejection reasons,
  SDR/HDR combinations, full/limited range rejection, black/white/reference
  whites/highlights, gradients, metadata and software ten-bit repacking.

These are not panel-luminance measurements, proof of native HDR capture from the
SDR Hyprland portal, or a guarantee of 3440×1440/145-fps sustained HDR throughput.
Visual/reference-meter calibration and a live HDR desktop producer remain
separate validation tasks. Do not mistake an SDR sharing copy for HDR content.

Builds now require libwayland-client, wayland-scanner and wayland-protocols with
color-management-v1 (>=1.41). Nix supplies them for dev shells/packages.
`mkStargazeClient` explicitly includes wayland-protocols in its actual
`buildInputs` (for both normal and portable clients): that derivation replaces
the shared inputs, and placing it only in native inputs did not expose
`wayland-protocols.pc` to the build script. The normal and portable clients and
the wrapped server were rebuilt without derivation overrides during the handoff. Mesa
loader/driver paths remain pinned together for the client. NVRTC stays pinned to
CUDA 13.2: floating CUDA 13.4 produced PTX the deployed driver could not load,
which otherwise silently forced CPU conversion.

Current handoff validation on zeus: formatting and workspace check pass;
all-target pedantic Clippy has no new diagnostic categories/files against
`b7904f6` (pre-existing warnings remain); workspace nextest passes **282 tests,
28 skipped**. NVIDIA Main10/static-metadata round trip, CPU/GPU HDR conversion,
real RGB10 DMA-BUF paths and live SDR portal capture pass again. The wrapped
server's synthetic HDR source with `--use-hdr false --port 10401` exits with an
explicit no-tone-mapper rejection. Neither the deployment on 9002 nor compositor
configuration was changed. AMD presentation was **not rerun** during this
handoff; the earlier successful user-observed zeus→athena synthetic stream
remains evidence at 1920×1080/60, not actual HDR desktop capture or a target-rate
benchmark. That run used a 100.x address, not a demonstrated clean LAN path.

Reproduce the main hardware tests:

```sh
nix develop .#cuda -c cargo test -p stargaze-server native_hdr10_round_trip -- --ignored
nix develop .#cuda -c cargo test -p stargaze-server hdr_gpu_input_formats -- --ignored
nix develop .#cuda -c cargo test -p stargaze-server hdr10_dma_buf_paths -- --ignored
# Generate the native NVIDIA fixture, then copy it to athena:
STARGAZE_TEST_DUMP_HEVC=/tmp/native.hevc \
  nix develop .#cuda -c cargo test -p stargaze-server native_hdr10_round_trip -- --ignored
# On athena, with its Wayland session environment and matching Mesa paths:
STARGAZE_TEST_HEVC=/tmp/native.hevc \
  nix develop -c cargo test -p stargaze-client hdr10_ -- --ignored --test-threads=1 --nocapture
```

For the PipeWire test, compile/run `tools/hdr-pipewire-source.c` using the command
in its header, then use its printed node id as `STARGAZE_TEST_PW_NODE` when running
`nix develop .#cuda -c cargo test -p stargaze-server hdr_pipewire_source -- --ignored`.
