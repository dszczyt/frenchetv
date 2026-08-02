use crate::drm::DrmProxy;
use crate::screens::channel_list::ChannelListAction;
use crate::screens::otp::OtpAction;
use crate::screens::player::PlayerAction;
use crate::screens::setup::SetupAction;
use crate::screens::{
    ChannelListScreen, OtpScreen, PlayerScreen, PushWaitScreen, RestoringScreen, SetupScreen,
};
use frenchetv_core::session as keyring_session;
use frenchetv_core::{
    AuthPhase, Channel, Config, Operator, OperatorError, OperatorKind, OperatorRegistry, StreamUrl,
};
use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

type SharedOperator = Arc<TokioMutex<Box<dyn Operator>>>;
/// Shared logo cache: logo_url → decoded egui texture.
pub type LogoCache = Arc<Mutex<HashMap<String, egui::TextureHandle>>>;

/// Messages sent from Tokio tasks back to the UI thread.
enum AsyncMsg {
    AuthErr(String),
    /// Operator requires mobile push approval — show the push-wait screen.
    PushAuthPending,
    /// Operator requires a one-time code; show the OTP entry screen. The code
    /// the user types is sent back through `responder`.
    OtpRequired {
        responder: tokio::sync::oneshot::Sender<String>,
    },
    /// Authentication + channel fetch both succeeded.
    ChannelsOk {
        channels: Vec<Channel>,
        operator: SharedOperator,
        session_token: Option<String>,
        kind_str: String,
        username: String,
    },
    ChannelsErr(String),
    StreamOk {
        stream: StreamUrl,
    },
    StreamErr(String),
    /// DRM proxy started; pass proxy_mpd_url to mpv instead of the real stream URL.
    DrmProxyReady {
        proxy_mpd_url: String,
        proxy: Box<DrmProxy>,
    },
    DrmProxyErr(String),
    /// A 401/403 was received after login — session is invalid, must re-authenticate.
    SessionExpired,
    /// Background Widevine CDM download finished.
    WidevineDone,
    /// Background Widevine CDM download failed.
    WidevineErr(String),
}

enum Screen {
    /// A saved session is being validated + the channel list fetched. Only
    /// entered when `Config` already names an operator — never shows the
    /// operator picker while that restore is in flight (see `App::new`).
    Restoring(RestoringScreen),
    Setup(SetupScreen),
    PushWait(PushWaitScreen),
    Otp(OtpScreen),
    ChannelList(ChannelListScreen),
    Player(Box<PlayerScreen>),
}

pub struct App {
    force_software_renderer: bool,
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
    /// Channel back to the auth task waiting for the user's one-time code.
    pending_otp: Option<tokio::sync::oneshot::Sender<String>>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = mpsc::sync_channel(16);
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let logos: LogoCache = Arc::new(Mutex::new(HashMap::new()));
        let config = Config::load().unwrap_or_default();

        egui_extras::install_image_loaders(&cc.egui_ctx);
        crate::theme::install(&cc.egui_ctx);

        let force_software_renderer = std::env::args().any(|a| a == "--force-software-renderer");
        if force_software_renderer {
            tracing::info!("renderer: forced software mode via --force-software-renderer");
        }

        // Resolve *before* picking the initial screen: an operator is already
        // configured only if all three of these line up. When they do, start on
        // `Restoring` instead of `Setup` — the operator picker must not flash up
        // for a user who already has one configured (see `Screen::Restoring` doc).
        let kind_str = config.operator.kind.clone();
        let username = config.operator.username.clone();
        let restore = (!kind_str.is_empty() && !username.is_empty())
            .then(|| OperatorKind::from_config_str(&kind_str))
            .flatten()
            .and_then(|kind| {
                keyring_session::load_session(&kind_str, &username)
                    .map(|token| (kind, kind_str.clone(), username.clone(), token))
            });

        let app = Self {
            screen: if restore.is_some() {
                Screen::Restoring(RestoringScreen::new())
            } else {
                Screen::Setup(SetupScreen::new())
            },
            channels: Vec::new(),
            current_operator: None,
            current_session: None,
            logos,
            tx,
            rx,
            rt,
            egui_ctx: cc.egui_ctx.clone(),
            force_software_renderer,
            _drm_proxy: None,
            pending_otp: None,
        };

        if let Some((kind, kind_str, username, token)) = restore {
            app.start_restore_session(kind, kind_str, username, token);
        }

        // Download Widevine CDM in the background if not already present.
        if !crate::widevine::is_installed() {
            app.start_download_widevine();
        } else {
            tracing::info!(
                "widevine: CDM already present at {:?}",
                crate::widevine::cdm_path()
            );
        }

        app
    }

    /// Spawn concurrent logo fetches (max 20 in-flight) for all channels that
    /// have a logo_url. Decoded textures are inserted into the shared LogoCache;
    /// each insertion triggers a repaint so the UI updates incrementally.
    fn start_fetch_logos(&self, channels: Vec<Channel>) {
        let logos = Arc::clone(&self.logos);
        let ctx = self.egui_ctx.clone();
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
                    let logos = Arc::clone(&logos);
                    let ctx = ctx.clone();
                    let sem = Arc::clone(&sem);
                    set.spawn(async move {
                        let _permit = sem.acquire().await.ok()?;
                        let bytes = client.get(&url).send().await.ok()?.bytes().await.ok()?;
                        let img = image::load_from_memory(&bytes).ok()?;
                        let rgba = img.to_rgba8();
                        let (w, h) = rgba.dimensions();
                        let pixels: Vec<egui::Color32> = rgba
                            .pixels()
                            .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3]))
                            .collect();
                        let texture = ctx.load_texture(
                            url.as_str(),
                            egui::ColorImage {
                                size: [w as usize, h as usize],
                                pixels,
                            },
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
        let tx = self.tx.clone();
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
        let tx = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        self.rt.spawn(async move {
            let mut op = OperatorRegistry::build(&kind);
            if let Err(e) = op.restore_session(&token).await {
                tracing::info!("Session restore failed ({}); showing setup screen", e);
                keyring_session::clear_session(&kind_str, &username);
                // Must send something here: the app started on `Screen::Restoring`
                // (no operator picker) precisely because a saved session looked
                // usable — if this branch returns silently, that screen has no
                // other way to know the restore failed and is stuck forever.
                let _ = tx.send(AsyncMsg::SessionExpired);
                ctx.request_repaint();
                return;
            }
            match op.fetch_channels().await {
                Ok(channels) => {
                    let session_token = op.session_token().map(str::to_string);
                    let shared = Arc::new(TokioMutex::new(op));
                    let _ = tx.send(AsyncMsg::ChannelsOk {
                        channels,
                        operator: shared,
                        session_token,
                        kind_str,
                        username,
                    });
                }
                Err(OperatorError::InvalidCredentials) => {
                    let _ = tx.send(AsyncMsg::SessionExpired);
                }
                Err(e) => {
                    let _ = tx.send(AsyncMsg::ChannelsErr(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    fn start_auth(
        &self,
        kind: OperatorKind,
        username: String,
        password: String,
        extra: Option<String>,
    ) {
        let tx = self.tx.clone();
        let ctx = self.egui_ctx.clone();
        let kind_str = kind.config_str().to_string();
        self.rt.spawn(async move {
            let mut op = OperatorRegistry::build(&kind);
            if let Some(extra) = extra.as_deref() {
                op.set_extra_credential(extra);
            }

            // Drive the (possibly multi-step) auth flow to completion. Each phase
            // method returns the next AuthPhase; loop until `Done`.
            let mut phase = if op.uses_phased_auth() {
                match op.begin_auth(&username).await {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                        ctx.request_repaint();
                        return;
                    }
                }
            } else {
                match op.authenticate(&username, &password).await {
                    Ok(()) => AuthPhase::Done,
                    Err(e) => {
                        let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                        ctx.request_repaint();
                        return;
                    }
                }
            };

            loop {
                let next = match phase {
                    AuthPhase::Done => break,
                    AuthPhase::Password => op.complete_auth_password(&password).await,
                    AuthPhase::Push => {
                        let _ = tx.send(AsyncMsg::PushAuthPending);
                        ctx.request_repaint();
                        op.wait_for_push_auth(&password).await
                    }
                    AuthPhase::Otp => {
                        // Ask the UI for the one-time code; await the user's reply.
                        let (code_tx, code_rx) = tokio::sync::oneshot::channel::<String>();
                        let _ = tx.send(AsyncMsg::OtpRequired { responder: code_tx });
                        ctx.request_repaint();
                        match code_rx.await {
                            Ok(code) => op.submit_otp(&code).await,
                            Err(_) => return, // user cancelled OTP entry
                        }
                    }
                };
                phase = match next {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(AsyncMsg::AuthErr(e.to_string()));
                        ctx.request_repaint();
                        return;
                    }
                };
            }

            match op.fetch_channels().await {
                Ok(channels) => {
                    let session_token = op.session_token().map(str::to_string);
                    let shared = Arc::new(TokioMutex::new(op));
                    let _ = tx.send(AsyncMsg::ChannelsOk {
                        channels,
                        operator: shared,
                        session_token,
                        kind_str,
                        username,
                    });
                }
                Err(OperatorError::InvalidCredentials) => {
                    let _ = tx.send(AsyncMsg::SessionExpired);
                }
                Err(e) => {
                    let _ = tx.send(AsyncMsg::ChannelsErr(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

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
        tracing::debug!(
            "resolve_stream: starting for channel '{}' (id={})",
            channel.name,
            channel.id
        );
        self.rt.spawn(async move {
            let result = {
                let op = op.lock().await;
                op.resolve_stream(&channel).await
            };
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
        use crate::drm::{fmp4, license, proxy};

        let tx = self.tx.clone();
        let ctx = self.egui_ctx.clone();

        self.rt.spawn(async move {
            let protection = match stream.protection.as_ref() {
                Some(p) => p.clone(),
                None => {
                    let _ = tx.send(AsyncMsg::DrmProxyErr(
                        "stream has no protection data".into(),
                    ));
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
            // Use a cookie-store-enabled client so any Broadpeak session cookies set
            // by the CDN during the manifest fetch are automatically included in the
            // proxy's subsequent segment requests (same client instance).
            let mpd_url = stream.url.to_string();
            tracing::info!("DRM: stream URL = {}", mpd_url);
            let cdn_client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .cookie_store(true)
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(AsyncMsg::DrmProxyErr(format!(
                        "CDN client build failed: {}",
                        e
                    )));
                    ctx.request_repaint();
                    return;
                }
            };

            let mut req = cdn_client.get(&mpd_url);
            for (k, v) in &stream.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let mpd_resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(AsyncMsg::DrmProxyErr(format!("MPD fetch failed: {}", e)));
                    ctx.request_repaint();
                    return;
                }
            };
            // Log any session cookies the CDN set (they are now in cdn_client's jar).
            for value in mpd_resp.headers().get_all(reqwest::header::SET_COOKIE) {
                if let Ok(s) = value.to_str() {
                    let pair = s.split(';').next().unwrap_or("").trim();
                    tracing::info!("DRM: CDN Set-Cookie during MPD fetch: {}", pair);
                }
            }

            // Capture the final URL *before* consuming the response body — reqwest
            // follows 307 redirects automatically, so the CDN may have redirected
            // e.g. from `.../1-64/...` to `.../CDN_1/3/...`.  Segment URLs in the
            // MPD are relative to the final URL, not the original request URL.
            let final_mpd_url = mpd_resp.url().to_string();
            if final_mpd_url != mpd_url {
                tracing::info!("DRM: MPD redirected: {} → {}", mpd_url, final_mpd_url);
            }

            let mpd_text = match mpd_resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx.send(AsyncMsg::DrmProxyErr(format!("MPD read failed: {}", e)));
                    ctx.request_repaint();
                    return;
                }
            };

            tracing::info!(
                "widevine: protection la_url={} pssh={:?} mpd_len={}",
                protection.la_url,
                protection
                    .pssh
                    .as_ref()
                    .map(|p| format!("{} bytes", p.len())),
                mpd_text.len()
            );
            tracing::debug!(
                "widevine: MPD text =\n{}",
                &mpd_text[..mpd_text.len().min(4000)]
            );

            // ── Fetch init segment: extract Widevine PSSH for license exchange ──
            // The init segment's moov/pssh (authored by Orange) contains the full
            // WidevineCencHeader with provider/content_id fields that the license
            // server requires to return the correct content key.  Our hand-built PSSH
            // (from cenc:default_KID only) causes the server to return a key under a
            // different ID, producing CDM NoKey errors.
            let init_pssh: Option<Vec<u8>> =
                if let Some(probe_url) = probe_init_segment_url(&mpd_text, &final_mpd_url) {
                    tracing::info!(
                        "DRM: fetching init segment for Widevine PSSH: {}",
                        &probe_url[..probe_url.len().min(150)]
                    );
                    let mut req = cdn_client.get(&probe_url);
                    for (k, v) in &stream.headers {
                        req = req.header(k.as_str(), v.as_str());
                    }
                    match req.send().await {
                        Ok(r) if r.status().is_success() => match r.bytes().await {
                            Ok(bytes) => {
                                let pssh = fmp4::extract_widevine_pssh(&bytes);
                                tracing::info!(
                                    "DRM: init segment {} bytes, Widevine PSSH: {}",
                                    bytes.len(),
                                    pssh.as_ref()
                                        .map(|p| format!("{} bytes", p.len()))
                                        .unwrap_or_else(|| "not found".into())
                                );
                                pssh
                            }
                            Err(e) => {
                                tracing::warn!("DRM: init segment body error: {}", e);
                                None
                            }
                        },
                        Ok(r) => {
                            tracing::warn!("DRM: init segment → HTTP {} (no PSSH)", r.status());
                            None
                        }
                        Err(e) => {
                            tracing::warn!("DRM: init segment fetch error: {}", e);
                            None
                        }
                    }
                } else {
                    tracing::warn!("DRM: cannot derive init segment URL from MPD");
                    None
                };

            // PSSH priority: (1) init segment moov/pssh, (2) operator-provided, (3) MPD-derived.
            let pssh = if let Some(p) = init_pssh {
                p
            } else if let Some(p) = protection.pssh.as_ref() {
                p.clone()
            } else {
                match license::extract_pssh_from_mpd(&mpd_text) {
                    Some(p) => p,
                    None => {
                        let _ = tx.send(AsyncMsg::DrmProxyErr(
                            "PSSH not found in init segment or MPD".into(),
                        ));
                        ctx.request_repaint();
                        return;
                    }
                }
            };

            // --- 3. License exchange ---
            if let Err(e) = license::acquire_license(
                &cdm,
                &pssh,
                &protection.la_url,
                &protection.license_headers,
            )
            .await
            {
                let _ = tx.send(AsyncMsg::DrmProxyErr(format!(
                    "License exchange failed: {}",
                    e
                )));
                ctx.request_repaint();
                return;
            }

            // --- 4. Start proxy (pass the same cdn_client so its cookie jar is reused) ---
            let cdn_headers = stream.headers.clone();
            let drm_proxy =
                match proxy::start(cdm, mpd_text, final_mpd_url, cdn_headers, cdn_client).await {
                    Ok(p) => p,
                    Err(e) => {
                        let _ =
                            tx.send(AsyncMsg::DrmProxyErr(format!("Proxy start failed: {}", e)));
                        ctx.request_repaint();
                        return;
                    }
                };

            let proxy_mpd_url = drm_proxy.mpd_url.clone();
            let _ = tx.send(AsyncMsg::DrmProxyReady {
                proxy_mpd_url,
                proxy: Box::new(drm_proxy),
            });
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
                    Screen::Setup(s) => {
                        s.set_error(format!("Connexion échouée : {}", err));
                    }
                    Screen::PushWait(_) | Screen::Otp(_) => {
                        self.pending_otp = None;
                        let mut s = SetupScreen::new();
                        s.set_error(format!("Connexion échouée : {}", err));
                        self.screen = Screen::Setup(s);
                    }
                    _ => {}
                },
                AsyncMsg::PushAuthPending => {
                    self.screen = Screen::PushWait(PushWaitScreen::new());
                }
                AsyncMsg::OtpRequired { responder } => {
                    self.pending_otp = Some(responder);
                    self.screen = Screen::Otp(OtpScreen::new());
                }
                AsyncMsg::ChannelsOk {
                    channels,
                    operator,
                    session_token,
                    kind_str,
                    username,
                } => {
                    if let Some(ref token) = session_token {
                        keyring_session::save_session(&kind_str, &username, token);
                        let mut cfg = Config::load().unwrap_or_default();
                        cfg.operator.kind = kind_str.clone();
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
                AsyncMsg::ChannelsErr(err) => match &mut self.screen {
                    Screen::Setup(s) => {
                        s.set_error(format!("Erreur chargement chaînes : {}", err));
                    }
                    Screen::Restoring(_) => {
                        // Startup restore got a session but failed fetching
                        // channels — fall back to Setup so the error is visible
                        // instead of leaving the "Reconnexion en cours…" spinner
                        // spinning forever with no way out.
                        let mut s = SetupScreen::new();
                        s.set_error(format!("Erreur chargement chaînes : {}", err));
                        self.screen = Screen::Setup(s);
                    }
                    _ => {}
                },
                AsyncMsg::StreamOk { stream } => {
                    if stream.protection.is_some() {
                        // DRM stream — start the proxy pipeline before handing off to mpv.
                        self.start_drm_proxy(stream);
                    } else if let Screen::Player(player) = &mut self.screen {
                        player.start_playing(&stream);
                    }
                }
                AsyncMsg::DrmProxyReady {
                    proxy_mpd_url,
                    proxy,
                } => {
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
            Screen::Restoring(restoring) => {
                restoring.show(ctx);
            }
            Screen::Setup(setup) => {
                if let SetupAction::StartAuth {
                    operator,
                    username,
                    password,
                    extra,
                } = setup.show(ctx)
                {
                    self.start_auth(operator, username, password, extra);
                }
            }
            Screen::PushWait(pw) => {
                pw.show(ctx);
            }
            Screen::Otp(otp) => match otp.show(ctx) {
                OtpAction::Submit(code) => {
                    if let Some(tx) = self.pending_otp.take() {
                        let _ = tx.send(code);
                    }
                }
                OtpAction::Cancel => {
                    // Dropping the responder aborts the waiting auth task.
                    self.pending_otp = None;
                    self.screen = Screen::Setup(SetupScreen::new());
                }
                OtpAction::None => {}
            },
            Screen::ChannelList(list) => match list.show(ctx) {
                ChannelListAction::SelectChannel(channel) => {
                    self.start_resolve_stream((*channel).clone());
                    self.screen = Screen::Player(Box::new(PlayerScreen::new(
                        *channel,
                        self.egui_ctx.clone(),
                        self.force_software_renderer,
                    )));
                }
                ChannelListAction::ChangeProvider => {
                    self.current_operator = None;
                    self.channels = Vec::new();
                    self.screen = Screen::Setup(SetupScreen::new());
                }
                ChannelListAction::None => {}
            },
            Screen::Player(player) => {
                let channels = self.channels.clone();
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
                                let prev = if idx == 0 {
                                    channels.len() - 1
                                } else {
                                    idx - 1
                                };
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

/// Extract the first init-segment URL from a raw DASH MPD and resolve it
/// against the MPD's own URL so the result is always absolute.
fn probe_init_segment_url(mpd: &str, mpd_base_url: &str) -> Option<String> {
    // Directory of the MPD URL (everything up to and including the last '/').
    // Strip query string first.
    let mpd_path = mpd_base_url.split('?').next().unwrap_or(mpd_base_url);
    let mpd_dir = if let Some(pos) = mpd_path.rfind('/') {
        &mpd_base_url[..pos + 1] // keep the trailing slash; excludes query
    } else {
        mpd_base_url
    };

    // Extract <BaseURL> value (may be relative like "dash/").
    let raw_base = if let Some(s) = mpd.find("<BaseURL>") {
        let after = &mpd[s + "<BaseURL>".len()..];
        let end = after.find("</BaseURL>")?;
        after[..end].trim().to_string()
    } else {
        String::new()
    };

    // Resolve base to absolute.
    let base_url = if raw_base.starts_with("http://") || raw_base.starts_with("https://") {
        raw_base
    } else if raw_base.is_empty() {
        mpd_dir.to_string()
    } else {
        format!("{}{}", mpd_dir, raw_base)
    };
    let base_url = if base_url.ends_with('/') {
        base_url
    } else {
        format!("{}/", base_url)
    };

    // Extract first initialization template attribute value.
    let init_pos = mpd.find("initialization=\"")?;
    let after_init = &mpd[init_pos + "initialization=\"".len()..];
    let end_quote = after_init.find('"')?;
    let init_template = after_init[..end_quote].replace("&amp;", "&");

    // Extract first Representation id.
    let rep_pos = mpd.find("<Representation id=\"")?;
    let after_rep = &mpd[rep_pos + "<Representation id=\"".len()..];
    let end_quote2 = after_rep.find('"')?;
    let rep_id = &after_rep[..end_quote2];

    let init_relative = init_template.replace("$RepresentationID$", rep_id);

    let full = if init_relative.starts_with("http://") || init_relative.starts_with("https://") {
        init_relative
    } else {
        format!("{}{}", base_url, init_relative)
    };
    Some(full)
}
