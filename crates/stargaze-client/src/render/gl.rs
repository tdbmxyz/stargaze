//! OpenGL ES renderer with zero-copy dma-buf presentation.
//!
//! Used when the decoder is hardware-accelerated: VAAPI frames arrive as
//! DRM PRIME dma-bufs ([`DmaBufFrame`]) and are imported straight into GL
//! textures via `EGL_EXT_image_dma_buf_import`, so decoded video never
//! touches CPU memory. CPU frames (zero-copy fallback, software decode)
//! are uploaded into plane textures and drawn with the same shaders.
//!
//! Colorspace: the stream is BT.709 limited range end to end, and the
//! conversion happens in our fragment shaders (or, for the whole-frame
//! import strategy, via explicit EGL colorspace hints) — never left to a
//! driver's default guess.

use std::ffi::{CString, c_void};

use anyhow::anyhow;
use stargaze_core::decode::{DecodedFrame, FramePixels};
use tracing::info;

use crate::decode::{DmaBufFrame, VideoFrame};

use super::stats::rasterize_overlay;

// EGL_EXT_image_dma_buf_import / _modifiers constants.
const EGL_LINUX_DMA_BUF_EXT: khronos_egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: khronos_egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: khronos_egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: khronos_egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: khronos_egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE1_FD_EXT: khronos_egl::Attrib = 0x3275;
const EGL_DMA_BUF_PLANE1_OFFSET_EXT: khronos_egl::Attrib = 0x3276;
const EGL_DMA_BUF_PLANE1_PITCH_EXT: khronos_egl::Attrib = 0x3277;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: khronos_egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: khronos_egl::Attrib = 0x3444;
const EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT: khronos_egl::Attrib = 0x3445;
const EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT: khronos_egl::Attrib = 0x3446;
const EGL_YUV_COLOR_SPACE_HINT_EXT: khronos_egl::Attrib = 0x327B;
const EGL_SAMPLE_RANGE_HINT_EXT: khronos_egl::Attrib = 0x327C;
const EGL_ITU_REC709_EXT: khronos_egl::Attrib = 0x3282;
const EGL_YUV_NARROW_RANGE_EXT: khronos_egl::Attrib = 0x3287;

const GL_TEXTURE_EXTERNAL_OES: gl::types::GLenum = 0x8D65;

const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const DRM_FORMAT_R8: u32 = fourcc(*b"R8  ");
const DRM_FORMAT_NV12: u32 = fourcc(*b"NV12");

const fn fourcc(code: [u8; 4]) -> u32 {
    (code[0] as u32) | ((code[1] as u32) << 8) | ((code[2] as u32) << 16) | ((code[3] as u32) << 24)
}

/// Shared vertex shader: a unit quad stretched to `u_rect` (NDC corners),
/// with the texture's first uploaded row mapped to the top of the rect.
const VS_SRC: &str = "\
attribute vec2 a_pos;
uniform vec4 u_rect; // x0, y0 (bottom-left), x1, y1 (top-right) in NDC
varying vec2 v_tex;
void main() {
    v_tex = vec2(a_pos.x, 1.0 - a_pos.y);
    gl_Position = vec4(mix(u_rect.xy, u_rect.zw, a_pos), 0.0, 1.0);
}
";

/// BT.709 limited-range YUV → RGB, shared by the YUV fragment shaders.
const BT709_FN: &str = "\
vec3 bt709(float luma, float cb, float cr) {
    float y = 1.16438 * (luma - 0.0625);
    float u = cb - 0.5;
    float v = cr - 0.5;
    return vec3(
        y + 1.79274 * v,
        y - 0.21325 * u - 0.53291 * v,
        y + 2.11240 * u
    );
}
";

/// NV12 from two regular textures (CPU upload, or dma-buf planes bound
/// to `GL_TEXTURE_2D`).
fn fs_nv12() -> String {
    format!(
        "precision mediump float;
varying vec2 v_tex;
uniform sampler2D u_tex0;
uniform sampler2D u_tex1;
{BT709_FN}
void main() {{
    float luma = texture2D(u_tex0, v_tex).r;
    vec2 uv = texture2D(u_tex1, v_tex).rg;
    gl_FragColor = vec4(bt709(luma, uv.r, uv.g), 1.0);
}}
"
    )
}

/// NV12 from two dma-buf planes bound to `GL_TEXTURE_EXTERNAL_OES`
/// (NVIDIA can't bind dma-buf EGL images to `GL_TEXTURE_2D`).
fn fs_nv12_ext() -> String {
    format!(
        "#extension GL_OES_EGL_image_external : require
precision mediump float;
varying vec2 v_tex;
uniform samplerExternalOES u_tex0;
uniform samplerExternalOES u_tex1;
{BT709_FN}
void main() {{
    float luma = texture2D(u_tex0, v_tex).r;
    vec2 uv = texture2D(u_tex1, v_tex).rg;
    gl_FragColor = vec4(bt709(luma, uv.r, uv.g), 1.0);
}}
"
    )
}

/// A whole NV12 dma-buf imported as one external image: the driver does
/// the YUV→RGB conversion (steered BT.709/narrow by EGL hints).
const FS_RGB_EXT: &str = "\
#extension GL_OES_EGL_image_external : require
precision mediump float;
varying vec2 v_tex;
uniform samplerExternalOES u_tex0;
void main() {
    gl_FragColor = vec4(texture2D(u_tex0, v_tex).rgb, 1.0);
}
";

/// Planar I420 (software decode reaching the GL renderer).
fn fs_i420() -> String {
    format!(
        "precision mediump float;
varying vec2 v_tex;
uniform sampler2D u_tex0;
uniform sampler2D u_tex1;
uniform sampler2D u_tex2;
{BT709_FN}
void main() {{
    float luma = texture2D(u_tex0, v_tex).r;
    float cb = texture2D(u_tex1, v_tex).r;
    float cr = texture2D(u_tex2, v_tex).r;
    gl_FragColor = vec4(bt709(luma, cb, cr), 1.0);
}}
"
    )
}

/// Premade RGBA (the stats overlay), alpha-blended over the video.
const FS_OVERLAY: &str = "\
precision mediump float;
varying vec2 v_tex;
uniform sampler2D u_tex0;
void main() {
    gl_FragColor = texture2D(u_tex0, v_tex);
}
";

type EglImageTargetFn = unsafe extern "C" fn(target: gl::types::GLenum, image: *const c_void);

/// A linked shader program and its `u_rect` uniform location.
struct Program {
    id: gl::types::GLuint,
    u_rect: gl::types::GLint,
}

/// EGL entry points needed for dma-buf import.
struct EglState {
    egl: khronos_egl::DynamicInstance<khronos_egl::EGL1_5>,
    display: khronos_egl::Display,
    image_target: EglImageTargetFn,
    /// Whether `EGL_EXT_image_dma_buf_import_modifiers` is available.
    modifiers_supported: bool,
}

/// How dma-buf frames are imported. Probed on the first frame and then
/// sticky for the session.
enum DmabufMode {
    Undecided,
    /// Y and UV planes as separate R8/GR88 images, our shader converts.
    TwoPlane {
        target: gl::types::GLenum,
    },
    /// One NV12 image, driver-converted (BT.709/narrow via EGL hints).
    WholeFrame,
}

/// Layout the CPU plane textures are currently allocated for.
#[derive(PartialEq, Eq, Clone, Copy)]
enum CpuLayout {
    None,
    Nv12,
    I420,
}

/// One plane of a DRM PRIME frame, flattened from the descriptor.
struct PlaneInfo {
    fd: libc::c_int,
    offset: usize,
    pitch: usize,
    modifier: u64,
    /// DRM fourcc of this plane when imported on its own.
    format: u32,
}

pub(super) struct GlRenderer {
    // Dropped before the window (fields drop in declaration order).
    _gl_context: sdl2::video::GLContext,
    window: sdl2::video::Window,
    egl: Option<EglState>,
    prog_nv12: Program,
    prog_i420: Program,
    prog_nv12_ext: Option<Program>,
    prog_rgb_ext: Option<Program>,
    prog_overlay: Program,
    cpu_tex: [gl::types::GLuint; 3],
    cpu_layout: CpuLayout,
    cpu_dims: (u32, u32),
    overlay_tex: gl::types::GLuint,
    dmabuf_tex: [gl::types::GLuint; 2],
    dmabuf_mode: DmabufMode,
}

impl GlRenderer {
    /// Creates the window, a GLES context, the shader programs, and the
    /// EGL dma-buf import state.
    ///
    /// # Errors
    ///
    /// Returns an error if the GLES context or the mandatory shader
    /// programs can't be created. A missing EGL/dma-buf stack is *not* an
    /// error — dma-buf frames will then fail per-frame and the caller
    /// falls back to CPU frames.
    pub(super) fn new(
        video: &sdl2::VideoSubsystem,
        width: u32,
        height: u32,
        fullscreen: bool,
        hidden: bool,
    ) -> Result<Self, anyhow::Error> {
        let gl_attr = video.gl_attr();
        gl_attr.set_context_profile(sdl2::video::GLProfile::GLES);
        gl_attr.set_context_version(3, 0);

        let mut builder = video.window("Stargaze", width, height);
        builder.position_centered().opengl();
        if fullscreen {
            builder.fullscreen_desktop();
        }
        if hidden {
            builder.hidden();
        }
        let window = builder
            .build()
            .map_err(|e| anyhow!("GL window creation failed: {e}"))?;

        let gl_context = window
            .gl_create_context()
            .map_err(|e| anyhow!("GLES context creation failed: {e}"))?;
        window
            .gl_make_current(&gl_context)
            .map_err(|e| anyhow!("gl_make_current failed: {e}"))?;
        // Match the SDL canvas path: no vsync (it adds up to a frame of
        // input latency); pacing comes from the decoded-frame channel.
        let _ = video.gl_set_swap_interval(sdl2::video::SwapInterval::Immediate);

        gl::load_with(|symbol| match video.gl_get_proc_address(symbol) {
            f if (f as usize) == 0 => std::ptr::null(),
            f => f.cast::<c_void>(),
        });

        unsafe {
            let version = gl::GetString(gl::VERSION);
            let renderer = gl::GetString(gl::RENDERER);
            info!(
                version = ?cstr_or_unknown(version),
                renderer = ?cstr_or_unknown(renderer),
                "GL renderer initialized"
            );
        }

        let prog_nv12 = link_program(VS_SRC, &fs_nv12())?;
        let prog_i420 = link_program(VS_SRC, &fs_i420())?;
        let prog_overlay = link_program(VS_SRC, FS_OVERLAY)?;
        // External-image programs need GL_OES_EGL_image_external; keep
        // going without them (the TwoPlane/TEXTURE_2D strategy and CPU
        // uploads still work).
        let prog_nv12_ext = link_program(VS_SRC, &fs_nv12_ext())
            .map_err(|e| info!("external NV12 shader unavailable: {e}"))
            .ok();
        let prog_rgb_ext = link_program(VS_SRC, FS_RGB_EXT)
            .map_err(|e| info!("external RGB shader unavailable: {e}"))
            .ok();

        let mut quad_vbo = 0;
        let mut textures = [0u32; 6];
        unsafe {
            // Unit quad as a triangle strip; a_pos is location 0 in every
            // program (bound before linking).
            let quad: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
            gl::GenBuffers(1, &raw mut quad_vbo);
            gl::BindBuffer(gl::ARRAY_BUFFER, quad_vbo);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                std::mem::size_of_val(&quad).cast_signed(),
                quad.as_ptr().cast(),
                gl::STATIC_DRAW,
            );
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 0, std::ptr::null());
            gl::EnableVertexAttribArray(0);

            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::GenTextures(
                i32::try_from(textures.len()).expect("fixed-size array"),
                textures.as_mut_ptr(),
            );
        }

        let egl = init_egl();

        Ok(Self {
            _gl_context: gl_context,
            window,
            egl,
            prog_nv12,
            prog_i420,
            prog_nv12_ext,
            prog_rgb_ext,
            prog_overlay,
            cpu_tex: [textures[0], textures[1], textures[2]],
            cpu_layout: CpuLayout::None,
            cpu_dims: (0, 0),
            overlay_tex: textures[3],
            dmabuf_tex: [textures[4], textures[5]],
            dmabuf_mode: DmabufMode::Undecided,
        })
    }

    pub(super) fn window_mut(&mut self) -> &mut sdl2::video::Window {
        &mut self.window
    }

    /// Presents a black frame (shown until the first video frame arrives).
    pub(super) fn clear_black(&mut self) {
        let (dw, dh) = self.window.drawable_size();
        unsafe {
            gl::Viewport(0, 0, dw.cast_signed(), dh.cast_signed());
            gl::ClearColor(0.0, 0.0, 0.0, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
        self.window.gl_swap_window();
    }

    /// Draws `frame` (and the overlay text, when given) and swaps.
    ///
    /// # Errors
    ///
    /// Returns an error if a dma-buf frame can't be imported (the caller
    /// then switches the pipeline to CPU frames) or on GL failures.
    pub(super) fn present(
        &mut self,
        frame: &VideoFrame,
        overlay: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let (dw, dh) = self.window.drawable_size();
        unsafe {
            gl::Viewport(0, 0, dw.cast_signed(), dh.cast_signed());
            gl::ClearColor(0.0, 0.0, 0.0, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }

        match frame {
            VideoFrame::Cpu(f) => self.draw_cpu(f)?,
            VideoFrame::DmaBuf(f) => self.draw_dmabuf(f)?,
        }

        if let Some(text) = overlay {
            self.draw_overlay(text, dw, dh);
        }

        self.window.gl_swap_window();
        Ok(())
    }

    /// Uploads a CPU frame's planes and draws them full-window.
    fn draw_cpu(&mut self, frame: &DecodedFrame) -> Result<(), anyhow::Error> {
        let (w, h) = (frame.width, frame.height);
        let layout = match &frame.pixels {
            FramePixels::Nv12 { .. } => CpuLayout::Nv12,
            FramePixels::I420 { .. } => CpuLayout::I420,
        };
        if self.cpu_layout != layout || self.cpu_dims != (w, h) {
            self.alloc_cpu_textures(layout, w, h);
        }

        let (cw, ch) = (w.div_euclid(2), h.div_euclid(2));
        unsafe {
            match &frame.pixels {
                FramePixels::Nv12 { y, uv } => {
                    upload_plane(self.cpu_tex[0], w, h, gl::RED, y);
                    upload_plane(self.cpu_tex[1], cw, ch, gl::RG, uv);
                }
                FramePixels::I420 { y, u, v } => {
                    upload_plane(self.cpu_tex[0], w, h, gl::RED, y);
                    upload_plane(self.cpu_tex[1], cw, ch, gl::RED, u);
                    upload_plane(self.cpu_tex[2], cw, ch, gl::RED, v);
                }
            }

            let (prog, planes) = match layout {
                CpuLayout::Nv12 => (&self.prog_nv12, 2),
                _ => (&self.prog_i420, 3),
            };
            for (unit, tex) in (0u32..).zip(self.cpu_tex.iter()).take(planes) {
                gl::ActiveTexture(gl::TEXTURE0 + unit);
                gl::BindTexture(gl::TEXTURE_2D, *tex);
            }
            draw_quad(prog, [-1.0, -1.0, 1.0, 1.0]);
        }
        check_gl_error("CPU frame draw")
    }

    /// (Re)allocates the CPU plane textures for `layout` at `w`×`h`.
    fn alloc_cpu_textures(&mut self, layout: CpuLayout, w: u32, h: u32) {
        let (cw, ch) = (w.div_euclid(2), h.div_euclid(2));
        let plan: &[(u32, u32, gl::types::GLenum)] = match layout {
            CpuLayout::Nv12 => &[(w, h, gl::R8), (cw, ch, gl::RG8)],
            _ => &[(w, h, gl::R8), (cw, ch, gl::R8), (cw, ch, gl::R8)],
        };
        unsafe {
            for (i, (pw, ph, internal)) in plan.iter().enumerate() {
                gl::BindTexture(gl::TEXTURE_2D, self.cpu_tex[i]);
                set_sampling_params(gl::TEXTURE_2D);
                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    (*internal).cast_signed(),
                    pw.cast_signed(),
                    ph.cast_signed(),
                    0,
                    if *internal == gl::RG8 {
                        gl::RG
                    } else {
                        gl::RED
                    },
                    gl::UNSIGNED_BYTE,
                    std::ptr::null(),
                );
            }
        }
        self.cpu_layout = layout;
        self.cpu_dims = (w, h);
        info!(
            w,
            h,
            nv12 = (layout == CpuLayout::Nv12),
            "CPU plane textures allocated"
        );
    }

    /// Imports and draws a dma-buf frame using the session's import
    /// strategy, probing the strategies on the first frame.
    fn draw_dmabuf(&mut self, frame: &DmaBufFrame) -> Result<(), anyhow::Error> {
        let planes = nv12_planes(frame.descriptor()).map_err(|e| anyhow!(e))?;

        match self.dmabuf_mode {
            DmabufMode::TwoPlane { target } => self.draw_two_plane(frame, &planes, target),
            DmabufMode::WholeFrame => self.draw_whole_frame(frame, &planes),
            DmabufMode::Undecided => {
                let attempts: [(&str, DmabufMode); 3] = [
                    (
                        "two-plane GL_TEXTURE_2D",
                        DmabufMode::TwoPlane {
                            target: gl::TEXTURE_2D,
                        },
                    ),
                    (
                        "two-plane GL_TEXTURE_EXTERNAL_OES",
                        DmabufMode::TwoPlane {
                            target: GL_TEXTURE_EXTERNAL_OES,
                        },
                    ),
                    ("whole-frame external", DmabufMode::WholeFrame),
                ];
                let mut last_err = None;
                for (name, mode) in attempts {
                    let result = match mode {
                        DmabufMode::TwoPlane { target } => {
                            self.draw_two_plane(frame, &planes, target)
                        }
                        _ => self.draw_whole_frame(frame, &planes),
                    };
                    match result {
                        Ok(()) => {
                            info!("dma-buf import strategy: {name}");
                            self.dmabuf_mode = mode;
                            return Ok(());
                        }
                        Err(e) => {
                            info!("dma-buf import strategy {name} failed: {e}");
                            last_err = Some(e);
                        }
                    }
                }
                Err(last_err.unwrap_or_else(|| anyhow!("no dma-buf import strategy available")))
            }
        }
    }

    /// Y and UV planes as separate EGL images bound to `target`.
    fn draw_two_plane(
        &self,
        frame: &DmaBufFrame,
        planes: &(PlaneInfo, PlaneInfo),
        target: gl::types::GLenum,
    ) -> Result<(), anyhow::Error> {
        let prog = if target == gl::TEXTURE_2D {
            &self.prog_nv12
        } else {
            self.prog_nv12_ext
                .as_ref()
                .ok_or_else(|| anyhow!("external NV12 shader unavailable"))?
        };
        let egl = self
            .egl
            .as_ref()
            .ok_or_else(|| anyhow!("EGL unavailable"))?;

        let (w, h) = (frame.width, frame.height);
        let (cw, ch) = (w.div_euclid(2), h.div_euclid(2));
        let image_y = create_dmabuf_image(egl, w, h, planes.0.format, &planes.0, None)?;
        let image_uv = match create_dmabuf_image(egl, cw, ch, planes.1.format, &planes.1, None) {
            Ok(img) => img,
            Err(e) => {
                let _ = egl.egl.destroy_image(egl.display, image_y);
                return Err(e);
            }
        };

        let result = (|| {
            unsafe {
                bind_image(egl, target, self.dmabuf_tex[0], image_y)?;
                bind_image(egl, target, self.dmabuf_tex[1], image_uv)?;
                gl::ActiveTexture(gl::TEXTURE0);
                gl::BindTexture(target, self.dmabuf_tex[0]);
                gl::ActiveTexture(gl::TEXTURE1);
                gl::BindTexture(target, self.dmabuf_tex[1]);
                draw_quad(prog, [-1.0, -1.0, 1.0, 1.0]);
            }
            check_gl_error("two-plane dma-buf draw")
        })();

        let _ = egl.egl.destroy_image(egl.display, image_y);
        let _ = egl.egl.destroy_image(egl.display, image_uv);
        result
    }

    /// The whole NV12 buffer as one external EGL image; the driver
    /// converts, steered to BT.709 narrow range by the import hints.
    fn draw_whole_frame(
        &self,
        frame: &DmaBufFrame,
        planes: &(PlaneInfo, PlaneInfo),
    ) -> Result<(), anyhow::Error> {
        let prog = self
            .prog_rgb_ext
            .as_ref()
            .ok_or_else(|| anyhow!("external RGB shader unavailable"))?;
        let egl = self
            .egl
            .as_ref()
            .ok_or_else(|| anyhow!("EGL unavailable"))?;

        let image = create_dmabuf_image(
            egl,
            frame.width,
            frame.height,
            DRM_FORMAT_NV12,
            &planes.0,
            Some(&planes.1),
        )?;

        let result = (|| {
            unsafe {
                bind_image(egl, GL_TEXTURE_EXTERNAL_OES, self.dmabuf_tex[0], image)?;
                gl::ActiveTexture(gl::TEXTURE0);
                gl::BindTexture(GL_TEXTURE_EXTERNAL_OES, self.dmabuf_tex[0]);
                draw_quad(prog, [-1.0, -1.0, 1.0, 1.0]);
            }
            check_gl_error("whole-frame dma-buf draw")
        })();

        let _ = egl.egl.destroy_image(egl.display, image);
        result
    }

    /// Rasterizes the overlay text and blends it over the top-left corner.
    fn draw_overlay(&mut self, text: &str, dw: u32, dh: u32) {
        if dw == 0 || dh == 0 {
            return;
        }
        let (pixels, ow, oh) = rasterize_overlay(text);
        #[allow(clippy::cast_precision_loss)]
        let (x1, y0) = (
            -1.0 + 2.0 * (ow as f32 / dw as f32),
            1.0 - 2.0 * (oh as f32 / dh as f32),
        );
        unsafe {
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.overlay_tex);
            set_sampling_params(gl::TEXTURE_2D);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::RGBA8.cast_signed(),
                ow.cast_signed(),
                oh.cast_signed(),
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                pixels.as_ptr().cast(),
            );
            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
            draw_quad(&self.prog_overlay, [-1.0, y0.max(-1.0), x1.min(1.0), 1.0]);
            gl::Disable(gl::BLEND);
        }
    }
}

/// Loads EGL and resolves what dma-buf import needs. `None` (with a log)
/// when any piece is missing.
fn init_egl() -> Option<EglState> {
    // SAFETY: libEGL stays loaded for the process lifetime.
    let egl = unsafe {
        khronos_egl::DynamicInstance::<khronos_egl::EGL1_5>::load_required_from_filename(
            "libEGL.so.1",
        )
    }
    .map_err(|e| info!("libEGL unavailable, no zero-copy rendering: {e}"))
    .ok()?;

    // SDL's GLES context on Wayland is an EGL context, so the display is
    // current on this thread.
    let Some(display) = egl.get_current_display() else {
        info!("No current EGL display (GLX context?), no zero-copy rendering");
        return None;
    };

    let extensions = egl
        .query_string(Some(display), khronos_egl::EXTENSIONS)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !extensions.contains("EGL_EXT_image_dma_buf_import") {
        info!("EGL_EXT_image_dma_buf_import missing, no zero-copy rendering");
        return None;
    }
    let modifiers_supported = extensions.contains("EGL_EXT_image_dma_buf_import_modifiers");

    let Some(image_target_ptr) = egl.get_proc_address("glEGLImageTargetTexture2DOES") else {
        info!("glEGLImageTargetTexture2DOES missing, no zero-copy rendering");
        return None;
    };
    let image_target =
        unsafe { std::mem::transmute::<extern "system" fn(), EglImageTargetFn>(image_target_ptr) };

    info!(modifiers_supported, "EGL dma-buf import available");
    Some(EglState {
        egl,
        display,
        image_target,
        modifiers_supported,
    })
}

/// Flattens an NV12 DRM PRIME descriptor into (Y, UV) plane infos.
///
/// VAAPI exports either two single-plane layers (R8 + GR88) or one
/// two-plane NV12 layer; both are handled.
fn nv12_planes(
    desc: &ffmpeg_sys_next::AVDRMFrameDescriptor,
) -> Result<(PlaneInfo, PlaneInfo), String> {
    const DRM_FORMAT_GR88: u32 = fourcc(*b"GR88");

    let plane_info = |layer: &ffmpeg_sys_next::AVDRMLayerDescriptor,
                      plane_idx: usize,
                      format: u32|
     -> Result<PlaneInfo, String> {
        let plane = &layer.planes[plane_idx];
        let obj_idx = usize::try_from(plane.object_index)
            .ok()
            .filter(|i| *i < desc.nb_objects.unsigned_abs() as usize)
            .ok_or_else(|| format!("bad object index {}", plane.object_index))?;
        let object = &desc.objects[obj_idx];
        Ok(PlaneInfo {
            fd: object.fd,
            offset: usize::try_from(plane.offset).map_err(|_| "negative plane offset")?,
            pitch: usize::try_from(plane.pitch).map_err(|_| "negative plane pitch")?,
            modifier: object.format_modifier,
            format,
        })
    };

    match (desc.nb_layers, desc.layers[0].nb_planes) {
        (2, _) => Ok((
            plane_info(&desc.layers[0], 0, desc.layers[0].format)?,
            plane_info(&desc.layers[1], 0, desc.layers[1].format)?,
        )),
        (1, 2) => Ok((
            plane_info(&desc.layers[0], 0, DRM_FORMAT_R8)?,
            plane_info(&desc.layers[0], 1, DRM_FORMAT_GR88)?,
        )),
        (layers, planes) => Err(format!(
            "unsupported DRM frame layout ({layers} layers, {planes} planes)"
        )),
    }
}

/// Creates an EGL image from one dma-buf plane (or, when `plane1` is
/// given, a two-plane image — used for whole-NV12 import, with BT.709
/// narrow-range hints).
fn create_dmabuf_image(
    egl: &EglState,
    width: u32,
    height: u32,
    drm_fourcc: u32,
    plane0: &PlaneInfo,
    plane1: Option<&PlaneInfo>,
) -> Result<khronos_egl::Image, anyhow::Error> {
    type A = khronos_egl::Attrib;

    let mut attribs: Vec<A> = vec![
        EGL_LINUX_DRM_FOURCC_EXT,
        drm_fourcc as A,
        khronos_egl::WIDTH as A,
        width as A,
        khronos_egl::HEIGHT as A,
        height as A,
        EGL_DMA_BUF_PLANE0_FD_EXT,
        plane0.fd.cast_unsigned() as A,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT,
        plane0.offset,
        EGL_DMA_BUF_PLANE0_PITCH_EXT,
        plane0.pitch,
    ];
    // DRM_FORMAT_MOD_INVALID means "no explicit modifier" — passing it
    // makes eglCreateImage fail with EGL_BAD_PARAMETER on NVIDIA.
    let with_modifiers = egl.modifiers_supported && plane0.modifier != DRM_FORMAT_MOD_INVALID;
    if with_modifiers {
        attribs.extend_from_slice(&[
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (plane0.modifier & 0xFFFF_FFFF) as A,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            (plane0.modifier >> 32) as A,
        ]);
    }
    if let Some(p1) = plane1 {
        attribs.extend_from_slice(&[
            EGL_DMA_BUF_PLANE1_FD_EXT,
            p1.fd.cast_unsigned() as A,
            EGL_DMA_BUF_PLANE1_OFFSET_EXT,
            p1.offset,
            EGL_DMA_BUF_PLANE1_PITCH_EXT,
            p1.pitch,
        ]);
        if with_modifiers && p1.modifier != DRM_FORMAT_MOD_INVALID {
            attribs.extend_from_slice(&[
                EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT,
                (p1.modifier & 0xFFFF_FFFF) as A,
                EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT,
                (p1.modifier >> 32) as A,
            ]);
        }
        // Whole-YUV import: pin the conversion the driver will apply.
        attribs.extend_from_slice(&[
            EGL_YUV_COLOR_SPACE_HINT_EXT,
            EGL_ITU_REC709_EXT,
            EGL_SAMPLE_RANGE_HINT_EXT,
            EGL_YUV_NARROW_RANGE_EXT,
        ]);
    }
    attribs.push(khronos_egl::ATTRIB_NONE);

    egl.egl
        .create_image(
            egl.display,
            unsafe { khronos_egl::Context::from_ptr(khronos_egl::NO_CONTEXT) },
            EGL_LINUX_DMA_BUF_EXT,
            unsafe { khronos_egl::ClientBuffer::from_ptr(std::ptr::null_mut()) },
            &attribs,
        )
        .map_err(|e| anyhow!("eglCreateImage(dma-buf) failed: {e}"))
}

/// Binds an EGL image as the storage of `tex` on `target`.
///
/// # Safety
///
/// Requires a current GL context; `image` must be valid.
unsafe fn bind_image(
    egl: &EglState,
    target: gl::types::GLenum,
    tex: gl::types::GLuint,
    image: khronos_egl::Image,
) -> Result<(), anyhow::Error> {
    unsafe {
        gl::BindTexture(target, tex);
        set_sampling_params(target);
        while gl::GetError() != gl::NO_ERROR {}
        (egl.image_target)(target, image.as_ptr());
        let err = gl::GetError();
        if err != gl::NO_ERROR {
            return Err(anyhow!(
                "glEGLImageTargetTexture2DOES(0x{target:x}) failed: GL error 0x{err:x}"
            ));
        }
    }
    Ok(())
}

/// Linear min/mag filtering, clamped edges (every texture here).
unsafe fn set_sampling_params(target: gl::types::GLenum) {
    unsafe {
        gl::TexParameteri(target, gl::TEXTURE_MIN_FILTER, gl::LINEAR.cast_signed());
        gl::TexParameteri(target, gl::TEXTURE_MAG_FILTER, gl::LINEAR.cast_signed());
        gl::TexParameteri(target, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE.cast_signed());
        gl::TexParameteri(target, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE.cast_signed());
    }
}

/// Re-uploads the full contents of a tightly packed plane texture.
unsafe fn upload_plane(
    tex: gl::types::GLuint,
    w: u32,
    h: u32,
    format: gl::types::GLenum,
    data: &[u8],
) {
    unsafe {
        gl::BindTexture(gl::TEXTURE_2D, tex);
        gl::TexSubImage2D(
            gl::TEXTURE_2D,
            0,
            0,
            0,
            w.cast_signed(),
            h.cast_signed(),
            format,
            gl::UNSIGNED_BYTE,
            data.as_ptr().cast(),
        );
    }
}

/// Draws the unit quad with `prog`, stretched to `rect` (NDC corners).
unsafe fn draw_quad(prog: &Program, rect: [f32; 4]) {
    unsafe {
        gl::UseProgram(prog.id);
        gl::Uniform4f(prog.u_rect, rect[0], rect[1], rect[2], rect[3]);
        gl::DrawArrays(gl::TRIANGLE_STRIP, 0, 4);
    }
}

fn check_gl_error(context: &str) -> Result<(), anyhow::Error> {
    let err = unsafe { gl::GetError() };
    if err == gl::NO_ERROR {
        Ok(())
    } else {
        Err(anyhow!("{context}: GL error 0x{err:x}"))
    }
}

fn cstr_or_unknown(ptr: *const u8) -> String {
    if ptr.is_null() {
        "unknown".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(ptr.cast()) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Compiles and links a program, binding `a_pos` to attribute 0 and the
/// `u_tex{0,1,2}` samplers to texture units 0..2.
fn link_program(vs_src: &str, fs_src: &str) -> Result<Program, anyhow::Error> {
    unsafe {
        let compile =
            |src: &str, kind: gl::types::GLenum| -> Result<gl::types::GLuint, anyhow::Error> {
                let shader = gl::CreateShader(kind);
                let c_src = CString::new(src).map_err(|e| anyhow!("shader source: {e}"))?;
                gl::ShaderSource(shader, 1, &c_src.as_ptr(), std::ptr::null());
                gl::CompileShader(shader);
                let mut status = 0;
                gl::GetShaderiv(shader, gl::COMPILE_STATUS, &raw mut status);
                if status == 0 {
                    let log = shader_log(shader, true);
                    gl::DeleteShader(shader);
                    return Err(anyhow!("shader compile failed: {log}"));
                }
                Ok(shader)
            };

        let vs = compile(vs_src, gl::VERTEX_SHADER)?;
        let fs = match compile(fs_src, gl::FRAGMENT_SHADER) {
            Ok(fs) => fs,
            Err(e) => {
                gl::DeleteShader(vs);
                return Err(e);
            }
        };

        let id = gl::CreateProgram();
        gl::AttachShader(id, vs);
        gl::AttachShader(id, fs);
        let a_pos = CString::new("a_pos").expect("static name");
        gl::BindAttribLocation(id, 0, a_pos.as_ptr());
        gl::LinkProgram(id);
        gl::DeleteShader(vs);
        gl::DeleteShader(fs);

        let mut status = 0;
        gl::GetProgramiv(id, gl::LINK_STATUS, &raw mut status);
        if status == 0 {
            let log = shader_log(id, false);
            gl::DeleteProgram(id);
            return Err(anyhow!("program link failed: {log}"));
        }

        gl::UseProgram(id);
        for (unit, name) in (0i32..).zip(["u_tex0", "u_tex1", "u_tex2"]) {
            let c_name = CString::new(name).expect("static name");
            let loc = gl::GetUniformLocation(id, c_name.as_ptr());
            if loc >= 0 {
                gl::Uniform1i(loc, unit);
            }
        }
        let u_rect = CString::new("u_rect").expect("static name");
        let u_rect = gl::GetUniformLocation(id, u_rect.as_ptr());

        Ok(Program { id, u_rect })
    }
}

fn shader_log(object: gl::types::GLuint, is_shader: bool) -> String {
    let mut buf = vec![0u8; 1024];
    let mut len = 0;
    unsafe {
        if is_shader {
            gl::GetShaderInfoLog(object, 1024, &raw mut len, buf.as_mut_ptr().cast());
        } else {
            gl::GetProgramInfoLog(object, 1024, &raw mut len, buf.as_mut_ptr().cast());
        }
    }
    buf.truncate(usize::try_from(len).unwrap_or(0));
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stargaze_core::decode::FrameStats;

    /// Builds a CPU frame whose top half is pure red and bottom half pure
    /// green (BT.709 limited-range encodings from the server's converter).
    fn half_frame(w: u32, h: u32, nv12: bool) -> DecodedFrame {
        let red = (63u8, 102u8, 240u8);
        let green = (172u8, 41u8, 26u8);
        let (w_, h_) = (w as usize, h as usize);

        let mut y_plane = vec![0u8; w_ * h_];
        for (row, chunk) in y_plane.chunks_exact_mut(w_).enumerate() {
            chunk.fill(if row < h_ / 2 { red.0 } else { green.0 });
        }
        let (cw, ch) = (w_ / 2, h_ / 2);
        let pixels = if nv12 {
            let mut uv = vec![0u8; cw * ch * 2];
            for (row, chunk) in uv.chunks_exact_mut(cw * 2).enumerate() {
                let (_, u, v) = if row < ch / 2 { red } else { green };
                for pair in chunk.chunks_exact_mut(2) {
                    pair[0] = u;
                    pair[1] = v;
                }
            }
            FramePixels::Nv12 { y: y_plane, uv }
        } else {
            let mut u_plane = vec![0u8; cw * ch];
            let mut v_plane = vec![0u8; cw * ch];
            for row in 0..ch {
                let (_, u, v) = if row < ch / 2 { red } else { green };
                u_plane[row * cw..(row + 1) * cw].fill(u);
                v_plane[row * cw..(row + 1) * cw].fill(v);
            }
            FramePixels::I420 {
                y: y_plane,
                u: u_plane,
                v: v_plane,
            }
        };

        DecodedFrame {
            pixels,
            width: w,
            height: h,
            pts: 0,
            stats: FrameStats::default(),
        }
    }

    /// Renders known BT.709 NV12 and I420 frames through the GL renderer
    /// and reads the framebuffer back: catches colorspace-matrix and
    /// vertical-flip regressions in the shaders.
    ///
    /// Run with a live compositor (`--test-threads=1` — SDL can only be
    /// initialized by one test at a time per process):
    /// ```bash
    /// WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/1000 \
    ///   nix develop -c cargo test -p stargaze-client -- --ignored --test-threads=1 gl_renderer
    /// ```
    #[test]
    #[ignore = "requires a display (Wayland/X11) and a GPU"]
    fn gl_renderer_draws_bt709_colors_upright() {
        let (w, h) = (64u32, 64u32);
        let sdl = sdl2::init().expect("SDL init");
        let video = sdl.video().expect("SDL video");
        let mut renderer = GlRenderer::new(&video, w, h, false, true).expect("GL renderer");

        let mut check = |nv12: bool| {
            let frame = VideoFrame::Cpu(half_frame(w, h, nv12));
            renderer.present(&frame, None).expect("present");

            // Read the back buffer (present swapped, so draw again first).
            renderer.present(&frame, None).expect("present");
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            unsafe {
                gl::ReadPixels(
                    0,
                    0,
                    w.cast_signed(),
                    h.cast_signed(),
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    pixels.as_mut_ptr().cast(),
                );
            }
            // glReadPixels row 0 is the *bottom* of the window.
            let sample = |x: u32, y_from_top: u32| {
                let row = h - 1 - y_from_top;
                let off = ((row * w + x) * 4) as usize;
                (pixels[off], pixels[off + 1], pixels[off + 2])
            };
            let top = sample(w / 2, 8);
            let bottom = sample(w / 2, h - 8);
            eprintln!(
                "{}: top {top:?} (want ~(255,0,0)), bottom {bottom:?} (want ~(0,255,0))",
                if nv12 { "NV12" } else { "I420" }
            );
            let close = |a: (u8, u8, u8), b: (u8, u8, u8)| {
                (i16::from(a.0) - i16::from(b.0)).abs() <= 12
                    && (i16::from(a.1) - i16::from(b.1)).abs() <= 12
                    && (i16::from(a.2) - i16::from(b.2)).abs() <= 12
            };
            assert!(close(top, (255, 0, 0)), "top half wrong: {top:?}");
            assert!(close(bottom, (0, 255, 0)), "bottom half wrong: {bottom:?}");
        };

        check(true);
        check(false);
    }
}
