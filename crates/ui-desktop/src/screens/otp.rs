use egui::{Align, Color32, FontId, Layout, RichText, Vec2};

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
            .frame(egui::Frame::none().fill(Color32::from_rgb(13, 15, 20)))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 4.0);

                    ui.label(RichText::new("🔑").font(FontId::proportional(56.0)));
                    ui.add_space(24.0);
                    ui.label(
                        RichText::new("Code de vérification")
                            .font(FontId::proportional(22.0))
                            .color(Color32::WHITE),
                    );
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new(
                            "Saisissez le code à usage unique envoyé\n\
                             par SMS ou via l'application Bouygues Telecom.",
                        )
                        .font(FontId::proportional(15.0))
                        .color(Color32::from_rgb(160, 160, 160)),
                    );
                    ui.add_space(24.0);

                    if self.submitted {
                        ui.label(
                            RichText::new("Vérification…")
                                .font(FontId::proportional(15.0))
                                .color(Color32::from_rgb(10, 132, 255)),
                        );
                        ui.add_space(8.0);
                        ui.add(egui::Spinner::new().size(24.0));
                        ctx.request_repaint();
                        return;
                    }

                    let width = 280.0_f32.min(ui.available_width() - 40.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(width, 0.0),
                        Layout::top_down(Align::Center),
                        |ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut self.code)
                                    .hint_text("123456")
                                    .desired_width(f32::INFINITY)
                                    .font(FontId::proportional(20.0)),
                            );
                            let submit =
                                resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                            ui.add_space(16.0);

                            let btn = egui::Button::new(
                                RichText::new("Valider")
                                    .font(FontId::proportional(18.0))
                                    .color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(10, 132, 255))
                            .rounding(8.0)
                            .min_size(Vec2::new(200.0, 44.0));

                            let trimmed = self.code.trim().to_string();
                            if (ui.add(btn).clicked() || submit) && !trimmed.is_empty() {
                                self.submitted = true;
                                action = OtpAction::Submit(trimmed);
                            }

                            ui.add_space(8.0);
                            if ui
                                .button(
                                    RichText::new("Annuler")
                                        .font(FontId::proportional(13.0))
                                        .color(Color32::from_rgb(160, 160, 160)),
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
