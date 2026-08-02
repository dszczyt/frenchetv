use crate::theme::{color, space, text};
use egui::{FontId, RichText};
use egui_phosphor::regular as icon;

/// Shown at startup while a previously saved session is being restored (session
/// token validated + channel list fetched) — before we know whether it will
/// succeed. Deliberately has no operator picker or any other input: an operator
/// is already configured, so re-showing that choice would be a regression, not
/// a fallback. `App` only ever constructs this screen when `Config` already has
/// an operator + username; on failure it hands off to `SetupScreen` instead.
pub struct RestoringScreen;

impl RestoringScreen {
    pub fn new() -> Self {
        Self
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(color::BG))
            .show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() / 3.0);

                        ui.add(egui::Spinner::new().size(32.0).color(color::ORANGE_BRAND));

                        ui.add_space(space::LG);

                        ui.label(
                            RichText::new(format!("{}  Reconnexion en cours…", icon::BROADCAST))
                                .font(FontId::proportional(text::SUBTITLE))
                                .color(color::TEXT_MUTED),
                        );
                    });
                });
            });

        // Keep repainting so the spinner animates while the background task runs.
        ctx.request_repaint();
    }
}

impl Default for RestoringScreen {
    fn default() -> Self {
        Self::new()
    }
}
