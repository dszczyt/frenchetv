//! Embedded mpv player using `libmpv2`.
//!
//! Two render backends:
//! - `GlRenderer` (Linux/EGL): offscreen OpenGL FBO, GPU-side decode where possible.
//! - `SoftwareRenderer`: mpv software pixel-buffer, works everywhere.
//!
//! `ActiveRenderer::probe()` tries GL first at construction; falls back to software.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ── SoftwareRenderer ──────────────────────────────────────────────────────────

/// Raw software render context.
///
/// The high-level `libmpv2` crate exposes only the OpenGL backend; the SW
/// API (`MPV_RENDER_API_TYPE_SW`) is only available via the sys-level FFI.
/// This wrapper manages the raw context lifetime manually.
pub struct SoftwareRenderer {
    /// Raw mpv render context (SW backend).
    ctx: *mut libmpv2_sys::mpv_render_context,
    /// Owned texture handle kept alive so the GPU texture is not freed.
    texture: Option<egui::TextureHandle>,
    /// Set to true by mpv's update callback — signals a new frame is ready.
    needs_update: Arc<AtomicBool>,
    /// Raw pointer to the boxed `CallbackState` — freed in Drop.
    /// Must NOT be renamed with `_` prefix; it IS used in Drop.
    callback_state_ptr: *mut std::ffi::c_void,
}

// SAFETY: the raw pointer is only accessed from the owning thread.
unsafe impl Send for SoftwareRenderer {}

struct CallbackState {
    flag: Arc<AtomicBool>,
    egui_ctx: egui::Context,
}

unsafe extern "C" fn update_callback(cb_ctx: *mut std::ffi::c_void) {
    if cb_ctx.is_null() {
        return;
    }
    let state = &*(cb_ctx as *const CallbackState);
    state.flag.store(true, Ordering::Release);
    // Rate-limit egui wakeups to ~25 fps. The callback fires for every mpv
    // internal event; uncapped it keeps the event loop spinning at 100+ Hz.
    state.egui_ctx.request_repaint_after(std::time::Duration::from_millis(40));
}

impl SoftwareRenderer {
    /// Creates a software render context tied to `mpv`.
    ///
    /// # Safety
    /// `mpv.ctx` must remain valid for the lifetime of this renderer.
    pub fn new(mpv: &libmpv2::Mpv, egui_ctx: egui::Context) -> Result<Self, libmpv2::Error> {
        use std::ptr;

        // Build the render param array for SW init:
        //   [ApiType("sw"), {type=0, data=null}]
        let api_type_str = libmpv2_sys::MPV_RENDER_API_TYPE_SW;
        let params: [libmpv2_sys::mpv_render_param; 2] = [
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: api_type_str.as_ptr() as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param { type_: 0, data: ptr::null_mut() },
        ];

        let mut raw_ctx: *mut libmpv2_sys::mpv_render_context = ptr::null_mut();
        let err = unsafe {
            libmpv2_sys::mpv_render_context_create(
                &mut raw_ctx,
                mpv.ctx.as_ptr(),
                params.as_ptr() as *mut libmpv2_sys::mpv_render_param,
            )
        };
        if err != 0 {
            return Err(libmpv2::Error::Raw(err));
        }

        let needs_update = Arc::new(AtomicBool::new(false));
        let callback_box = Box::new(CallbackState {
            flag: Arc::clone(&needs_update),
            egui_ctx,
        });
        let callback_ptr = Box::into_raw(callback_box) as *mut std::ffi::c_void;

        unsafe {
            libmpv2_sys::mpv_render_context_set_update_callback(
                raw_ctx,
                Some(update_callback),
                callback_ptr,
            );
        }

        Ok(Self {
            ctx: raw_ctx,
            texture: None,
            needs_update,
            callback_state_ptr: callback_ptr,
        })
    }

    /// Renders a new frame if mpv has one ready; returns cached texture otherwise.
    pub fn poll_frame(
        &mut self,
        ctx: &egui::Context,
        width: u32,
        height: u32,
    ) -> Option<egui::load::SizedTexture> {
        if width == 0 || height == 0 {
            return None;
        }

        // Fast pre-check: callback has not fired since last poll.
        if !self.needs_update.swap(false, Ordering::AcqRel) {
            return self.texture.as_ref().map(|t| {
                egui::load::SizedTexture::new(t.id(), egui::vec2(width as f32, height as f32))
            });
        }

        // The callback fires for *every* mpv internal event, not only new frames.
        // Check the authoritative flag before doing any pixel work.
        let update_flags = unsafe { libmpv2_sys::mpv_render_context_update(self.ctx) };
        if update_flags & (libmpv2_sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as u64) == 0 {
            return self.texture.as_ref().map(|t| {
                egui::load::SizedTexture::new(t.id(), egui::vec2(width as f32, height as f32))
            });
        }

        // Cap render resolution — software scaling at high res is expensive.
        // 1280×720 = 3.7 MB/frame vs 8 MB at 1080p; IPTV quality is unaffected.
        const MAX_W: u32 = 1280;
        const MAX_H: u32 = 720;
        let rw = width.min(MAX_W);
        let rh = height.min(MAX_H);

        let stride = (rw * 4) as usize;
        let mut pixels = vec![0u8; stride * rh as usize];
        let mut sw_size = [rw as i32, rh as i32];
        // Use rgba directly — avoids the bgr0→rgba swap loop (saves 3.7 MB/frame of CPU work).
        let format_str = b"rgba\0";
        let mut sw_stride = stride;

        let render_params: [libmpv2_sys::mpv_render_param; 5] = [
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_SIZE,
                data: sw_size.as_mut_ptr() as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_FORMAT,
                data: format_str.as_ptr() as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_STRIDE,
                data: &mut sw_stride as *mut usize as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_SW_POINTER,
                data: pixels.as_mut_ptr() as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param { type_: 0, data: std::ptr::null_mut() },
        ];

        let err = unsafe {
            libmpv2_sys::mpv_render_context_render(
                self.ctx,
                render_params.as_ptr() as *mut libmpv2_sys::mpv_render_param,
            )
        };
        if err != 0 {
            tracing::warn!("software render failed: error code {}", err);
            return None;
        }

        // rgba format requested — no conversion needed.
        let image = egui::ColorImage::from_rgba_unmultiplied([rw as usize, rh as usize], &pixels);
        if let Some(ref mut tex) = self.texture {
            tex.set(image, egui::TextureOptions::LINEAR);
        } else {
            self.texture =
                Some(ctx.load_texture("mpv_frame_sw", image, egui::TextureOptions::LINEAR));
        }

        // Return render dimensions — egui uses this for maintain_aspect_ratio.
        self.texture.as_ref().map(|t| {
            egui::load::SizedTexture::new(t.id(), egui::vec2(rw as f32, rh as f32))
        })
    }
}

impl Drop for SoftwareRenderer {
    fn drop(&mut self) {
        unsafe {
            // Unregister callback before freeing context to prevent use-after-free.
            libmpv2_sys::mpv_render_context_set_update_callback(self.ctx, None, std::ptr::null_mut());
            libmpv2_sys::mpv_render_context_free(self.ctx);
            // Drop the callback state box.
            if !self.callback_state_ptr.is_null() {
                drop(Box::from_raw(self.callback_state_ptr as *mut CallbackState));
            }
        }
    }
}

// ── GlRenderer (Linux/EGL only) ──────────────────────────────────────────────

/// OpenGL render context using EGL for an offscreen PBuffer surface.
///
/// mpv renders into an FBO attached to a renderbuffer; pixels are read back
/// with `glReadPixels` and uploaded to an egui texture each frame.
#[cfg(target_os = "linux")]
pub struct GlRenderer {
    /// Raw mpv render context (OpenGL backend).
    ctx: *mut libmpv2_sys::mpv_render_context,
    /// EGL instance (dynamic load of libEGL.so.1).
    /// Heap-allocated so that the raw pointer passed to mpv as `get_proc_address_ctx`
    /// remains stable for the entire lifetime of the render context.
    egl: Box<khronos_egl::DynamicInstance<khronos_egl::EGL1_4>>,
    /// EGL display connection.
    egl_display: khronos_egl::Display,
    /// EGL rendering context.
    egl_ctx: khronos_egl::Context,
    /// EGL PBuffer surface (1×1 offscreen).
    egl_surface: khronos_egl::Surface,
    /// OpenGL framebuffer object name.
    fbo: gl::types::GLuint,
    /// OpenGL renderbuffer object name (color attachment).
    rbo: gl::types::GLuint,
    /// Dimensions the renderbuffer was last allocated for.
    rbo_size: (u32, u32),
    /// Cached egui texture.
    texture: Option<egui::TextureHandle>,
    /// Signals a new frame is available from mpv.
    needs_update: Arc<AtomicBool>,
    /// Owns the `CallbackState` box; freed in Drop.
    callback_state_ptr: *mut std::ffi::c_void,
}

#[cfg(target_os = "linux")]
// SAFETY: raw pointers are only accessed from the owning thread.
unsafe impl Send for GlRenderer {}

#[cfg(target_os = "linux")]
impl GlRenderer {
    /// Tries to build a GL render context for `mpv`.
    ///
    /// Fails if EGL/OpenGL are unavailable on the current platform.
    pub fn try_new(mpv: &libmpv2::Mpv, egui_ctx: egui::Context) -> Result<Self, String> {
        use std::ptr;

        // --- EGL initialisation ---
        let egl = unsafe {
            khronos_egl::DynamicInstance::<khronos_egl::EGL1_4>::load_required()
                .map_err(|e| format!("failed to load libEGL: {e}"))?
        };

        let display = unsafe {
            egl.get_display(khronos_egl::DEFAULT_DISPLAY)
                .ok_or("eglGetDisplay(DEFAULT_DISPLAY) returned None")?
        };
        egl.initialize(display).map_err(|e| format!("eglInitialize: {e:?}"))?;

        // Bind OpenGL (desktop) API before choosing config.
        let _ = egl.bind_api(khronos_egl::OPENGL_API);

        let config_attribs = [
            khronos_egl::SURFACE_TYPE,    khronos_egl::PBUFFER_BIT,
            khronos_egl::RENDERABLE_TYPE, khronos_egl::OPENGL_BIT,
            khronos_egl::RED_SIZE,        8,
            khronos_egl::GREEN_SIZE,      8,
            khronos_egl::BLUE_SIZE,       8,
            khronos_egl::ALPHA_SIZE,      8,
            khronos_egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &config_attribs)
            .map_err(|e| format!("eglChooseConfig: {e:?}"))?
            .ok_or("no EGL config found for desktop OpenGL")?;

        // Minimal 1×1 PBuffer — actual rendering goes into an FBO.
        let pbuffer_attribs = [
            khronos_egl::WIDTH,  1,
            khronos_egl::HEIGHT, 1,
            khronos_egl::NONE,
        ];
        let surface = egl
            .create_pbuffer_surface(display, config, &pbuffer_attribs)
            .map_err(|e| format!("eglCreatePbufferSurface: {e:?}"))?;

        let ctx_attribs = [
            khronos_egl::CONTEXT_MAJOR_VERSION, 3,
            khronos_egl::CONTEXT_MINOR_VERSION, 3,
            khronos_egl::NONE,
        ];
        let egl_ctx = egl
            .create_context(display, config, None, &ctx_attribs)
            .map_err(|e| format!("eglCreateContext: {e:?}"))?;

        egl.make_current(display, Some(surface), Some(surface), Some(egl_ctx))
            .map_err(|e| format!("eglMakeCurrent: {e:?}"))?;

        // --- Load GL function pointers ---
        gl::load_with(|sym| {
            egl.get_proc_address(sym)
                .map(|f| f as *const std::ffi::c_void)
                .unwrap_or(ptr::null())
        });

        // --- Create FBO + renderbuffer (1×1; resized on first poll_frame) ---
        let mut fbo: gl::types::GLuint = 0;
        let mut rbo: gl::types::GLuint = 0;
        unsafe {
            gl::GenFramebuffers(1, &mut fbo);
            gl::GenRenderbuffers(1, &mut rbo);
            gl::BindFramebuffer(gl::FRAMEBUFFER, fbo);
            gl::BindRenderbuffer(gl::RENDERBUFFER, rbo);
            gl::RenderbufferStorage(gl::RENDERBUFFER, gl::RGBA8, 1, 1);
            gl::FramebufferRenderbuffer(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::RENDERBUFFER,
                rbo,
            );
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        }

        // --- mpv OpenGL render context ---
        let needs_update = Arc::new(AtomicBool::new(false));
        let callback_box = Box::new(CallbackState {
            flag: Arc::clone(&needs_update),
            egui_ctx,
        });
        let callback_ptr = Box::into_raw(callback_box) as *mut std::ffi::c_void;

        // The get_proc_address callback passed to mpv.
        // SAFETY: egl pointer stored in ctx arg; must outlive the render context.
        // We pass a raw pointer to the EGL instance wrapped in a Box stored on the heap.
        // However, since the EGL instance must outlive the mpv context we store it in
        // the struct — mpv will call this only while the context is alive.
        // We use a simple static trampoline with a context pointer approach.
        //
        // mpv calls: get_proc_address(ctx, name) -> *mut c_void
        // The `ctx` arg carries our EGL instance pointer.

        // We heap-alloc the EGL instance so we can hand a stable raw pointer to mpv.
        let egl_box: Box<khronos_egl::DynamicInstance<khronos_egl::EGL1_4>> = Box::new(egl);

        unsafe extern "C" fn gl_get_proc_address(
            ctx: *mut std::ffi::c_void,
            name: *const std::os::raw::c_char,
        ) -> *mut std::ffi::c_void {
            if ctx.is_null() || name.is_null() {
                return ptr::null_mut();
            }
            let egl = &*(ctx as *const khronos_egl::DynamicInstance<khronos_egl::EGL1_4>);
            let cstr = std::ffi::CStr::from_ptr(name);
            let sym = match cstr.to_str() {
                Ok(s) => s,
                Err(_) => return ptr::null_mut(),
            };
            egl.get_proc_address(sym)
                .map(|f| f as *mut std::ffi::c_void)
                .unwrap_or(ptr::null_mut())
        }

        let egl_raw = Box::into_raw(egl_box) as *mut std::ffi::c_void;

        let mut opengl_init = libmpv2_sys::mpv_opengl_init_params {
            get_proc_address: Some(gl_get_proc_address),
            get_proc_address_ctx: egl_raw,
        };

        let api_type_ptr = libmpv2_sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut std::ffi::c_void;
        let mut flip_y: std::os::raw::c_int = 0;

        let params: [libmpv2_sys::mpv_render_param; 4] = [
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
                data: api_type_ptr,
            },
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: &mut opengl_init as *mut _ as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip_y as *mut _ as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param { type_: 0, data: ptr::null_mut() },
        ];

        let mut raw_ctx: *mut libmpv2_sys::mpv_render_context = ptr::null_mut();
        let err = unsafe {
            libmpv2_sys::mpv_render_context_create(
                &mut raw_ctx,
                mpv.ctx.as_ptr(),
                params.as_ptr() as *mut libmpv2_sys::mpv_render_param,
            )
        };
        if err != 0 {
            // Clean up GL resources before returning.
            unsafe {
                let egl_reclaim = Box::from_raw(egl_raw as *mut khronos_egl::DynamicInstance<khronos_egl::EGL1_4>);
                gl::DeleteRenderbuffers(1, &rbo);
                gl::DeleteFramebuffers(1, &fbo);
                egl_reclaim.make_current(display, None, None, None).ok();
                egl_reclaim.destroy_surface(display, surface).ok();
                egl_reclaim.destroy_context(display, egl_ctx).ok();
                egl_reclaim.terminate(display).ok();
                drop(Box::from_raw(callback_ptr as *mut CallbackState));
            }
            return Err(format!("mpv_render_context_create failed: {err}"));
        }

        unsafe {
            libmpv2_sys::mpv_render_context_set_update_callback(
                raw_ctx,
                Some(update_callback),
                callback_ptr,
            );
        }

        // Reclaim the EGL box so we store it in the struct.
        // Do NOT dereference — keeping it as a Box preserves the stable heap address
        // that mpv holds as `get_proc_address_ctx`.
        let egl_owned = unsafe {
            Box::from_raw(egl_raw as *mut khronos_egl::DynamicInstance<khronos_egl::EGL1_4>)
        };

        Ok(Self {
            ctx: raw_ctx,
            egl: egl_owned,
            egl_display: display,
            egl_ctx,
            egl_surface: surface,
            fbo,
            rbo,
            rbo_size: (1, 1),
            texture: None,
            needs_update,
            callback_state_ptr: callback_ptr,
        })
    }

    /// Renders the next mpv frame into an egui texture if one is available.
    ///
    /// Returns `None` when no texture exists yet (e.g. mpv hasn't decoded
    /// a frame) or the dimensions are zero.
    pub fn poll_frame(
        &mut self,
        ctx: &egui::Context,
        width: u32,
        height: u32,
    ) -> Option<egui::load::SizedTexture> {
        if width == 0 || height == 0 {
            return None;
        }

        // Fast pre-check: callback has not fired since last poll.
        if !self.needs_update.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return self.texture.as_ref().map(|t| {
                egui::load::SizedTexture::new(t.id(), egui::vec2(width as f32, height as f32))
            });
        }

        // Gate on mpv's authoritative frame-ready flag — the callback fires for
        // every internal mpv event, not only new decoded frames.
        let update_flags = unsafe { libmpv2_sys::mpv_render_context_update(self.ctx) };
        if update_flags & (libmpv2_sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME as u64) == 0 {
            return self.texture.as_ref().map(|t| {
                egui::load::SizedTexture::new(t.id(), egui::vec2(width as f32, height as f32))
            });
        }

        // Cap render resolution at 720p — glReadPixels at 1080p stalls integrated
        // Intel GPUs (Skylake) badly. Software renderer uses the same cap.
        const MAX_W: u32 = 1280;
        const MAX_H: u32 = 720;
        let rw = width.min(MAX_W);
        let rh = height.min(MAX_H);

        // Make our offscreen EGL context current.
        if self
            .egl
            .make_current(
                self.egl_display,
                Some(self.egl_surface),
                Some(self.egl_surface),
                Some(self.egl_ctx),
            )
            .is_err()
        {
            return None;
        }

        // Resize the renderbuffer when dimensions change.
        if self.rbo_size != (rw, rh) {
            unsafe {
                gl::BindRenderbuffer(gl::RENDERBUFFER, self.rbo);
                gl::RenderbufferStorage(
                    gl::RENDERBUFFER,
                    gl::RGBA8,
                    rw as gl::types::GLsizei,
                    rh as gl::types::GLsizei,
                );
                gl::BindRenderbuffer(gl::RENDERBUFFER, 0);
            }
            self.rbo_size = (rw, rh);
        }

        // Ask mpv to render into the FBO.
        let mut fbo_params = libmpv2_sys::mpv_opengl_fbo {
            fbo: self.fbo as std::os::raw::c_int,
            w:   rw as std::os::raw::c_int,
            h:   rh as std::os::raw::c_int,
            internal_format: 0, // 0 means use default (GL_RGBA)
        };
        // flip_y=0: mpv renders with video row 0 at the top of the FBO (OpenGL y=h).
        // glReadPixels reads from y=0 (bottom) upward, so row 0 of the pixel
        // buffer = bottom row of FBO = bottom of video → need to flip rows.
        // flip_y=1 would double-flip (mpv flips + glReadPixels flips = 180° rotation).
        let mut flip_y: std::os::raw::c_int = 0;

        let render_params: [libmpv2_sys::mpv_render_param; 3] = [
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo_params as *mut _ as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param {
                type_: libmpv2_sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
                data: &mut flip_y as *mut _ as *mut std::ffi::c_void,
            },
            libmpv2_sys::mpv_render_param { type_: 0, data: std::ptr::null_mut() },
        ];

        let err = unsafe {
            libmpv2_sys::mpv_render_context_render(
                self.ctx,
                render_params.as_ptr() as *mut libmpv2_sys::mpv_render_param,
            )
        };
        if err != 0 {
            tracing::warn!("GL render failed: error code {}", err);
            return None;
        }

        // Read back the pixels from the capped-resolution FBO.
        // flip_y=0: glReadPixels from y=0 upward gives correct top-down order.
        let pixel_count = (rw * rh * 4) as usize;
        let mut pixels = vec![0u8; pixel_count];
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.fbo);
            gl::ReadPixels(
                0, 0,
                rw as gl::types::GLsizei,
                rh as gl::types::GLsizei,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                pixels.as_mut_ptr() as *mut std::ffi::c_void,
            );
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        }

        let image = egui::ColorImage::from_rgba_unmultiplied(
            [rw as usize, rh as usize],
            &pixels,
        );

        if let Some(ref mut tex) = self.texture {
            tex.set(image, egui::TextureOptions::LINEAR);
        } else {
            self.texture =
                Some(ctx.load_texture("mpv_frame_gl", image, egui::TextureOptions::LINEAR));
        }

        // Return render dimensions — egui uses this for maintain_aspect_ratio.
        self.texture.as_ref().map(|t| {
            egui::load::SizedTexture::new(t.id(), egui::vec2(rw as f32, rh as f32))
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for GlRenderer {
    fn drop(&mut self) {
        unsafe {
            // Unregister mpv callback first, then free mpv render context.
            libmpv2_sys::mpv_render_context_set_update_callback(
                self.ctx, None, std::ptr::null_mut(),
            );
            libmpv2_sys::mpv_render_context_free(self.ctx);

            // Delete GL resources while context is still current.
            self.egl
                .make_current(
                    self.egl_display,
                    Some(self.egl_surface),
                    Some(self.egl_surface),
                    Some(self.egl_ctx),
                )
                .ok();
            gl::DeleteRenderbuffers(1, &self.rbo);
            gl::DeleteFramebuffers(1, &self.fbo);

            // Detach context, then destroy EGL objects.
            self.egl
                .make_current(self.egl_display, None, None, None)
                .ok();
            self.egl.destroy_surface(self.egl_display, self.egl_surface).ok();
            self.egl.destroy_context(self.egl_display, self.egl_ctx).ok();
            self.egl.terminate(self.egl_display).ok();

            // Drop the update-callback state box.
            if !self.callback_state_ptr.is_null() {
                drop(Box::from_raw(self.callback_state_ptr as *mut CallbackState));
            }
        }
    }
}

// ── ActiveRenderer ────────────────────────────────────────────────────────────

/// Holds whichever render backend was selected at startup.
enum ActiveRenderer {
    #[cfg(target_os = "linux")]
    Gl(GlRenderer),
    Software(SoftwareRenderer),
}

impl ActiveRenderer {
    /// Probe at startup: try GL (Linux only), fall back to software.
    fn probe(
        mpv: &libmpv2::Mpv,
        egui_ctx: egui::Context,
        force_software: bool,
    ) -> Self {
        #[cfg(target_os = "linux")]
        if !force_software {
            match GlRenderer::try_new(mpv, egui_ctx.clone()) {
                Ok(r) => return Self::Gl(r),
                Err(e) => tracing::warn!("renderer: GL unavailable ({}), falling back to software", e),
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = force_software;
        tracing::info!("renderer: software (pixel buffer)");
        // SoftwareRenderer::new can only fail if libmpv itself errors — treat as fatal.
        Self::Software(
            SoftwareRenderer::new(mpv, egui_ctx)
                .expect("software render context creation failed"),
        )
    }

    fn poll_frame(
        &mut self,
        ctx: &egui::Context,
        width: u32,
        height: u32,
    ) -> Option<egui::load::SizedTexture> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Gl(r) => r.poll_frame(ctx, width, height),
            Self::Software(r) => r.poll_frame(ctx, width, height),
        }
    }
}

// ── LibMpvPlayer ──────────────────────────────────────────────────────────────

/// Embedded mpv player — renders into egui via `render_frame()` each frame.
///
/// Drop order: `renderer` is declared before `mpv` so the `RenderContext`
/// inside is destroyed before the mpv handle.
pub struct LibMpvPlayer {
    renderer: ActiveRenderer,       // MUST be declared before mpv (drop order)
    mpv: libmpv2::Mpv,
    has_cdm_support: bool,
    /// Last observed time-pos, used to detect backward jumps in the audio stream.
    last_time_pos: f64,
}

impl LibMpvPlayer {
    /// Creates the player and probes the best render backend.
    ///
    /// `force_software`: skip GL probe, always use software renderer.
    /// `egui_ctx`: used by mpv's update callback to wake the egui frame loop.
    pub fn new(egui_ctx: egui::Context, force_software: bool) -> Self {
        // Probe CDM support (same as old MpvPlayer).
        let has_cdm_support = std::process::Command::new("mpv")
            .arg("--list-options")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("cdm-store"))
            .unwrap_or(false);
        if has_cdm_support {
            tracing::info!("mpv: CDM support detected");
        } else {
            tracing::debug!("mpv: no CDM support (standard build)");
        }

        let mpv = libmpv2::Mpv::new().expect("failed to create mpv instance");
        // Must set vo=libmpv before any loadfile so mpv uses the render context.
        mpv.set_property("vo", "libmpv").expect("mpv: set vo=libmpv failed");
        // Override any user config that might enable looping (loop=yes in
        // ~/.config/mpv/mpv.conf is a common culprit for the "audio repeats"
        // symptom because libmpv loads user config by default).
        let _ = mpv.set_property("loop-file", "no");
        let _ = mpv.set_property("loop-playlist", "no");
        // Force ALSA audio output. PipeWire's PulseAudio compatibility layer
        // wraps its internal ring buffer silently when underrun occurs — mpv has
        // no visibility into this and cannot apply audio-stream-silence there.
        // ALSA handles underruns differently (xrun → silence) and honours
        // mpv's audio-buffer request directly.
        let _ = mpv.set_property("ao", "alsa,pulse,pipewire");

        let renderer = ActiveRenderer::probe(&mpv, egui_ctx, force_software);

        Self { renderer, mpv, has_cdm_support, last_time_pos: f64::NAN }
    }

    /// Start playing a stream URL.
    pub fn play(
        &mut self,
        url: &str,
        auth_header: Option<&str>,
        extra_headers: &[(String, String)],
    ) {
        // Stop any current playback first.
        let _ = self.mpv.command("stop", &[]);

        // Clear header list, then rebuild.
        let _ = self.mpv.command("change-list", &["http-header-fields", "clr", ""]);

        if let Some(auth) = auth_header {
            let _ = self.mpv.command(
                "change-list",
                &["http-header-fields", "append", &format!("Authorization: {}", auth)],
            );
        }

        for (name, value) in extra_headers {
            match name.to_lowercase().as_str() {
                "referer" | "referrer" => {
                    let _ = self.mpv.set_property("referrer", value.as_str());
                }
                "user-agent" => {
                    let _ = self.mpv.set_property("user-agent", value.as_str());
                }
                _ => {
                    let _ = self.mpv.command(
                        "change-list",
                        &["http-header-fields", "append", &format!("{}: {}", name, value)],
                    );
                }
            }
        }

        // Live DASH: force software decode — hardware decoders (vaapi/vdpau)
        // refuse to init when pixel_format is "none" during DASH probing.
        let _ = self.mpv.set_property("hwdec", "no");
        // Do NOT set demuxer-lavf-analyzeduration: the default (0 for live streams)
        // is correct. Setting it to 1 caused lavf to read 1 s of live DASH audio
        // during codec probing then "rewind" — replaying that 1 s from its buffer,
        // producing the backward-replay loop symptom.

        // Audio buffer: default 0.2 s is fine now that the MPD bitrate filter
        // keeps only representations whose segments arrive well within one
        // segment duration.  A large buffer caused multi-second startup delay.
        let _ = self.mpv.set_property("audio-buffer", "0.2");

        // Cache: enable the demuxer read-ahead cache and let mpv use its default
        // size (150 MiB forward).  Do NOT set demuxer-max-bytes here — any value
        // lower than the default would reduce the buffer and make stalls more likely.
        let _ = self.mpv.set_property("cache", "yes");

        // 3 seconds of read-ahead is enough to absorb normal CDN jitter on the
        // low-bitrate representations (≤ 1.5 Mbps) selected by the MPD filter.
        let _ = self.mpv.set_property("demuxer-readahead-secs", 3.0f64);

        // Do NOT use cache-pause for live streams: when mpv pauses at the CDN live
        // edge and then unpauses, it seeks forward to the new live edge.  On DASH
        // that seek often overshoots backward by ~1 s and the player then rapidly
        // replays buffered audio to catch up — exactly the "looping" symptom.
        let _ = self.mpv.set_property("cache-pause", "no");

        // video-sync=audio (default): sync video to the audio clock.
        // Previously we used video-sync=desync to prevent A/V sync correction
        // from seeking audio backward when 3+ Mbps video segments stalled 1-6 s.
        // The MPD bitrate filter eliminates those stalls, so we can use normal
        // A/V sync again — desync broke frame pacing and caused choppy playback.

        // Output silence when the audio output buffer runs dry instead of letting
        // PulseAudio/ALSA replay whatever is in the hardware ring buffer.
        let _ = self.mpv.set_property("audio-stream-silence", "yes");

        // Prevent any looping that might have been inherited from user config.
        let _ = self.mpv.set_property("loop-file", "no");
        let _ = self.mpv.set_property("loop-playlist", "no");

        if self.has_cdm_support {
            let cdm_path = crate::widevine::dir().to_string_lossy().into_owned();
            let _ = self.mpv.set_property("cdm-store", cdm_path.as_str());
        }

        if let Err(e) = self.mpv.command("loadfile", &[url, "replace"]) {
            tracing::error!("mpv loadfile failed: {}", e);
        }
    }

    /// Stop playback.
    pub fn stop(&mut self) {
        let _ = self.mpv.command("stop", &[]);
    }

    /// Called every egui frame from `PlayerScreen::show()`.
    ///
    /// Returns a `SizedTexture` pointing at the latest decoded video frame,
    /// or `None` if no frame has been rendered yet.
    pub fn render_frame(
        &mut self,
        ctx: &egui::Context,
        width: u32,
        height: u32,
    ) -> Option<egui::load::SizedTexture> {
        // Drain the mpv event queue non-blocking to catch seek / end-of-file events.
        // These log at WARN so they're visible without --verbose and help diagnose
        // the "audio jumps back 1 second" symptom.
        loop {
            match self.mpv.event_context_mut().wait_event(0.0) {
                Some(Ok(libmpv2::events::Event::Seek)) => {
                    let pos = self.mpv
                        .get_property::<f64>("time-pos")
                        .unwrap_or(f64::NAN);
                    tracing::warn!("mpv: seek event (time-pos={:.3})", pos);
                }
                Some(Ok(libmpv2::events::Event::PlaybackRestart)) => {
                    tracing::debug!("mpv: playback-restart after seek");
                }
                Some(Ok(libmpv2::events::Event::EndFile(reason))) => {
                    tracing::warn!("mpv: end-file reason={}", reason);
                }
                None | Some(Ok(libmpv2::events::Event::QueueOverflow)) => break,
                Some(_) => {}
            }
        }
        // Detect backward jumps in the DASH presentation timeline.
        // These are NOT reported as mpv Seek events — they come from the demuxer
        // receiving a segment whose PTS is earlier than the previous segment's end.
        if let Ok(pos) = self.mpv.get_property::<f64>("time-pos") {
            if self.last_time_pos.is_finite() && pos < self.last_time_pos - 0.5 {
                tracing::warn!(
                    "mpv: time-pos jumped backward {:.3} → {:.3} (Δ={:.3} s)",
                    self.last_time_pos, pos, pos - self.last_time_pos
                );
            }
            self.last_time_pos = pos;
        }
        self.renderer.poll_frame(ctx, width, height)
    }

    /// Returns true if mpv is playing (not idle/stopped).
    #[allow(dead_code)]
    pub fn is_playing(&self) -> bool {
        self.mpv.get_property::<bool>("core-idle")
            .map(|idle| !idle)
            .unwrap_or(false)
    }
}
