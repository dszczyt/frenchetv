use crate::player::libmpv::LibMpvPlayer;
#[cfg(unix)]
use crate::player::mpv_ipc::MpvIpcPlayer;
use crate::theme::{color, fade, motion, space, text};
use egui::{Color32, FontId, Key, RichText, Vec2};
use egui_phosphor::regular as icon;
use frenchetv_core::Channel;
use frenchetv_core::StreamUrl;

/// Stable id for animating the info-overlay fade in/out.
fn overlay_anim_id() -> egui::Id {
    egui::Id::new("player_info_overlay")
}

#[derive(PartialEq, Eq)]
enum PlayerState {
    Loading,
    Playing,
}

/// Either backend. See `player::mpv_ipc` for why the second one exists —
/// diagnostic-only, toggled with `FRENCHETV_MPV_SUBPROCESS=1`.
enum Player {
    Embedded(LibMpvPlayer),
    #[cfg(unix)]
    Subprocess(MpvIpcPlayer),
}

impl Player {
    fn play(&mut self, url: &str, auth_header: Option<&str>, extra_headers: &[(String, String)]) {
        match self {
            Self::Embedded(p) => p.play(url, auth_header, extra_headers),
            #[cfg(unix)]
            Self::Subprocess(p) => p.play(url, auth_header, extra_headers),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Embedded(p) => p.stop(),
            #[cfg(unix)]
            Self::Subprocess(p) => p.stop(),
        }
    }

    fn render_frame(
        &mut self,
        ctx: &egui::Context,
        width: u32,
        height: u32,
    ) -> Option<egui::load::SizedTexture> {
        match self {
            Self::Embedded(p) => p.render_frame(ctx, width, height),
            #[cfg(unix)]
            Self::Subprocess(p) => p.render_frame(ctx, width, height),
        }
    }
}

pub struct PlayerScreen {
    pub channel: Channel,
    player: Player,
    state: PlayerState,
    /// Rectangle this player occupied in the guide before being promoted to
    /// fullscreen. Drives the zoom so selecting a channel grows the picture
    /// already on screen instead of cutting to a fresh one.
    zoom_from: Option<egui::Rect>,
    zoom_t: f32,
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
    /// A frame sized for `size`, for painting somewhere other than fullscreen.
    ///
    /// The guide uses this to show live video in the focused row: the same
    /// player, drawn into a small rectangle, so selecting the channel is a
    /// change of destination rather than a new stream.
    pub fn frame_for(
        &mut self,
        ctx: &egui::Context,
        size: egui::Vec2,
    ) -> Option<egui::load::SizedTexture> {
        if self.state != PlayerState::Playing {
            return None;
        }
        self.player
            .render_frame(ctx, size.x.max(1.0) as u32, size.y.max(1.0) as u32)
    }

    /// Where a zoom-to-fullscreen transition starts from, if this player was
    /// promoted out of a guide row.
    pub fn zoom_from(&mut self, rect: egui::Rect) {
        self.zoom_from = Some(rect);
        self.zoom_t = 0.0;
    }

    pub fn channel_id(&self) -> &str {
        &self.channel.id
    }

    pub fn new(channel: Channel, egui_ctx: egui::Context, force_software: bool) -> Self {
        #[cfg(unix)]
        let player = if std::env::var_os("FRENCHETV_MPV_SUBPROCESS").is_some() {
            tracing::info!("player: using subprocess-mpv diagnostic backend");
            Player::Subprocess(MpvIpcPlayer::new())
        } else {
            Player::Embedded(LibMpvPlayer::new(egui_ctx, force_software))
        };
        #[cfg(not(unix))]
        let player = Player::Embedded(LibMpvPlayer::new(egui_ctx, force_software));

        Self {
            channel,
            player,
            state: PlayerState::Loading,
            zoom_from: None,
            zoom_t: 0.0,
            info_visible: false,
            info_hide_timer: 0.0,
            fullscreen: false,
        }
    }

    /// Called when the stream has been resolved — starts mpv playback.
    /// Point this player at a different channel without tearing it down.
    ///
    /// Destroying a player means `mpv_render_context_free` and an mpv handle
    /// shutdown, both of which block until in-flight work finishes — on the UI
    /// thread that is a visible freeze, and doing it once per focus change
    /// while navigating a list is a freeze per row.
    pub fn switch_channel(&mut self, channel: Channel) {
        self.channel = channel;
        self.state = PlayerState::Loading;
    }

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
            let toggle_fs = i.key_pressed(Key::F);
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

        // Fade the overlay in/out instead of snapping it, and keep repainting
        // while the fade itself is still in flight (the block above only
        // repaints while `info_visible` is true, which stops as soon as it
        // flips to false — mid fade-out).
        let overlay_t =
            ctx.animate_bool_with_time(overlay_anim_id(), self.info_visible, motion::NORMAL);
        if (overlay_t - if self.info_visible { 1.0 } else { 0.0 }).abs() > 0.001 {
            ctx.request_repaint();
        }

        let mut action = action;

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
                                ui.add(egui::Spinner::new().size(40.0).color(color::TEXT));
                                ui.add_space(space::MD);
                                ui.label(
                                    RichText::new(format!("Chargement de {}…", self.channel.name))
                                        .font(FontId::proportional(text::BODY))
                                        .color(color::TEXT_MUTED),
                                );
                            });
                        });
                        ctx.request_repaint();
                    }
                    PlayerState::Playing => match self.player.render_frame(ctx, w, h) {
                        Some(sized_texture) => {
                            // Promoted out of a guide row: grow from where the
                            // picture already was rather than cutting to a new
                            // one. Same stream throughout — only the
                            // destination rectangle changes.
                            if let Some(from) = self.zoom_from {
                                const ZOOM_SECS: f32 = 0.28;
                                self.zoom_t = (self.zoom_t
                                    + ctx.input(|i| i.unstable_dt) / ZOOM_SECS)
                                    .min(1.0);
                                // Ease-out: fast at first, settling into place.
                                let t = 1.0 - (1.0 - self.zoom_t).powi(3);
                                let full = ui.max_rect();
                                let rect = egui::Rect::from_min_max(
                                    from.min + (full.min - from.min) * t,
                                    from.max + (full.max - from.max) * t,
                                );
                                ui.painter().image(
                                    sized_texture.id,
                                    rect,
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                                if self.zoom_t >= 1.0 {
                                    self.zoom_from = None;
                                }
                                ctx.request_repaint();
                            } else {
                                ui.centered_and_justified(|ui| {
                                    ui.add(
                                        egui::Image::new(sized_texture)
                                            .maintain_aspect_ratio(true)
                                            .fit_to_exact_size(available),
                                    );
                                });
                            }
                        }
                        None => {
                            ui.centered_and_justified(|ui| {
                                ui.add(egui::Spinner::new().size(40.0).color(color::TEXT));
                            });
                            ctx.request_repaint();
                        }
                    },
                }

                let rect = ui.max_rect();

                // Info overlay (channel name + key hints), faded by `overlay_t`.
                if overlay_t > 0.004 {
                    let overlay_height = 80.0;
                    let overlay_rect = egui::Rect::from_min_size(
                        egui::pos2(rect.min.x, rect.max.y - overlay_height),
                        Vec2::new(rect.width(), overlay_height),
                    );
                    ui.painter()
                        .rect_filled(overlay_rect, 0.0, fade(color::SCRIM, overlay_t));
                    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(overlay_rect), |ui| {
                        ui.add_space(space::SM + space::XS);
                        ui.horizontal(|ui| {
                            ui.add_space(space::MD);
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new(&self.channel.name)
                                        .font(FontId::proportional(text::TITLE))
                                        .color(fade(color::TEXT, overlay_t)),
                                );
                                ui.label(
                                    RichText::new("← → Changer  ↵ Info  F Plein écran  Esc Retour")
                                        .font(FontId::proportional(text::LABEL - 2.0))
                                        .color(fade(color::TEXT_MUTED, overlay_t)),
                                );
                            });
                        });
                    });
                }

                // Escape route, shown/hidden in lockstep with the info overlay
                // below. Keyboard Esc/Backspace still work when it's hidden.
                if overlay_t > 0.004 {
                    let back_rect = egui::Rect::from_min_size(
                        rect.min + Vec2::splat(space::MD),
                        Vec2::splat(40.0),
                    );
                    let back_resp = ui.put(
                        back_rect,
                        egui::Button::new(
                            RichText::new(icon::ARROW_LEFT)
                                .size(18.0)
                                .color(fade(color::TEXT, overlay_t)),
                        )
                        .fill(fade(color::SCRIM, overlay_t))
                        .rounding(20.0),
                    );
                    if back_resp.clicked() {
                        action = PlayerAction::Back;
                    }
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
