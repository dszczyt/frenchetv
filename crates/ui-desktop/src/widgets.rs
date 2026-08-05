//! Small themed building blocks shared across screens.

use crate::theme::{color, lerp_color, motion, radius, text};
use egui::{Color32, Id, Response, RichText, Sense, Stroke, Ui, Vec2};

/// A hand-rolled clickable card with animated hover/selected styling.
///
/// Plain `Frame::show()` paints its background *before* the click response
/// exists, so a naive hover effect always lags a frame behind (or requires a
/// `push_id` + `.interact()` workaround just to detect clicks). Here we
/// allocate the response *first* — a single `allocate_response` call that
/// both reserves the layout space and gives us this frame's real hover/click
/// state — then paint the background from it, then lay out `add_contents`
/// into that same rect via `new_child` (which does not allocate a second
/// time in the parent, unlike `allocate_new_ui`/`scope_builder`).
///
/// `id` must be derived from stable content identity (e.g. a channel id),
/// not a loop index — otherwise animation state bleeds across tiles when the
/// list is filtered or reordered.
pub fn hover_card(
    ui: &mut Ui,
    id: Id,
    size: Vec2,
    selected: bool,
    enabled: bool,
    add_contents: impl FnOnce(&mut Ui),
) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.allocate_response(size, sense);
    let hover_t = ui
        .ctx()
        .animate_bool_with_time(id, enabled && response.hovered(), motion::FAST);
    let pressed = enabled && response.is_pointer_button_down_on();

    let bg = if selected {
        color::SURFACE_SELECTED
    } else if pressed {
        color::SURFACE_HOVER
    } else {
        lerp_color(color::SURFACE, color::SURFACE_HOVER, hover_t)
    };
    let (border_color, border_width) = if selected {
        (color::ACCENT, 2.0_f32)
    } else {
        (
            lerp_color(color::BORDER, color::BORDER_STRONG, hover_t),
            1.0_f32,
        )
    };

    ui.painter().rect_filled(response.rect, radius::MD, bg);
    ui.painter().rect_stroke(
        response.rect,
        radius::MD,
        Stroke::new(border_width, border_color),
    );

    let mut content_ui =
        ui.new_child(egui::UiBuilder::new().max_rect(response.rect.shrink(radius::SM)));
    add_contents(&mut content_ui);

    response
}

/// The one accent-filled call-to-action button a screen should have. Unlike
/// `Button::fill()` (which hard-overrides the color for every state), this
/// scopes the *inactive/hovered/active* theme colors so egui's own
/// hover/press state selection keeps working — the button still visibly
/// reacts to the pointer instead of looking identical at rest and on hover.
pub fn accent_button(ui: &mut Ui, label: &str, enabled: bool, min_size: Vec2) -> Response {
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.weak_bg_fill = color::ACCENT;
        w.inactive.bg_fill = color::ACCENT;
        w.inactive.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        w.hovered.weak_bg_fill = color::ACCENT_HOVER;
        w.hovered.bg_fill = color::ACCENT_HOVER;
        w.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        w.active.weak_bg_fill = color::ACCENT_HOVER;
        w.active.bg_fill = color::ACCENT_HOVER;
        w.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);

        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(label).size(text::BODY).strong())
                .rounding(radius::SM)
                .min_size(min_size),
        )
    })
    .inner
}

/// A muted section label used above form fields.
pub fn field_label(ui: &mut Ui, text_str: &str) {
    ui.label(
        RichText::new(text_str)
            .size(text::SMALL)
            .color(color::TEXT_MUTED),
    );
}
