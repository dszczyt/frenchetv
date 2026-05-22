use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;
use frenchetv_core::{AuthPhase, Channel, Config, Operator, OperatorError, OperatorKind, OperatorRegistry, StreamUrl};
use frenchetv_core::session as keyring_session;
use crate::drm::DrmProxy;
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
    StreamOk { stream: StreamUrl },
    StreamErr(String),
    /// DRM proxy started; pass proxy_mpd_url to mpv instead of the real stream URL.
    DrmProxyReady { proxy_mpd_url: String, proxy: Box<DrmProxy> },
    DrmProxyErr(String),
    /// A 401/403 was received after login — session is invalid, must re-authenticate.
    SessionExpired,
    /// Background Widevine CDM download finished.
    WidevineDone,
    /// Background Widevine CDM download failed.
    WidevineErr(String),
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
    /// (kind_str, username) of the active session — used to clear credentials on expiry.
    current_session: Option<(String, String)>,
    /// Decoded channel logos, populated asynchronously after channel list loads.
    logos: LogoCache,
    tx: mpsc::SyncSender<AsyncMsg>,
    rx: mpsc::Receiver<AsyncMsg>,
    rt: tokio::runtime::Runtime,
    egui_ctx: egui::Context,
    /// Keep the DRM proxy alive while playback is active (Drop aborts the listener).
    _drm_proxy: Option<Box<DrmProxy>>,
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
            current_session: None,
            logos,
            tx,
            rx,
            rt,
            egui_ctx: cc.egui_ctx.clone(),
            _drm_proxy: None,
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

        // Download Widevine CDM in the background if not already present.
        if !crate::widevine::is_installed() {
            app.start_download_widevine();
        } else {
            tracing::info!("widevine: CDM already present at {:?}", crate::widevine::cdm_path());
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

    fn start_download_widevine(&self) {
        let tx  = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        self.rt.spawn(async move {
            tracing::info!("widevine: starting CDM download");
            match crate::widevine::install().await {
                Ok(()) => {
                    tracing::info!("widevine: CDM installed successfully");
                    let _ = tx.send(AsyncMsg::WidevineDone);
                }
                Err(e) => {
                    tracing::warn!("widevine: CDM download failed: {}", e);
                    let _ = tx.send(AsyncMsg::WidevineErr(e.to_string()));
                }
            }
            ctx.request_repaint();
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
                Err(OperatorError::InvalidCredentials) => {
                    let _ = tx.send(AsyncMsg::SessionExpired);
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
                Err(OperatorError::InvalidCredentials) => {
                    let _ = tx.send(AsyncMsg::SessionExpired);
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
        tracing::debug!("resolve_stream: starting for channel '{}' (id={})", channel.name, channel.id);
        self.rt.spawn(async move {
            let result = { let op = op.lock().await; op.resolve_stream(&channel).await };
            match result {
                Ok(stream) => {
                    tracing::debug!("resolve_stream: ok → {}", stream.url);
                    let _ = tx.send(AsyncMsg::StreamOk { stream });
                }
                Err(OperatorError::InvalidCredentials) => {
                    tracing::warn!("resolve_stream: 401/403 → SessionExpired");
                    let _ = tx.send(AsyncMsg::SessionExpired);
                }
                Err(e) => {
                    // Print full source chain to expose the root transport error.
                    let mut msg = format!("{}", e);
                    let mut src: Option<&dyn std::error::Error> = std::error::Error::source(&e);
                    while let Some(cause) = src {
                        msg.push_str(&format!(" → {}", cause));
                        src = cause.source();
                    }
                    tracing::error!("resolve_stream error: {}", msg);
                    let _ = tx.send(AsyncMsg::StreamErr(msg));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Start the Widevine DRM pipeline for a protected stream:
    /// 1. Open CDM, initialize it.
    /// 2. Fetch the DASH MPD to get the PSSH (or use `protection.pssh` if available).
    /// 3. Do the license exchange.
    /// 4. Start the local HTTP proxy.
    /// 5. Send `DrmProxyReady` so the player can start mpv against the proxy URL.
    fn start_drm_proxy(&self, stream: StreamUrl) {
        use crate::drm::cdm::CdmHandle;
        use crate::drm::{license, proxy};

        let tx  = self.tx.clone();
        let ctx = self.egui_ctx.clone();

        self.rt.spawn(async move {
            let protection = match stream.protection.as_ref() {
                Some(p) => p.clone(),
                None => {
                    let _ = tx.send(AsyncMsg::DrmProxyErr("stream has no protection data".into()));
                    ctx.request_repaint();
                    return;
                }
            };

            // --- 1. Open and initialize CDM ---
            let cdm_path = crate::widevine::cdm_path();
            let cdm_path_str = cdm_path.to_string_lossy().into_owned();
            let mut cdm_handle = match CdmHandle::open(&cdm_path_str) {
                Ok(h) => h,
                Err(e) => {
                    let _ = tx.send(AsyncMsg::DrmProxyErr(format!("CDM open failed: {}", e)));
                    ctx.request_repaint();
                    return;
                }
            };
            if let Err(e) = cdm_handle.initialize() {
                let _ = tx.send(AsyncMsg::DrmProxyErr(format!("CDM init failed: {}", e)));
                ctx.request_repaint();
                return;
            }
            let cdm = std::sync::Arc::new(std::sync::Mutex::new(cdm_handle));

            // --- 2. Fetch MPD and extract PSSH ---
            let mpd_url = stream.url.to_string();
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default();

            let mut req = client.get(&mpd_url);
            for (k, v) in &stream.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let mpd_resp = req.send().await;
            let mpd_text = match mpd_resp {
                Ok(r) => match r.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = tx.send(AsyncMsg::DrmProxyErr(format!("MPD read failed: {}", e)));
                        ctx.request_repaint();
                        return;
                    }
                },
                Err(e) => {
                    let _ = tx.send(AsyncMsg::DrmProxyErr(format!("MPD fetch failed: {}", e)));
                    ctx.request_repaint();
                    return;
                }
            };

            tracing::info!(
                "widevine: protection la_url={} pssh={:?} mpd_len={}",
                protection.la_url,
                protection.pssh.as_ref().map(|p| format!("{} bytes", p.len())),
                mpd_text.len()
            );
            // Log the MPD at debug level so we can inspect its ContentProtection structure.
            tracing::debug!("widevine: MPD text =\n{}", &mpd_text[..mpd_text.len().min(4000)]);

            let pssh = if let Some(p) = protection.pssh.as_ref() {
                p.clone()
            } else {
                match license::extract_pssh_from_mpd(&mpd_text) {
                    Some(p) => p,
                    None => {
                        let _ = tx.send(AsyncMsg::DrmProxyErr("PSSH not found in MPD".into()));
                        ctx.request_repaint();
                        return;
                    }
                }
            };

            // --- 3. License exchange ---
            if let Err(e) = license::acquire_license(&cdm, &pssh, &protection.la_url, &protection.license_headers).await {
                let _ = tx.send(AsyncMsg::DrmProxyErr(format!("License exchange failed: {}", e)));
                ctx.request_repaint();
                return;
            }

            // --- 4. Start proxy ---
            let cdn_headers = stream.headers.clone();
            let drm_proxy = match proxy::start(cdm, mpd_text, mpd_url, cdn_headers).await {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(AsyncMsg::DrmProxyErr(format!("Proxy start failed: {}", e)));
                    ctx.request_repaint();
                    return;
                }
            };

            let proxy_mpd_url = drm_proxy.mpd_url.clone();
            let _ = tx.send(AsyncMsg::DrmProxyReady { proxy_mpd_url, proxy: Box::new(drm_proxy) });
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
                    self.current_session = Some((kind_str, username));
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
                AsyncMsg::StreamOk { stream } => {
                    if stream.protection.is_some() {
                        // DRM stream — start the proxy pipeline before handing off to mpv.
                        self.start_drm_proxy(stream);
                    } else if let Screen::Player(player) = &mut self.screen {
                        player.start_playing(&stream);
                    }
                }
                AsyncMsg::DrmProxyReady { proxy_mpd_url, proxy } => {
                    // Keep proxy alive; give mpv the local URL.
                    self._drm_proxy = Some(proxy);
                    if let Screen::Player(player) = &mut self.screen {
                        // Build a plain StreamUrl pointing to the proxy.
                        if let Ok(proxy_url) = proxy_mpd_url.parse::<url::Url>() {
                            player.start_playing(&StreamUrl::direct(proxy_url));
                        }
                    }
                }
                AsyncMsg::DrmProxyErr(err) => {
                    tracing::error!("DRM proxy error: {}", err);
                    let channels = self.channels.clone();
                    self.screen = Screen::ChannelList(self.make_channel_list(channels));
                }
                AsyncMsg::StreamErr(err) => {
                    tracing::error!("stream resolution failed: {}", err);
                    let channels = self.channels.clone();
                    self.screen = Screen::ChannelList(self.make_channel_list(channels));
                }
                AsyncMsg::SessionExpired => {
                    tracing::info!("Session expired — clearing credentials, returning to setup");
                    if let Some((kind_str, username)) = self.current_session.take() {
                        keyring_session::clear_session(&kind_str, &username);
                    }
                    self.current_operator = None;
                    self.channels = Vec::new();
                    let mut s = SetupScreen::new();
                    s.set_error("Session expirée. Veuillez vous reconnecter.".to_string());
                    self.screen = Screen::Setup(s);
                }
                AsyncMsg::WidevineDone => {
                    // CDM is now on disk; mpv will pick it up on next play().
                    tracing::info!("widevine: CDM ready at {:?}", crate::widevine::cdm_path());
                }
                AsyncMsg::WidevineErr(err) => {
                    // Non-fatal — DRM streams will fail to play, but the app
                    // continues working for non-DRM content.
                    tracing::warn!("widevine: install failed: {}", err);
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
                    self.start_resolve_stream(channel.clone());
                    self.screen = Screen::Player(PlayerScreen::new(channel));
                }
            }
            Screen::Player(player) => {
                let channels   = self.channels.clone();
                let current_id = player.channel.id.clone();
                match player.show(ctx) {
                    PlayerAction::Back => {
                        self._drm_proxy = None;
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
