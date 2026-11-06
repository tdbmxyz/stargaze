# Color safety and HDR audit

## Support boundary

**This is an SDR streaming implementation with HDR safety checks, not a native
HDR10 implementation.** The current end-to-end backend cannot truthfully
advertise HDR10. Unsupported HDR is rejected, never relabeled or truncated to
SDR. There is no HDR-to-SDR tone/gamut mapper.

An SDR stream remains an SDR surface on an HDR desktop. The compositor, not
Stargaze, maps its SDR luminance to the output. Do not disable desktop HDR, change
calibration/saturation, enable direct scanout, or switch content profiles for
Stargaze. In particular, athena's `sdr_min_luminance=0`,
`sdr_max_luminance=160`, saturation 1 and permanently HDR MSI output are unchanged.

### Deploying with an untagged portal

Capture without color metadata now **fails closed by default**, at any depth.
The Hyprland portal audited below omits those fields. For a **known SDR source**
like zeus's currently observed sRGB desktop, explicitly assert the historical
8-bit RGB interpretation:

```sh
stargaze-server --allow-untagged-sdr --resolution 3440x1440 --framerate 145 --bitrate 80
```

This assertion is not detection or tone mapping. It permits incomplete metadata
only for 8-bit RGB and never overrides contradictory metadata. **It cannot detect
an SDR→HDR transition that the producer does not signal.** Keep the source SDR
while using it; remove the assertion if that guarantee is unavailable. Untagged
10-bit, YUV capture, PQ, HLG, wide-gamut, linear or limited-range RGB are rejected.
No client-side desktop setting needs changing. Older SDR network peers are
still supported; this capture safety boundary is separate from wire compatibility.

No deployed service, NixOS configuration, flake-consumer revision pin or display
configuration was changed during this work. Updating a pin from
`1628f7cc600a8180047c10670287b14ff7f07226` alone does not supply the explicit
source assertion above. Do not deploy this branch on an untagged source without
accounting for that intentional startup-policy change.

## Audit: baseline and actual deployment

Audited `develop` at `8316a84` (the integration merge immediately following
`1628f7c`); the reported color assumptions are still present there. Read-only
checks on the actual hosts found:

| Stage | Observation |
|---|---|
| zeus output | Headless `Main`, 3440×1440 at 145 Hz, `XRGB8888`, `colorManagementPreset=srgb`, no HDR capability reported. RTX 3090, NVIDIA 595.112.07. |
| zeus capture | Active XDPH producer and Stargaze consumer both negotiate BGRA, modifier `0x030000000e08014`, 3440×1440. Format pods contain size/modifier/framerate/maxFramerate, **no** range, matrix, transfer, primaries or mastering/content-light metadata. Advertised maximum rate was 120, despite the configured 145 cap; not a measured streaming rate. |
| athena output | Hyprland 0.57, MSI MEG 342C OLED (`DP-2`), 3440×1440 at 174.962 Hz, `XRGB2101010`, HDR preset, saturation 1, SDR 0–160. Other attached outputs are SDR. |

A 10-bit storage format does **not** specify PQ, HLG, BT.2020, HDR mastering
luminance, or whether the compositor has already produced an SDR sharing copy.
Conversely, an 8-bit buffer is not proof of SDR. An HDR output is not evidence of
HDR capture. Output observations contextualize the source assertion; they are
not substituted for missing frame metadata.

### Capture contract investigation

- XDPH `ScreencopyShared.cpp::build_format` / `fixate_format` emits
  media type/subtype, format, modifier, size and frame rates. It does not emit
  `SPA_FORMAT_VIDEO_colorRange`, `colorMatrix`, `transferFunction`, or
  `colorPrimaries`. Its screencopy buffer metadata is Header/VideoTransform/
  VideoDamage, not HDR metadata. This matches the live `pw-dump` observation.
- Hyprland 0.57's wlr screencopy adapter uses a managed screenshare session and
  sends buffer format/size/stride, DMA-BUF format, timing/flags/damage. The
  wlr-screencopy protocol does not carry a color description. Internal compositor
  HDR/color management or an SDR sharing copy cannot repair this missing
  application-facing contract.
- SPA has independent range/matrix/transfer/primaries fields, but its enum values
  are **not CICP values**. New capture code logs their original values, validates
  each format event and attaches the validated source description to frames.
  Missing fields remain visibly missing; no primaries or HDR mastering values
  are manufactured from fourcc or bit depth.
- Portal ScreenCast stream properties (resolution/node/restore token) are not a
  substitute for a frame color description. No available static HDR mastering
  or content-light data was observed on this path.

Upstream references (retrieved during the audit; moving branches are not a
promise about future versions):

- [XDPH format builders](https://github.com/hyprwm/xdg-desktop-portal-hyprland/blob/master/src/shared/ScreencopyShared.cpp)
- [XDPH stream metadata](https://github.com/hyprwm/xdg-desktop-portal-hyprland/blob/master/src/portals/Screencopy.cpp)
- [Hyprland 0.57 screencopy adapter](https://github.com/hyprwm/Hyprland/blob/v0.57.0/src/protocols/Screencopy.cpp)
- [wlr-screencopy protocol](https://wayland.app/protocols/wlr-screencopy-unstable-v1)
- [SPA color enums](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/master/spa/include/spa/param/video/color.h)

## Pipeline changes and remaining limits

### Conversion and encode

- Source acceptance is full-range sRGB with sRGB/BT.709 primaries. Explicit
  10-bit sRGB is SDR: precision reduction to 8-bit is permitted, **not called HDR
  tone mapping**. Unsupported transfer/range/gamut fails before DMA-BUF import
  or CPU copying. NV12 capture is no longer advertised: the old fallback copied
  only the luma plane and did not have a safe planar/color contract.
- CPU and CUDA converters use the same limited-range BT.709 matrix, now with
  zero-sum chroma coefficients so every neutral gray has U=V=128. Range maps
  full RGB black/white to Y=16/235. This matrix operates on existing nonlinear
  code values; it does not apply another transfer curve or saturation boost.
- The SDR swscale fallback now explicitly selects 709 coefficients, full RGB
  input and limited NV12 output. Its cache is rebuilt on input format/size
  changes. Red/blue layouts use distinct X2RGB/X2BGR definitions; ABGR/ARGB
  2101010 DRM mappings are corrected. Unrepresentable 10:10:10:2 variants are
  rejected instead of aliased to 2:10:10:10.
- NVENC remains NV12/8-bit HEVC, no B frames/lookahead added. VUI and submitted
  frames now signal limited 709 matrix/primaries **and sRGB transfer**, since
  desktop code values were not transformed by a BT.709 OETF. Hardware round-trip
  tests check actual decoded VUI, not just configured encoder fields.
- Baseline DMA-BUF staging uses an 8-bit RGBA GL texture/PBO/readback or CUDA
  staging; CPU uses 8-bit NV12. Both would destroy HDR precision before NVENC.
  Neither path is exposed as native HDR10.
- Nix CUDA NVRTC is pinned to `cudaPackages_13_2`, matching cudarc's cuda-13020
  and deployed driver support. The floating CUDA 13 alias resolved to 13.4 and
  hardware testing failed with `CUDA_ERROR_UNSUPPORTED_PTX_VERSION`, silently
  degrading to CPU conversion. Pinning 13.2 restored the GPU path.

### Transport and negotiation

Session prefixes, control enum indices, media headers and IDR/loss ordering are
unchanged. A trailing `SGCL`, version 1 postcard extension carries complete
end-to-end client capabilities or the selected CICP color/range/depth description.
Old peers ignore the suffix. New peers accept exactly absent legacy extensions
as legacy SDR; malformed/truncated/unknown versions are errors, not a downgrade
opportunity. Pre-bitrate and pre-surround session prefixes remain readable.

Current clients advertise SDR only. A decoder supporting Main10 does not imply
HDR presentation. HDR10 has a distinct description, but selection is rejected
because the complete pipeline is unsupported, even if a future client advertises
HDR10. No HDR reaches an old SDR peer. Static HDR metadata transport/SEI is **not
implemented**; it is not fabricated or advertised.

### Decode

Before **either** VAAPI DMA-BUF export or CPU transfer, check the original
AVFrame's color metadata and hardware frames context's software layout. Only
8-bit NV12/YUV420P SDR is permitted. Full-range YUV, non-709 matrices/primaries,
PQ/HLG/linear transfer, P010/Main10 and HDR side data fail clearly. The arbitrary
swscale downconversion path is removed. Hardware transfers copy frame properties.

New explicitly negotiated sessions must match their VUI description, including
transfer/range; a missing or changed field terminates decoding. Only old SDR
sessions retain the historical unspecified/BT.709 transfer compatibility
interpretation. This cannot undo incorrect colors already encoded by old servers.

### Presentation: why SDL/OpenGL cannot advertise HDR here

The GL shaders and whole-NV12 EGL import convert limited 709 YUV to SDR RGB. The
GL shader now expands exact digital offsets (16/255, 128/255), rather than
16/256 and 0.5, preserving neutral black/white. SDL canvas still explicitly
selects BT.709, avoiding SDL's automatic BT.601 choice. No additional gamma,
gamut expansion, paper-white scaling or saturation adjustment is applied.

The SDL2 window is an SDR/default surface; Hyprland's default SDR interpretation
and output mapping are retained. **EGL YUV import hints describe the input image,
not the Wayland output surface.** Requesting an sRGB framebuffer or a 10-bit EGL
config alone would not attach a PQ/BT.2020 image description. The current app
uses no Wayland color-management objects and SDL2 exposes no HDR presentation
color-description API. Nix's sdl2-compat internally using SDL3 does not turn this
SDL2 app into an HDR client. SDL3's renderer/window color APIs and a native
Wayland integration are possible migration directions, not implemented support.

- [SDL2 Wayland window implementation](https://github.com/libsdl-org/SDL/blob/SDL2/src/video/wayland/SDL_waylandwindow.c)
- [sdl2-compat](https://github.com/libsdl-org/sdl2-compat/blob/main/src/sdl2_compat.c)
- [SDL3 renderer output properties](https://wiki.libsdl.org/SDL3/SDL_GetRendererProperties)
- [Wayland color-management-v1](https://wayland.app/protocols/color-management-v1)

### Reconnects, transitions, output movement

Every handshake renegotiates, every capture format event invalidates the previous
interpretation before validation, and every decoded frame is checked before
presentation. Unsafe source/bitstream changes stop the pipeline/session; they do
not reuse stale SDR assumptions. Resolution changes retain the existing
stop/restart policy. Source pipeline restart is required after rejection; a
reconnect alone does not recreate a dead server capture pipeline. Client decoder
recreation retains the negotiated contract. No media buffering or frame dropping
was added; the existing IDR recovery and render-latest policy are untouched.

Moving an SDR surface between SDR/HDR outputs delegates output mapping to the
compositor and never selects a new stream mode. This requires no HDR-output
polling or automatic desktop switching. Unsignaled source changes cannot be
detected under the explicit untagged-SDR assertion described above.

## Validation

- Workspace formatting, check, pedantic all-targets lint and nextest pass locally:
  **268 tests passed; 22 hardware-dependent tests skipped** in the default suite.
  Lint output was compared with the baseline: no added diagnostic categories /
  file occurrences (existing pedantic warnings remain). Nix server derivation
  evaluation succeeds; the final packaged application was not rebuilt/deployed.
- Unit regressions cover legacy/current negotiation, malformed extensions,
  HDR capability vs output state, source precision vs dynamic range, missing /
  contradictory metadata, full/limited range, PQ/HLG/linear/2020 rejection,
  unsafe transitions, HDR side data, neutral gradients, exact black/reference
  white, channel order, and swscale/direct conversion agreement.
- **zeus hardware:** RTX 3090 NVENC CPU/GPU round trips at 3440×1440 verify
  colors and decoded sRGB/709/limited VUI. A CUDA pre-encode gradient test checks
  all 256 gray codes **byte-for-byte** against CPU output, including black,
  white and neutral chroma. A separate live portal→DMA-BUF→EGL/CUDA→NVENC
  capture test also passes at the actual 3440×1440 source size, logging all four
  SPA color fields as absent. This is not a 145-fps throughput benchmark.
  Its prior idle-desktop teardown hang was fixed by stopping capture before
  joining the encoder, and it now awaits the real negotiated dimensions.
  A separate server startup without the source assertion exits with an explicit
  missing-color error before encoding. GL color/gradient tests also pass on
  zeus's SDR headless desktop with the NVIDIA GLES driver.
- **athena hardware, HDR desktop unchanged:** AMD VAAPI decodes a synthetic SDR
  NVENC black fixture as 8-bit NV12, preserves its sRGB/709/limited description,
  exports DMA-BUF and transfers to CPU with correct black/chroma. A synthetic
  Main10 BT.2020/PQ fixture is hardware decoded and rejected before presentation.
  GL NV12/I420 color and neutral-gradient readbacks pass on the same machine
  with the package's Mesa 26.2.4 driver paths pinned (radeonsi/Rembrandt), as
  well as with the host's Mesa 26.2.6.
  Gradient code values use an RGBA8 FBO to avoid undefined post-swap buffers and
  compositor scaling; they do **not** measure the panel's luminance/calibration.
- Not verified: native HDR capture, HDR SEI/mastering metadata fidelity, native
  HDR presentation, PQ reference-white/highlight rendering, HDR gradients,
  actual HDR→SDR tone/gamut mapping, HDR DMA-BUF/CPU agreement or live HDR source
  transitions. These modes are rejected, not claimed implemented. No full
  capture-to-panel session was deployed/replaced during validation.

Reproduce GPU tests:

```sh
nix develop .#cuda -c cargo test -p stargaze-server cpu_gpu_sdr_gradient_agreement -- --ignored
nix develop .#cuda -c cargo test -p stargaze-server cpu_frame_gpu_and_cpu_conversions_round_trip_correct_colors -- --ignored
WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/1000 \
  nix develop -c cargo test -p stargaze-client gl_renderer -- --ignored --test-threads=1
```

The ignored `vaapi_color_fixture` test takes `STARGAZE_TEST_HEVC` pointing to a
one-frame Annex-B HEVC file. Set `STARGAZE_TEST_EXPECT_HDR=1` for the HDR rejection
fixture. Generate synthetic fixtures with explicit **frame** metadata as well
as codec options (codec options alone did not preserve transfer/primaries):

```sh
# Inside nix develop .#cuda; repeat with p010le / main10 / bt2020nc / bt2020 /
# smpte2084 to test HDR rejection, not a claimed HDR streaming pipeline.
ffmpeg -f lavfi -i color=black:size=640x480:rate=1 \
  -vf 'format=nv12,setparams=range=limited:colorspace=bt709:color_primaries=bt709:color_trc=iec61966-2-1' \
  -frames:v 1 -c:v hevc_nvenc -preset p1 -tune ull -bf 0 \
  -color_range tv -colorspace bt709 -color_primaries bt709 -color_trc iec61966-2-1 \
  -f hevc /tmp/sdr.hevc
```

## Native HDR10: remaining implementation gates

1. Obtain an authoritative capture color contract, including transitions,
   appropriate precision and available mastering/content-light metadata.
2. Preserve >=10-bit precision through CPU and DMA-BUF imports and conversion;
   implement 2020 NCL/PQ RGB→P010 without confusing storage, transfer or range.
3. Add Main10/P010 NVENC, 2020/PQ VUI and applicable verified SEI. Only send
   HDR to peers with explicit end-to-end support; require new keyframes on
   mode changes. Do not invent mastering metadata when unavailable.
4. Add VAAPI P010 and software 10-bit handling, precision-preserving DMA-BUF
   plane formats, and a >=10-bit color-described Wayland presentation surface.
   Confirm compositor acceptance/capabilities and recreate/reject on output or
   color-description changes. Mixed SDR/HDR UI overlays need a defined white.
5. Separately implement a documented tone/gamut mapper if HDR→SDR fallback is
   desired. Validate absolute PQ black, reference white, highlights and wide-
   gamut gradients on the actual source and display; compare GPU/CPU paths.

Until all these gates exist and are verified, claiming “native HDR10 supported”
from NVENC/VAAPI device capability alone would be unsafe.
