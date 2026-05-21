use std::sync::{mpsc, Arc};
use tokio::sync::Mutex;
use frenchetv_core::{Channel, Config, Operator, OperatorKind, OperatorRegistry, StreamUrl};
use crate::screens::{ChannelListScreen, PlayerScreen, SetupScreen};
use crate::screens::setup::SetupAction;
use crate::screens::channel_list::ChannelListAction;
use crate::screens::player::PlayerAction;

type SharedOperator = Arc<Mutex<Box<dyn Operator>>>;

/// Messages sent from Tokio tasks back to the UI thread.
enum AsyncMsg {
    AuthErr(String),
    /// Authentication + channel fetch both succeeded. Carries the live operator
    /// (with token set) so it can be reused for resolve_stream.
    ChannelsOk { channels: Vec<Channel>, operator: SharedOperator },
    ChannelsErr(String),
    StreamOk { channel: Channel, stream: StreamUrl },
    StreamErr(String),
}

enum Screen {
    Setup(SetupScreen),
    ChannelList(ChannelListScreen),
    Player(PlayerScreen),
}

pub struct App {
    screen: Screen,
    /// Channels loaded after setup; kept for channel switching in the player.
    channels: Vec<Channel>,
    /// The authenticated operator. Holds the session token between calls.
    current_operator: Option<SharedOperator>,
    tx: mpsc::SyncSender<AsyncMsg>,
    rx: mpsc::Receiver<AsyncMsg>,
    rt: tokio::runtime::Runtime,
    egui_ctx: egui::Context,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::sync_channel(16);
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        // Load config but don't crash if it's missing or malformed
        let _ = Config::load();

        Self {
            screen: Screen::Setup(SetupScreen::new()),
            channels: Vec::new(),
            current_operator: None,
            tx,
            rx,
            rt,
            egui_ctx: cc.egui_ctx.clone(),
        }
    }

    /// Spawn: authenticate → fetch_channels → send ChannelsOk (or AuthErr / ChannelsErr).
    /// The operator is kept alive in the SharedOperator so tokens persist.
    fn start_auth(&self, kind: OperatorKind, username: String, password: String) {
        let tx = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        self.rt.spawn(async move {
            let mut op = OperatorRegistry::build(&kind);
            if let Err(e) = op.authenticate(&username, &password).await {
                let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                ctx.request_repaint();
                return;
            }
            match op.fetch_channels().await {
                Ok(channels) => {
                    let shared = Arc::new(Mutex::new(op));
                    let _ = tx.send(AsyncMsg::ChannelsOk { channels, operator: shared });
                }
                Err(e) => {
                    let _ = tx.send(AsyncMsg::ChannelsErr(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Spawn: resolve_stream using the stored (authenticated) operator.
    fn start_resolve_stream(&self, channel: Channel) {
        let tx = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        let op = match &self.current_operator {
            Some(op) => op.clone(),
            None => {
                tracing::error!("resolve_stream called with no operator");
                return;
            }
        };
        self.rt.spawn(async move {
            // Release lock before sending so concurrent resolutions don't block
            let result = {
                let op = op.lock().await;
                op.resolve_stream(&channel).await
            };
            match result {
                Ok(stream) => {
                    let _ = tx.send(AsyncMsg::StreamOk { channel, stream });
                }
                Err(e) => {
                    let _ = tx.send(AsyncMsg::StreamErr(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    fn drain_async_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AsyncMsg::AuthErr(err) => {
                    if let Screen::Setup(s) = &mut self.screen {
                        s.set_error(format!("Connexion échouée : {}", err));
                    }
                }
                AsyncMsg::ChannelsOk { channels, operator } => {
                    self.channels = channels.clone();
                    self.current_operator = Some(operator);
                    self.screen = Screen::ChannelList(ChannelListScreen::new(channels));
                }
                AsyncMsg::ChannelsErr(err) => {
                    if let Screen::Setup(s) = &mut self.screen {
                        s.set_error(format!("Erreur chargement chaînes : {}", err));
                    }
                }
                AsyncMsg::StreamOk { channel, stream } => {
                    self.screen = Screen::Player(PlayerScreen::new(channel, &stream));
                }
                AsyncMsg::StreamErr(err) => {
                    tracing::error!("stream resolution failed: {}", err);
                    self.screen = Screen::ChannelList(ChannelListScreen::new(self.channels.clone()));
                }
            }
            ctx.request_repaint();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_async_messages(ctx);

        match &mut self.screen {
            Screen::Setup(setup) => {
                if let SetupAction::StartAuth { operator, username, password } = setup.show(ctx) {
                    self.start_auth(operator, username, password);
                }
            }
            Screen::ChannelList(list) => {
                if let ChannelListAction::SelectChannel(channel) = list.show(ctx) {
                    self.start_resolve_stream(channel);
                }
            }
            Screen::Player(player) => {
                let channels = self.channels.clone();
                // Extract before show() borrows player mutably
                let current_id = player.channel.id.clone();
                match player.show(ctx) {
                    PlayerAction::Back => {
                        self.screen = Screen::ChannelList(ChannelListScreen::new(channels));
                    }
                    PlayerAction::NextChannel => {
                        if let Some(idx) = channels.iter().position(|c| c.id == current_id) {
                            let next = channels[(idx + 1) % channels.len()].clone();
                            self.start_resolve_stream(next);
                        }
                    }
                    PlayerAction::PrevChannel => {
                        if let Some(idx) = channels.iter().position(|c| c.id == current_id) {
                            if !channels.is_empty() {
                                let prev = if idx == 0 { channels.len() - 1 } else { idx - 1 };
                                self.start_resolve_stream(channels[prev].clone());
                            }
                        }
                    }
                    PlayerAction::None => {}
                }
            }
        }
    }
}
