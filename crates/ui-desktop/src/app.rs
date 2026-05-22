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
        /// Session token to persist (wassup / equivalent).
        session_token: Option<String>,
        /// Operator name and username for keyring key.
        operator_name: String,
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
        let initial_screen = Screen::Setup(SetupScreen::new());

        let mut app = Self {
            screen: initial_screen,
            channels: Vec::new(),
            current_operator: None,
            tx,
            rx,
            rt,
            egui_ctx: cc.egui_ctx.clone(),
        };

        // Attempt silent session restore if we have a saved operator + username.
        let op_kind = &config.operator.kind;
        let username = &config.operator.username;
        if !op_kind.is_empty() && !username.is_empty() {
            if let Some(kind) = Self::parse_operator_kind(op_kind) {
                if let Some(token) = keyring_session::load_session(op_kind, username) {
                    app.start_restore_session(kind, username.clone(), token);
                }
            }
        }

        app
    }

    /// Map config string → OperatorKind.
    fn parse_operator_kind(s: &str) -> Option<OperatorKind> {
        match s {
            "orange"   => Some(OperatorKind::Orange),
            "bouygues" => Some(OperatorKind::Bouygues),
            _ => None,
        }
    }

    /// Spawn: try to restore session from stored token.
    /// On success → ChannelsOk (skips auth screen).
    /// On failure → clears keyring, shows setup screen (via AuthErr).
    fn start_restore_session(&self, kind: OperatorKind, username: String, token: String) {
        let tx = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        self.rt.spawn(async move {
            let mut op = OperatorRegistry::build(&kind);
            let op_name = op.name().to_string();
            if let Err(e) = op.restore_session(&token).await {
                tracing::info!("Session restore failed ({}); will re-auth", e);
                // Clear stale token; user will see setup screen.
                keyring_session::clear_session(&op_name, &username);
                let _ = tx.send(AsyncMsg::AuthErr(
                    format!("Session expirée, veuillez vous reconnecter.")
                ));
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
                        operator_name: op_name,
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
    /// For operators that use phased auth (e.g. Orange push notification), sends
    /// PushAuthPending first so the UI can show the wait screen, then continues
    /// polling in the same task.
    fn start_auth(&self, kind: OperatorKind, username: String, password: String) {
        let tx = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        self.rt.spawn(async move {
            let mut op = OperatorRegistry::build(&kind);
            let op_name = op.name().to_string();

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
                        // Signal the UI to show the push-wait screen, then keep polling.
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
                        operator_name: op_name,
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
                    // Show error on setup or push-wait screen; go back to setup if needed.
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
                AsyncMsg::ChannelsOk { channels, operator, session_token, operator_name, username } => {
                    // Persist session token + operator config for next launch.
                    if let Some(ref token) = session_token {
                        keyring_session::save_session(&operator_name, &username, token);
                        // Save operator kind + username to config (not the password).
                        let kind_str = match operator_name.as_str() {
                            "Orange TV"    => "orange",
                            "Bouygues Bbox" => "bouygues",
                            other           => other,
                        };
                        let mut cfg = Config::load().unwrap_or_default();
                        cfg.operator.kind     = kind_str.to_string();
                        cfg.operator.username = username.clone();
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to save config: {}", e);
                        }
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
