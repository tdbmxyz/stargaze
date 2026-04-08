# Fix Green Flickering Artifacts

## Issues to Address

After fixing the QUIC datagram transport (MTU config, header size estimation,
datagram send buffer), video frames now reach the client and decode
successfully — but the rendered output is **flickering green bands** with no
recognizable screencast content.

This is a classic symptom of a **pixel data layout mismatch** somewhere in the
decode → scale → render pipeline.

## Important Notes

### The full pixel pipeline

```
PipeWire (BGRA8 / DMA-BUF)
  → upload as Pixel::BGRA sw_frame
  → av_hwframe_transfer_data (BGRA → NV12 on GPU)
  → NVENC H.265 encode
  ─── QUIC datagrams ───
  → FFmpeg HEVC software decode → YUV420P output
  → sws_scale (YUV420P → NV12)
  → NV12 byte copy (strip stride padding)
  → SDL2 texture.update(None, &data, width) with NV12 texture
  → canvas.copy + present
```

### Suspect areas (ordered by likelihood)

1. **SDL2 `texture.update` pitch for NV12** — The call is
   `texture.update(None, &frame.data, width)` where `width` is the frame
   width in pixels.  For NV12, SDL2's `SDL_UpdateTexture` expects the pitch
   (stride in bytes) of the **Y plane**, which equals `width` only when there
   is no padding.  But `SDL_UpdateTexture` on an NV12 texture is documented as
   unreliable — SDL2 recommends `SDL_UpdateNV12Texture` (or
   `SDL_UpdateYUVTexture`) which takes separate Y and UV planes with
   independent pitches.  Using the generic `update` on a multi-planar format
   likely misinterprets the interleaved Y+UV buffer, producing green garbage.

2. **NV12 byte layout after sws_scale** — The decoder strips stride padding
   line-by-line (Y then UV) and concatenates into a single `Vec<u8>`.  If the
   scaler output stride differs from `width`, or if the UV plane height is
   miscalculated (`height / 2` assumes even height — fine for 1080/1440 but
   worth a sanity check), the packed buffer is malformed.

3. **Capture pixel format mismatch** — PipeWire may negotiate a 10-bit format
   (`Bgra10` / `Rgba10` / `xRGB2101010`) but the encoder always creates
   `Pixel::BGRA` (8-bit) software frames.  Feeding 10-bit data into an 8-bit
   frame garbles colour and structure.  Need to verify the actually-negotiated
   `PixelFormat` at runtime from the diagnostic logs.

4. **Resolution mismatch between decoder output and SDL texture** — The SDL
   texture is created with `config.width × config.height` from the session
   handshake.  If the decoder produces a different resolution (e.g. the capture
   resolution changed after the handshake), `texture.update` silently
   reinterprets the buffer with wrong row stride → green bands.

5. **PipeWire Streaming→Paused cycling** (known issue) — `ack_format` fires on
   every `param_changed`, causing repeated format renegotiation.  This could
   produce transient corrupt frames or frames with stale metadata, but
   unlikely to cause *persistent* green output on every frame.

### Key code locations

| Component | File | Function |
|---|---|---|
| Encoder init | `crates/stargaze-server/src/encode/ffmpeg.rs:68` | `init_encoder` |
| Encoder upload (CPU) | same, `:311` | `upload_and_encode` |
| Encoder upload (DMA-BUF) | same, `:389` | `upload_dmabuf_and_encode` |
| Decoder loop | `crates/stargaze-client/src/decode/ffmpeg.rs:80` | `run_decode_loop` |
| YUV420P→NV12 scale | same, `:137` | `drain_decoded_frames` |
| NV12 stride strip | same, `:189-210` | (inline in `drain_decoded_frames`) |
| SDL render + texture | `crates/stargaze-client/src/render/sdl.rs:46-201` | `run_sdl_loop` |

## Implementation Strategy

### Step 1 — Fix SDL2 NV12 texture update (most likely fix)

Replace `texture.update(None, &frame.data, width)` with SDL2's dedicated
`SDL_UpdateNV12Texture` call.  The `sdl2` Rust crate may not expose this
directly — check if the crate has `update_nv12` or if raw FFI via
`sdl2_sys::SDL_UpdateNV12Texture` is needed.

The call needs:
- Y plane pointer and pitch (= width)
- UV plane pointer and pitch (= width)
- Y plane starts at offset `0`, UV plane at offset `width * height`

If the crate doesn't have `update_nv12`, an alternative is switching the
texture format to `IYUV` / `YV12` and using `update_yuv` (the `sdl2` crate
does expose `update_yuv`).  This would require changing the scaler output
from NV12 to YUV420P and splitting into three planes.

### Step 2 — Add diagnostic logging at each pipeline stage

Before and after the scaler, log:
- Input frame format, width, height
- Scaler output format, width, height, stride(0), stride(1)
- Packed buffer size vs expected size (`width * height * 3 / 2`)
- First few bytes of Y and UV planes

This confirms whether the data leaving the decoder is correct, narrowing the
bug to either the scaler/copy stage or the SDL render stage.

### Step 3 — Verify capture pixel format at runtime

Check the server logs for the negotiated `PixelFormat`.  If it's `Bgra10` or
`Rgba10`, the encoder's `Pixel::BGRA` software frame is wrong — it should use
`Pixel::X2RGB10` or an equivalent 10-bit format, or the capture pipeline
should force 8-bit negotiation.

### Step 4 — Guard against resolution drift

Compare the decoder's output `width × height` against the SDL texture
dimensions on every frame.  If they differ, either recreate the texture or
log a loud warning.  This also catches the case where PipeWire renegotiation
changes the capture resolution mid-stream.

## Tests

- Verify `cargo test --workspace` still passes after each change (existing
  106 tests should remain green).
- Manual verification: run server + client and confirm the screencast displays
  correctly with no green artefacts.
- If adding `update_nv12` via FFI: add a unit test that creates a small NV12
  texture, updates it, and reads back pixel data to confirm correct layout
  (only if the sdl2 test infrastructure allows headless rendering).
