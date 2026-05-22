use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;

use crate::channel::{Channel, ChannelCategory, StreamTemplate};
use crate::channel::m3u::parse_m3u;
use crate::epg::EpgData;
use crate::error::OperatorError;
use crate::stream::StreamUrl;
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

        let wassup = self.wassup.as_deref().unwrap_or("").to_string();

        let resp = self.client
            .get(&self.homepage_url)
            .header("Cookie", format!("wassup={}", wassup))
            .send()
            .await?;

        let status = resp.status();
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

        self.tv_token = Some(token);
        self.tv_token_expires = Some(
            std::time::Instant::now() + std::time::Duration::from_secs(25 * 60),
        );

        Ok(())
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

/// Shape of one item in the Orange channel list response.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrangeChannel {
    #[serde(rename = "idEPG")]
    id_epg: String,
    name: String,
    #[serde(default)]
    display_order: Option<u32>,
    #[serde(default)]
    logos: Vec<OrangeLogo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrangeLogo {
    #[serde(rename = "urlService", default)]
    url_service: Option<String>,
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
            "authnByApp" => {
                // Orange already sent a push to the mobile app.
                // Extract the tracking ID so the poll loop can call /api/authnByApp.
                let tracking_id = body1
                    .pointer("/data/authnByAppScreen/idTracking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                tracing::info!(
                    "Orange: authnByApp push sent (idTracking={:?}); waiting for approval",
                    tracking_id
                );
                self.authn_tracking_id = Some(tracking_id);
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
    /// Two polling modes:
    ///  - `authnByApp` flow: POST `/api/authnByApp` with `{"idTracking": "…"}`
    ///  - Legacy push flow: POST `/api/push` with `{}`
    async fn wait_for_push_auth(&mut self) -> Result<()> {
        use std::time::Duration;

        const POLL_INTERVAL: Duration = Duration::from_secs(3);
        const MAX_ATTEMPTS: u32 = 30; // 30 × 3 s = 90 s

        let login_base = self.login_base.clone();
        let referer = format!("{}/", login_base);

        // Choose endpoint based on which push variant was detected.
        // authnByApp: re-poll /api/access — the SPA keeps calling the same endpoint
        //   until nextStep changes from "authnByApp" to something else (approval or error).
        // Legacy push: poll /api/push.
        let (poll_url, poll_body) = if self.authn_tracking_id.is_some() {
            (
                format!("{}/api/access", login_base),
                serde_json::json!({}),
            )
        } else {
            (
                format!("{}/api/push", login_base),
                serde_json::json!({}),
            )
        };
        tracing::info!("Orange: polling {}", poll_url);

        for attempt in 0..MAX_ATTEMPTS {
            tokio::time::sleep(POLL_INTERVAL).await;

            let builder = self
                .client
                .post(&poll_url)
                .header("Origin", &login_base)
                .header("Referer", &referer)
                .header("Accept", "application/json, text/plain, */*")
                .json(&poll_body);
            let resp = match self.with_xsrf(builder).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("push poll {}: network error: {}", attempt + 1, e);
                    continue;
                }
            };

            let status = resp.status();
            let wassup = resp
                .cookies()
                .find(|c| c.name() == "wassup")
                .map(|c| c.value().to_string());

            let body_text = resp.text().await.unwrap_or_default();
            tracing::debug!(
                "push poll {}: {} — {:.300}",
                attempt + 1,
                status,
                body_text
            );

            if !status.is_success() {
                // Non-2xx — log and keep polling (transient errors are common)
                continue;
            }

            let body: serde_json::Value =
                serde_json::from_str(&body_text).unwrap_or(serde_json::Value::Null);
            let next_step = body.get("nextStep").and_then(|v| v.as_str()).unwrap_or("(none)");
            tracing::info!("push poll {}: nextStep={:?}", attempt + 1, next_step);

            match next_step {
                "end" => {
                    if let Some(w) = wassup {
                        self.wassup = Some(w);
                    }
                    return self.ensure_tv_token().await;
                }
                "feedback" => {
                    return Err(OperatorError::AuthFailed(format!(
                        "push auth rejected: {}",
                        &body_text[..body_text.len().min(300)]
                    )));
                }
                // "authnByApp" / "push" / unknown — still pending; keep polling
                other => {
                    tracing::debug!("push poll {}: still waiting (nextStep={:?})", attempt + 1, other);
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
        let wassup = self.wassup.as_deref().unwrap_or("").to_string();

        let resp = self.client
            .get(&self.channels_url)
            .header("tv_token", format!("Bearer {}", tv_token))
            .header("Cookie", format!("wassup={}", wassup))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let channels_raw: Vec<OrangeChannel> = match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Orange: channel list parse error: {}, using fallback", e);
                        return Ok(parse_m3u(FALLBACK_M3U));
                    }
                };

                let placeholder = url::Url::parse(PLACEHOLDER_URL)
                    .expect("placeholder URL is valid");

                let channels = channels_raw
                    .into_iter()
                    .map(|c| {
                        let logo_url = c
                            .logos
                            .into_iter()
                            .find_map(|l| l.url_service);
                        Channel {
                            id: c.id_epg,
                            name: c.name,
                            logo_url,
                            number: c.display_order,
                            category: ChannelCategory::Other("".to_string()),
                            stream_template: StreamTemplate::Direct(placeholder.clone()),
                        }
                    })
                    .collect();

                Ok(channels)
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
        let wassup = self.wassup.as_deref().unwrap_or("");

        let stream_url = format!(
            "{}/{}?deviceModel=WEB_PC&customerOrangePopulation=OTT_Metro",
            self.stream_base, channel.id
        );

        let resp = self.client
            .get(&stream_url)
            .header("tv_token", format!("Bearer {}", tv_token))
            .header("Cookie", format!("wassup={}", wassup))
            .send()
            .await?;

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

        Ok(StreamUrl::direct(parsed))
    }

    async fn fetch_epg(&self, _hours: u8) -> Result<Option<EpgData>> {
        Ok(None)
    }
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
                ResponseTemplate::new(200).set_body_json(json!([
                    {
                        "idEPG": "TF1",
                        "name": "TF1",
                        "displayOrder": 1,
                        "logos": [{"urlService": "https://logos.example.com/tf1.png"}]
                    },
                    {
                        "idEPG": "BFMTV",
                        "name": "BFM TV",
                        "displayOrder": 15
                    }
                ])),
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
        assert_eq!(channels[0].id, "TF1");
        assert_eq!(channels[0].number, Some(1));
        assert_eq!(
            channels[0].logo_url.as_deref(),
            Some("https://logos.example.com/tf1.png")
        );
        assert_eq!(channels[1].id, "BFMTV");
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
        };

        let stream = op.resolve_stream(&channel).await.unwrap();
        assert_eq!(stream.url.as_str(), "https://cdn.example.com/TF1/index.mpd");
        assert!(stream.auth_header.is_none());
    }
}
