//! EGL → GL → CUDA bridge for importing DMA-BUF frames.
//!
//! Uses a headless EGL device context (no display server) to:
//! 1. Import DMA-BUF fds as EGL images
//! 2. Bind them to a persistent GL texture
//! 3. Copy via CUDA-GL interop into device memory
//!
//! Reference: <https://github.com/Adonca2203/waycap-rs> (Sunshine pattern)

use std::ffi::c_void;
use std::os::unix::io::RawFd;
use std::ptr;

use stargaze_core::capture::{DmaBufInfo, PixelFormat};
use stargaze_core::encode::EncodeError;
use tracing::{debug, info, warn};

// ── EGL constants not in khronos-egl ────────────────────────────────────

/// `EGL_LINUX_DMA_BUF_EXT` (0x3270) — target for `eglCreateImage`.
const EGL_LINUX_DMA_BUF_EXT: khronos_egl::Enum = 0x3270;
/// `EGL_LINUX_DRM_FOURCC_EXT` (0x3271)
const EGL_LINUX_DRM_FOURCC_EXT: khronos_egl::Attrib = 0x3271;

// Plane 0 attributes
const EGL_DMA_BUF_PLANE0_FD_EXT: khronos_egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: khronos_egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: khronos_egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: khronos_egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: khronos_egl::Attrib = 0x3444;

/// `EGL_PLATFORM_DEVICE_EXT` (0x313F) — platform type for headless device display.
const EGL_PLATFORM_DEVICE_EXT: khronos_egl::Enum = 0x313F;

/// `EGL_PLATFORM_GBM_KHR` (0x31D7) — platform type for GBM-backed display.
/// Required for DMA-BUF import on NVIDIA (unlike DEVICE_EXT, GBM integrates with DRM/KMS).
const EGL_PLATFORM_GBM_KHR: khronos_egl::Enum = 0x31D7;

/// `GL_TEXTURE_EXTERNAL_OES` (0x8D65) — required by NVIDIA for DMA-BUF EGL image binding.
/// NVIDIA does not support binding linear/implicit-modifier DMA-BUF images to `GL_TEXTURE_2D`.
const GL_TEXTURE_EXTERNAL_OES: gl::types::GLenum = 0x8D65;

// ── GBM FFI (minimal — only what we need for EGL display creation) ──

/// Opaque GBM device handle.
enum GbmDevice {}

/// `gbm_create_device(fd)` — create a GBM device from a DRM render node fd.
type GbmCreateDeviceFn = unsafe extern "C" fn(fd: RawFd) -> *mut GbmDevice;
/// `gbm_device_destroy(device)` — destroy a GBM device.
type GbmDeviceDestroyFn = unsafe extern "C" fn(device: *mut GbmDevice);

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

// ── CUDA-GL FFI (not in cudarc — loaded dynamically from libcuda.so) ────

use cudarc::driver::sys::CUgraphicsResource;

/// Function pointer type for `cuGraphicsGLRegisterImage`.
type CuGraphicsGLRegisterImageFn = unsafe extern "C" fn(
    resource: *mut CUgraphicsResource,
    image: gl::types::GLuint,
    target: gl::types::GLenum,
    flags: std::ffi::c_uint,
) -> cudarc::driver::sys::CUresult;

/// Load `cuGraphicsGLRegisterImage` from `libcuda.so.1` at runtime.
///
/// This function is part of the CUDA driver API but not exposed by cudarc.
fn load_cu_graphics_gl_register_image() -> Result<CuGraphicsGLRegisterImageFn, EncodeError> {
    unsafe {
        let lib = libloading::Library::new("libcuda.so.1")
            .map_err(|e| EncodeError::InitError(format!("Failed to load libcuda.so.1: {e}")))?;
        let func: libloading::Symbol<'_, CuGraphicsGLRegisterImageFn> =
            lib.get(b"cuGraphicsGLRegisterImage\0").map_err(|e| {
                EncodeError::InitError(format!(
                    "Failed to find cuGraphicsGLRegisterImage in libcuda.so.1: {e}"
                ))
            })?;
        // Extract raw fn pointer before leaking the library handle.
        // libcuda.so.1 must stay loaded for the process lifetime since CUDA is always active.
        let raw_fn = *func;
        std::mem::forget(lib);
        Ok(raw_fn)
    }
}

// ── GPU NV12 conversion ─────────────────────────────────────────────────

/// CUDA kernel: RGBA → NV12, BT.709 limited range (matches the CPU
/// converter in `super::convert` and the colorspace the encoder
/// advertises). One thread per 2x2 pixel block.
const NV12_KERNEL_SRC: &str = r#"
extern "C" __global__ void rgba_to_nv12(
    const unsigned char* __restrict__ src, unsigned long long src_pitch,
    unsigned char* __restrict__ y_plane, unsigned long long y_pitch,
    unsigned char* __restrict__ uv_plane, unsigned long long uv_pitch,
    int width, int height)
{
    int x = (blockIdx.x * blockDim.x + threadIdx.x) * 2;
    int yy = (blockIdx.y * blockDim.y + threadIdx.y) * 2;
    if (x >= width || yy >= height) return;

    int rsum = 0, gsum = 0, bsum = 0;
    for (int dy = 0; dy < 2; dy++) {
        const unsigned char* row = src + (unsigned long long)(yy + dy) * src_pitch + x * 4;
        unsigned char* yrow = y_plane + (unsigned long long)(yy + dy) * y_pitch + x;
        for (int dx = 0; dx < 2; dx++) {
            int r = row[dx * 4 + 0];
            int g = row[dx * 4 + 1];
            int b = row[dx * 4 + 2];
            rsum += r; gsum += g; bsum += b;
            yrow[dx] = (unsigned char)(((47 * r + 157 * g + 16 * b + 128) >> 8) + 16);
        }
    }
    int r = (rsum + 2) >> 2;
    int g = (gsum + 2) >> 2;
    int b = (bsum + 2) >> 2;
    int u = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
    int v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    unsigned char* uv = uv_plane + (unsigned long long)(yy / 2) * uv_pitch + x;
    uv[0] = (unsigned char)min(max(u, 16), 240);
    uv[1] = (unsigned char)min(max(v, 16), 240);
}
"#;

/// On-GPU RGBA→NV12 converter: NVRTC-compiled kernel plus a persistent
/// pitched device buffer the GL texture is copied into before conversion.
struct GpuNv12Converter {
    module: cudarc::driver::sys::CUmodule,
    func: cudarc::driver::sys::CUfunction,
    rgba_buf: cudarc::driver::sys::CUdeviceptr,
    rgba_pitch: usize,
}

impl GpuNv12Converter {
    /// Compiles the kernel and allocates the staging buffer.
    ///
    /// Requires the CUDA context to be current on this thread. Fails
    /// gracefully (caller falls back to the CPU path) when NVRTC is not
    /// available at runtime.
    fn new(width: u32, height: u32) -> Result<Self, EncodeError> {
        use cudarc::driver::sys as cu;

        let ptx = cudarc::nvrtc::compile_ptx(NV12_KERNEL_SRC)
            .map_err(|e| EncodeError::InitError(format!("NVRTC compile failed: {e:?}")))?;
        let ptx_src = std::ffi::CString::new(ptx.to_src())
            .map_err(|e| EncodeError::InitError(format!("PTX contains NUL: {e}")))?;

        unsafe {
            let mut module: cu::CUmodule = ptr::null_mut();
            let res = cu::cuModuleLoadData(&raw mut module, ptx_src.as_ptr().cast());
            if res != cu::CUresult::CUDA_SUCCESS {
                return Err(EncodeError::InitError(format!(
                    "cuModuleLoadData failed: {res:?}"
                )));
            }

            let mut func: cu::CUfunction = ptr::null_mut();
            let res = cu::cuModuleGetFunction(&raw mut func, module, c"rgba_to_nv12".as_ptr());
            if res != cu::CUresult::CUDA_SUCCESS {
                cu::cuModuleUnload(module);
                return Err(EncodeError::InitError(format!(
                    "cuModuleGetFunction failed: {res:?}"
                )));
            }

            let mut rgba_buf: cu::CUdeviceptr = 0;
            let mut rgba_pitch: usize = 0;
            let res = cu::cuMemAllocPitch_v2(
                &raw mut rgba_buf,
                &raw mut rgba_pitch,
                width as usize * 4,
                height as usize,
                16,
            );
            if res != cu::CUresult::CUDA_SUCCESS {
                cu::cuModuleUnload(module);
                return Err(EncodeError::InitError(format!(
                    "cuMemAllocPitch failed: {res:?}"
                )));
            }

            info!(
                width,
                height, "GPU NV12 converter initialized (NVRTC kernel)"
            );
            Ok(Self {
                module,
                func,
                rgba_buf,
                rgba_pitch,
            })
        }
    }

    /// Launches the conversion kernel: staging RGBA buffer → NV12 planes.
    ///
    /// Requires the CUDA context to be current on this thread.
    unsafe fn launch(
        &self,
        y_plane: cudarc::driver::sys::CUdeviceptr,
        y_pitch: usize,
        uv_plane: cudarc::driver::sys::CUdeviceptr,
        uv_pitch: usize,
        width: u32,
        height: u32,
    ) -> Result<(), EncodeError> {
        use cudarc::driver::sys as cu;

        let src_pitch = self.rgba_pitch as u64;
        let y_pitch = y_pitch as u64;
        let uv_pitch = uv_pitch as u64;
        let w = width.cast_signed();
        let h = height.cast_signed();

        let mut params: [*mut c_void; 8] = [
            ptr::from_ref(&self.rgba_buf).cast_mut().cast(),
            ptr::from_ref(&src_pitch).cast_mut().cast(),
            ptr::from_ref(&y_plane).cast_mut().cast(),
            ptr::from_ref(&y_pitch).cast_mut().cast(),
            ptr::from_ref(&uv_plane).cast_mut().cast(),
            ptr::from_ref(&uv_pitch).cast_mut().cast(),
            ptr::from_ref(&w).cast_mut().cast(),
            ptr::from_ref(&h).cast_mut().cast(),
        ];

        // One thread per 2x2 block.
        let block = (16u32, 16u32);
        let grid_x = width.div_ceil(2).div_ceil(block.0);
        let grid_y = height.div_ceil(2).div_ceil(block.1);

        unsafe {
            let res = cu::cuLaunchKernel(
                self.func,
                grid_x,
                grid_y,
                1,
                block.0,
                block.1,
                1,
                0,               // shared mem
                ptr::null_mut(), // default stream
                params.as_mut_ptr(),
                ptr::null_mut(),
            );
            if res != cu::CUresult::CUDA_SUCCESS {
                return Err(EncodeError::EncodeFrameError {
                    frame: 0,
                    reason: format!("cuLaunchKernel(rgba_to_nv12) failed: {res:?}"),
                });
            }
            // The encoder consumes the frame on its own stream — make the
            // conversion result visible before send_frame.
            let res = cu::cuCtxSynchronize();
            if res != cu::CUresult::CUDA_SUCCESS {
                return Err(EncodeError::EncodeFrameError {
                    frame: 0,
                    reason: format!("cuCtxSynchronize after NV12 kernel failed: {res:?}"),
                });
            }
        }
        Ok(())
    }
}

impl Drop for GpuNv12Converter {
    fn drop(&mut self) {
        unsafe {
            if self.rgba_buf != 0 {
                cudarc::driver::sys::cuMemFree_v2(self.rgba_buf);
            }
            if !self.module.is_null() {
                cudarc::driver::sys::cuModuleUnload(self.module);
            }
        }
    }
}

// ── EglCudaBridge ───────────────────────────────────────────────────────

/// Manages EGL context, GL texture, and CUDA-GL interop for DMA-BUF import.
///
/// Created once during encoder initialization. For each frame:
/// 1. `import_dmabuf()` creates an EGL image, binds to GL texture, copies to CUDA
/// 2. Returns linear CPU data (de-tiled by the GPU) for the existing encode path
pub(crate) struct EglCudaBridge {
    egl: khronos_egl::DynamicInstance<khronos_egl::EGL1_5>,
    display: khronos_egl::Display,
    context: khronos_egl::Context,
    texture_id: gl::types::GLuint,
    cuda_resource: CUgraphicsResource,
    width: u32,
    height: u32,
    dmabuf_modifiers_supported: bool,
    shader_program: gl::types::GLuint,
    quad_vao: gl::types::GLuint,
    quad_vbo: gl::types::GLuint,
    /// GBM device handle (must outlive the EGL display).
    gbm_device: *mut GbmDevice,
    /// `gbm_device_destroy` function pointer (loaded from `libgbm.so.1`).
    gbm_destroy_fn: Option<GbmDeviceDestroyFn>,
    /// DRM render node fd (must outlive the GBM device).
    drm_fd: RawFd,
    /// On-GPU NV12 converter. `None` when NVRTC is unavailable — frames
    /// then take the legacy GPU→CPU→GPU path.
    gpu_converter: Option<GpuNv12Converter>,
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
        let egl = unsafe {
            khronos_egl::DynamicInstance::<khronos_egl::EGL1_5>::load_required_from_filename(
                "libEGL.so.1",
            )
        }
        .map_err(|e| {
            EncodeError::InitError(format!(
                "Failed to load libEGL.so.1: {e}. \
                 Ensure libglvnd or NVIDIA EGL is in LD_LIBRARY_PATH."
            ))
        })?;

        // Step 2: Get EGL display (GBM-backed preferred, device platform fallback).
        let (display, gbm_device, gbm_destroy_fn, drm_fd) = Self::get_headless_display(&egl)?;

        egl.initialize(display)
            .map_err(|e| EncodeError::InitError(format!("eglInitialize failed: {e}")))?;

        // Desktop GL is required for FBO rendering and CUDA-GL interop.
        // We use GL_TEXTURE_EXTERNAL_OES for DMA-BUF import + shader blit to GL_TEXTURE_2D.
        egl.bind_api(khronos_egl::OPENGL_API)
            .map_err(|e| EncodeError::InitError(format!("eglBindAPI(OPENGL_API) failed: {e}")))?;

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
        let dmabuf_modifiers_supported = ext_str.contains("EGL_EXT_image_dma_buf_import_modifiers");

        debug!(
            dmabuf_modifiers = dmabuf_modifiers_supported,
            "EGL DMA-BUF import supported"
        );

        // Step 4: Choose config and create context.
        let config_attribs = [
            khronos_egl::SURFACE_TYPE,
            khronos_egl::PBUFFER_BIT,
            khronos_egl::RENDERABLE_TYPE,
            khronos_egl::OPENGL_BIT,
            khronos_egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &config_attribs)
            .map_err(|e| EncodeError::InitError(format!("eglChooseConfig failed: {e}")))?
            .ok_or_else(|| EncodeError::InitError("No suitable EGL config found".to_string()))?;

        // EGL_CONTEXT_CLIENT_VERSION (0x3098) — same as EGL_CONTEXT_MAJOR_VERSION.
        // Request OpenGL 3.x compatibility profile (no minor version), matching Sunshine.
        // Specifying MAJOR+MINOR gives a core profile where GL_OES_EGL_image may fail.
        const EGL_CONTEXT_CLIENT_VERSION: khronos_egl::Int = 0x3098;
        let ctx_attribs = [EGL_CONTEXT_CLIENT_VERSION, 3, khronos_egl::NONE];
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
            // create_pbuffer_surface takes &[Int] = &[i32]
            let pbuf_attribs: [khronos_egl::Int; 5] = [
                khronos_egl::WIDTH,
                1,
                khronos_egl::HEIGHT,
                1,
                khronos_egl::NONE,
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

        unsafe {
            let gl_version = gl::GetString(gl::VERSION);
            let gl_renderer = gl::GetString(gl::RENDERER);
            if !gl_version.is_null() {
                let ver = std::ffi::CStr::from_ptr(gl_version.cast());
                info!(version = %ver.to_string_lossy(), "GL context");
            }
            if !gl_renderer.is_null() {
                let ren = std::ffi::CStr::from_ptr(gl_renderer.cast());
                info!(renderer = %ren.to_string_lossy(), "GL renderer");
            }

            // glGetString(GL_EXTENSIONS) is invalid in core profile — use glGetStringi.
            let mut num_ext: gl::types::GLint = 0;
            gl::GetIntegerv(gl::NUM_EXTENSIONS, &mut num_ext);
            let mut has_egl_image = false;
            for i in 0..num_ext as gl::types::GLuint {
                let ext = gl::GetStringi(gl::EXTENSIONS, i);
                if !ext.is_null() {
                    let name = std::ffi::CStr::from_ptr(ext.cast());
                    if name.to_bytes() == b"GL_OES_EGL_image" {
                        has_egl_image = true;
                        break;
                    }
                }
            }
            info!(
                has_gl_oes_egl_image = has_egl_image,
                extension_count = num_ext,
                "GL extensions"
            );
            if !has_egl_image {
                warn!("GL_OES_EGL_image NOT found — glEGLImageTargetTexture2DOES will likely fail");
            }

            // Drain any stale GL errors before texture creation.
            while gl::GetError() != gl::NO_ERROR {}
        }

        // Step 6: Create persistent GL texture (RGBA8, sized to frame).
        let texture_id = unsafe {
            let mut tex = 0;
            gl::GenTextures(1, &mut tex);
            gl::BindTexture(gl::TEXTURE_2D, tex);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                ptr::null(),
            );
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
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

        let (shader_program, quad_vao, quad_vbo) = unsafe { Self::create_blit_resources()? };

        // Step 7: Push CUDA context and register GL texture.
        unsafe {
            let res = cudarc::driver::sys::cuCtxPushCurrent_v2(cuda_ctx);
            if res != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                return Err(EncodeError::InitError(format!(
                    "cuCtxPushCurrent failed during EGL bridge init: {res:?}"
                )));
            }
        }

        let register_image_fn = load_cu_graphics_gl_register_image()?;

        let cuda_resource = unsafe {
            let mut resource: CUgraphicsResource = ptr::null_mut();
            let res = register_image_fn(
                &mut resource,
                texture_id,
                gl::TEXTURE_2D,
                0x00, // CU_GRAPHICS_REGISTER_FLAGS_NONE
            );
            if res != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                // Pop CUDA context before returning error.
                let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
                cudarc::driver::sys::cuCtxPopCurrent_v2(&mut old);
                return Err(EncodeError::InitError(format!(
                    "cuGraphicsGLRegisterImage failed: {res:?}"
                )));
            }

            // Set map flags (read-only from CUDA's perspective).
            let res = cudarc::driver::sys::cuGraphicsResourceSetMapFlags_v2(
                resource, 0, // CU_GRAPHICS_MAP_RESOURCE_FLAGS_NONE
            );
            if res != cudarc::driver::sys::CUresult::CUDA_SUCCESS {
                cudarc::driver::sys::cuGraphicsUnregisterResource(resource);
                let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
                cudarc::driver::sys::cuCtxPopCurrent_v2(&mut old);
                return Err(EncodeError::InitError(format!(
                    "cuGraphicsResourceSetMapFlags failed: {res:?}"
                )));
            }
            resource
        };

        // Compile the on-GPU NV12 converter while the CUDA context is
        // still current. Failure is non-fatal (CPU fallback path).
        let gpu_converter = match GpuNv12Converter::new(width, height) {
            Ok(c) => Some(c),
            Err(e) => {
                warn!("GPU NV12 converter unavailable ({e}), using CPU conversion fallback");
                None
            }
        };

        unsafe {
            let mut old: cudarc::driver::sys::CUcontext = ptr::null_mut();
            cudarc::driver::sys::cuCtxPopCurrent_v2(&mut old);
        }

        info!(
            width,
            height,
            gpu_nv12 = gpu_converter.is_some(),
            "EGL-GL-CUDA bridge initialized (headless, DMA-BUF import ready)"
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
            shader_program,
            quad_vao,
            quad_vbo,
            gbm_device,
            gbm_destroy_fn,
            drm_fd,
            gpu_converter,
        })
    }

    /// Whether the fully-GPU import path (no CPU round trip) is available.
    pub(crate) fn has_gpu_path(&self) -> bool {
        self.gpu_converter.is_some()
    }

    /// Compile the external-texture blit shader and create a fullscreen quad VAO.
    ///
    /// # Safety
    /// Requires active GL context on this thread.
    unsafe fn create_blit_resources()
    -> Result<(gl::types::GLuint, gl::types::GLuint, gl::types::GLuint), EncodeError> {
        use std::ffi::CString;

        let vert_src = CString::new(
            "#version 330\n\
             layout(location=0) in vec2 pos;\n\
             out vec2 uv;\n\
             void main() {\n\
               uv = pos * 0.5 + 0.5;\n\
               gl_Position = vec4(pos, 0.0, 1.0);\n\
             }\n",
        )
        .unwrap();

        let frag_src = CString::new(
            "#version 330\n\
             #extension GL_OES_EGL_image_external : require\n\
             in vec2 uv;\n\
             out vec4 frag;\n\
             uniform samplerExternalOES tex;\n\
             void main() {\n\
               frag = texture2D(tex, uv);\n\
             }\n",
        )
        .unwrap();

        let compile_shader =
            |src: &CString, kind: gl::types::GLenum| -> Result<gl::types::GLuint, EncodeError> {
                unsafe {
                    let shader = gl::CreateShader(kind);
                    gl::ShaderSource(shader, 1, &src.as_ptr(), ptr::null());
                    gl::CompileShader(shader);

                    let mut ok: gl::types::GLint = 0;
                    gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut ok);
                    if ok == 0 {
                        let mut len: gl::types::GLint = 0;
                        gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut len);
                        let mut buf = vec![0u8; len as usize];
                        gl::GetShaderInfoLog(shader, len, ptr::null_mut(), buf.as_mut_ptr().cast());
                        gl::DeleteShader(shader);
                        let log = String::from_utf8_lossy(&buf);
                        return Err(EncodeError::InitError(format!(
                            "Shader compile failed: {log}"
                        )));
                    }
                    Ok(shader)
                }
            };

        let vs = compile_shader(&vert_src, gl::VERTEX_SHADER)?;
        let fs = compile_shader(&frag_src, gl::FRAGMENT_SHADER)?;

        unsafe {
            let program = gl::CreateProgram();
            gl::AttachShader(program, vs);
            gl::AttachShader(program, fs);
            gl::LinkProgram(program);

            let mut ok: gl::types::GLint = 0;
            gl::GetProgramiv(program, gl::LINK_STATUS, &mut ok);
            if ok == 0 {
                let mut len: gl::types::GLint = 0;
                gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut len);
                let mut buf = vec![0u8; len as usize];
                gl::GetProgramInfoLog(program, len, ptr::null_mut(), buf.as_mut_ptr().cast());
                gl::DeleteProgram(program);
                gl::DeleteShader(vs);
                gl::DeleteShader(fs);
                let log = String::from_utf8_lossy(&buf);
                return Err(EncodeError::InitError(format!("Shader link failed: {log}")));
            }

            gl::DeleteShader(vs);
            gl::DeleteShader(fs);

            #[rustfmt::skip]
            let quad_verts: [f32; 12] = [
                -1.0, -1.0,
                 1.0, -1.0,
                -1.0,  1.0,
                -1.0,  1.0,
                 1.0, -1.0,
                 1.0,  1.0,
            ];

            let mut vao = 0;
            let mut vbo = 0;
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut vbo);

            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (quad_verts.len() * std::mem::size_of::<f32>()) as gl::types::GLsizeiptr,
                quad_verts.as_ptr().cast(),
                gl::STATIC_DRAW,
            );
            gl::EnableVertexAttribArray(0);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 0, ptr::null());
            gl::BindVertexArray(0);

            debug!(program, vao, vbo, "Created blit shader and quad VAO");
            Ok((program, vao, vbo))
        }
    }

    /// Get a headless EGL display backed by the NVIDIA GPU.
    ///
    /// Tries GBM-backed display first (required for DMA-BUF import on NVIDIA),
    /// then falls back to EGL device platform, then default display.
    fn get_headless_display(
        egl: &khronos_egl::DynamicInstance<khronos_egl::EGL1_5>,
    ) -> Result<
        (
            khronos_egl::Display,
            *mut GbmDevice,
            Option<GbmDeviceDestroyFn>,
            RawFd,
        ),
        EncodeError,
    > {
        // Try GBM-backed display first (Sunshine pattern — needed for DMA-BUF + NVIDIA).
        match Self::get_gbm_display(egl) {
            Ok((display, gbm_dev, destroy_fn, drm_fd)) => {
                return Ok((display, gbm_dev, Some(destroy_fn), drm_fd));
            }
            Err(e) => {
                warn!("GBM display init failed ({e}), trying EGL device platform fallback");
            }
        }

        let client_exts = egl
            .query_string(None, khronos_egl::EXTENSIONS)
            .ok()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();

        let has_device_enum = client_exts.contains("EGL_EXT_device_enumeration")
            || client_exts.contains("EGL_EXT_device_base");
        let has_platform_device = client_exts.contains("EGL_EXT_platform_device");

        if has_device_enum && has_platform_device {
            match Self::get_device_display(egl) {
                Ok(display) => return Ok((display, ptr::null_mut(), None, -1)),
                Err(e) => {
                    debug!("EGL device enumeration failed ({e}), falling back to default display");
                }
            }
        } else {
            debug!(
                client_exts,
                "EGL device enumeration extensions not available, using default display"
            );
        }

        let display = unsafe { egl.get_display(khronos_egl::DEFAULT_DISPLAY) };
        let display = display.ok_or_else(|| {
            EncodeError::InitError(
                "eglGetDisplay(EGL_DEFAULT_DISPLAY) returned EGL_NO_DISPLAY".to_string(),
            )
        })?;
        Ok((display, ptr::null_mut(), None, -1))
    }

    /// Open a DRM render node, create a GBM device, and get an EGL display from it.
    fn get_gbm_display(
        egl: &khronos_egl::DynamicInstance<khronos_egl::EGL1_5>,
    ) -> Result<
        (
            khronos_egl::Display,
            *mut GbmDevice,
            GbmDeviceDestroyFn,
            RawFd,
        ),
        EncodeError,
    > {
        // Load GBM functions dynamically.
        let gbm_lib = unsafe { libloading::Library::new("libgbm.so.1") }.map_err(|e| {
            EncodeError::InitError(format!(
                "Failed to load libgbm.so.1: {e}. Ensure mesa-gbm or libgbm is installed."
            ))
        })?;
        let gbm_create: libloading::Symbol<'_, GbmCreateDeviceFn> =
            unsafe { gbm_lib.get(b"gbm_create_device\0") }
                .map_err(|e| EncodeError::InitError(format!("gbm_create_device not found: {e}")))?;
        let gbm_destroy: libloading::Symbol<'_, GbmDeviceDestroyFn> = unsafe {
            gbm_lib.get(b"gbm_device_destroy\0")
        }
        .map_err(|e| EncodeError::InitError(format!("gbm_device_destroy not found: {e}")))?;
        let gbm_create_fn = *gbm_create;
        let gbm_destroy_fn = *gbm_destroy;
        // Keep libgbm loaded for the process lifetime.
        std::mem::forget(gbm_lib);

        // Find a DRM render node.
        let drm_fd = Self::open_drm_render_node()?;

        let gbm_dev = unsafe { gbm_create_fn(drm_fd) };
        if gbm_dev.is_null() {
            unsafe { libc::close(drm_fd) };
            return Err(EncodeError::InitError(
                "gbm_create_device returned NULL".to_string(),
            ));
        }

        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_GBM_KHR,
                gbm_dev.cast::<c_void>(),
                &[khronos_egl::ATTRIB_NONE],
            )
        }
        .map_err(|e| {
            unsafe {
                gbm_destroy_fn(gbm_dev);
                libc::close(drm_fd);
            }
            EncodeError::InitError(format!("eglGetPlatformDisplay(GBM) failed: {e}"))
        })?;

        info!("Using GBM-backed EGL display (DRM render node)");
        Ok((display, gbm_dev, gbm_destroy_fn, drm_fd))
    }

    /// Find and open a DRM render node (e.g., `/dev/dri/renderD128`).
    fn open_drm_render_node() -> Result<RawFd, EncodeError> {
        for i in 128..136 {
            let path = std::ffi::CString::new(format!("/dev/dri/renderD{i}")).unwrap();
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd >= 0 {
                info!(node = %format!("/dev/dri/renderD{i}"), "Opened DRM render node");
                return Ok(fd);
            }
        }
        Err(EncodeError::InitError(
            "No DRM render node found (/dev/dri/renderD128..135)".to_string(),
        ))
    }

    /// Enumerate EGL devices and open a platform display on the first one.
    fn get_device_display(
        egl: &khronos_egl::DynamicInstance<khronos_egl::EGL1_5>,
    ) -> Result<khronos_egl::Display, EncodeError> {
        type EglQueryDevicesExtFn = unsafe extern "C" fn(
            max_devices: khronos_egl::Int,
            devices: *mut *mut c_void,
            num_devices: *mut khronos_egl::Int,
        ) -> khronos_egl::Boolean;

        let query_fn: EglQueryDevicesExtFn = egl
            .get_proc_address("eglQueryDevicesEXT")
            .map(|p| unsafe { std::mem::transmute(p) })
            .ok_or_else(|| {
                EncodeError::InitError("eglQueryDevicesEXT not available".to_string())
            })?;

        let mut num_devices: khronos_egl::Int = 0;
        if unsafe { query_fn(0, ptr::null_mut(), &mut num_devices) } == khronos_egl::FALSE {
            return Err(EncodeError::InitError(
                "eglQueryDevicesEXT(count) failed".to_string(),
            ));
        }
        if num_devices == 0 {
            return Err(EncodeError::InitError("No EGL devices found".to_string()));
        }

        let mut devices: Vec<*mut c_void> = vec![ptr::null_mut(); num_devices as usize];
        if unsafe { query_fn(num_devices, devices.as_mut_ptr(), &mut num_devices) }
            == khronos_egl::FALSE
        {
            return Err(EncodeError::InitError(
                "eglQueryDevicesEXT(list) failed".to_string(),
            ));
        }

        debug!(num_devices, "EGL devices enumerated");

        for (idx, &device) in devices.iter().enumerate() {
            let display = unsafe {
                egl.get_platform_display(
                    EGL_PLATFORM_DEVICE_EXT,
                    device,
                    &[khronos_egl::ATTRIB_NONE],
                )
            };
            match display {
                Ok(d) => {
                    info!(device_index = idx, "Using EGL platform device display");
                    return Ok(d);
                }
                Err(e) => {
                    debug!(device_index = idx, error = %e, "EGL platform display failed for device");
                }
            }
        }

        Err(EncodeError::InitError(
            "eglGetPlatformDisplay failed for all enumerated devices".to_string(),
        ))
    }

    /// Import a DMA-BUF frame fully on the GPU: EGL image → GL texture →
    /// CUDA staging buffer → NV12 kernel directly into the encoder's
    /// hardware frame planes. No CPU round trip.
    ///
    /// Requires the CUDA context to be current on this thread and
    /// [`Self::has_gpu_path`] to be true.
    pub(crate) fn import_dmabuf_to_hw_frame(
        &self,
        info: &DmaBufInfo,
        hw_frame: &mut ffmpeg_next::frame::Video,
    ) -> Result<(), EncodeError> {
        use cudarc::driver::sys as cu;

        let converter =
            self.gpu_converter
                .as_ref()
                .ok_or_else(|| EncodeError::EncodeFrameError {
                    frame: 0,
                    reason: "GPU NV12 converter not available".to_string(),
                })?;

        self.import_and_blit(info)?;

        // Map the GL texture and copy device-to-device into the staging
        // buffer (the kernel can't read a CUarray through a raw pointer).
        unsafe {
            gl::Finish();

            let mut resource = self.cuda_resource;
            let res = cu::cuGraphicsMapResources(1, &raw mut resource, ptr::null_mut());
            if res != cu::CUresult::CUDA_SUCCESS {
                return Err(EncodeError::EncodeFrameError {
                    frame: 0,
                    reason: format!("cuGraphicsMapResources failed: {res:?}"),
                });
            }

            let mut cuda_array: cu::CUarray = ptr::null_mut();
            let res = cu::cuGraphicsSubResourceGetMappedArray(&raw mut cuda_array, resource, 0, 0);
            if res == cu::CUresult::CUDA_SUCCESS {
                let copy_desc = cu::CUDA_MEMCPY2D {
                    srcXInBytes: 0,
                    srcY: 0,
                    srcMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_ARRAY,
                    srcHost: ptr::null(),
                    srcDevice: 0,
                    srcArray: cuda_array,
                    srcPitch: 0,

                    dstXInBytes: 0,
                    dstY: 0,
                    dstMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_DEVICE,
                    dstHost: ptr::null_mut(),
                    dstDevice: converter.rgba_buf,
                    dstArray: ptr::null_mut(),
                    dstPitch: converter.rgba_pitch,

                    WidthInBytes: self.width as usize * 4,
                    Height: self.height as usize,
                };
                let copy_res = cu::cuMemcpy2D_v2(&raw const copy_desc);

                let mut resource = self.cuda_resource;
                cu::cuGraphicsUnmapResources(1, &raw mut resource, ptr::null_mut());

                if copy_res != cu::CUresult::CUDA_SUCCESS {
                    return Err(EncodeError::EncodeFrameError {
                        frame: 0,
                        reason: format!("cuMemcpy2D (array→device) failed: {copy_res:?}"),
                    });
                }
            } else {
                let mut resource = self.cuda_resource;
                cu::cuGraphicsUnmapResources(1, &raw mut resource, ptr::null_mut());
                return Err(EncodeError::EncodeFrameError {
                    frame: 0,
                    reason: format!("cuGraphicsSubResourceGetMappedArray failed: {res:?}"),
                });
            }
        }

        // Convert into the hw frame's NV12 planes.
        unsafe {
            let raw = hw_frame.as_mut_ptr();
            let y_plane = (*raw).data[0] as cu::CUdeviceptr;
            let uv_plane = (*raw).data[1] as cu::CUdeviceptr;
            let y_pitch = usize::try_from((*raw).linesize[0]).unwrap_or(self.width as usize);
            let uv_pitch = usize::try_from((*raw).linesize[1]).unwrap_or(self.width as usize);
            converter.launch(
                y_plane,
                y_pitch,
                uv_plane,
                uv_pitch,
                self.width,
                self.height,
            )?;
        }

        Ok(())
    }

    /// Shared front half of both import paths: create the EGL image from
    /// the DMA-BUF and blit it into the persistent GL texture.
    fn import_and_blit(&self, info: &DmaBufInfo) -> Result<(), EncodeError> {
        use std::os::unix::io::AsRawFd;

        // Ensure EGL context is current on this thread.
        self.egl
            .make_current(self.display, None, None, Some(self.context))
            .map_err(|e| EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("eglMakeCurrent failed in import_and_blit: {e}"),
            })?;

        let drm_fourcc = pixel_format_to_drm_fourcc(info.format);

        let mut attribs: Vec<khronos_egl::Attrib> = vec![
            EGL_LINUX_DRM_FOURCC_EXT,
            drm_fourcc as khronos_egl::Attrib,
            khronos_egl::WIDTH as khronos_egl::Attrib,
            info.width as khronos_egl::Attrib,
            khronos_egl::HEIGHT as khronos_egl::Attrib,
            info.height as khronos_egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            info.fd.as_raw_fd() as khronos_egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            info.offset as khronos_egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            info.stride as khronos_egl::Attrib,
        ];

        // DRM_FORMAT_MOD_INVALID means "no explicit modifier" — passing it to
        // eglCreateImage causes EGL_BAD_PARAMETER on NVIDIA.
        const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
        let has_explicit_modifier =
            self.dmabuf_modifiers_supported && info.modifier != DRM_FORMAT_MOD_INVALID;
        if has_explicit_modifier {
            attribs.extend_from_slice(&[
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (info.modifier & 0xFFFF_FFFF) as khronos_egl::Attrib,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (info.modifier >> 32) as khronos_egl::Attrib,
            ]);
        }

        attribs.push(khronos_egl::ATTRIB_NONE);

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

        let copy_result = unsafe { self.egl_image_to_gl_texture(egl_image) };

        // Always destroy the EGL image (it's per-frame).
        let _ = self.egl.destroy_image(self.display, egl_image);

        copy_result
    }

    /// Import a DMA-BUF frame: EGL image → GL texture → CUDA device → CPU buffer.
    ///
    /// Returns a linear (de-tiled) CPU buffer of BGRA/RGBA pixels.
    /// The caller feeds this into `upload_cpu_data_and_encode()`.
    /// Fallback path when the GPU converter is unavailable.
    pub(crate) fn import_dmabuf_to_cpu(&self, info: &DmaBufInfo) -> Result<Vec<u8>, EncodeError> {
        self.import_and_blit(info)?;
        let cpu_buf = unsafe { self.gl_texture_to_cpu_via_cuda()? };
        Ok(cpu_buf)
    }

    /// Bind EGL image to external GL texture, blit into persistent texture via shader.
    ///
    /// Uses `GL_TEXTURE_EXTERNAL_OES` because NVIDIA's GL driver doesn't support
    /// binding DMA-BUF EGL images to `GL_TEXTURE_2D` for linear/implicit-modifier
    /// buffers (returns `GL_INVALID_OPERATION` / 0x502).
    ///
    /// # Safety
    /// Requires active EGL context on this thread. `egl_image` must be valid.
    unsafe fn egl_image_to_gl_texture(
        &self,
        egl_image: khronos_egl::Image,
    ) -> Result<(), EncodeError> {
        type EglImageTargetFn =
            unsafe extern "C" fn(target: gl::types::GLenum, image: *const c_void);

        let proc = self.egl.get_proc_address("glEGLImageTargetTexture2DOES");
        let egl_image_target: EglImageTargetFn = match proc {
            Some(p) => unsafe { std::mem::transmute::<extern "system" fn(), EglImageTargetFn>(p) },
            None => {
                return Err(EncodeError::EncodeFrameError {
                    frame: 0,
                    reason: "glEGLImageTargetTexture2DOES not available".to_string(),
                });
            }
        };

        let mut temp_tex = 0;
        unsafe {
            gl::GenTextures(1, &mut temp_tex);
            gl::BindTexture(GL_TEXTURE_EXTERNAL_OES, temp_tex);
            gl::TexParameteri(
                GL_TEXTURE_EXTERNAL_OES,
                gl::TEXTURE_WRAP_S,
                gl::CLAMP_TO_EDGE as i32,
            );
            gl::TexParameteri(
                GL_TEXTURE_EXTERNAL_OES,
                gl::TEXTURE_WRAP_T,
                gl::CLAMP_TO_EDGE as i32,
            );
            gl::TexParameteri(
                GL_TEXTURE_EXTERNAL_OES,
                gl::TEXTURE_MIN_FILTER,
                gl::LINEAR as i32,
            );
            gl::TexParameteri(
                GL_TEXTURE_EXTERNAL_OES,
                gl::TEXTURE_MAG_FILTER,
                gl::LINEAR as i32,
            );

            while gl::GetError() != gl::NO_ERROR {}

            egl_image_target(GL_TEXTURE_EXTERNAL_OES, egl_image.as_ptr());
        }

        let gl_err = unsafe { gl::GetError() };
        if gl_err != gl::NO_ERROR {
            unsafe { gl::DeleteTextures(1, &temp_tex) };
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!(
                    "glEGLImageTargetTexture2DOES(EXTERNAL_OES) failed: GL error 0x{gl_err:x}"
                ),
            });
        }

        let mut fbo = 0;
        unsafe {
            gl::GenFramebuffers(1, &mut fbo);
            gl::BindFramebuffer(gl::FRAMEBUFFER, fbo);
            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                self.texture_id,
                0,
            );
        }

        let status = unsafe { gl::CheckFramebufferStatus(gl::FRAMEBUFFER) };
        if status != gl::FRAMEBUFFER_COMPLETE {
            unsafe {
                gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
                gl::DeleteFramebuffers(1, &fbo);
                gl::DeleteTextures(1, &temp_tex);
            }
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("FBO not complete: 0x{status:x}"),
            });
        }

        unsafe {
            gl::Viewport(0, 0, self.width as i32, self.height as i32);
            gl::UseProgram(self.shader_program);

            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(GL_TEXTURE_EXTERNAL_OES, temp_tex);
            gl::Uniform1i(
                gl::GetUniformLocation(self.shader_program, c"tex".as_ptr()),
                0,
            );

            gl::BindVertexArray(self.quad_vao);
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
            gl::BindVertexArray(0);

            gl::UseProgram(0);
        }

        // DEBUG: dump glReadPixels from FBO on first frame to isolate GL vs CUDA.
        static GL_DUMP_DONE: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !GL_DUMP_DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
            let w = self.width as usize;
            let h = self.height as usize;
            let mut pixels = vec![0u8; w * h * 4];
            unsafe {
                gl::Finish();
                gl::ReadPixels(
                    0,
                    0,
                    self.width as i32,
                    self.height as i32,
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    pixels.as_mut_ptr().cast(),
                );
            }
            let mut rgb = Vec::with_capacity(w * h * 3);
            for pixel in pixels.chunks_exact(4) {
                rgb.push(pixel[0]);
                rgb.push(pixel[1]);
                rgb.push(pixel[2]);
            }
            let header = format!("P6\n{w} {h}\n255\n");
            let path = "/tmp/stargaze_gl_blit.ppm";
            if let Ok(mut f) = std::fs::File::create(path) {
                use std::io::Write;
                let _ = f.write_all(header.as_bytes());
                let _ = f.write_all(&rgb);
                tracing::info!(path, width = w, height = h, "Dumped GL FBO readback to PPM");
            }
        }

        let gl_err = unsafe { gl::GetError() };

        unsafe {
            gl::BindTexture(GL_TEXTURE_EXTERNAL_OES, 0);
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::DeleteFramebuffers(1, &fbo);
            gl::DeleteTextures(1, &temp_tex);
        }

        if gl_err != gl::NO_ERROR {
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("Shader blit failed: GL error 0x{gl_err:x}"),
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

        unsafe { gl::Finish() };

        let mut resource = self.cuda_resource;
        let res = unsafe { cu::cuGraphicsMapResources(1, &mut resource, ptr::null_mut()) };
        if res != cu::CUresult::CUDA_SUCCESS {
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("cuGraphicsMapResources failed: {res:?}"),
            });
        }

        let mut cuda_array: cu::CUarray = ptr::null_mut();
        let res =
            unsafe { cu::cuGraphicsSubResourceGetMappedArray(&mut cuda_array, resource, 0, 0) };
        if res != cu::CUresult::CUDA_SUCCESS {
            unsafe { cu::cuGraphicsUnmapResources(1, &mut resource, ptr::null_mut()) };
            return Err(EncodeError::EncodeFrameError {
                frame: 0,
                reason: format!("cuGraphicsSubResourceGetMappedArray failed: {res:?}"),
            });
        }

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
            srcPitch: 0,

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

        let res = unsafe { cu::cuMemcpy2D_v2(&copy_desc) };

        let mut resource = self.cuda_resource;
        unsafe { cu::cuGraphicsUnmapResources(1, &mut resource, ptr::null_mut()) };

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
        if !self.cuda_resource.is_null() {
            unsafe {
                cudarc::driver::sys::cuGraphicsUnregisterResource(self.cuda_resource);
            }
        }
        if self.texture_id != 0 {
            unsafe {
                gl::DeleteTextures(1, &self.texture_id);
            }
        }
        if self.shader_program != 0 {
            unsafe {
                gl::DeleteProgram(self.shader_program);
            }
        }
        if self.quad_vao != 0 {
            unsafe {
                gl::DeleteVertexArrays(1, &self.quad_vao);
            }
        }
        if self.quad_vbo != 0 {
            unsafe {
                gl::DeleteBuffers(1, &self.quad_vbo);
            }
        }
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
        // GBM device must be destroyed after EGL terminate (it backs the display).
        if !self.gbm_device.is_null()
            && let Some(destroy_fn) = self.gbm_destroy_fn
        {
            unsafe { destroy_fn(self.gbm_device) };
        }
        // DRM fd must be closed after GBM device is destroyed.
        if self.drm_fd >= 0 {
            unsafe { libc::close(self.drm_fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies the NV12 kernel compiles under NVRTC.
    ///
    /// Requires an NVIDIA driver with libnvrtc available.
    /// Run manually with:
    /// ```bash
    /// cargo test --package stargaze-server -- --ignored nv12_kernel_compiles
    /// ```
    #[test]
    #[ignore = "requires NVIDIA driver with NVRTC"]
    fn nv12_kernel_compiles() {
        let ptx = cudarc::nvrtc::compile_ptx(NV12_KERNEL_SRC);
        assert!(ptx.is_ok(), "kernel must compile: {:?}", ptx.err());
    }
}
