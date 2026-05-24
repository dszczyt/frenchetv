# Embedded Video Player Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the subprocess `MpvPlayer` (separate window) with an embedded `LibMpvPlayer` using `libmpv2` that renders video directly inside the egui window, with a GL renderer (tried first) falling back to software automatically.

**Architecture:** `LibMpvPlayer` owns an embedded mpv instance and an `ActiveRenderer` chosen at construction time by probing GL availability. Two backends: `GlRenderer` uses an EGL/OpenGL offscreen context + FBO + `glReadPixels` (Linux only); `SoftwareRenderer` uses mpv's software pixel-buffer render path (universal). Both produce an `egui::TextureHandle` updated each frame from mpv's update callback.

**Tech Stack:** `libmpv2 = "3"`, `khronos-egl = "6"` (Linux GL path), `gl = "0.14"` (Linux GL path), `egui 0.30`, `eframe 0.30/wgpu`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| **Modify** | `crates/ui-desktop/Cargo.toml` | Add `libmpv2`, `khronos-egl`, `gl` |
| **Modify** | `crates/ui-desktop/src/main.rs` | No change needed; `App::new` parses args |
| **Create** | `crates/ui-desktop/src/player/libmpv.rs` | All player logic: `LibMpvPlayer`, `ActiveRenderer`, `SoftwareRenderer`, `GlRenderer` |
| **Modify** | `crates/ui-desktop/src/player/mod.rs` | Expose `libmpv` module, remove `mpv` |
| **Delete** | `crates/ui-desktop/src/player/mpv.rs` | Replaced by libmpv.rs |
| **Modify** | `crates/ui-desktop/src/screens/player.rs` | Accept `egui_ctx` + `force_software`, call `render_frame()` |
| **Modify** | `crates/ui-desktop/src/app.rs` | Parse `--force-software-renderer`, pass to `PlayerScreen::new` |

---

## Task 1: Add Dependencies

**Files:**
- Modify: `crates/ui-desktop/Cargo.toml`

- [ ] **Step 1: Uncomment `libmpv2` and add GL deps**

Replace in `[dependencies]`:
```toml
libmpv2 = "3"
```

Add after the existing dependencies:
```toml
[target.'cfg(target_os = "linux")'.dependencies]
khronos-egl = { version = "6", features = ["dynamic"] }
gl = "0.14"
```

- [ ] **Step 2: Verify the crate resolves**

```bash
cargo fetch 2>&1 | tail -5
```
Expected: `Finished` or new crate names downloaded, no errors.

- [ ] **Step 3: Check libmpv-dev is installed**

```bash
pkg-config --modversion mpv
```
Expected: a version string like `0.38.0`. If missing: `sudo pacman -S mpv` (Arch) or `sudo apt install libmpv-dev`.

- [ ] **Step 4: Verify build compiles with libmpv2**

```bash
cargo build -p ui-desktop 2>&1 | grep -E "^error|Finished"
```
Expected: `Finished`. If there are linker errors about `libmpv`, ensure `libmpv-dev` is installed.

- [ ] **Step 5: Commit**

```bash
git add crates/ui-desktop/Cargo.toml
git commit -m "build: enable libmpv2 crate + linux GL deps (khronos-egl, gl)"
```

---

## Task 2: SoftwareRenderer

**Files:**
- Create: `crates/ui-desktop/src/player/libmpv.rs`

- [ ] **Step 1: Create the file with SoftwareRenderer**

```rust
//! Embedded mpv player using `libmpv2`.
//!
//! Two render backends:
//! - `GlRenderer` (Linux/EGL): offscreen OpenGL FBO, GPU-side decode where possible.
//! - `SoftwareRenderer`: mpv software pixel-buffer, works everywhere.
//!
//! `ActiveRenderer::probe()` tries GL first at construction; falls back to software.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use libmpv2::render::{RenderContext, RenderParam, RenderParamApiType};

// ── SoftwareRenderer ──────────────────────────────────────────────────────────

pub struct SoftwareRenderer {
    render_ctx: RenderContext,
    /// Owned texture handle kept alive so the GPU texture is not freed.
    texture: Option<egui::TextureHandle>,
    /// Set to true by mpv's update callback — signals a new frame is ready.
    needs_update: Arc<AtomicBool>,
}

impl SoftwareRenderer {
    /// Creates a software render context tied to `mpv`.
    ///
    /// # Safety
    /// `mpv.ctx` must remain valid for the lifetime of this renderer.
    pub fn new(mpv: &libmpv2::Mpv, egui_ctx: egui::Context) -> Result<Self, libmpv2::Error> {
        let render_ctx = RenderContext::new(
            unsafe { mpv.ctx.as_ptr() },
            &[RenderParam::ApiType(RenderParamApiType::Software)],
        )?;

        let needs_update = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&needs_update);
        render_ctx.set_update_callback(move || {
            flag.store(true, Ordering::Release);
            egui_ctx.request_repaint();
        });

        Ok(Self { render_ctx, texture: None, needs_update })
    }

    /// Renders a new frame if mpv signalled one is ready.
    ///
    /// Returns the current texture (new or cached).
    pub fn poll_frame(
        &mut self,
        ctx: &egui::Context,
        width: u32,
        height: u32,
    ) -> Option<egui::load::SizedTexture> {
        if width == 0 || height == 0 {
            return None;
        }

        // Only re-render if mpv flagged a new frame.
        if !self.needs_update.swap(false, Ordering::AcqRel) {
            return self.texture.as_ref().map(|t| {
                egui::load::SizedTexture::new(t.id(), egui::vec2(width as f32, height as f32))
            });
        }

        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let format = b"bgr0\0";
        let stride = (width * 4) as usize;

        let result = unsafe {
            self.render_ctx.render(&[
                RenderParam::SwSize([width as i32, height as i32]),
                RenderParam::SwFormat(format.as_ptr() as *const std::ffi::c_char),
                RenderParam::SwStride(stride),
                RenderParam::SwPointer(pixels.as_mut_ptr() as *mut std::ffi::c_void),
            ])
        };

        if let Err(e) = result {
            tracing::warn!("software render failed: {}", e);
            return None;
        }

        // bgr0 → rgba: swap R/B, set alpha=255
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }

        let image = egui::ColorImage::from_rgba_unmultiplied(
            [width as usize, height as usize],
            &pixels,
        );

        if let Some(ref mut tex) = self.texture {
            tex.set(image, egui::TextureOptions::LINEAR);
        } else {
            self.texture = Some(ctx.load_texture("mpv_frame_sw", image, egui::TextureOptions::LINEAR));
        }

        self.texture.as_ref().map(|t| {
            egui::load::SizedTexture::new(t.id(), egui::vec2(width as f32, height as f32))
        })
    }
}
```

- [ ] **Step 2: Verify it compiles (module not wired yet — just syntax check)**

```bash
cargo build -p ui-desktop 2>&1 | grep "^error\[" | head -10
```
Expected: no errors referencing `libmpv.rs` (file not in mod tree yet, so it's silently ignored).

---

## Task 3: GlRenderer (Linux/EGL)

**Files:**
- Modify: `crates/ui-desktop/src/player/libmpv.rs`

- [ ] **Step 0: Verify `OpenGLInitParams` API before writing code**

```bash
cargo doc -p ui-desktop --open 2>/dev/null; cargo doc -p ui-desktop 2>&1 | grep -i "opengl\|init_param" | head -10
```

Check what fields `libmpv2::render::OpenGLInitParams` actually has. The C struct is:
```c
struct mpv_opengl_init_params {
    void *(*get_proc_address)(void *ctx, const char *name);
    void *get_proc_address_ctx;
};
```
Adjust the Rust code in this task to match the actual crate wrapper. If the crate uses a `Box<dyn Fn>` closure, use that. If it exposes raw C function pointers, use `unsafe extern "C" fn`.

- [ ] **Step 1: Add GL renderer struct and `try_new` to `libmpv.rs`**

Append after `SoftwareRenderer`:

```rust
// ── GlRenderer (Linux / EGL only) ─────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub struct GlRenderer {
    render_ctx: RenderContext,
    egl: khronos_egl::DynamicInstance<khronos_egl::EGL1_4>,
    egl_display: khronos_egl::Display,
    egl_context: khronos_egl::Context,
    egl_surface: khronos_egl::Surface,
    fbo: u32,
    rbo: u32,
    texture: Option<egui::TextureHandle>,
    needs_update: Arc<AtomicBool>,
    frame_width: u32,
    frame_height: u32,
}

#[cfg(target_os = "linux")]
impl GlRenderer {
    pub fn try_new(mpv: &libmpv2::Mpv, egui_ctx: egui::Context) -> Result<Self, String> {
        use khronos_egl as egl;
        use std::ptr;

        let egl = unsafe {
            egl::DynamicInstance::<egl::EGL1_4>::load_required()
                .map_err(|e| format!("EGL load failed: {}", e))?
        };

        let display = egl.get_display(egl::DEFAULT_DISPLAY)
            .ok_or("EGL default display unavailable")?;
        egl.initialize(display)
            .map_err(|e| format!("EGL initialize failed: {:?}", e))?;

        let attribs = [
            egl::RED_SIZE,   8,
            egl::GREEN_SIZE, 8,
            egl::BLUE_SIZE,  8,
            egl::ALPHA_SIZE, 8,
            egl::SURFACE_TYPE, egl::PBUFFER_BIT,
            egl::RENDERABLE_TYPE, egl::OPENGL_BIT,
            egl::NONE,
        ];
        let config = egl.choose_first_config(display, &attribs)
            .map_err(|e| format!("EGL choose config failed: {:?}", e))?
            .ok_or("no suitable EGL config found")?;

        egl.bind_api(egl::OPENGL_API)
            .map_err(|e| format!("EGL bind OpenGL API failed: {:?}", e))?;

        let ctx_attribs = [egl::NONE];
        let egl_context = egl.create_context(display, config, None, &ctx_attribs)
            .map_err(|e| format!("EGL create context failed: {:?}", e))?;

        let surf_attribs = [egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE];
        let egl_surface = egl.create_pbuffer_surface(display, config, &surf_attribs)
            .map_err(|e| format!("EGL pbuffer surface failed: {:?}", e))?;

        egl.make_current(display, Some(egl_surface), Some(egl_surface), Some(egl_context))
            .map_err(|e| format!("EGL make_current failed: {:?}", e))?;

        // Load GL function pointers
        gl::load_with(|s| {
            egl.get_proc_address(s)
                .map_or(ptr::null(), |f| f as *const _)
        });

        // Create FBO + renderbuffer (1×1 placeholder; resized per frame)
        let (mut fbo, mut rbo) = (0u32, 0u32);
        unsafe {
            gl::GenFramebuffers(1, &mut fbo);
            gl::GenRenderbuffers(1, &mut rbo);
        }

        // Create mpv render context with GL proc address callback
        let get_proc = |name: &str| -> *mut std::ffi::c_void {
            egl.get_proc_address(name)
                .map_or(ptr::null_mut(), |f| f as *mut _)
        };
        let init_params = libmpv2::render::OpenGLInitParams {
            get_proc_address: Box::new(get_proc),
        };
        let render_ctx = RenderContext::new(
            unsafe { mpv.ctx.as_ptr() },
            &[
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(init_params),
            ],
        ).map_err(|e| format!("mpv GL render context failed: {}", e))?;

        let needs_update = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&needs_update);
        render_ctx.set_update_callback(move || {
            flag.store(true, Ordering::Release);
            egui_ctx.request_repaint();
        });

        tracing::info!("renderer: OpenGL/EGL (offscreen FBO)");

        Ok(Self {
            render_ctx,
            egl,
            egl_display: display,
            egl_context,
            egl_surface,
            fbo,
            rbo,
            texture: None,
            needs_update,
            frame_width: 0,
            frame_height: 0,
        })
    }

    pub fn poll_frame(
        &mut self,
        ctx: &egui::Context,
        width: u32,
        height: u32,
    ) -> Option<egui::load::SizedTexture> {
        if width == 0 || height == 0 {
            return None;
        }
        if !self.needs_update.swap(false, Ordering::AcqRel) {
            return self.texture.as_ref().map(|t| {
                egui::load::SizedTexture::new(t.id(), egui::vec2(width as f32, height as f32))
            });
        }

        // Make EGL context current for this thread
        self.egl.make_current(
            self.egl_display,
            Some(self.egl_surface),
            Some(self.egl_surface),
            Some(self.egl_context),
        ).ok()?;

        // Resize renderbuffer if needed
        if self.frame_width != width || self.frame_height != height {
            unsafe {
                gl::BindRenderbuffer(gl::RENDERBUFFER, self.rbo);
                gl::RenderbufferStorage(gl::RENDERBUFFER, gl::RGBA8, width as i32, height as i32);
                gl::BindFramebuffer(gl::FRAMEBUFFER, self.fbo);
                gl::FramebufferRenderbuffer(
                    gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0,
                    gl::RENDERBUFFER, self.rbo,
                );
            }
            self.frame_width = width;
            self.frame_height = height;
        }

        // Ask mpv to render into our FBO
        let fbo = libmpv2::render::OpenGLFbo {
            fbo: self.fbo as i32,
            w: width as i32,
            h: height as i32,
            internal_format: 0, // 0 = GL_RGBA (default)
        };
        if let Err(e) = self.render_ctx.render(&[
            RenderParam::Fbo(fbo),
            RenderParam::FlipY(true),
        ]) {
            tracing::warn!("GL render failed: {}", e);
            return None;
        }

        // Read pixels back to CPU
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.fbo);
            gl::ReadPixels(
                0, 0, width as i32, height as i32,
                gl::RGBA, gl::UNSIGNED_BYTE,
                pixels.as_mut_ptr() as *mut _,
            );
        }

        let image = egui::ColorImage::from_rgba_unmultiplied(
            [width as usize, height as usize],
            &pixels,
        );
        if let Some(ref mut tex) = self.texture {
            tex.set(image, egui::TextureOptions::LINEAR);
        } else {
            self.texture = Some(ctx.load_texture("mpv_frame_gl", image, egui::TextureOptions::LINEAR));
        }
        self.texture.as_ref().map(|t| {
            egui::load::SizedTexture::new(t.id(), egui::vec2(width as f32, height as f32))
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for GlRenderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteFramebuffers(1, &self.fbo);
            gl::DeleteRenderbuffers(1, &self.rbo);
        }
        let _ = self.egl.make_current(self.egl_display, None, None, None);
        let _ = self.egl.destroy_surface(self.egl_display, self.egl_surface);
        let _ = self.egl.destroy_context(self.egl_display, self.egl_context);
    }
}
```

- [ ] **Step 2: Check GL renderer compiles**

```bash
cargo build -p ui-desktop 2>&1 | grep "^error\[" | head -10
```
Expected: no errors (file still not in mod tree).

---

## Task 4: ActiveRenderer + LibMpvPlayer

**Files:**
- Modify: `crates/ui-desktop/src/player/libmpv.rs`

- [ ] **Step 1: Append `ActiveRenderer` enum + `LibMpvPlayer`**

```rust
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

        let renderer = ActiveRenderer::probe(&mpv, egui_ctx, force_software);

        Self { renderer, mpv, has_cdm_support }
    }

    /// Start playing a stream URL.
    ///
    /// Translates all header/CDM options to mpv properties, exactly as the
    /// old subprocess player did via CLI flags.
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
        let _ = self.mpv.set_property("demuxer-lavf-analyzeduration", 1i64);

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
        self.renderer.poll_frame(ctx, width, height)
    }

    /// Returns true if mpv is playing (not idle/stopped).
    pub fn is_playing(&self) -> bool {
        self.mpv.get_property::<String>("core-idle")
            .map(|v| v == "no")
            .unwrap_or(false)
    }
}
```

- [ ] **Step 2: Add necessary `use` imports at the top of `libmpv.rs`**

The file top should be:
```rust
//! Embedded mpv player using `libmpv2`.
// ... (docstring from Task 2)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use libmpv2::render::{RenderContext, RenderParam, RenderParamApiType};

#[cfg(target_os = "linux")]
use libmpv2::render::OpenGLInitParams;
```

- [ ] **Step 3: File still not in module tree — compile check via direct path**

```bash
rustc --edition 2021 --crate-type lib \
  crates/ui-desktop/src/player/libmpv.rs \
  2>&1 | grep "^error" | head -10
```
Expected: errors about unresolved `crate::widevine` and `egui` (normal — not linked). No syntax errors.

---

## Task 5: Wire `player/mod.rs` and delete `mpv.rs`

**Files:**
- Modify: `crates/ui-desktop/src/player/mod.rs`
- Delete: `crates/ui-desktop/src/player/mpv.rs`

- [ ] **Step 1: Replace mod.rs content**

```rust
pub mod libmpv;
```

- [ ] **Step 2: Delete mpv.rs**

```bash
rm crates/ui-desktop/src/player/mpv.rs
```

- [ ] **Step 3: Build — expect many errors about `MpvPlayer` (to be fixed in Tasks 6–7)**

```bash
cargo build -p ui-desktop 2>&1 | grep "^error" | head -20
```
Expected: errors like `use of undeclared type MpvPlayer`, `module mpv not found`. These will be fixed next.

---

## Task 6: Update `PlayerScreen`

**Files:**
- Modify: `crates/ui-desktop/src/screens/player.rs`

- [ ] **Step 1: Replace the entire file**

```rust
use egui::{Color32, FontId, Key, RichText, Vec2};
use frenchetv_core::Channel;
use crate::player::libmpv::LibMpvPlayer;
use frenchetv_core::StreamUrl;

enum PlayerState {
    Loading,
    Playing,
}

pub struct PlayerScreen {
    pub channel: Channel,
    player: LibMpvPlayer,
    state: PlayerState,
    info_visible: bool,
    info_hide_timer: f32,
}

#[derive(Debug)]
pub enum PlayerAction {
    None,
    Back,
    NextChannel,
    PrevChannel,
}

impl PlayerScreen {
    /// Create a loading player screen.
    ///
    /// `egui_ctx` is passed to `LibMpvPlayer` so mpv's update callback can
    /// wake the egui frame loop when a new frame is ready.
    /// `force_software` skips GL renderer probe and always uses software path.
    pub fn new(channel: Channel, egui_ctx: egui::Context, force_software: bool) -> Self {
        Self {
            channel,
            player: LibMpvPlayer::new(egui_ctx, force_software),
            state: PlayerState::Loading,
            info_visible: false,
            info_hide_timer: 0.0,
        }
    }

    /// Called when the stream has been resolved — starts mpv playback.
    pub fn start_playing(&mut self, stream: &StreamUrl) {
        self.player.play(
            stream.url.as_str(),
            stream.auth_header.as_deref(),
            &stream.headers,
        );
        self.state = PlayerState::Playing;
        self.info_visible = true;
        self.info_hide_timer = 3.0;
    }

    pub fn show(&mut self, ctx: &egui::Context) -> PlayerAction {
        let (dt, action, toggle_info) = ctx.input(|i| {
            let dt = i.unstable_dt;
            let action = if i.key_pressed(Key::Escape) || i.key_pressed(Key::Backspace) {
                PlayerAction::Back
            } else if i.key_pressed(Key::ArrowRight) {
                PlayerAction::NextChannel
            } else if i.key_pressed(Key::ArrowLeft) {
                PlayerAction::PrevChannel
            } else {
                PlayerAction::None
            };
            let toggle_info = i.key_pressed(Key::Enter);
            (dt, action, toggle_info)
        });

        if toggle_info {
            self.info_visible = !self.info_visible;
            self.info_hide_timer = 3.0;
        }

        if self.info_visible {
            self.info_hide_timer -= dt;
            if self.info_hide_timer <= 0.0 {
                self.info_visible = false;
            }
            ctx.request_repaint();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::BLACK))
            .show(ctx, |ui| {
                let available = ui.available_size();
                let w = available.x as u32;
                let h = available.y as u32;

                match self.state {
                    PlayerState::Loading => {
                        // Show spinner while stream is resolving.
                        ui.centered_and_justified(|ui| {
                            ui.vertical_centered(|ui| {
                                ui.add_space(available.y / 2.0 - 32.0);
                                ui.add(egui::Spinner::new().size(40.0).color(Color32::WHITE));
                                ui.add_space(16.0);
                                ui.label(
                                    RichText::new(format!("Chargement de {}…", self.channel.name))
                                        .font(FontId::proportional(16.0))
                                        .color(Color32::from_rgb(160, 160, 160)),
                                );
                            });
                        });
                        ctx.request_repaint();
                    }
                    PlayerState::Playing => {
                        match self.player.render_frame(ctx, w, h) {
                            Some(sized_texture) => {
                                // Video frame available — fill the panel.
                                ui.add(
                                    egui::Image::new(sized_texture)
                                        .fit_to_exact_size(available),
                                );
                            }
                            None => {
                                // mpv is loading/buffering — show spinner.
                                ui.centered_and_justified(|ui| {
                                    ui.add(egui::Spinner::new().size(40.0).color(Color32::WHITE));
                                });
                                ctx.request_repaint();
                            }
                        }
                    }
                }

                // Info overlay (channel name + key hints).
                if self.info_visible {
                    let rect = ui.max_rect();
                    let overlay_height = 80.0;
                    let overlay_rect = egui::Rect::from_min_size(
                        egui::pos2(rect.min.x, rect.max.y - overlay_height),
                        Vec2::new(rect.width(), overlay_height),
                    );
                    ui.painter().rect_filled(
                        overlay_rect,
                        0.0,
                        Color32::from_rgba_unmultiplied(0, 0, 0, 180),
                    );
                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(overlay_rect), |ui| {
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.add_space(16.0);
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(&self.channel.name)
                                        .font(FontId::proportional(22.0))
                                        .color(Color32::WHITE),
                                );
                                ui.label(
                                    RichText::new("← → Changer  ↵ Info  Esc Retour")
                                        .font(FontId::proportional(12.0))
                                        .color(Color32::from_rgb(160, 160, 160)),
                                );
                            });
                        });
                    });
                }
            });

        action
    }
}

impl Drop for PlayerScreen {
    fn drop(&mut self) {
        self.player.stop();
    }
}
```

- [ ] **Step 2: Build — expect errors about `PlayerScreen::new` signature in app.rs**

```bash
cargo build -p ui-desktop 2>&1 | grep "^error" | head -10
```
Expected: errors about `PlayerScreen::new` called with wrong argument count in `app.rs`.

---

## Task 7: Update `app.rs`

**Files:**
- Modify: `crates/ui-desktop/src/app.rs`

- [ ] **Step 1: Add `force_software_renderer` field to `App` struct**

Find:
```rust
pub struct App {
    screen: Screen,
```
Replace with:
```rust
pub struct App {
    force_software_renderer: bool,
    screen: Screen,
```

- [ ] **Step 2: Parse CLI flag + store in App::new**

Find the `let app = Self {` block (around line 67) and add the `force_software_renderer` field. Also add the parse line before it.

Add before `let app = Self {`:
```rust
let force_software_renderer = std::env::args().any(|a| a == "--force-software-renderer");
if force_software_renderer {
    tracing::info!("renderer: forced software mode via --force-software-renderer");
}
```

Add inside the `Self { ... }` initializer:
```rust
force_software_renderer,
```

- [ ] **Step 3: Update all `PlayerScreen::new(channel)` calls**

Find (appears twice in app.rs):
```rust
self.screen = Screen::Player(PlayerScreen::new(channel));
```
Replace both occurrences with:
```rust
self.screen = Screen::Player(PlayerScreen::new(
    channel,
    self.egui_ctx.clone(),
    self.force_software_renderer,
));
```

- [ ] **Step 4: Remove the old `use crate::player::mpv::MpvPlayer` import if present**

Search for any `mpv::MpvPlayer` import in app.rs:
```bash
grep -n "mpv\|MpvPlayer" crates/ui-desktop/src/app.rs
```
If found, remove those lines.

- [ ] **Step 5: Build — should be clean**

```bash
cargo build -p ui-desktop 2>&1 | grep -E "^error|Finished"
```
Expected: `Finished dev profile`. If there are remaining errors, read the error output and fix the specific lines indicated.

- [ ] **Step 6: Commit**

```bash
git add crates/ui-desktop/Cargo.toml \
        crates/ui-desktop/src/player/libmpv.rs \
        crates/ui-desktop/src/player/mod.rs \
        crates/ui-desktop/src/screens/player.rs \
        crates/ui-desktop/src/app.rs
git rm crates/ui-desktop/src/player/mpv.rs
git commit -m "feat: embedded video player (libmpv2, GL→software fallback)"
```

---

## Task 8: Verify and Test

**Files:** none (testing only)

- [ ] **Step 1: Run clippy**

```bash
cargo clippy -p ui-desktop 2>&1 | grep "^error\|^warning.*unused" | head -20
```
Expected: no errors. Warnings about unused imports or variables: fix them.

- [ ] **Step 2: Launch in software mode to test fallback flag**

```bash
RUST_LOG=info cargo run -p ui-desktop -- --force-software-renderer 2>&1 | grep "renderer:"
```
Expected log line: `renderer: software (pixel buffer)`

- [ ] **Step 3: Launch normally and observe renderer selection**

```bash
RUST_LOG=info cargo run -p ui-desktop 2>&1 | grep "renderer:"
```
Expected: either `renderer: OpenGL/EGL (offscreen FBO)` or `renderer: GL unavailable (...), falling back to software`.

- [ ] **Step 4: Play France 2 and verify embedded video**

Run the app, log into Orange, select France 2.

Expected:
- Video plays inside the egui window (not a separate window).
- Info overlay (channel name, `← → Changer ↵ Info Esc Retour`) appears for 3s then hides.
- `←` / `→` switches channels.
- `Esc` returns to channel list.
- No `Errors when loading file` in mpv output.

- [ ] **Step 5: Commit test results and any fixes**

```bash
git add -A
git commit -m "fix: address clippy warnings in embedded player"
```
(Only needed if Step 1 had warnings that required fixes.)
