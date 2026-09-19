//! UI shared between the desktop and Android front-ends.
//!
//! Deliberately narrow: only the channel guide and the preview cache live
//! here. The other screens stay duplicated in each crate until something
//! forces the issue — consolidating them is a separate refactor with its own
//! risk, and `ui-desktop`'s `widgets.rs` would drag `theme` and
//! `egui-phosphor` into the Android build with it.
//!
//! This crate is free of `cfg(target_os = …)`, which matters: everything in
//! `ui-android` is Android-gated, so the workspace's host-side clippy and test
//! runs walk straight past it. Code that lives here is actually covered by the
//! normal CI gate.

pub mod guide;
pub mod preview;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Shared logo cache: logo_url -> decoded egui texture.
///
/// Both front-ends already declared this same type; it is defined once here so
/// the guide can take it from either.
pub type LogoCache = Arc<Mutex<HashMap<String, egui::TextureHandle>>>;

/// The guide's palette.
///
/// A local, minimal set rather than a dependency on `ui-desktop`'s `theme`:
/// that module is built around `egui-phosphor` icon fonts the Android build
/// does not carry.
pub mod palette {
    use egui::Color32;

    pub const BACKGROUND: Color32 = Color32::from_rgb(13, 15, 20);
    pub const SURFACE: Color32 = Color32::from_rgb(25, 27, 34);
    pub const SURFACE_ALT: Color32 = Color32::from_rgb(20, 22, 28);
    pub const SURFACE_SELECTED: Color32 = Color32::from_rgb(20, 40, 70);
    pub const BORDER: Color32 = Color32::from_rgb(60, 60, 70);
    pub const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
    pub const ACCENT_DIM: Color32 = Color32::from_rgb(50, 100, 160);
    pub const TEXT: Color32 = Color32::WHITE;
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(160, 160, 170);
    pub const TEXT_FAINT: Color32 = Color32::from_rgb(100, 100, 110);
    pub const NOW_LINE: Color32 = Color32::from_rgb(255, 90, 90);
}
