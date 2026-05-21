use egui::{Color32, FontId, Key, RichText, Vec2};
use frenchetv_core::Channel;
use crate::player::mpv::MpvPlayer;
use frenchetv_core::StreamUrl;

pub struct PlayerScreen {
    channel: Channel,
    player: MpvPlayer,
    info_visible: bool,
    info_hide_timer: f32,  // seconds remaining to show info overlay
}

#[derive(Debug)]
pub enum PlayerAction {
    None,
    Back,
    NextChannel,
    PrevChannel,
}

impl PlayerScreen {
    /// `stream` is the resolved stream to play.
    pub fn new(channel: Channel, stream: &StreamUrl) -> Self {
        let mut player = MpvPlayer::new();
        player.play(
            stream.url.as_str(),
            stream.auth_header.as_deref(),
        );
        Self {
            channel,
            player,
            info_visible: true,
            info_hide_timer: 3.0,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> PlayerAction {
        let mut action = PlayerAction::None;

        // Tick the info overlay hide timer using egui's dt
        let dt = ctx.input(|i| i.unstable_dt);
        if self.info_visible {
            self.info_hide_timer -= dt;
            if self.info_hide_timer <= 0.0 {
                self.info_visible = false;
            }
            // Keep requesting repaints while timer is running
            ctx.request_repaint();
        }

        // Keyboard input
        ctx.input(|i| {
            if i.key_pressed(Key::Escape) || i.key_pressed(Key::Backspace) {
                action = PlayerAction::Back;
            }
            if i.key_pressed(Key::Enter) {
                self.info_visible = !self.info_visible;
                self.info_hide_timer = 3.0;
            }
            if i.key_pressed(Key::ArrowRight) {
                action = PlayerAction::NextChannel;
            }
            if i.key_pressed(Key::ArrowLeft) {
                action = PlayerAction::PrevChannel;
            }
        });

        // Black background (mpv renders in its own window)
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::BLACK))
            .show(ctx, |ui| {
                // "mpv plays in a separate window" note — shown always in v0.1
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new("▶  Lecture en cours dans la fenêtre mpv")
                            .font(FontId::proportional(18.0))
                            .color(Color32::from_rgb(80, 80, 80)),
                    );
                });

                // Info overlay
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

                    ui.allocate_ui_at_rect(overlay_rect, |ui| {
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
        let _ = self.player.stop();
    }
}
