use crate::screens::{ChannelListScreen, PlayerScreen, SetupScreen};

enum Screen {
    Setup(SetupScreen),
    ChannelList(ChannelListScreen),
    Player(PlayerScreen),
}

pub struct App {
    screen: Screen,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            screen: Screen::Setup(SetupScreen::new()),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Dispatch to the active screen.
        // Return values (actions) are ignored here — full async wiring is Task 13.
        match &mut self.screen {
            Screen::Setup(setup) => {
                setup.show(ctx);
            }
            Screen::ChannelList(list) => {
                list.show(ctx);
            }
            Screen::Player(player) => {
                player.show(ctx);
            }
        }
    }
}
