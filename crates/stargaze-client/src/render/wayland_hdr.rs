//! Color-management-v1 on SDL's native Wayland surface, without moving input
//! or GL off the main thread. SDL retains ownership of the native connection.

use std::ffi::{CStr, c_char, c_void};
use std::ptr::NonNull;

unsafe extern "C" {
    fn sg_hdr_probe() -> libc::c_int;
    fn sg_hdr_attach(
        display: *mut c_void,
        surface: *mut c_void,
        error: *mut c_char,
        error_size: usize,
    ) -> *mut c_void;
    fn sg_hdr_destroy(surface: *mut c_void);
    fn sg_hdr_update(
        surface: *mut c_void,
        metadata: *const Metadata,
        error: *mut c_char,
        error_size: usize,
    ) -> libc::c_int;
}

/// Probe the compositor on an independent connection. Call from a blocking
/// task; actual EGL/VAAPI creation remains authoritative and may still fail.
pub(crate) fn supported() -> bool {
    unsafe { sg_hdr_probe() != 0 }
}

#[repr(C)]
#[derive(Default)]
struct Metadata {
    primaries: [u32; 6],
    white: [u32; 2],
    min_luminance: u32,
    max_luminance: u32,
    max_cll: u32,
    max_fall: u32,
    flags: u32,
}

pub(super) struct HdrSurface {
    pointer: NonNull<c_void>,
    metadata: stargaze_core::hdr::HdrMetadata,
}

impl HdrSurface {
    pub(super) fn update_metadata(
        &mut self,
        metadata: stargaze_core::hdr::HdrMetadata,
    ) -> anyhow::Result<()> {
        if metadata == self.metadata {
            return Ok(());
        }
        metadata.validate().map_err(anyhow::Error::msg)?;
        let mut m = Metadata::default();
        if let Some(mastering) = metadata.mastering {
            m.primaries = std::array::from_fn(|i| u32::from(mastering.primaries[i / 2][i % 2]));
            m.white = mastering.white_point.map(u32::from);
            m.min_luminance = mastering.min_luminance;
            m.max_luminance = mastering.max_luminance;
            m.flags |= 1;
        }
        if let Some(light) = metadata.content_light {
            m.max_cll = u32::from(light.max_cll);
            m.max_fall = u32::from(light.max_fall);
            m.flags |= 2;
        }
        let mut error = [0 as c_char; 256];
        let ok = unsafe {
            sg_hdr_update(
                self.pointer.as_ptr(),
                &raw const m,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            ok != 0,
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        self.metadata = metadata;
        tracing::info!(
            ?metadata,
            "HDR mastering/content-light metadata applied to Wayland surface"
        );
        Ok(())
    }
    pub(super) fn attach(
        video: &sdl2::VideoSubsystem,
        window: &sdl2::video::Window,
    ) -> anyhow::Result<Self> {
        let gl_attr = video.gl_attr();
        let bits = (
            gl_attr.red_size(),
            gl_attr.green_size(),
            gl_attr.blue_size(),
        );
        anyhow::ensure!(
            bits.0 >= 10 && bits.1 >= 10 && bits.2 >= 10,
            "HDR requires a real 10-bit EGL back buffer, got {bits:?}"
        );
        tracing::info!(?bits, "Verified HDR EGL back buffer precision");
        let mut wm: sdl2::sys::SDL_SysWMinfo = unsafe { std::mem::zeroed() };
        unsafe {
            sdl2::sys::SDL_GetVersion(&raw mut wm.version);
        }
        anyhow::ensure!(
            unsafe { sdl2::sys::SDL_GetWindowWMInfo(window.raw(), &raw mut wm) }
                == sdl2::sys::SDL_bool::SDL_TRUE
                && wm.subsystem == sdl2::sys::SDL_SYSWM_TYPE::SDL_SYSWM_WAYLAND,
            "HDR requires an SDL Wayland window"
        );
        let wl = unsafe { wm.info.wl };
        let mut error = [0 as c_char; 256];
        let ptr = unsafe {
            sg_hdr_attach(
                wl.display.cast(),
                wl.surface.cast(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        let ptr = NonNull::new(ptr).ok_or_else(|| {
            anyhow::anyhow!(
                "{}",
                unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
            )
        })?;
        tracing::info!(
            primaries = "BT.2020",
            transfer = "ST2084/PQ",
            intent = "perceptual",
            "Wayland HDR image description acknowledged and attached to presentation surface"
        );
        Ok(Self {
            pointer: ptr,
            metadata: stargaze_core::hdr::HdrMetadata::default(),
        })
    }
}

impl Drop for HdrSurface {
    fn drop(&mut self) {
        unsafe {
            sg_hdr_destroy(self.pointer.as_ptr());
        }
    }
}
