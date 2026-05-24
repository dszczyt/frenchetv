use egui::{Color32, FontId, Key, RichText, Vec2};
use frenchetv_core::Channel;
use crate::player::libmpv::LibMpvPlayer;
use frenchetv_core::StreamUrl;

enum PlayerState {
    Loading,
    Playing,
}

pub struct PlayerScreen {
    pub channel: Channel,
    player: LibMpvPlayer,
    state: PlayerState,
    info_visible: bool,
    info_hide_timer: f32,
    fullscreen: bool,
}

#[derive(Debug)]
pub enum PlayerAction {
    None,
    Back,
    NextChannel,
    PrevChannel,
}

impl PlayerScreen {
    /// Create a loading player screen.
    ///
    /// `egui_ctx` is passed to `LibMpvPlayer` so mpv's update callback can
    /// wake the egui frame loop when a new frame is ready.
    /// `force_software` skips GL renderer probe and always uses software path.
    pub fn new(channel: Channel, egui_ctx: egui::Context, force_software: bool) -> Self {
        Self {
            channel,
            player: LibMpvPlayer::new(egui_ctx, force_software),
            state: PlayerState::Loading,
            info_visible: false,
            info_hide_timer: 0.0,
            fullscreen: false,
        }
    }

    /// Called when the stream has been resolved — starts mpv playback.
    pub fn start_playing(&mut self, stream: &StreamUrl) {
        self.player.play(
            stream.url.as_str(),
            stream.auth_header.as_deref(),
            &stream.headers,
        );
        self.state = PlayerState::Playing;
        self.info_visible = true;
        self.info_hide_timer = 3.0;
    }

    pub fn show(&mut self, ctx: &egui::Context) -> PlayerAction {
        let (dt, action, toggle_info, toggle_fs) = ctx.input(|i| {
            let dt = i.unstable_dt;
            let action = if i.key_pressed(Key::Escape) || i.key_pressed(Key::Backspace) {
                PlayerAction::Back
            } else if i.key_pressed(Key::ArrowRight) {
                PlayerAction::NextChannel
            } else if i.key_pressed(Key::ArrowLeft) {
                PlayerAction::PrevChannel
            } else {
                PlayerAction::None
            };
            let toggle_info = i.key_pressed(Key::Enter);
            let toggle_fs   = i.key_pressed(Key::F);
            (dt, action, toggle_info, toggle_fs)
        });

        if toggle_fs {
            self.fullscreen = !self.fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }

        if toggle_info {
            self.info_visible = !self.info_visible;
            self.info_hide_timer = 3.0;
        }

        if self.info_visible {
            self.info_hide_timer -= dt;
            if self.info_hide_timer <= 0.0 {
                self.info_visible = false;
            }
            ctx.request_repaint();
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::BLACK))
            .show(ctx, |ui| {
                let available = ui.available_size();
                let w = available.x as u32;
                let h = available.y as u32;

                match self.state {
                    PlayerState::Loading => {
                        ui.centered_and_justified(|ui| {
                            ui.vertical_centered(|ui| {
                                ui.add_space(available.y / 2.0 - 32.0);
                                ui.add(egui::Spinner::new().size(40.0).color(Color32::WHITE));
                                ui.add_space(16.0);
                                ui.label(
                                    RichText::new(format!("Chargement de {}…", self.channel.name))
                                        .font(FontId::proportional(16.0))
                                        .color(Color32::from_rgb(160, 160, 160)),
                                );
                            });
                        });
                        ctx.request_repaint();
                    }
                    PlayerState::Playing => {
                        match self.player.render_frame(ctx, w, h) {
                            Some(sized_texture) => {
                                ui.add(
                                    egui::Image::new(sized_texture)
                                        .fit_to_exact_size(available),
                                );
                            }
                            None => {
                                ui.centered_and_justified(|ui| {
                                    ui.add(egui::Spinner::new().size(40.0).color(Color32::WHITE));
                                });
                                ctx.request_repaint();
                            }
                        }
                    }
                }

                // Info overlay (channel name + key hints).
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
                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(overlay_rect), |ui| {
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
                                    RichText::new("← → Changer  ↵ Info  F Plein écran  Esc Retour")
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
        self.player.stop();
    }
}
