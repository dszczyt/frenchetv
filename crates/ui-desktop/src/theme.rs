//! Shared design tokens and the one-time `egui::Style`/font setup applied at
//! startup. All screens should pull colors/spacing/radius from here instead of
//! hardcoding RGB triples, so the app reads as one consistent surface instead
//! of five ad-hoc dark screens.

use egui::{Color32, Stroke, Visuals};

/// Color tokens. `ACCENT` matches the app's existing interactive blue — kept
/// as-is (not swapped to a "streaming red") so this stays a systemization of
/// the current identity rather than a re-brand.
pub mod color {
    use egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0c);
    pub const SURFACE: Color32 = Color32::from_rgb(0x18, 0x1a, 0x21);
    pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x22, 0x25, 0x2e);
    pub const SURFACE_SELECTED: Color32 = Color32::from_rgb(0x14, 0x28, 0x45);
    pub const BORDER: Color32 = Color32::from_rgb(0x2c, 0x2f, 0x3a);
    pub const BORDER_STRONG: Color32 = Color32::from_rgb(0x40, 0x44, 0x52);

    pub const TEXT: Color32 = Color32::from_rgb(0xf5, 0xf6, 0xf8);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xac);
    pub const TEXT_DISABLED: Color32 = Color32::from_rgb(0x58, 0x5c, 0x66);

    /// Existing iOS-blue interactive accent, tokenized.
    pub const ACCENT: Color32 = Color32::from_rgb(0x0a, 0x84, 0xff);
    pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x3d, 0xa1, 0xff);
    /// ~31% accent tint. `from_rgba_premultiplied` requires each channel
    /// already scaled by alpha (0x50/255 ≈ 0.31) — passing the raw 0x0a/0x84/0xff
    /// unmultiplied would violate that contract and paint a bright, near-additive
    /// blue instead of a soft tint.
    pub const ACCENT_SOFT: Color32 = Color32::from_rgba_premultiplied(0x03, 0x29, 0x50, 0x50);

    pub const ERROR: Color32 = Color32::from_rgb(0xff, 0x45, 0x3a);
    /// Orange-brand push notification color (push_wait screen only) — kept
    /// distinct from the semantic `WARNING` token below.
    pub const ORANGE_BRAND: Color32 = Color32::from_rgb(0xff, 0x78, 0x00);
    pub const WARNING: Color32 = Color32::from_rgb(0xff, 0x9f, 0x0a);

    /// Modal/overlay scrim — ~59% black, inside the 40-60% legibility band.
    pub const SCRIM: Color32 = Color32::from_black_alpha(150);
}

/// 4/8pt spacing scale.
pub mod space {
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 16.0;
    pub const LG: f32 = 24.0;
    pub const XL: f32 = 32.0;
    pub const XXL: f32 = 48.0;
}

/// Corner-radius scale.
pub mod radius {
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 12.0;
}

/// Micro-interaction durations (seconds). Kept in the 120-250ms band.
pub mod motion {
    pub const FAST: f32 = 0.12;
    pub const NORMAL: f32 = 0.2;
}

/// Type scale (px).
pub mod text {
    pub const TITLE: f32 = 22.0;
    pub const SUBTITLE: f32 = 18.0;
    pub const BODY: f32 = 16.0;
    pub const LABEL: f32 = 14.0;
    pub const SMALL: f32 = 13.0;
}

/// Byte-wise lerp between two colors. Good enough for UI hover/press fades;
/// avoids depending on an unconfirmed `Color32::lerp` API.
pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        mix(a.a(), b.a()),
    )
}

/// Scale a color's existing alpha by `t` (0..1) — used to fade a whole group
/// of shapes/text in or out together instead of snapping visibility on/off.
pub fn fade(color: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let a = (color.a() as f32 * t).round() as u8;
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
}

/// Install the Phosphor icon font and apply the global dark theme. Call once
/// from `App::new`, before the first frame is shown.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    apply_visuals(&mut style.visuals);
    style.spacing.item_spacing = egui::vec2(space::SM, space::SM);
    style.spacing.button_padding = egui::vec2(space::MD, space::SM);
    ctx.set_style(style);
}

fn apply_visuals(visuals: &mut Visuals) {
    *visuals = Visuals::dark();
    // Deliberately NOT setting `override_text_color`: egui bakes unstyled
    // RichText (no `.color()`/`.strong()`/`.weak()`) to that override at
    // *creation* time rather than at paint time, which would permanently fix
    // every plain button/label to one color and defeat the per-state
    // (inactive/hovered/active) `fg_stroke` colors set below. Plain
    // `ui.label()` text still gets a consistent default via
    // `noninteractive.fg_stroke` a few lines down.
    visuals.window_fill = color::BG;
    visuals.panel_fill = color::BG;
    visuals.extreme_bg_color = color::SURFACE;
    visuals.faint_bg_color = color::SURFACE;
    visuals.hyperlink_color = color::ACCENT;
    visuals.error_fg_color = color::ERROR;
    visuals.warn_fg_color = color::WARNING;

    // Visible focus ring + text-selection tint, driven by the accent token —
    // this is what makes keyboard focus visible on TextEdit/Button app-wide.
    visuals.selection.bg_fill = color::ACCENT_SOFT;
    visuals.selection.stroke = Stroke::new(2.0_f32, color::ACCENT);

    // Turn hover into a pointing-hand cursor for every interactive widget,
    // without needing `.on_hover_cursor()` at each call site.
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);

    let w = &mut visuals.widgets;
    w.noninteractive.bg_fill = color::BG;
    w.noninteractive.weak_bg_fill = color::SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0_f32, color::BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0_f32, color::TEXT);

    w.inactive.bg_fill = color::SURFACE;
    w.inactive.weak_bg_fill = color::SURFACE;
    w.inactive.bg_stroke = Stroke::new(1.0_f32, color::BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0_f32, color::TEXT_MUTED);

    w.hovered.bg_fill = color::SURFACE_HOVER;
    w.hovered.weak_bg_fill = color::SURFACE_HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0_f32, color::BORDER_STRONG);
    w.hovered.fg_stroke = Stroke::new(1.0_f32, color::TEXT);

    w.active.bg_fill = color::SURFACE_SELECTED;
    w.active.weak_bg_fill = color::SURFACE_SELECTED;
    w.active.bg_stroke = Stroke::new(1.5_f32, color::ACCENT);
    w.active.fg_stroke = Stroke::new(1.0_f32, color::TEXT);

    w.open.bg_fill = color::SURFACE_HOVER;
    w.open.weak_bg_fill = color::SURFACE_HOVER;
    w.open.bg_stroke = Stroke::new(1.5_f32, color::ACCENT);
    w.open.fg_stroke = Stroke::new(1.0_f32, color::TEXT);
}
