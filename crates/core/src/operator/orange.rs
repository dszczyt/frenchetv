use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;

use crate::channel::{Channel, ChannelCategory, StreamTemplate};
use crate::channel::m3u::parse_m3u;
use crate::epg::EpgData;
use crate::error::OperatorError;
use crate::stream::{ProtectionData, StreamUrl};
use super::traits::{AuthPhase, Operator, Result};

/// Fallback M3U shipped with the binary.
const FALLBACK_M3U: &str = include_str!("../../../../assets/channels/orange.m3u");

/// Placeholder URL used for channels fetched from the live API (stream resolved separately).
const PLACEHOLDER_URL: &str = "https://placeholder.invalid/";

pub struct OrangeOperator {
    client: reqwest::Client,
    login_base: String,
    homepage_url: String,
    channels_url: String,
    stream_base: String,
    /// The `wassup` session cookie extracted after login.
    pub(crate) wassup: Option<String>,
    /// The `tv_token` extracted from the homepage HTML.
    pub(crate) tv_token: Option<String>,
    /// When the current `tv_token` expires (25 min after extraction).
    tv_token_expires: Option<std::time::Instant>,
    /// XSRF token captured from step-0 GET or `/api/access`; forwarded via X-XSRF-TOKEN.
    xsrf_token: Option<String>,
    /// Tracking ID returned when `/api/access` responds with `authnByApp`.
    /// Used to poll `POST /api/authnByApp` until the user approves on their phone.
    authn_tracking_id: Option<String>,
    /// Set to true once we have fired try_trigger_aba so we don't double-send.
    trigger_aba_sent: bool,
}

impl OrangeOperator {
    pub fn new() -> Self {
        Self::new_with_bases(
            "https://login.orange.fr",
            "https://tv.orange.fr/",
            "https://rp-ott-mediation-tv.woopic.com/api-gw/pds/v1/live/ew?everywherePopulation=OTT_Metro",
            "https://mediation-tv.orange.fr/all/api-gw/stream/v2/auth/accountToken/live",
        )
    }

    /// Constructs an operator pointing at custom base URLs — used in tests.
    pub fn new_with_bases(
        login_base: &str,
        homepage_url: &str,
        channels_url: &str,
        stream_base: &str,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .cookie_store(true)
                .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120.0.0.0")
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client build"),
            login_base: login_base.to_string(),
            homepage_url: homepage_url.to_string(),
            channels_url: channels_url.to_string(),
            stream_base: stream_base.to_string(),
            wassup: None,
            tv_token: None,
            tv_token_expires: None,
            xsrf_token: None,
            authn_tracking_id: None,
            trigger_aba_sent: false,
        }
    }

    /// Extract `tv_token` from homepage HTML by string search (no regex).
    fn extract_tv_token(html: &str) -> Option<String> {
        let key = r#""token":""#;
        let start = html.find(key)? + key.len();
        let end = html[start..].find('"')? + start;
        Some(html[start..end].to_string())
    }

    /// Refresh `tv_token` if it is absent or will expire within 5 minutes.
    async fn ensure_tv_token(&mut self) -> Result<()> {
        let needs_refresh = match self.tv_token_expires {
            None => true,
            Some(exp) => {
                exp.saturating_duration_since(std::time::Instant::now())
                    < std::time::Duration::from_secs(5 * 60)
            }
        };

        if !needs_refresh {
            return Ok(());
        }

        // Use explicit wassup if we have it; otherwise rely on cookie_store (AOM flow).
        let mut req = self.client.get(&self.homepage_url);
        if let Some(w) = &self.wassup {
            req = req.header("Cookie", format!("wassup={}", w));
        }
        let resp = req.send().await?;

        let status = resp.status();
        // Capture refreshed wassup before consuming response body.
        let refreshed_wassup = resp
            .cookies()
            .find(|c| c.name() == "wassup")
            .map(|c| c.value().to_string());

        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(OperatorError::InvalidCredentials);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::UnexpectedResponse {
                status: status.as_u16(),
                body,
            });
        }

        let html = resp.text().await?;
        let token = Self::extract_tv_token(&html)
            .ok_or_else(|| OperatorError::AuthFailed("tv_token not found in homepage HTML".into()))?;

        tracing::info!("Orange: tv_token extracted (len={})", token.len());
        self.tv_token = Some(token);
        self.tv_token_expires = Some(
            std::time::Instant::now() + std::time::Duration::from_secs(25 * 60),
        );

        // Orange slides session expiry on use — persist the refreshed token.
        if let Some(w) = refreshed_wassup {
            tracing::debug!("Orange: wassup refreshed in ensure_tv_token (len={})", w.len());
            self.wassup = Some(w);
        }

        Ok(())
    }

    /// Trigger the Orange AOM (App-based Orange Mobile) push notification.
    ///
    /// Discovered by reading the Orange login SPA source
    /// (idme.cdn.s.woopic.com/idme-front-1.24.0/static/index-GtMxwKBR.js):
    ///   `postAuthentAOM()` → `POST ${apiURL}/aom` with body `{}`
    /// where `apiURL = '/api'` (injected at runtime via VITE_IDME_FRONT_API_URL).
    async fn trigger_aom_push(&self, login_base: &str, referer: &str) {
        let url = format!("{}/api/aom", login_base);
        tracing::info!("Orange: triggering AOM push via POST {}", url);
        let builder = self
            .client
            .post(&url)
            .header("Origin", login_base)
            .header("Referer", referer)
            .header("Accept", "application/json, text/plain, */*")
            .json(&serde_json::json!({}));
        match self.with_xsrf(builder).send().await {
            Ok(r) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                tracing::info!("Orange POST /api/aom: {} — {:.300}", s, b);
            }
            Err(e) => tracing::warn!("Orange POST /api/aom: {}", e),
        }
    }

    /// Attach the XSRF token header if we have one.
    fn with_xsrf<'a>(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        match &self.xsrf_token {
            Some(tok) => builder.header("X-XSRF-TOKEN", tok.clone()),
            None => builder,
        }
    }
}

impl Default for OrangeOperator {
    fn default() -> Self {
        Self::new()
    }
}

/// Top-level Orange channel list response: `{"channels": [...]}`
#[derive(Deserialize)]
struct OrangeChannelList {
    channels: Vec<OrangeChannel>,
}

/// Shape of one item in the Orange channel list response.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrangeChannel {
    #[serde(rename = "idEPG")]
    id_epg: u32,
    name: String,
    #[serde(default)]
    display_order: Option<u32>,
    #[serde(default)]
    logos: Vec<OrangeLogoSet>,
    /// Platform availability flags — absence of "W_PC" means locked on desktop.
    #[serde(default)]
    allowed_device_categories: Vec<String>,
    /// Technical stream descriptors — live[0].techChannelId is the stream API key.
    #[serde(default)]
    technical_channels: OrangeTechnicalChannels,
    /// Human-readable channel identifier used in editorial/content systems.
    #[serde(default)]
    edito_channel_id: String,
}

#[derive(Deserialize, Default)]
struct OrangeTechnicalChannels {
    #[serde(default)]
    live: Vec<OrangeTechLiveChannel>,
}

/// One live technical channel entry — carries the ID used by the stream mediation API.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrangeTechLiveChannel {
    tech_channel_id: String,
    /// Relative path used in stream URL, e.g. "livetv_tf1_ctv".
    /// JSON key uses uppercase "URL" so we override serde's lowercase conversion.
    #[serde(rename = "liveTargetURLRelativePath", default)]
    live_target_url_relative_path: String,
}

/// One logo variant (e.g. "webTVLogo", "webTVSquare", "mobileAppli", …).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrangeLogoSet {
    definition_type: String,
    #[serde(default)]
    list_logos: Vec<OrangeLogoItem>,
}

#[derive(Deserialize)]
struct OrangeLogoItem {
    path: String,
}

#[async_trait]
impl Operator for OrangeOperator {
    fn name(&self) -> &'static str {
        "Orange TV"
    }

    fn requires_auth(&self) -> bool {
        true
    }

    fn uses_phased_auth(&self) -> bool {
        true
    }

    /// Phase 1: visit login page, call /api/access and /api/login, detect auth method.
    async fn begin_auth(&mut self, username: &str) -> Result<AuthPhase> {
        let login_base = self.login_base.clone();
        let referer = format!("{}/", login_base);

        // Step 0 — Seed session cookies; SPAs typically set XSRF-TOKEN here.
        if let Ok(r) = self
            .client
            .get(&referer)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .send()
            .await
        {
            if let Some(c) = r.cookies().find(|c| c.name().eq_ignore_ascii_case("xsrf-token")) {
                self.xsrf_token = Some(c.value().to_string());
                tracing::debug!("Orange: XSRF captured from step 0 (len={})", c.value().len());
            }
        }

        // Step 1 — POST /api/access (send XSRF if we have it; capture any rotation).
        let builder = self
            .client
            .post(format!("{}/api/access", login_base))
            .header("Origin", &login_base)
            .header("Referer", &referer)
            .header("Accept", "application/json, text/plain, */*")
            .json(&serde_json::json!({}));
        let resp = self.with_xsrf(builder).send().await?;

        // Update XSRF-TOKEN if the server rotated it.
        if let Some(c) = resp.cookies().find(|c| c.name().eq_ignore_ascii_case("xsrf-token")) {
            self.xsrf_token = Some(c.value().to_string());
            tracing::debug!("Orange: XSRF updated from step 1 (len={})", c.value().len());
        }

        let status = resp.status();
        let body1_text = resp.text().await.unwrap_or_default();
        tracing::debug!("Orange /api/access: {} — {:.200}", status, body1_text);

        if !status.is_success() {
            return Err(OperatorError::UnexpectedResponse {
                status: status.as_u16(),
                body: body1_text,
            });
        }

        let body1: serde_json::Value =
            serde_json::from_str(&body1_text).unwrap_or(serde_json::Value::Null);
        let access_next = body1.get("nextStep").and_then(|v| v.as_str()).unwrap_or("(none)");
        tracing::info!("Orange /api/access nextStep={:?}", access_next);

        match access_next {
            "feedback" => {
                return Err(OperatorError::AuthFailed(format!(
                    "orange.fr rejected /api/access: {}",
                    &body1_text[..body1_text.len().min(300)]
                )));
            }
            "redirect" => {
                let location = body1
                    .pointer("/data/location")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unknown)");
                let msg = if location.contains("captcha") {
                    "Orange a détecté un trop grand nombre de tentatives — veuillez patienter \
                     quelques minutes avant de réessayer."
                } else {
                    "Orange a redirigé la connexion vers une page inattendue."
                };
                tracing::warn!("Orange /api/access redirect to: {}", location);
                return Err(OperatorError::AuthFailed(msg.to_string()));
            }
            "authnByApp" => {
                // /api/access says the account uses app-based auth.
                // SPA source: `triggerABA` action → `postAuthentAOM()` → POST /api/aom {}.
                // Send the push immediately so the user doesn't wait 45 s.
                let tracking_id = body1
                    .pointer("/data/authnByAppScreen/idTracking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.authn_tracking_id = Some(tracking_id);
                self.trigger_aba_sent = false; // reset in case of re-auth

                self.trigger_aom_push(&login_base, &referer).await;
                self.trigger_aba_sent = true;

                return Ok(AuthPhase::Push);
            }
            _ => {} // continue to /api/login
        }

        // Step 2 — POST /api/login (only reached when /api/access does NOT return authnByApp)
        let builder = self
            .client
            .post(format!("{}/api/login", login_base))
            .header("Origin", &login_base)
            .header("Referer", &referer)
            .header("Accept", "application/json, text/plain, */*")
            .json(&serde_json::json!({
                "login": username,
                "loginOrigin": "input"
            }));
        let resp = self.with_xsrf(builder).send().await?;

        let status = resp.status();
        let body2_text = resp.text().await.unwrap_or_default();
        tracing::debug!("Orange /api/login: {} — {:.300}", status, body2_text);

        if !status.is_success() {
            return Err(OperatorError::AuthFailed(format!(
                "login rejected (HTTP {}): {}",
                status,
                &body2_text[..body2_text.len().min(300)]
            )));
        }

        let body2: serde_json::Value =
            serde_json::from_str(&body2_text).unwrap_or(serde_json::Value::Null);
        let next_step = body2.get("nextStep").and_then(|v| v.as_str()).unwrap_or("(none)");
        tracing::info!("Orange /api/login nextStep={:?}", next_step);

        match next_step {
            "feedback" => Err(OperatorError::AuthFailed(format!(
                "account not recognized: {}",
                &body2_text[..body2_text.len().min(300)]
            ))),
            "push" | "push_notification" => {
                tracing::info!("Orange: push auth required after login; waiting for approval");
                Ok(AuthPhase::Push)
            }
            _ => Ok(AuthPhase::Password),
        }
    }

    /// Phase 2a: submit password, extract wassup cookie, fetch tv_token.
    async fn complete_auth_password(&mut self, password: &str) -> Result<()> {
        let login_base = self.login_base.clone();
        let referer = format!("{}/", login_base);

        let builder = self
            .client
            .post(format!("{}/api/password", login_base))
            .header("Origin", &login_base)
            .header("Referer", &referer)
            .header("Accept", "application/json, text/plain, */*")
            .json(&serde_json::json!({
                "password": password,
                "remember": true
            }));
        let resp = self.with_xsrf(builder).send().await?;

        let status = resp.status();
        // Read cookies before consuming the body.
        let wassup = resp
            .cookies()
            .find(|c| c.name() == "wassup")
            .map(|c| c.value().to_string());

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::debug!("Orange /api/password: {} — {:.300}", status, body);
            return Err(OperatorError::AuthFailed(format!(
                "password rejected (HTTP {}): {}",
                status,
                &body[..body.len().min(300)]
            )));
        }

        match wassup {
            Some(v) => self.wassup = Some(v),
            None => {
                let body3_text = resp.text().await.unwrap_or_default();
                tracing::debug!("Orange /api/password no wassup: {:.300}", body3_text);
                let body3: serde_json::Value =
                    serde_json::from_str(&body3_text).unwrap_or(serde_json::Value::Null);
                let next = body3.get("nextStep").and_then(|v| v.as_str()).unwrap_or("(none)");
                tracing::info!("Orange /api/password nextStep={:?} (no wassup cookie)", next);
                if next == "feedback" {
                    return Err(OperatorError::AuthFailed(format!(
                        "password rejected: {}",
                        &body3_text[..body3_text.len().min(300)]
                    )));
                }
                return Err(OperatorError::AuthFailed(format!(
                    "wassup cookie not received (nextStep={:?}): {}",
                    next,
                    &body3_text[..body3_text.len().min(200)]
                )));
            }
        }

        self.ensure_tv_token().await
    }

    /// Phase 2b: poll until Orange signals push approval, then fetch tv_token.
    ///
    /// SPA source (idme-front-1.24.0):
    ///   `pollingABA` → `pollAuthentAOM()` → `GET ${apiURL}/aom`
    /// where apiURL = '/api' (VITE_IDME_FRONT_API_URL runtime config).
    /// Legacy push variant falls back to `POST /api/push`.
    async fn wait_for_push_auth(&mut self, password: &str) -> Result<()> {
        use std::time::Duration;

        const POLL_INTERVAL: Duration = Duration::from_secs(3);
        const MAX_ATTEMPTS: u32 = 30; // 30 × 3 s = 90 s

        let login_base = self.login_base.clone();
        let referer = format!("{}/", login_base);

        let is_aom = self.authn_tracking_id.is_some();
        let poll_url = if is_aom {
            format!("{}/api/aom", login_base)
        } else {
            format!("{}/api/push", login_base)
        };
        tracing::info!("Orange: polling {} ({})", poll_url, if is_aom { "GET" } else { "POST" });

        for attempt in 0..MAX_ATTEMPTS {
            tokio::time::sleep(POLL_INTERVAL).await;

            // AOM flow: GET /api/aom (no body).  Legacy: POST /api/push with {}.
            let resp = if is_aom {
                let b = self.client
                    .get(&poll_url)
                    .header("Origin", &login_base)
                    .header("Referer", &referer)
                    .header("Accept", "application/json, text/plain, */*");
                match self.with_xsrf(b).send().await {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("aom poll {}: {}", attempt + 1, e); continue; }
                }
            } else {
                let b = self.client
                    .post(&poll_url)
                    .header("Origin", &login_base)
                    .header("Referer", &referer)
                    .header("Accept", "application/json, text/plain, */*")
                    .json(&serde_json::json!({}));
                match self.with_xsrf(b).send().await {
                    Ok(r) => r,
                    Err(e) => { tracing::warn!("push poll {}: {}", attempt + 1, e); continue; }
                }
            };

            let status = resp.status();
            let wassup = resp
                .cookies()
                .find(|c| c.name() == "wassup")
                .map(|c| c.value().to_string());
            let body_text = resp.text().await.unwrap_or_default();

            if !status.is_success() {
                tracing::debug!("poll {}: {} — {:.500}", attempt + 1, status, body_text);
                continue;
            }

            let body: serde_json::Value =
                serde_json::from_str(&body_text).unwrap_or(serde_json::Value::Null);
            let next_step = body.get("nextStep").and_then(|v| v.as_str()).unwrap_or("(none)");
            tracing::info!("poll {}: nextStep={:?}", attempt + 1, next_step);
            if next_step != "authnByApp" && next_step != "pollingABA" {
                tracing::debug!("poll {}: FULL BODY = {}", attempt + 1, body_text);
            }

            match next_step {
                // Authentication complete — cookie may arrive here or already be in jar.
                "end" | "final" => {
                    if let Some(ref w) = wassup {
                        tracing::info!("Orange: wassup cookie received in {:?} response (len={})", next_step, w.len());
                        self.wassup = Some(w.clone());
                    } else {
                        tracing::info!("Orange: no wassup in {:?} response — relying on cookie_store", next_step);
                    }
                    tracing::info!("Orange: AOM approved (nextStep={:?}); fetching tv_token", next_step);
                    return self.ensure_tv_token().await;
                }
                "feedback" => {
                    return Err(OperatorError::AuthFailed(format!(
                        "push auth rejected: {}", body_text
                    )));
                }
                "redirect" => {
                    let location = body
                        .pointer("/data/location")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(unknown)");
                    return Err(OperatorError::AuthFailed(format!(
                        "AOM auth redirect to error page: {}", location
                    )));
                }
                "remoteAccounts" => {
                    // User approved on phone; server shows account list for selection.
                    let account_list = body
                        .pointer("/data/remoteAccountsScreen/accountList")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();

                    tracing::info!("Orange remoteAccounts: {} account(s)", account_list.len());

                    if account_list.is_empty() {
                        tracing::warn!("Orange remoteAccounts: empty list; body={}", body_text);
                        continue;
                    }

                    let first = &account_list[0];
                    let login_val = first.get("login").and_then(|v| v.as_str()).unwrap_or("");
                    tracing::info!("Orange: selecting account {:?}", login_val);

                    let b = self.client
                        .post(format!("{}/api/login", login_base))
                        .header("Origin", &login_base)
                        .header("Referer", &referer)
                        .header("Accept", "application/json, text/plain, */*")
                        .json(first);
                    let login_resp = match self.with_xsrf(b).send().await {
                        Ok(r) => r,
                        Err(e) => { tracing::warn!("Orange /api/login: {}", e); continue; }
                    };

                    let login_status = login_resp.status();
                    let login_body = login_resp.text().await.unwrap_or_default();
                    tracing::info!("Orange /api/login: {} — {:.300}", login_status, login_body);

                    let login_json: serde_json::Value =
                        serde_json::from_str(&login_body).unwrap_or_default();
                    let login_next = login_json
                        .get("nextStep").and_then(|v| v.as_str()).unwrap_or("(none)");

                    match login_next {
                        "password" => return self.complete_auth_password(password).await,
                        "end"      => return self.ensure_tv_token().await,
                        "feedback" => return Err(OperatorError::AuthFailed(format!(
                            "account selection rejected: {}", login_body
                        ))),
                        "authnByApp" if !self.trigger_aba_sent => {
                            // Server wants another AOM trigger after account selection.
                            tracing::info!("Orange: re-triggering AOM push after remoteAccounts");
                            self.trigger_aom_push(&login_base, &referer).await;
                            self.trigger_aba_sent = true;
                        }
                        _ => {
                            tracing::warn!("Orange /api/login unexpected nextStep={:?}", login_next);
                        }
                    }
                }
                // pollingABA / authnByApp / unknown — still waiting
                other => {
                    tracing::debug!("poll {}: still waiting ({})", attempt + 1, other);
                }
            }
        }

        Err(OperatorError::AuthFailed(
            "push auth timed out (90 s); please try again".into(),
        ))
    }

    /// Convenience: single-call auth for tests and password-based accounts.
    /// If the account requires push, returns `AuthFailed` — use the phased methods.
    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        match self.begin_auth(username).await? {
            AuthPhase::Password => self.complete_auth_password(password).await,
            AuthPhase::Push => Err(OperatorError::AuthFailed(
                "account requires mobile push auth; use the phased auth flow".into(),
            )),
        }
    }

    async fn fetch_channels(&self) -> Result<Vec<Channel>> {
        let tv_token = match &self.tv_token {
            Some(t) => t.clone(),
            None => {
                warn!("Orange: tv_token not available, using fallback");
                return Ok(parse_m3u(FALLBACK_M3U));
            }
        };
        let mut req = self.client
            .get(&self.channels_url)
            .header("tv_token", format!("Bearer {}", tv_token))
            .timeout(std::time::Duration::from_secs(5));
        if let Some(w) = &self.wassup {
            req = req.header("Cookie", format!("wassup={}", w));
        }
        let resp = req.send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let body_text = match r.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("Orange: channel list read error: {}, using fallback", e);
                        return Ok(parse_m3u(FALLBACK_M3U));
                    }
                };
                let channels_raw: Vec<OrangeChannel> = match serde_json::from_str::<OrangeChannelList>(&body_text) {
                    Ok(wrapper) => wrapper.channels,
                    Err(e) => {
                        warn!("Orange: channel list parse error: {} — body: {:.300}", e, body_text);
                        return Ok(parse_m3u(FALLBACK_M3U));
                    }
                };

                let placeholder = url::Url::parse(PLACEHOLDER_URL)
                    .expect("placeholder URL is valid");

                let channels: Vec<Channel> = channels_raw
                    .into_iter()
                    .map(|c| {
                        // Prefer the web TV horizontal logo; fall back to square variant.
                        // Mobile variants use relative paths — skip those.
                        let logo_url = ["webTVLogo", "webTVSquare"]
                            .iter()
                            .find_map(|&def| {
                                c.logos.iter()
                                    .find(|l| l.definition_type == def)
                                    .and_then(|l| l.list_logos.first())
                                    .map(|item| item.path.clone())
                                    .filter(|p| p.starts_with("http"))
                            });
                        let locked = !c.allowed_device_categories.is_empty()
                            && !c.allowed_device_categories.iter().any(|s| s == "W_PC");
                        // Prefer liveTargetURLRelativePath → techChannelId → idEPG.
                        let stream_id = c.technical_channels.live.first()
                            .map(|tc| {
                                if !tc.live_target_url_relative_path.is_empty() {
                                    tc.live_target_url_relative_path.clone()
                                } else {
                                    tc.tech_channel_id.clone()
                                }
                            })
                            .unwrap_or_else(|| c.id_epg.to_string());
                        Channel {
                            id: stream_id,
                            name: c.name,
                            logo_url,
                            number: c.display_order,
                            category: ChannelCategory::Other("".to_string()),
                            stream_template: StreamTemplate::Direct(placeholder.clone()),
                            locked,
                        }
                    })
                    .collect();

                let with_logo = channels.iter().filter(|c| c.logo_url.is_some()).count();
                let locked_count = channels.iter().filter(|c| c.locked).count();
                tracing::info!(
                    "Orange: {} channels, {} with logo, {} locked",
                    channels.len(), with_logo, locked_count
                );
                if let Some(ch) = channels.iter().find(|c| c.logo_url.is_some()) {
                    tracing::debug!("Orange: sample logo_url = {:?}", ch.logo_url);
                }
                Ok(channels)
            }
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED
                   || r.status() == reqwest::StatusCode::FORBIDDEN => {
                Err(OperatorError::InvalidCredentials)
            }
            Ok(r) => {
                warn!("Orange: channel list returned {}, using fallback", r.status());
                Ok(parse_m3u(FALLBACK_M3U))
            }
            Err(e) => {
                warn!("Orange: channel list network error: {}, using fallback", e);
                Ok(parse_m3u(FALLBACK_M3U))
            }
        }
    }

    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl> {
        // If this channel came from the fallback M3U, return its URL directly.
        match &channel.stream_template {
            StreamTemplate::Direct(url) if url.as_str() != PLACEHOLDER_URL => {
                return Ok(StreamUrl::direct(url.clone()));
            }
            _ => {}
        }

        let tv_token = self.tv_token.as_deref().unwrap_or("");

        let stream_url = format!(
            "{}/{}?deviceModel=WEB_PC&customerOrangePopulation=OTT_Metro",
            self.stream_base, channel.id
        );

        tracing::debug!("Orange resolve_stream: GET {} (tv_token len={})", stream_url, tv_token.len());
        let mut req = self.client
            .get(&stream_url)
            .header("tv_token", format!("Bearer {}", tv_token))
            .header("Accept", "application/json, text/plain, */*")
            .header("Origin", "https://tv.orange.fr")
            .header("Referer", "https://tv.orange.fr/");
        if let Some(w) = &self.wassup {
            req = req.header("Cookie", format!("wassup={}", w));
        }
        let resp = req.send().await?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::BAD_REQUEST
        {
            return Err(OperatorError::InvalidCredentials);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::UnexpectedResponse {
                status: status.as_u16(),
                body,
            });
        }

        let json: serde_json::Value = resp.json().await?;

        // Try several known paths for the stream URL
        let url_str = if let Some(s) = json.get("url").and_then(|v| v.as_str()) {
            s.to_string()
        } else if let Some(s) = json
            .get("stream")
            .and_then(|v| v.get("url"))
            .and_then(|v| v.as_str())
        {
            s.to_string()
        } else if let Some(s) = json.get("streamUrl").and_then(|v| v.as_str()) {
            s.to_string()
        } else if let Some(s) = json.get("hls").and_then(|v| v.as_str()) {
            s.to_string()
        } else {
            // Fallback: first top-level string value that starts with "http"
            json.as_object()
                .and_then(|obj| {
                    obj.values()
                        .find_map(|v| v.as_str().filter(|s| s.starts_with("http")))
                        .map(|s| s.to_string())
                })
                .ok_or_else(|| {
                    OperatorError::ParseChannels("stream URL not found in response".into())
                })?
        };

        let parsed = url::Url::parse(&url_str).map_err(|e| {
            OperatorError::ParseChannels(format!("invalid stream URL '{}': {}", url_str, e))
        })?;

        // Orange's CDN requires Origin/Referer on every segment request.
        let mut stream = StreamUrl::direct(parsed)
            .with_header("Origin",     "https://tv.orange.fr")
            .with_header("Referer",    "https://tv.orange.fr/")
            .with_header("User-Agent", "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/110.0.0.0 Safari/537.36");

        // Extract Widevine DRM parameters from protectionData array if present.
        // Orange returns: [{"keySystem":"com.widevine.alpha","licenseServerURL":"...","initData":"base64..."}]
        stream.protection = extract_widevine_protection(&json, &tv_token, &self.stream_base);
        Ok(stream)
    }

    async fn fetch_epg(&self, _hours: u8) -> Result<Option<EpgData>> {
        Ok(None)
    }

    fn session_token(&self) -> Option<&str> {
        self.wassup.as_deref()
    }

    /// Restore `wassup` from keyring and verify the session by fetching tv_token.
    async fn restore_session(&mut self, token: &str) -> Result<()> {
        self.wassup = Some(token.to_string());
        self.ensure_tv_token().await
    }
}

/// Extract Widevine ProtectionData from the stream-resolution JSON response.
/// Orange's API returns a `protectionData` array; each entry has a `keySystem`,
/// `licenseServerURL`, and optionally `initData` (base64-encoded PSSH box).
fn extract_widevine_protection(
    json: &serde_json::Value,
    tv_token: &str,
    stream_base: &str,
) -> Option<ProtectionData> {
    use base64::engine::Engine as _;

    let arr = json.get("protectionData")?.as_array()?;
    for entry in arr {
        let ks = entry.get("keySystem").and_then(|v| v.as_str()).unwrap_or("");
        if ks != "com.widevine.alpha" {
            continue;
        }
        let raw_url = entry
            .get("licenseServerURL")
            .and_then(|v| v.as_str())
            .or_else(|| entry.get("laUrl").and_then(|v| v.as_str()))?;

        // Resolve relative URLs (e.g. "/widevine/license?...") against the
        // stream API host (e.g. "https://mediation-tv.orange.fr/...").
        let la_url = if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
            raw_url.to_string()
        } else {
            // Extract scheme://host from stream_base then append the relative path.
            let host = stream_base
                .trim_end_matches('/')
                .splitn(4, '/')   // ["https:", "", "host", "path..."]
                .take(3)
                .collect::<Vec<_>>()
                .join("/");
            format!("{}{}", host, raw_url)
        };

        let pssh = entry
            .get("initData")
            .and_then(|v| v.as_str())
            .and_then(|b64| {
                base64::engine::general_purpose::STANDARD.decode(b64)
                    .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(b64))
                    .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(b64))
                    .ok()
            });

        // Pass tv_token as a license request header so the license server
        // can validate the subscriber's entitlement.
        let mut license_headers = Vec::new();
        if !tv_token.is_empty() {
            license_headers.push(("tv_token".to_string(), format!("Bearer {}", tv_token)));
        }

        return Some(ProtectionData { la_url, pssh, license_headers });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path};
    use serde_json::json;

    // ---------------------------------------------------------------------------
    // Test 1 — successful authentication sets wassup and tv_token
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn test_authenticate_success() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/password"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "Set-Cookie",
                        "wassup=test_wassup_value; Path=/; HttpOnly",
                    )
                    .set_body_json(json!({})),
            )
            .mount(&mock)
            .await;

        // Homepage returns HTML with embedded tv_token
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        r#"<!DOCTYPE html><html><head></head><body>some html "token":"test_tv_token_value" more html</body></html>"#,
                    ),
            )
            .mount(&mock)
            .await;

        let homepage = format!("{}/", mock.uri());
        let mut op = OrangeOperator::new_with_bases(
            &mock.uri(),
            &homepage,
            &format!("{}/channels", mock.uri()),
            &format!("{}/stream", mock.uri()),
        );

        op.authenticate("user@orange.fr", "pass1234").await.unwrap();

        assert_eq!(op.wassup.as_deref(), Some("test_wassup_value"));
        assert_eq!(op.tv_token.as_deref(), Some("test_tv_token_value"));
    }

    // ---------------------------------------------------------------------------
    // Test 2 — 401 on /api/login returns InvalidCredentials
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn test_authenticate_invalid_credentials() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "invalid_credentials"
            })))
            .mount(&mock)
            .await;

        let mut op = OrangeOperator::new_with_bases(
            &mock.uri(),
            &format!("{}/", mock.uri()),
            &format!("{}/channels", mock.uri()),
            &format!("{}/stream", mock.uri()),
        );

        let err = op
            .authenticate("bad@example.com", "wrong")
            .await
            .unwrap_err();
        // 401 from /api/login maps to AuthFailed with the HTTP status in the message.
        assert!(
            matches!(&err, OperatorError::AuthFailed(msg) if msg.contains("401")),
            "expected AuthFailed(401 ...), got {:?}",
            err
        );
    }

    // ---------------------------------------------------------------------------
    // Test 3 — begin_auth detects push nextStep
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn test_begin_auth_detects_push() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "nextStep": "push"
            })))
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&mock)
            .await;

        let mut op = OrangeOperator::new_with_bases(
            &mock.uri(),
            &format!("{}/", mock.uri()),
            &format!("{}/channels", mock.uri()),
            &format!("{}/stream", mock.uri()),
        );

        let phase = op.begin_auth("user@orange.fr").await.unwrap();
        assert_eq!(phase, AuthPhase::Push);
    }

    // ---------------------------------------------------------------------------
    // Test 4 — 500 from channels endpoint falls back to M3U
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn test_fetch_channels_falls_back_on_api_error() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/channels"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let mut op = OrangeOperator::new_with_bases(
            &mock.uri(),
            &format!("{}/", mock.uri()),
            &format!("{}/channels", mock.uri()),
            &format!("{}/stream", mock.uri()),
        );
        op.wassup = Some("test_wassup".into());
        op.tv_token = Some("test_token".into());
        op.tv_token_expires =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(25 * 60));

        let channels = op.fetch_channels().await.unwrap();
        assert!(!channels.is_empty(), "fallback M3U must supply at least 1 channel");
    }

    // ---------------------------------------------------------------------------
    // Test 5 — API response is parsed correctly
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn test_fetch_channels_parses_api_response() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/channels"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "channels": [
                        {
                            "idEPG": 1,
                            "name": "TF1",
                            "displayOrder": 1,
                            "logos": [
                                {"definitionType": "webTVLogo", "listLogos": [{"path": "https://logos.example.com/tf1.png", "size": "180x96"}]},
                                {"definitionType": "mobileAppli", "listLogos": [{"path": "%2Flogos%2Ftf1.png", "size": "183x183"}]}
                            ],
                            "technicalChannels": {
                                "live": [{"techChannelId": "100001", "liveTargetURLRelativePath": "livetv_tf1_ctv", "type": "CLOUDTV", "usi": 100001}]
                            }
                        },
                        {
                            "idEPG": 15,
                            "name": "BFM TV",
                            "displayOrder": 15
                        }
                    ]
                })),
            )
            .mount(&mock)
            .await;

        let mut op = OrangeOperator::new_with_bases(
            &mock.uri(),
            &format!("{}/", mock.uri()),
            &format!("{}/channels", mock.uri()),
            &format!("{}/stream", mock.uri()),
        );
        op.wassup = Some("test_wassup".into());
        op.tv_token = Some("test_token".into());
        op.tv_token_expires =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(25 * 60));

        let channels = op.fetch_channels().await.unwrap();
        assert_eq!(channels.len(), 2);
        // TF1 has liveTargetURLRelativePath → id comes from that (not techChannelId)
        assert_eq!(channels[0].id, "livetv_tf1_ctv");
        assert_eq!(channels[0].number, Some(1));
        assert_eq!(
            channels[0].logo_url.as_deref(),
            Some("https://logos.example.com/tf1.png")
        );
        // BFM TV has no technicalChannels → id falls back to idEPG
        assert_eq!(channels[1].id, "15");
        assert_eq!(channels[1].number, Some(15));
    }

    // ---------------------------------------------------------------------------
    // Test 6 — stream resolution returns URL from API JSON
    // ---------------------------------------------------------------------------
    #[tokio::test]
    async fn test_resolve_stream() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/stream/TF1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "url": "https://cdn.example.com/TF1/index.mpd"
                })),
            )
            .mount(&mock)
            .await;

        let mut op = OrangeOperator::new_with_bases(
            &mock.uri(),
            &format!("{}/", mock.uri()),
            &format!("{}/channels", mock.uri()),
            &format!("{}/stream", mock.uri()),
        );
        op.wassup = Some("test_wassup".into());
        op.tv_token = Some("test_token".into());
        op.tv_token_expires =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(25 * 60));

        let channel = Channel {
            id: "TF1".into(),
            name: "TF1".into(),
            logo_url: None,
            number: Some(1),
            category: ChannelCategory::Other("".to_string()),
            stream_template: StreamTemplate::Direct(
                url::Url::parse(PLACEHOLDER_URL).unwrap(),
            ),
            locked: false,
        };

        let stream = op.resolve_stream(&channel).await.unwrap();
        assert_eq!(stream.url.as_str(), "https://cdn.example.com/TF1/index.mpd");
        assert!(stream.auth_header.is_none());
    }
}
