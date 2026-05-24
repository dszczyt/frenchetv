use egui::{Color32, FontId, Key, RichText};
use frenchetv_core::Channel;

pub struct PlayerScreen {
    pub channel: Channel,
}

#[derive(Debug)]
pub enum PlayerAction {
    None,
    Back,
    NextChannel,
    PrevChannel,
}

impl PlayerScreen {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> PlayerAction {
        let (back, next, prev) = ctx.input(|i| {
            (
                i.key_pressed(Key::Escape) || i.key_pressed(Key::Backspace),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowLeft),
            )
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::BLACK))
            .show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(&self.channel.name)
                                .font(FontId::proportional(40.0))
                                .color(Color32::WHITE),
                        );
                        ui.add_space(16.0);
                        ui.label(
                            RichText::new("Lecture en cours…")
                                .font(FontId::proportional(24.0))
                                .color(Color32::from_rgb(160, 160, 160)),
                        );
                        ui.add_space(32.0);
                        ui.label(
                            RichText::new("← Chaîne précédente   → Chaîne suivante   ↩ Retour")
                                .font(FontId::proportional(18.0))
                                .color(Color32::from_rgb(120, 120, 120)),
                        );
                    });
                });
            });

        if back {
            PlayerAction::Back
        } else if next {
            PlayerAction::NextChannel
        } else if prev {
            PlayerAction::PrevChannel
        } else {
            PlayerAction::None
        }
    }
}
