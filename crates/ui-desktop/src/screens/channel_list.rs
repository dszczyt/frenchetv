use frenchetv_core::Channel;

pub struct ChannelListScreen {
    channels: Vec<Channel>,
}

pub enum ChannelListAction {
    None,
    SelectChannel(Channel),
}

impl ChannelListScreen {
    pub fn new(channels: Vec<Channel>) -> Self {
        Self { channels }
    }

    pub fn show(&mut self, _ctx: &egui::Context) -> ChannelListAction {
        ChannelListAction::None
    }
}
