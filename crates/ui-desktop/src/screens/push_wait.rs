use egui::{Color32, FontId, RichText, Vec2};

/// Displayed while we wait for the user to approve the Orange mobile push notification.
/// The background auth task keeps polling; this screen just shows a status message.
pub struct PushWaitScreen {
    elapsed: f32,
}

impl PushWaitScreen {
    pub fn new() -> Self {
        Self { elapsed: 0.0 }
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let dt = ctx.input(|i| i.unstable_dt);
        self.elapsed += dt;

        // Animate a simple dot-dot-dot indicator (cycles every 1.5 s).
        let dots = match (self.elapsed % 1.5) as u32 {
            0 => ".",
            1 => "..",
            _ => "...",
        };

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::from_rgb(13, 15, 20)))
            .show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() / 3.0);

                        // Orange logo-colour spinner text
                        ui.label(
                            RichText::new("📱")
                                .font(FontId::proportional(56.0)),
                        );

                        ui.add_space(24.0);

                        ui.label(
                            RichText::new("Approbation mobile requise")
                                .font(FontId::proportional(22.0))
                                .color(Color32::WHITE),
                        );

                        ui.add_space(12.0);

                        ui.label(
                            RichText::new(
                                "Une notification a été envoyée sur votre téléphone.\n\
                                 Appuyez sur la notification Orange pour approuver,\n\
                                 ou ouvrez l'appli Orange et cherchez\n\
                                 une demande de connexion en attente.",
                            )
                            .font(FontId::proportional(15.0))
                            .color(Color32::from_rgb(160, 160, 160)),
                        );

                        ui.add_space(32.0);

                        ui.label(
                            RichText::new(format!("En attente{}", dots))
                                .font(FontId::proportional(14.0))
                                .color(Color32::from_rgb(255, 120, 0)),
                        );

                        // Timeout hint (shown after 10 s)
                        if self.elapsed > 10.0 {
                            ui.add_space(16.0);
                            let remaining = (90.0 - self.elapsed).max(0.0) as u32;
                            ui.label(
                                RichText::new(format!("Expiration dans {}s", remaining))
                                    .font(FontId::proportional(12.0))
                                    .color(Color32::from_rgb(100, 100, 100)),
                            );
                        }

                        // Progress bar
                        ui.add_space(24.0);
                        let progress = (self.elapsed / 90.0).min(1.0);
                        let bar_width = 260.0_f32.min(ui.available_width() - 32.0);
                        let (rect, _) = ui.allocate_exact_size(
                            Vec2::new(bar_width, 4.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(
                            rect,
                            2.0,
                            Color32::from_rgb(40, 40, 50),
                        );
                        let fill_rect = egui::Rect::from_min_size(
                            rect.min,
                            Vec2::new(rect.width() * progress, rect.height()),
                        );
                        ui.painter().rect_filled(
                            fill_rect,
                            2.0,
                            Color32::from_rgb(255, 120, 0),
                        );
                    });
                });
            });

        // Keep repainting so the dots and timer animate.
        ctx.request_repaint();
    }
}

impl Default for PushWaitScreen {
    fn default() -> Self {
        Self::new()
    }
}
