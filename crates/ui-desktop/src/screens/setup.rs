pub struct SetupScreen;

pub enum SetupAction {
    None,
}

impl SetupScreen {
    pub fn new() -> Self {
        Self
    }

    pub fn show(&mut self, _ctx: &egui::Context) -> SetupAction {
        SetupAction::None
    }

    pub fn set_error(&mut self, _msg: impl Into<String>) {}

    pub fn set_loading(&mut self, _loading: bool) {}
}

impl Default for SetupScreen {
    fn default() -> Self {
        Self::new()
    }
}
