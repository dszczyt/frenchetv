# Embedded Video Player Design

**Date:** 2026-05-24  
**Status:** Approved

## Goal

Replace the subprocess `MpvPlayer` (which opens video in a separate window) with an embedded `LibMpvPlayer` that renders video directly inside the egui window, using the `libmpv2` Rust crate.

## Architecture

### `LibMpvPlayer`

Owns an embedded `libmpv2::Mpv` handle and an `ActiveRenderer`. Public API mirrors `MpvPlayer`:

```rust
pub struct LibMpvPlayer {
    mpv: Arc<libmpv2::Mpv>,
    renderer: Option<ActiveRenderer>,
    has_cdm_support: bool,
}

impl LibMpvPlayer {
    pub fn new() -> Self;
    pub fn play(&mut self, url: &str, auth_header: Option<&str>, extra_headers: &[(String, String)], egui_ctx: egui::Context);
    pub fn stop(&mut self);
    pub fn render_frame(&mut self, ctx: &egui::Context, size: (u32, u32)) -> Option<egui::TextureHandle>;
    pub fn is_playing(&mut self) -> bool;
}
```

`render_frame()` is new — called every egui frame from `PlayerScreen::show()`. Returns the current video frame as a texture, or `None` if no frame is ready yet.

### `ActiveRenderer`

Enum selecting the render backend at runtime:

```rust
enum ActiveRenderer {
    Gl(GlRenderer),
    Software(SoftwareRenderer),
}
```

`ActiveRenderer::create(mpv, egui_ctx)` tries `GlRenderer::try_new()` first. On any error, logs a warning and falls back to `SoftwareRenderer::new()`. The choice is invisible to the caller.

### Render Backends

**B — `GlRenderer`** (tried first)  
libmpv renders into its own EGL/OpenGL context using `MPV_RENDER_API_TYPE_OPENGL`. Frames are read back to CPU via `glReadPixels` and uploaded to an `egui::TextureHandle`. GPU-side hardware decode is available. Will fail at init if OpenGL is unavailable (pure-Vulkan backend, Android without GL ES, etc.).

**A — `SoftwareRenderer`** (fallback)  
libmpv software-renders into a `Vec<u8>` pixel buffer using `MPV_RENDER_API_TYPE_SW`. Buffer is uploaded to an `egui::TextureHandle` each frame. Works everywhere including Android. Performance is adequate for IPTV (1080p @ 30fps ≈ 8 MB/frame upload).

### Render Loop

mpv's update callback fires when a new frame is ready. The callback calls `egui_ctx.request_repaint()`. On each egui frame:

1. `player.render_frame(ctx, (width_px, height_px))` asks the active renderer for the latest frame.
2. Renderer calls `mpv_render_context_render()` into its target (pixel buffer or FBO).
3. Returns `Some(texture_handle)` if a frame was produced, `None` otherwise.
4. `PlayerScreen::show()` paints the texture as a full-rect `egui::Image` when `Some`, or shows the loading spinner when `None`.

### Feature Preservation

All existing mpv configuration transfers via `mpv.set_property()` and `mpv.set_property_string()`:

| Feature | Mechanism |
|---------|-----------|
| Software decode | `hwdec` = `"no"` |
| DASH probe duration | `demuxer-lavf-analyzeduration` = `1` |
| Authorization header | `http-header-fields` append |
| Referer | `referrer` property |
| User-Agent | `user-agent` property |
| Extra headers | `http-header-fields` append |
| CDM store | `cdm-store` property (if CDM support detected) |
| DRM streams | Player receives `proxy_mpd_url` pointing to `http://127.0.0.1:PORT/manifest.mpd` — DRM proxy is unchanged |

CDM support detection: probe `mpv --list-options | grep cdm-store` at startup, same as current `MpvPlayer::new()`.

### `PlayerScreen` changes

`PlayerScreen::show()` currently renders a placeholder text label when `Playing`. It changes to:

1. Call `player.render_frame(ctx, available_size_px)`.
2. If `Some(tex)`: paint `egui::Image::new(tex.id(), available_size)` filling `CentralPanel`.
3. If `None`: keep the existing spinner (reuse the `Loading` state render path).
4. Info overlay (channel name, key hints) and keyboard handling are unchanged.

`PlayerScreen::new()` gains a reference to `egui::Context` for the update callback wakeup.

## File Changes

| Action | Path |
|--------|------|
| Delete | `crates/ui-desktop/src/player/mpv.rs` |
| Create | `crates/ui-desktop/src/player/libmpv.rs` |
| Modify | `crates/ui-desktop/src/player/mod.rs` |
| Modify | `crates/ui-desktop/src/screens/player.rs` |
| Modify | `crates/ui-desktop/Cargo.toml` — uncomment `libmpv2 = "3"` |

## Android Path

`LibMpvPlayer` will compile for Android targets (`aarch64-linux-android`, `armv7-linux-androideabi`) provided `libmpv.so` is placed in `jniLibs/`. The software renderer wins the fallback automatically on Android (GL init fails or is skipped). This replaces the planned ExoPlayer integration in `ui-android` and unifies playback under one implementation. Building `libmpv.so` for Android (from mpv-android sources) is a separate task.

## Error Handling

- GL renderer init failure → warning log, transparent fallback to software.
- `render_frame()` returning `None` → spinner shown, no crash.
- mpv load failure → `tracing::error!`, player stays in Loading state.
- `stop()` is idempotent; called on `PlayerScreen::drop()`.

## Testing

- Unit: `LibMpvPlayer::new()` constructs without panic (requires `libmpv-dev` installed).
- Manual: Play France 2 via Orange DRM proxy — video appears in egui window, info overlay works, channel switch (←→) works, back (Esc) works.
- Fallback: Force GL renderer to fail by temporarily passing invalid GL params in `GlRenderer::try_new()` during a dev build — software renderer activates, video still plays.
