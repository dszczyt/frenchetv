use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;
use frenchetv_core::{AuthPhase, Channel, Config, Operator, OperatorKind, OperatorRegistry, StreamUrl};
use frenchetv_core::session as keyring_session;
use crate::screens::{ChannelListScreen, PlayerScreen, PushWaitScreen, SetupScreen};
use crate::screens::setup::SetupAction;
use crate::screens::channel_list::ChannelListAction;
use crate::screens::player::PlayerAction;

type SharedOperator = Arc<TokioMutex<Box<dyn Operator>>>;
/// Shared logo cache: logo_url → decoded egui texture.
pub type LogoCache = Arc<Mutex<HashMap<String, egui::TextureHandle>>>;

/// Messages sent from Tokio tasks back to the UI thread.
enum AsyncMsg {
    AuthErr(String),
    /// Operator requires mobile push approval — show the push-wait screen.
    PushAuthPending,
    /// Authentication + channel fetch both succeeded.
    ChannelsOk {
        channels: Vec<Channel>,
        operator: SharedOperator,
        session_token: Option<String>,
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
    channels: Vec<Channel>,
    current_operator: Option<SharedOperator>,
    /// Decoded channel logos, populated asynchronously after channel list loads.
    logos: LogoCache,
    tx: mpsc::SyncSender<AsyncMsg>,
    rx: mpsc::Receiver<AsyncMsg>,
    rt: tokio::runtime::Runtime,
    egui_ctx: egui::Context,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::sync_channel(16);
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let logos: LogoCache = Arc::new(Mutex::new(HashMap::new()));
        let config = Config::load().unwrap_or_default();

        let app = Self {
            screen: Screen::Setup(SetupScreen::new()),
            channels: Vec::new(),
            current_operator: None,
            logos,
            tx,
            rx,
            rt,
            egui_ctx: cc.egui_ctx.clone(),
        };

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

    /// Spawn concurrent logo fetches (max 20 in-flight) for all channels that
    /// have a logo_url. Decoded textures are inserted into the shared LogoCache;
    /// each insertion triggers a repaint so the UI updates incrementally.
    fn start_fetch_logos(&self, channels: Vec<Channel>) {
        let logos = Arc::clone(&self.logos);
        let ctx   = self.egui_ctx.clone();
        self.rt.spawn(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default();

            let sem = Arc::new(tokio::sync::Semaphore::new(20));
            let mut set = tokio::task::JoinSet::new();

            // Deduplicate URLs across channels.
            let mut seen = std::collections::HashSet::new();
            for url in channels.into_iter().filter_map(|c| c.logo_url) {
                if seen.insert(url.clone()) {
                    let client = client.clone();
                    let logos  = Arc::clone(&logos);
                    let ctx    = ctx.clone();
                    let sem    = Arc::clone(&sem);
                    set.spawn(async move {
                        let _permit = sem.acquire().await.ok()?;
                        let bytes = client.get(&url).send().await.ok()?.bytes().await.ok()?;
                        let img   = image::load_from_memory(&bytes).ok()?;
                        let rgba  = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        let pixels: Vec<egui::Color32> = rgba
                            .pixels()
                            .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
                            .collect();
                        let texture = ctx.load_texture(
                            url.as_str(),
                            egui::ColorImage { size: [w as usize, h as usize], pixels },
                            egui::TextureOptions::LINEAR,
                        );
                        logos.lock().ok()?.insert(url, texture);
                        ctx.request_repaint();
                        Some(())
                    });
                }
            }
            while set.join_next().await.is_some() {}
        });
    }

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
                ctx.request_repaint();
                return;
            }
            match op.fetch_channels().await {
                Ok(channels) => {
                    let session_token = op.session_token().map(str::to_string);
                    let shared = Arc::new(TokioMutex::new(op));
                    let _ = tx.send(AsyncMsg::ChannelsOk {
                        channels, operator: shared, session_token, kind_str, username,
                    });
                }
                Err(e) => { let _ = tx.send(AsyncMsg::ChannelsErr(e.to_string())); }
            }
            ctx.request_repaint();
        });
    }

    fn start_auth(&self, kind: OperatorKind, username: String, password: String) {
        let tx       = self.tx.clone();
        let ctx      = self.egui_ctx.clone();
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
                    Ok(AuthPhase::Password) => match op.complete_auth_password(&password).await {
                        Ok(()) => true,
                        Err(e) => {
                            let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                            ctx.request_repaint();
                            return;
                        }
                    },
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

            if !auth_ok { return; }

            match op.fetch_channels().await {
                Ok(channels) => {
                    let session_token = op.session_token().map(str::to_string);
                    let shared = Arc::new(TokioMutex::new(op));
                    let _ = tx.send(AsyncMsg::ChannelsOk {
                        channels, operator: shared, session_token, kind_str, username,
                    });
                }
                Err(e) => { let _ = tx.send(AsyncMsg::ChannelsErr(e.to_string())); }
            }
            ctx.request_repaint();
        });
    }

    fn start_resolve_stream(&self, channel: Channel) {
        let tx  = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        let op  = match &self.current_operator {
            Some(op) => op.clone(),
            None => { tracing::error!("resolve_stream called with no operator"); return; }
        };
        self.rt.spawn(async move {
            let result = { let op = op.lock().await; op.resolve_stream(&channel).await };
            match result {
                Ok(stream) => { let _ = tx.send(AsyncMsg::StreamOk { channel, stream }); }
                Err(e)     => { let _ = tx.send(AsyncMsg::StreamErr(e.to_string()));     }
            }
            ctx.request_repaint();
        });
    }

    fn make_channel_list(&self, channels: Vec<Channel>) -> ChannelListScreen {
        ChannelListScreen::new(channels, Arc::clone(&self.logos))
    }

    fn drain_async_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AsyncMsg::AuthErr(err) => match &mut self.screen {
                    Screen::Setup(s) => { s.set_error(format!("Connexion échouée : {}", err)); }
                    Screen::PushWait(_) => {
                        let mut s = SetupScreen::new();
                        s.set_error(format!("Connexion échouée : {}", err));
                        self.screen = Screen::Setup(s);
                    }
                    _ => {}
                },
                AsyncMsg::PushAuthPending => {
                    self.screen = Screen::PushWait(PushWaitScreen::new());
                }
                AsyncMsg::ChannelsOk { channels, operator, session_token, kind_str, username } => {
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
                    self.start_fetch_logos(channels.clone());
                    self.channels = channels.clone();
                    self.current_operator = Some(operator);
                    self.screen = Screen::ChannelList(self.make_channel_list(channels));
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
                    let channels = self.channels.clone();
                    self.screen = Screen::ChannelList(self.make_channel_list(channels));
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
            Screen::PushWait(pw) => { pw.show(ctx); }
            Screen::ChannelList(list) => {
                if let ChannelListAction::SelectChannel(channel) = list.show(ctx) {
                    self.start_resolve_stream(channel);
                }
            }
            Screen::Player(player) => {
                let channels   = self.channels.clone();
                let current_id = player.channel.id.clone();
                match player.show(ctx) {
                    PlayerAction::Back => {
                        self.screen = Screen::ChannelList(self.make_channel_list(channels));
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
