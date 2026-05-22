use std::sync::{mpsc, Arc};
use tokio::sync::Mutex;
use frenchetv_core::{AuthPhase, Channel, Config, Operator, OperatorKind, OperatorRegistry, StreamUrl};
use frenchetv_core::session as keyring_session;
use crate::screens::{ChannelListScreen, PlayerScreen, PushWaitScreen, SetupScreen};
use crate::screens::setup::SetupAction;
use crate::screens::channel_list::ChannelListAction;
use crate::screens::player::PlayerAction;

type SharedOperator = Arc<Mutex<Box<dyn Operator>>>;

/// Messages sent from Tokio tasks back to the UI thread.
enum AsyncMsg {
    AuthErr(String),
    /// Operator requires mobile push approval — show the push-wait screen.
    PushAuthPending,
    /// Authentication + channel fetch both succeeded. Carries the live operator
    /// (with token set) so it can be reused for resolve_stream.
    ChannelsOk {
        channels: Vec<Channel>,
        operator: SharedOperator,
        /// Session token to persist (e.g. wassup cookie).
        session_token: Option<String>,
        /// config_str() key ("orange", "bouygues") — used for keyring + config.
        kind_str: String,
        username: String,
    },
    ChannelsErr(String),
    StreamOk { channel: Channel, stream: StreamUrl },
    StreamErr(String),
}

enum Screen {
    Setup(SetupScreen),
    PushWait(PushWaitScreen),
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

        let config = Config::load().unwrap_or_default();

        let mut app = Self {
            screen: Screen::Setup(SetupScreen::new()),
            channels: Vec::new(),
            current_operator: None,
            tx,
            rx,
            rt,
            egui_ctx: cc.egui_ctx.clone(),
        };

        // Attempt silent session restore if we have a saved operator + username.
        let kind_str = config.operator.kind.clone();
        let username  = config.operator.username.clone();
        if !kind_str.is_empty() && !username.is_empty() {
            if let Some(kind) = OperatorKind::from_config_str(&kind_str) {
                if let Some(token) = keyring_session::load_session(&kind_str, &username) {
                    app.start_restore_session(kind, kind_str, username, token);
                }
            }
        }

        app
    }

    /// Spawn: restore session from stored token.
    /// Success → ChannelsOk (skip auth screen).
    /// Failure → clears stale keyring entry, Setup screen stays (no error shown).
    fn start_restore_session(
        &self,
        kind: OperatorKind,
        kind_str: String,
        username: String,
        token: String,
    ) {
        let tx  = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        self.rt.spawn(async move {
            let mut op = OperatorRegistry::build(&kind);
            if let Err(e) = op.restore_session(&token).await {
                tracing::info!("Session restore failed ({}); showing setup screen", e);
                keyring_session::clear_session(&kind_str, &username);
                // Don't send AuthErr — setup screen is already visible, no message needed.
                ctx.request_repaint();
                return;
            }
            match op.fetch_channels().await {
                Ok(channels) => {
                    let session_token = op.session_token().map(str::to_string);
                    let shared = Arc::new(Mutex::new(op));
                    let _ = tx.send(AsyncMsg::ChannelsOk {
                        channels,
                        operator: shared,
                        session_token,
                        kind_str,
                        username,
                    });
                }
                Err(e) => {
                    let _ = tx.send(AsyncMsg::ChannelsErr(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Spawn: authenticate → fetch_channels → send ChannelsOk (or AuthErr / ChannelsErr).
    /// For operators with phased auth (Orange push), sends PushAuthPending first.
    fn start_auth(&self, kind: OperatorKind, username: String, password: String) {
        let tx      = self.tx.clone();
        let ctx     = self.egui_ctx.clone();
        let kind_str = kind.config_str().to_string();
        self.rt.spawn(async move {
            let mut op = OperatorRegistry::build(&kind);

            let auth_ok = if op.uses_phased_auth() {
                match op.begin_auth(&username).await {
                    Err(e) => {
                        let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                        ctx.request_repaint();
                        return;
                    }
                    Ok(AuthPhase::Password) => {
                        match op.complete_auth_password(&password).await {
                            Ok(()) => true,
                            Err(e) => {
                                let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                                ctx.request_repaint();
                                return;
                            }
                        }
                    }
                    Ok(AuthPhase::Push) => {
                        let _ = tx.send(AsyncMsg::PushAuthPending);
                        ctx.request_repaint();
                        match op.wait_for_push_auth(&password).await {
                            Ok(()) => true,
                            Err(e) => {
                                let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                                ctx.request_repaint();
                                return;
                            }
                        }
                    }
                }
            } else {
                match op.authenticate(&username, &password).await {
                    Ok(()) => true,
                    Err(e) => {
                        let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                        ctx.request_repaint();
                        return;
                    }
                }
            };

            if !auth_ok {
                return;
            }

            match op.fetch_channels().await {
                Ok(channels) => {
                    let session_token = op.session_token().map(str::to_string);
                    let shared = Arc::new(Mutex::new(op));
                    let _ = tx.send(AsyncMsg::ChannelsOk {
                        channels,
                        operator: shared,
                        session_token,
                        kind_str,
                        username,
                    });
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
        let tx  = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        let op  = match &self.current_operator {
            Some(op) => op.clone(),
            None => {
                tracing::error!("resolve_stream called with no operator");
                return;
            }
        };
        self.rt.spawn(async move {
            let result = {
                let op = op.lock().await;
                op.resolve_stream(&channel).await
            };
            match result {
                Ok(stream) => { let _ = tx.send(AsyncMsg::StreamOk { channel, stream }); }
                Err(e)     => { let _ = tx.send(AsyncMsg::StreamErr(e.to_string()));     }
            }
            ctx.request_repaint();
        });
    }

    fn drain_async_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AsyncMsg::AuthErr(err) => {
                    match &mut self.screen {
                        Screen::Setup(s) => {
                            s.set_error(format!("Connexion échouée : {}", err));
                        }
                        Screen::PushWait(_) => {
                            let mut s = SetupScreen::new();
                            s.set_error(format!("Connexion échouée : {}", err));
                            self.screen = Screen::Setup(s);
                        }
                        _ => {}
                    }
                }
                AsyncMsg::PushAuthPending => {
                    self.screen = Screen::PushWait(PushWaitScreen::new());
                }
                AsyncMsg::ChannelsOk { channels, operator, session_token, kind_str, username } => {
                    // Persist session token + operator/username for next launch.
                    if let Some(ref token) = session_token {
                        keyring_session::save_session(&kind_str, &username, token);
                        let mut cfg = Config::load().unwrap_or_default();
                        cfg.operator.kind     = kind_str.clone();
                        cfg.operator.username = username.clone();
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to save config: {}", e);
                        }
                        tracing::info!("Session saved for {}:{}", kind_str, username);
                    }
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
            Screen::PushWait(pw) => {
                pw.show(ctx);
            }
            Screen::ChannelList(list) => {
                if let ChannelListAction::SelectChannel(channel) = list.show(ctx) {
                    self.start_resolve_stream(channel);
                }
            }
            Screen::Player(player) => {
                let channels    = self.channels.clone();
                let current_id  = player.channel.id.clone();
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
