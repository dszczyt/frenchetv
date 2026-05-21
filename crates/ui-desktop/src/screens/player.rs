use frenchetv_core::{Channel, StreamUrl};

use crate::player::mpv::MpvPlayer;

pub struct PlayerScreen {
    pub channel: Channel,
    player: MpvPlayer,
}

pub enum PlayerAction {
    None,
    Back,
    NextChannel,
    PrevChannel,
}

impl PlayerScreen {
    pub fn new(channel: Channel, stream: &StreamUrl) -> Self {
        let mut player = MpvPlayer::new();
        player.play(stream.url.as_str(), stream.auth_header.as_deref());
        Self { channel, player }
    }

    pub fn show(&mut self, _ctx: &egui::Context) -> PlayerAction {
        PlayerAction::None
    }
}
