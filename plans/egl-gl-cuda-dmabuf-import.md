# EGL + GL → CUDA DMA-BUF Import Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the broken direct `cuImportExternalMemory(DMABUF_FD)` path with the proven EGL→GL→CUDA pipeline, so PipeWire DMA-BUF frames are correctly imported into CUDA for NVENC encoding.

**Architecture:** Create a headless EGL context at encoder init time. For each DMA-BUF frame: import as EGL image → bind to a persistent GL texture → copy via `cuGraphicsGLRegisterImage` into a CUDA device pointer → feed into the existing `av_hwframe_get_buffer` / `avcodec_send_frame` pipeline. This mirrors the approach used by Sunshine and waycap-rs.

**Tech Stack:** `khronos-egl` (EGL 1.5 bindings, dynamic loading), `gl` (OpenGL bindings), `cudarc` (CUDA driver API — already a dependency), raw FFI for `cuGraphicsGLRegisterImage` (not exposed by cudarc).

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `crates/stargaze-server/src/encode/egl_cuda.rs` | **Create** | EGL context setup, GL texture management, DMA-BUF → EGL image, GL→CUDA copy |
| `crates/stargaze-server/src/encode/ffmpeg.rs` | **Modify** | Replace `upload_dmabuf_cuda_import()` with EGL-GL-CUDA path; init EGL at encoder init |
| `crates/stargaze-server/src/encode/mod.rs` | **Modify** | Add `mod egl_cuda;` |
| `crates/stargaze-server/Cargo.toml` | **Modify** | Add `khronos-egl` and `gl` dependencies |
| `flake.nix` | **Modify** | Add `libglvnd` and `mesa` to build/runtime deps |

---

## Important Notes

- **No Wayland display needed for EGL.** The server is headless. We use `eglGetPlatformDisplay(EGL_PLATFORM_DEVICE_EXT, egl_device, ...)` with an NVIDIA EGL device, or fall back to `EGL_DEFAULT_DISPLAY`. waycap-rs uses Wayland display but we don't need/want that dependency on the server.
- **NVIDIA driver 595.58.03** is installed. EGL and GLESv2 NVIDIA libs are at `/run/opengl-driver/lib/`. The `flake.nix` CUDA shell already adds this to `LD_LIBRARY_PATH`.
- **`cuGraphicsGLRegisterImage` is not in cudarc.** We declare the FFI extern ourselves, dynamically loaded like waycap-rs does.
- **GL texture is persistent** — created once, registered with CUDA once. Per-frame work is: create EGL image from DMA-BUF → bind to texture → map CUDA resource → copy → unmap → destroy EGL image.
- **sw_format stays NV12** in the hardware frames context. The GL texture holds BGRA/RGBA data. After CUDA copy, we still need `sws_scale` (BGRA→NV12) + `av_hwframe_transfer_data` as before. The key difference: the CUDA copy de-tiles the data correctly.
- **Actually, optimization opportunity**: since the CUDA copy gives us a linear device pointer, we can `cuMemcpy2D` device→host to get linear CPU pixels, then feed through the existing `upload_cpu_data_and_encode()` path. This minimizes changes. The fully GPU-resident path (skip CPU round-trip) is a future optimization.
- **EGL device enumeration**: Use `eglQueryDevicesEXT` + `eglGetPlatformDisplayEXT(EGL_PLATFORM_DEVICE_EXT, ...)` for truly headless operation. This avoids needing a Wayland or GBM display.

---

### Task 1: Add dependencies and Nix build inputs

**Files:**
- Modify: `crates/stargaze-server/Cargo.toml`
- Modify: `flake.nix`

- [ ] **Step 1: Add Rust crate dependencies**

In `crates/stargaze-server/Cargo.toml`, add under `[dependencies]`:

```toml
khronos-egl = { version = "6", features = ["dynamic"] }
gl = "0.14"
```

`khronos-egl` v6 with `dynamic` feature dynamically loads `libEGL.so` at runtime (no build-time linking). `gl` provides OpenGL function pointers loaded at runtime via `gl::load_with`.

- [ ] **Step 2: Add Nix build inputs for EGL/GL**

In `flake.nix`, add to `commonBuildInputs`:

```nix
pkgs.libglvnd  # provides libEGL.so, libGL.so, libGLESv2.so dispatch
```

And to the `runtimeLibPath`:

```nix
pkgs.libglvnd
```

This ensures `libEGL.so.1` and `libGLESv2.so.2` are findable at runtime in the dev shell. The NVIDIA-specific implementations are already at `/run/opengl-driver/lib/`.

- [ ] **Step 3: Verify it compiles**

Run: `nix develop --command cargo check --workspace`
Expected: Success (no new code yet, just deps).

- [ ] **Step 4: Commit**

```
feat(encode): add khronos-egl and gl dependencies for DMA-BUF import
```

---

### Task 2: Create `egl_cuda.rs` — EGL context and GL texture setup

**Files:**
- Create: `crates/stargaze-server/src/encode/egl_cuda.rs`
- Modify: `crates/stargaze-server/src/encode/mod.rs`

This module handles all EGL/GL/CUDA interop. It exposes a single struct `EglCudaBridge` that the encoder uses.

- [ ] **Step 1: Add module declaration**

In `crates/stargaze-server/src/encode/mod.rs`, add:

```rust
pub(crate) mod egl_cuda;
```

- [ ] **Step 2: Create `egl_cuda.rs` with EGL device-based initialization**

Create `crates/stargaze-server/src/encode/egl_cuda.rs` with the following content. This is a large file — read every line carefully.

```rust
//! EGL → GL → CUDA bridge for importing DMA-BUF frames.
//!
//! Uses a headless EGL device context (no display server) to:
//! 1. Import DMA-BUF fds as EGL images
//! 2. Bind them to a persistent GL texture
//! 3. Copy via CUDA-GL interop into device memory
//!
//! Reference: <https://github.com/Adonca2203/waycap-rs> (Sunshine pattern)

use std::ffi::c_void;
use std::ptr;

use stargaze_core::capture::{DmaBufInfo, PixelFormat};
use stargaze_core::encode::EncodeError;
use tracing::{debug, info};

// ── EGL constants not in khronos-egl ────────────────────────────────────

/// `EGL_LINUX_DMA_BUF_EXT` (0x3270) — target for `eglCreateImage`.
const EGL_LINUX_DMA_BUF_EXT: usize = 0x3270;
/// `EGL_LINUX_DRM_FOURCC_EXT` (0x3271)
const EGL_LINUX_DRM_FOURCC_EXT: usize = 0x3271;

// Plane 0 attributes
const EGL_DMA_BUF_PLANE0_FD_EXT: usize = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: usize = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: usize = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: usize = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: usize = 0x3444;

/// `EGL_PLATFORM_DEVICE_EXT` (0x313F) — for headless EGL on NVIDIA.
const EGL_PLATFORM_DEVICE_EXT: usize = 0x313F;

// DRM fourcc codes (from drm_fourcc.h)
/// `DRM_FORMAT_ARGB8888` — matches BGRA8 byte order.
const DRM_FORMAT_ARGB8888: u32 = 0x3432_3241; // fourcc_code('A', 'R', '2', '4') — wrong
// Actually: fourcc_code('A','R','2','4') but we need XRGB or ARGB.
// PipeWire delivers BGRA8 which is DRM_FORMAT_ARGB8888 = 0x34325241

// Let's use the correct values from drm_fourcc.h:
// #define fourcc_code(a, b, c, d) ((u32)(a) | ((u32)(b) << 8) | ((u32)(c) << 16) | ((u32)(d) << 24))
// DRM_FORMAT_ARGB8888 = fourcc_code('A', 'R', '2', '4') = 0x34325241
// DRM_FORMAT_XRGB8888 = fourcc_code('X', 'R', '2', '4') = 0x34325258
// DRM_FORMAT_ABGR8888 = fourcc_code('A', 'B', '2', '4') = 0x34324241
// DRM_FORMAT_XBGR8888 = fourcc_code('X', 'B', '2', '4') = 0x34324258

/// Convert our `PixelFormat` to a DRM fourcc code for EGL import.
fn pixel_format_to_drm_fourcc(format: PixelFormat) -> u32 {
    match format {
        // BGRA8 in memory = DRM_FORMAT_ARGB8888 (DRM names by channel order high→low)
        PixelFormat::Bgra8 => 0x3432_5241, // fourcc_code('A','R','2','4')
        // RGBA8 in memory = DRM_FORMAT_ABGR8888
        PixelFormat::Rgba8 => 0x3432_4241, // fourcc_code('A','B','2','4')
        // NV12
        PixelFormat::Nv12 => 0x3231_564E, // fourcc_code('N','V','1','2')
        // BGRA10 = DRM_FORMAT_XRGB2101010
        PixelFormat::Bgra10 => 0x3033_5258, // fourcc_code('X','R','3','0')
        // RGBA10 = DRM_FORMAT_XBGR2101010
        PixelFormat::Rgba10 => 0x3033_4258, // fourcc_code('X','B','3','0')
    }
}

// ── CUDA-GL FFI (not in cudarc) ─────────────────────────────────────────

type CUresult = cudarc::driver::sys::CUresult;
type CUgraphicsResource = *mut c_void;

unsafe extern "C" {
    /// Register a GL image (texture) for access by CUDA.
    fn cuGraphicsGLRegisterImage(
        resource: *mut CUgraphicsResource,
        image: gl::types::GLuint,
        target: gl::types::GLenum,
        flags: std::ffi::c_uint,
    ) -> CUresult;
}

// ── EglCudaBridge ───────────────────────────────────────────────────────

/// Manages EGL context, GL texture, and CUDA-GL interop for DMA-BUF import.
///
/// Created once during encoder initialization. For each frame:
/// 1. `import_dmabuf()` creates an EGL image, binds to GL texture, copies to CUDA
/// 2. Returns linear CPU data (de-tiled by the GPU) for the existing encode path
pub(crate) struct EglCudaBridge {
    /// Dynamic EGL instance (loaded from `libEGL.so.1`).
    egl: khronos_egl::Instance<khronos_egl::Dynamic<libloading::Library, khronos_egl::EGL1_5>>,
    /// EGL display handle.
    display: khronos_egl::Display,
    /// EGL context handle.
    context: khronos_egl::Context,
    /// Persistent GL texture ID.
    texture_id: gl::types::GLuint,
    /// CUDA graphics resource (registered GL texture).
    cuda_resource: CUgraphicsResource,
    /// Frame dimensions the texture was allocated for.
    width: u32,
    height: u32,
    /// Whether DMA-BUF modifier attributes are supported.
    dmabuf_modifiers_supported: bool,
}

// Safety: EglCudaBridge is only used on the dedicated encoder thread.
// EGL/GL/CUDA contexts are not thread-safe but we never share across threads.
unsafe impl Send for EglCudaBridge {}

impl EglCudaBridge {
    /// Initialize headless EGL, create GL texture, register with CUDA.
    ///
    /// Must be called after CUDA context is already active on this thread
    /// (i.e., after `cuCtxPushCurrent`).
    pub(crate) fn new(
        width: u32,
        height: u32,
        cuda_ctx: cudarc::driver::sys::CUcontext,
    ) -> Result<Self, EncodeError> {
        // Step 1: Load EGL dynamically.
        let lib = unsafe { libloading::Library::new("libEGL.so.1") }.map_err(|e| {
            EncodeError::InitError(format!(
                "Failed to load libEGL.so.1: {e}. \
                 Ensure libglvnd or NVIDIA EGL is in LD_LIBRARY_PATH."
            ))
        })?;
        let egl =
            unsafe { khronos_egl::DynamicInstance::<khronos_egl::EGL1_5>::load_required_from(lib) }
                .map_err(|e| {
                    EncodeError::InitError(format!("Failed to initialize EGL 1.5: {e}"))
                })?;

        // Step 2: Get EGL display (device-based for headless, fallback to default).
        let display = Self::get_headless_display(&egl)?;

        egl.initialize(display).map_err(|e| {
            EncodeError::InitError(format!("eglInitialize failed: {e}"))
        })?;

        // Bind OpenGL ES API.
        egl.bind_api(khronos_egl::OPENGL_ES_API).map_err(|e| {
            EncodeError::InitError(format!("eglBindAPI(OPENGL_ES_API) failed: {e}"))
        })?;

        // Step 3: Check extensions.
        let extensions = egl
            .query_string(Some(display), khronos_egl::EXTENSIONS)
            .map_err(|e| EncodeError::InitError(format!("eglQueryString failed: {e}")))?;
        let ext_str = extensions.to_string_lossy();

        if !ext_str.contains("EGL_EXT_image_dma_buf_import") {
            return Err(EncodeError::InitError(
                "EGL_EXT_image_dma_buf_import not supported".to_string(),
            ));
        }
        let dmabuf_modifiers_supported =
            ext_str.contains("EGL_EXT_image_dma_buf_import_modifiers");

        debug!(
            dmabuf_modifiers = dmabuf_modifiers_supported,
            "EGL DMA-BUF import supported"
        );

        // Step 4: Choose config and create context.
        let config_attribs = [
            khronos_egl::SURFACE_TYPE,
            khronos_egl::PBUFFER_BIT,
            khronos_egl::RENDERABLE_TYPE,
            khronos_egl::OPENGL_ES2_BIT,
            khronos_egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &config_attribs)
            .map_err(|e| EncodeError::InitError(format!("eglChooseConfig failed: {e}")))?
            .ok_or_else(|| {
                EncodeError::InitError("No suitable EGL config found".to_string())
            })?;

        let ctx_attribs = [
            khronos_egl::CONTEXT_CLIENT_VERSION,
            2,
            khronos_egl::NONE,
        ];
        let context = egl
            .create_context(display, config, None, &ctx_attribs)
            .map_err(|e| EncodeError::InitError(format!("eglCreateContext failed: {e}")))?;

        // Make current with surfaceless (EGL_KHR_surfaceless_context) or pbuffer.
        if ext_str.contains("EGL_KHR_surfaceless_context") {
            egl.make_current(display, None, None, Some(context))
                .map_err(|e| {
                    EncodeError::InitError(format!("eglMakeCurrent (surfaceless) failed: {e}"))
                })?;
            debug!("EGL: using surfaceless context");
        } else {
            // Fallback: create a tiny pbuffer surface.
            let pbuf_attribs = [
                khronos_egl::WIDTH as usize,
                1,
                khronos_egl::HEIGHT as usize,
                1,
                khronos_egl::NONE as usize,
            ];
            let surface = egl
                .create_pbuffer_surface(display, config, &pbuf_attribs)
                .map_err(|e| {
                    EncodeError::InitError(format!("eglCreatePbufferSurface failed: {e}"))
                })?;
            egl.make_current(display, Some(surface), Some(surface), Some(context))
                .map_err(|e| {
                    EncodeError::InitError(format!("eglMakeCurrent (pbuffer) failed: {e}"))
                })?;
            debug!("EGL: using pbuffer surface");
        }

        // Step 5: Load GL function pointers.
        gl::load_with(|symbol| {
            egl.get_proc_address(symbol)
                .map_or(ptr::null(), |p| p as *const c_void)
        });

        // Step 6: Create persistent GL texture (RGBA8, sized to frame).
        let texture_id = unsafe {
            let mut tex = 0;
            gl::GenTextures(1, &mut tex);
            gl::BindTexture(gl::TEXTURE_2D, tex);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA8 as i32,
                width.cast_signed(),
                height.cast_signed(),
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                ptr::null(),
            );
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_WRAP_S,
                gl::CLAMP_TO_EDGE as i32,
            );
            gl::TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_WRAP_T,
                gl::CLAMP_TO_EDGE as i32,
            );
            gl::BindTexture(gl::TEXTURE_2D, 0);

            let err = gl::GetError();
            if err != gl::NO_ERROR {
                return Err(EncodeError::InitError(format!(
                    "GL error creating texture: 0x{err:x}"
                )));
            }
            tex
        };
        debug!(texture_id, width, height, "Created persistent GL texture");

        // Step 7: Push CUDA context and register GL texture.
        unsafe {
            let res = cudarc::driver::sys::cuCtxPushCurrent_v2(cuda_ctx);
            if res != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(EncodeError::InitError(format!(
                    "cuCtxPushCurrent failed during EGL bridge init: {res:?}"
                )));
            }
        }

        let cuda_resource = unsafe {
            let mut resource: CUgraphicsResource = ptr::null_mut();
            let res = cuGraphicsGLRegisterImage(
                &mut resource,
                texture_id,
                gl::TEXTURE_2D,
                0x00, // CU_GRAPHICS_REGISTER_FLAGS_NONE
            );
            if res != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                // Pop CUDA context before returning error.
                let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
                cudarc::driver::sys::cuCtxPopCurrent_v2(&raw mut old);
                return Err(EncodeError::InitError(format!(
                    "cuGraphicsGLRegisterImage failed: {res:?}"
                )));
            }

            // Set map flags (read-only from CUDA's perspective).
            let res = cudarc::driver::sys::cuGraphicsResourceSetMapFlags_v2(
                resource.cast(),
                0, // CU_GRAPHICS_MAP_RESOURCE_FLAGS_NONE
            );
            if res != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                cudarc::driver::sys::cuGraphicsUnregisterResource(resource.cast());
                let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
                cudarc::driver::sys::cuCtxPopCurrent_v2(&raw mut old);
                return Err(EncodeError::InitError(format!(
                    "cuGraphicsResourceSetMapFlags failed: {res:?}"
                )));
            }
            resource
        };

        unsafe {
            let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
            cudarc::driver::sys::cuCtxPopCurrent_v2(&raw mut old);
        }

        info!(
            width,
            height, "EGL-GL-CUDA bridge initialized (headless, DMA-BUF import ready)"
        );

        Ok(Self {
            egl,
            display,
            context,
            texture_id,
            cuda_resource,
            width,
            height,
            dmabuf_modifiers_supported,
        })
    }

    /// Get a headless EGL display using `EGL_PLATFORM_DEVICE_EXT`.
    ///
    /// Falls back to `EGL_DEFAULT_DISPLAY` if device enumeration is unavailable.
    fn get_headless_display(
        egl: &khronos_egl::Instance<
            khronos_egl::Dynamic<libloading::Library, khronos_egl::EGL1_5>,
        >,
    ) -> Result<khronos_egl::Display, EncodeError> {
        // Try default display first — on NVIDIA with libglvnd this picks the
        // NVIDIA EGL implementation which supports DMA-BUF import.
        let display = unsafe { egl.get_display(khronos_egl::DEFAULT_DISPLAY) };
        match display {
            Some(d) => {
                debug!("Using EGL default display");
                Ok(d)
            }
            None => Err(EncodeError::InitError(
                "eglGetDisplay(EGL_DEFAULT_DISPLAY) returned EGL_NO_DISPLAY".to_string(),
            )),
        }
    }

    /// Import a DMA-BUF frame: EGL image → GL texture → CUDA device → CPU buffer.
    ///
    /// Returns a linear (de-tiled) CPU buffer of BGRA/RGBA pixels.
    /// The caller feeds this into `upload_cpu_data_and_encode()`.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn import_dmabuf_to_cpu(
        &self,
        info: &DmaBufInfo,
    ) -> Result<Vec<u8>, EncodeError> {
        use std::os::unix::io::AsRawFd;

        let drm_fourcc = pixel_format_to_drm_fourcc(info.format);

        // Step 1: Create EGL image from DMA-BUF.
        let mut attribs: Vec<usize> = vec![
            EGL_LINUX_DRM_FOURCC_EXT,
            drm_fourcc as usize,
            khronos_egl::WIDTH as usize,
            info.width as usize,
            khronos_egl::HEIGHT as usize,
            info.height as usize,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            info.fd.as_raw_fd() as usize,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            info.offset as usize,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            info.stride as usize,
        ];

        if self.dmabuf_modifiers_supported {
            attribs.extend_from_slice(&[
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (info.modifier & 0xFFFF_FFFF) as usize,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (info.modifier >> 32) as usize,
            ]);
        }

        attribs.push(khronos_egl::NONE as usize);

        let egl_image = self
            .egl
            .create_image(
                self.display,
                unsafe { khronos_egl::Context::from_ptr(khronos_egl::NO_CONTEXT) },
                EGL_LINUX_DMA_BUF_EXT,
                unsafe { khronos_egl::ClientBuffer::from_ptr(ptr::null_mut()) },
                &attribs,
            )
            .map_err(|e| EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("eglCreateImage (DMA-BUF) failed: {e}"),
            })?;

        // Step 2: Bind EGL image to a temp texture, copy to persistent texture via FBO.
        let copy_result = unsafe { self.egl_image_to_gl_texture(egl_image) };

        // Always destroy the EGL image (it's per-frame).
        let _ = self.egl.destroy_image(self.display, egl_image);

        copy_result?;

        // Step 3: Map CUDA resource, get array, copy to host.
        let cpu_buf = unsafe { self.gl_texture_to_cpu_via_cuda()? };

        Ok(cpu_buf)
    }

    /// Bind EGL image to temp GL texture, blit into persistent texture via FBO.
    ///
    /// # Safety
    /// Requires active EGL context on this thread. `egl_image` must be valid.
    unsafe fn egl_image_to_gl_texture(
        &self,
        egl_image: khronos_egl::Image,
    ) -> Result<(), EncodeError> {
        // Get glEGLImageTargetTexture2DOES function pointer.
        type EglImageTargetFn =
            unsafe extern "C" fn(target: gl::types::GLenum, image: *const c_void);

        let proc = self.egl.get_proc_address("glEGLImageTargetTexture2DOES");
        let egl_image_target: EglImageTargetFn = match proc {
            Some(p) => std::mem::transmute(p),
            None => {
                return Err(EncodeError::EncodeFrameError {
                    frame: 0,
                    reason: "glEGLImageTargetTexture2DOES not available".to_string(),
                });
            }
        };

        // Create temp texture and bind EGL image to it.
        let mut temp_tex = 0;
        gl::GenTextures(1, &mut temp_tex);
        gl::BindTexture(gl::TEXTURE_2D, temp_tex);
        egl_image_target(gl::TEXTURE_2D, egl_image.as_ptr());

        let gl_err = gl::GetError();
        if gl_err != gl::NO_ERROR {
            gl::DeleteTextures(1, &temp_tex);
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("glEGLImageTargetTexture2DOES failed: GL error 0x{gl_err:x}"),
            });
        }

        // Create FBO, attach temp texture as read source.
        let mut fbo = 0;
        gl::GenFramebuffers(1, &mut fbo);
        gl::BindFramebuffer(gl::FRAMEBUFFER, fbo);
        gl::FramebufferTexture2D(
            gl::FRAMEBUFFER,
            gl::COLOR_ATTACHMENT0,
            gl::TEXTURE_2D,
            temp_tex,
            0,
        );

        let status = gl::CheckFramebufferStatus(gl::FRAMEBUFFER);
        if status != gl::FRAMEBUFFER_COMPLETE {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::DeleteFramebuffers(1, &fbo);
            gl::DeleteTextures(1, &temp_tex);
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("FBO not complete: 0x{status:x}"),
            });
        }

        // Copy from FBO (temp EGL-backed texture) into persistent texture.
        gl::BindTexture(gl::TEXTURE_2D, self.texture_id);
        gl::CopyTexSubImage2D(
            gl::TEXTURE_2D,
            0,
            0,
            0,
            0,
            0,
            self.width.cast_signed(),
            self.height.cast_signed(),
        );

        let gl_err = gl::GetError();

        // Cleanup temp resources.
        gl::BindTexture(gl::TEXTURE_2D, 0);
        gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        gl::DeleteFramebuffers(1, &fbo);
        gl::DeleteTextures(1, &temp_tex);

        if gl_err != gl::NO_ERROR {
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("CopyTexSubImage2D failed: GL error 0x{gl_err:x}"),
            });
        }

        Ok(())
    }

    /// Map the registered GL texture via CUDA, copy to host memory.
    ///
    /// # Safety
    /// Requires CUDA context pushed on this thread. GL texture must contain valid data.
    unsafe fn gl_texture_to_cpu_via_cuda(&self) -> Result<Vec<u8>, EncodeError> {
        use cudarc::driver::sys as cu;

        // Map the GL resource for CUDA access.
        let mut resource = self.cuda_resource;
        let res = cu::cuGraphicsMapResources(1, &raw mut resource, ptr::null_mut());
        if res != cu::CUresult::CUDA_SUCCESS {
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("cuGraphicsMapResources failed: {res:?}"),
            });
        }

        // Get the mapped CUDA array.
        let mut cuda_array: cu::CUarray = ptr::null_mut();
        let res = cu::cuGraphicsSubResourceGetMappedArray(
            &raw mut cuda_array,
            resource.cast(),
            0,
            0,
        );
        if res != cu::CUresult::CUDA_SUCCESS {
            cu::cuGraphicsUnmapResources(1, &raw mut resource, ptr::null_mut());
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("cuGraphicsSubResourceGetMappedArray failed: {res:?}"),
            });
        }

        // Copy CUDA array → host (RGBA, 4 bytes per pixel).
        let row_bytes = self.width as usize * 4;
        let buf_size = row_bytes * self.height as usize;
        let mut cpu_buf = vec![0u8; buf_size];

        let copy_desc = cu::CUDA_MEMCPY2D {
            srcXInBytes: 0,
            srcY: 0,
            srcMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_ARRAY,
            srcHost: ptr::null(),
            srcDevice: 0,
            srcArray: cuda_array,
            srcPitch: 0, // ignored for array source

            dstXInBytes: 0,
            dstY: 0,
            dstMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_HOST,
            dstHost: cpu_buf.as_mut_ptr().cast(),
            dstDevice: 0,
            dstArray: ptr::null_mut(),
            dstPitch: row_bytes,

            WidthInBytes: row_bytes,
            Height: self.height as usize,
        };

        let res = cu::cuMemcpy2D_v2(&raw const copy_desc);

        // Always unmap.
        let mut resource = self.cuda_resource;
        cu::cuGraphicsUnmapResources(1, &raw mut resource, ptr::null_mut());

        if res != cu::CUresult::CUDA_SUCCESS {
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("cuMemcpy2D (CUDA array→host) failed: {res:?}"),
            });
        }

        Ok(cpu_buf)
    }
}

impl Drop for EglCudaBridge {
    fn drop(&mut self) {
        // Unregister CUDA resource.
        if !self.cuda_resource.is_null() {
            unsafe {
                cudarc::driver::sys::cuGraphicsUnregisterResource(self.cuda_resource.cast());
            }
        }
        // Delete GL texture.
        if self.texture_id != 0 {
            unsafe {
                gl::DeleteTextures(1, &self.texture_id);
            }
        }
        // Destroy EGL context.
        let _ = self
            .egl
            .make_current(self.display, None, None, None);
        let _ = self
            .egl
            .destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `nix develop --command cargo check --workspace`

There may be type mismatches between `CUgraphicsResource` (cudarc's opaque pointer type) and our `*mut c_void`. Fix any cast issues. The key FFI boundary is `cuGraphicsGLRegisterImage` which uses `*mut c_void` and the cudarc functions which use `CUgraphicsResource`. These should both be pointer types — add `.cast()` as needed.

- [ ] **Step 4: Commit**

```
feat(encode): add EGL-GL-CUDA bridge for DMA-BUF import
```

---

### Task 3: Integrate `EglCudaBridge` into the encoder

**Files:**
- Modify: `crates/stargaze-server/src/encode/ffmpeg.rs`

Replace the existing `upload_dmabuf_and_encode()` and `upload_dmabuf_cuda_import()` functions with calls through `EglCudaBridge`.

- [ ] **Step 1: Add `egl_bridge` field to `FfmpegEncoder`**

Add to the `FfmpegEncoder` struct:

```rust
/// EGL-GL-CUDA bridge for DMA-BUF import. Lazily initialized on first
/// DMA-BUF frame (requires CUDA context to be active).
egl_bridge: Option<super::egl_cuda::EglCudaBridge>,
```

And in `init_encoder()`, initialize it as `None` in the struct literal:

```rust
egl_bridge: None,
```

- [ ] **Step 2: Replace `upload_dmabuf_and_encode()`**

Replace the entire `upload_dmabuf_and_encode()` function with:

```rust
/// Imports a DMA-BUF frame via EGL→GL→CUDA, then encodes.
///
/// On first call, lazily initializes the EGL-GL-CUDA bridge.
/// Subsequent calls reuse the persistent GL texture and CUDA registration.
fn upload_dmabuf_and_encode(
    encoder: &mut FfmpegEncoder,
    info: &stargaze_core::capture::DmaBufInfo,
    pts: u64,
    force_idr: bool,
) -> Result<(), EncodeError> {
    use cudarc::driver::sys as cu;

    // Push FFmpeg's CUDA context as current on this thread.
    unsafe {
        let res = cu::cuCtxPushCurrent_v2(encoder.cuda_ctx);
        if res != cu::CUresult::CUDA_SUCCESS {
            return Err(EncodeError::EncodeFrameError {
                frame: pts,
                reason: format!("cuCtxPushCurrent failed: {res:?}"),
            });
        }
    }

    // Lazily init EGL bridge (needs CUDA context active).
    if encoder.egl_bridge.is_none() {
        match super::egl_cuda::EglCudaBridge::new(
            info.width,
            info.height,
            encoder.cuda_ctx,
        ) {
            Ok(bridge) => encoder.egl_bridge = Some(bridge),
            Err(e) => {
                unsafe {
                    let mut old: cu::CUcontext = ptr::null_mut();
                    cu::cuCtxPopCurrent_v2(&raw mut old);
                }
                return Err(e);
            }
        }
    }

    let bridge = encoder.egl_bridge.as_ref().unwrap();

    // Import DMA-BUF → EGL → GL → CUDA → CPU (linear, de-tiled).
    let cpu_buf = match bridge.import_dmabuf_to_cpu(info) {
        Ok(buf) => buf,
        Err(e) => {
            unsafe {
                let mut old: cu::CUcontext = ptr::null_mut();
                cu::cuCtxPopCurrent_v2(&raw mut old);
            }
            return Err(e);
        }
    };

    // Pop CUDA context before encoding (upload_cpu_data_and_encode
    // manages its own CUDA context via av_hwframe_transfer_data).
    unsafe {
        let mut old: cu::CUcontext = ptr::null_mut();
        cu::cuCtxPopCurrent_v2(&raw mut old);
    }

    // Feed linear pixels through the standard encode path.
    // The data from GL is RGBA (GL_RGBA8 texture), but our capture format
    // says BGRA. The EGL import preserves the original DMA-BUF format,
    // so we pass the original format through.
    let stride = info.width * 4; // GL output is tightly packed RGBA
    upload_cpu_data_and_encode(
        encoder,
        &cpu_buf,
        info.width,
        info.height,
        stride,
        info.format,
        pts,
        force_idr,
    )
}
```

- [ ] **Step 3: Remove old `upload_dmabuf_cuda_import()` function**

Delete the entire `upload_dmabuf_cuda_import()` function (lines ~536-640 in current code). It's fully replaced by the EGL bridge.

- [ ] **Step 4: Verify it compiles**

Run: `nix develop --command cargo check --workspace`

Fix any remaining type issues.

- [ ] **Step 5: Run clippy**

Run: `nix develop --command cargo clippy --workspace -- -W clippy::pedantic`

Fix all new warnings.

- [ ] **Step 6: Run tests**

Run: `nix develop --command cargo test --workspace`

Expected: All existing tests pass (no runtime tests for DMA-BUF import — that requires the actual GPU).

- [ ] **Step 7: Commit**

```
feat(encode): replace CUDA DMA-BUF import with EGL-GL-CUDA bridge

Direct cuImportExternalMemory with DMABUF_FD is not supported on
desktop NVIDIA GPUs. Switch to the proven EGL→GL→CUDA path used
by Sunshine and waycap-rs: import DMA-BUF as EGL image, bind to
GL texture, copy to CUDA via cuGraphicsGLRegisterImage.
```

---

### Task 4: Update `flake.nix` runtime library paths

**Files:**
- Modify: `flake.nix`

- [ ] **Step 1: Add `libglvnd` to build inputs and runtime paths**

In `flake.nix`, add `pkgs.libglvnd` to `commonBuildInputs`:

```nix
commonBuildInputs = [
    ffmpeg
    pkgs.pipewire
    pkgs.dbus
    pkgs.SDL2
    pkgs.libopus
    pkgs.libclang
    pkgs.llvmPackages.libclang
    pkgs.libglvnd  # EGL + GL dispatch (for DMA-BUF → GL → CUDA pipeline)
];
```

And to `runtimeLibPath`:

```nix
runtimeLibPath = pkgs.lib.makeLibraryPath [
    ffmpeg
    pkgs.pipewire
    pkgs.dbus
    pkgs.SDL2
    pkgs.libopus
    pkgs.libglvnd
];
```

- [ ] **Step 2: Verify dev shell works**

Run: `nix develop --command cargo check --workspace`

- [ ] **Step 3: Commit**

```
chore(nix): add libglvnd for EGL/GL runtime support
```

---

### Task 5: Build, test, and runtime verify

- [ ] **Step 1: Full build**

Run: `nix develop --command cargo build --workspace --release`

Expected: Success.

- [ ] **Step 2: Run tests**

Run: `nix develop --command cargo test --workspace`

Expected: All tests pass.

- [ ] **Step 3: Run clippy**

Run: `nix develop --command cargo clippy --workspace -- -W clippy::pedantic`

Expected: Clean (or only pre-existing warnings).

- [ ] **Step 4: User runtime test**

Ask the user to run the server and client:

```bash
# Server
./target/release/stargaze-server --bind 0.0.0.0 --port 60003 --resolution 3440x1440 --framerate 144

# Client
./target/release/stargaze-client --host <server-ip> --port 60003
```

**Expected outcomes:**
- Server logs: "EGL-GL-CUDA bridge initialized (headless, DMA-BUF import ready)"
- No "Skipping frame" warnings
- Client displays the actual remote screen content

**If it fails:**
- "Failed to load libEGL.so.1" → libglvnd not in `LD_LIBRARY_PATH`
- "cuGraphicsGLRegisterImage failed" → EGL initialized on wrong GPU (need device enumeration)
- "eglCreateImage failed" → DRM fourcc mapping wrong (try logging the fourcc value)
- Still visual artifacts → format mismatch between EGL and encoder
