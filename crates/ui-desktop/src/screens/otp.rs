use crate::theme::{color, space, text};
use crate::widgets::{accent_button, field_label};
use egui::{Align, FontId, Layout, RichText, Vec2};
use egui_phosphor::regular as icon;

/// Displayed when an operator requires a one-time code (e.g. Bouygues
/// `mfa-otp-bytel`). The user types the code received by SMS/app; on submit we
/// show a "verifying" state until the background auth task reports success/error.
pub struct OtpScreen {
    code: String,
    submitted: bool,
}

#[derive(Debug)]
pub enum OtpAction {
    None,
    /// User submitted the one-time code.
    Submit(String),
    /// User backed out; abort authentication.
    Cancel,
}

impl OtpScreen {
    pub fn new() -> Self {
        Self {
            code: String::new(),
            submitted: false,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> OtpAction {
        let mut action = OtpAction::None;

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(color::BG))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 4.0);

                    ui.label(RichText::new(icon::KEY).size(56.0).color(color::TEXT));
                    ui.add_space(space::LG);
                    ui.label(
                        RichText::new("Code de vérification")
                            .font(FontId::proportional(text::TITLE))
                            .color(color::TEXT),
                    );
                    ui.add_space(space::SM + space::XS);
                    ui.label(
                        RichText::new(
                            "Saisissez le code à usage unique envoyé\n\
                             par SMS ou via l'application Bouygues Telecom.",
                        )
                        .font(FontId::proportional(text::SUBTITLE - 3.0))
                        .color(color::TEXT_MUTED),
                    );
                    ui.add_space(space::LG);

                    if self.submitted {
                        ui.label(
                            RichText::new("Vérification…")
                                .font(FontId::proportional(text::SUBTITLE - 3.0))
                                .color(color::ACCENT),
                        );
                        ui.add_space(space::SM);
                        ui.add(egui::Spinner::new().size(24.0).color(color::TEXT));
                        ctx.request_repaint();
                        return;
                    }

                    let width = 280.0_f32.min(ui.available_width() - 40.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(width, 0.0),
                        Layout::top_down(Align::Center),
                        |ui| {
                            field_label(ui, "Code");
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut self.code)
                                    .hint_text("123456")
                                    .desired_width(f32::INFINITY)
                                    .font(FontId::proportional(20.0)),
                            );
                            let submit =
                                resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                            ui.add_space(space::MD);

                            let trimmed = self.code.trim().to_string();
                            let clicked =
                                accent_button(ui, "Valider", true, Vec2::new(200.0, 44.0))
                                    .clicked();
                            if (clicked || submit) && !trimmed.is_empty() {
                                self.submitted = true;
                                action = OtpAction::Submit(trimmed);
                            }

                            ui.add_space(space::SM);
                            if ui
                                .button(
                                    RichText::new("Annuler")
                                        .font(FontId::proportional(text::SMALL))
                                        .color(color::TEXT_MUTED),
                                )
                                .clicked()
                            {
                                action = OtpAction::Cancel;
                            }
                        },
                    );
                });
            });

        action
    }
}

impl Default for OtpScreen {
    fn default() -> Self {
        Self::new()
    }
}
