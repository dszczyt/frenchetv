use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;

use crate::channel::{Channel, ChannelCategory, StreamTemplate};
use crate::channel::m3u::parse_m3u;
use crate::epg::EpgData;
use crate::error::OperatorError;
use crate::stream::StreamUrl;
use super::traits::{Operator, Result};

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

    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        let login_base = self.login_base.clone();
        let referer = format!("{}/", login_base);

        // Step 0 — Seed session cookies by visiting the login page.
        // Errors suppressed: the API calls below will fail clearly if the host is down.
        let _ = self
            .client
            .get(&referer)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .send()
            .await;

        // Step 1 — POST /api/access
        let resp = self
            .client
            .post(format!("{}/api/access", login_base))
            .header("Origin", &login_base)
            .header("Referer", &referer)
            .header("Accept", "application/json, text/plain, */*")
            .json(&serde_json::json!({}))
            .send()
            .await?;

        // Capture XSRF-TOKEN before consuming the body (double-submit cookie pattern).
        let xsrf_token: Option<String> = resp
            .cookies()
            .find(|c| c.name().eq_ignore_ascii_case("xsrf-token"))
            .map(|c| c.value().to_string());

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::UnexpectedResponse {
                status: status.as_u16(),
                body,
            });
        }

        // `nextStep == "feedback"` means Orange rejected the request (CSRF/bot-check).
        let body1: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if matches!(
            body1.get("nextStep").and_then(|v| v.as_str()),
            Some("feedback")
        ) {
            return Err(OperatorError::AuthFailed(
                "orange.fr rejected /api/access (CSRF or bot-check failed)".into(),
            ));
        }

        // Step 2 — POST /api/login
        let mut builder = self
            .client
            .post(format!("{}/api/login", login_base))
            .header("Origin", &login_base)
            .header("Referer", &referer)
            .header("Accept", "application/json, text/plain, */*")
            .json(&serde_json::json!({
                "login": username,
                "loginOrigin": "input"
            }));
        if let Some(ref tok) = xsrf_token {
            builder = builder.header("X-XSRF-TOKEN", tok);
        }
        let resp = builder.send().await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if matches!(status.as_u16(), 400 | 401 | 403) {
                return Err(OperatorError::InvalidCredentials);
            }
            return Err(OperatorError::UnexpectedResponse {
                status: status.as_u16(),
                body,
            });
        }
        let body2: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        if matches!(
            body2.get("nextStep").and_then(|v| v.as_str()),
            Some("feedback")
        ) {
            return Err(OperatorError::InvalidCredentials);
        }

        // Step 3 — POST /api/password
        let mut builder = self
            .client
            .post(format!("{}/api/password", login_base))
            .header("Origin", &login_base)
            .header("Referer", &referer)
            .header("Accept", "application/json, text/plain, */*")
            .json(&serde_json::json!({
                "password": password,
                "remember": true
            }));
        if let Some(ref tok) = xsrf_token {
            builder = builder.header("X-XSRF-TOKEN", tok);
        }
        let resp = builder.send().await?;

        let status = resp.status();
        // Read cookies before consuming the body.
        let wassup = resp
            .cookies()
            .find(|c| c.name() == "wassup")
            .map(|c| c.value().to_string());

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if matches!(status.as_u16(), 400 | 401 | 403) {
                return Err(OperatorError::InvalidCredentials);
            }
            return Err(OperatorError::UnexpectedResponse {
                status: status.as_u16(),
                body,
            });
        }

        match wassup {
            Some(v) => self.wassup = Some(v),
            None => {
                // `nextStep == "feedback"` with 200 means bad password.
                let body3: serde_json::Value =
                    resp.json().await.unwrap_or(serde_json::Value::Null);
                if matches!(
                    body3.get("nextStep").and_then(|v| v.as_str()),
                    Some("feedback")
                ) {
                    return Err(OperatorError::InvalidCredentials);
                }
                return Err(OperatorError::AuthFailed("wassup cookie not received".into()));
            }
        }

        // Immediately fetch the tv_token while the session is fresh.
        self.ensure_tv_token().await?;

        Ok(())
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
        assert!(matches!(err, OperatorError::InvalidCredentials));
    }

    // ---------------------------------------------------------------------------
    // Test 3 — 500 from channels endpoint falls back to M3U
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
    // Test 4 — API response is parsed correctly
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
    // Test 5 — stream resolution returns URL from API JSON
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
