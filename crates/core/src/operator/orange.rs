use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;

use crate::channel::{Channel, ChannelCategory, StreamTemplate};
use crate::channel::m3u::parse_m3u;
use crate::epg::EpgData;
use crate::error::OperatorError;
use crate::stream::StreamUrl;
use super::traits::{Operator, Result};

/// These client credentials identify the Orange TV application.
/// They are embedded in the official Orange TV app binary.
const ORANGE_CLIENT_ID: &str = "f9ee4b08-50ec-4dfd-8693-e7a6a27de3cc";
const ORANGE_CLIENT_SECRET: &str = "secret";

/// Fallback M3U shipped with the binary.
const FALLBACK_M3U: &str = include_str!("../../../../assets/channels/orange.m3u");

pub struct OrangeOperator {
    client: reqwest::Client,
    api_base: String,
    sso_base: String,
    pub(crate) access_token: Option<String>,
    refresh_token: Option<String>,
}

impl OrangeOperator {
    pub fn new() -> Self {
        Self::new_with_bases(
            "https://rp-iptv.orange.fr",
            "https://sso.orange.fr",
        )
    }

    /// Constructs an operator pointing at custom base URLs — used in tests.
    pub fn new_with_bases(api_base: &str, sso_base: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .cookie_store(true)
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client build"),
            api_base: api_base.to_string(),
            sso_base: sso_base.to_string(),
            access_token: None,
            refresh_token: None,
        }
    }

    /// Try to refresh the access token using the stored refresh token.
    async fn refresh_access_token(&mut self) -> Result<()> {
        let refresh_token = self.refresh_token.as_ref()
            .ok_or_else(|| OperatorError::TokenRefreshFailed("no refresh token".into()))?
            .clone();

        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", ORANGE_CLIENT_ID),
            ("client_secret", ORANGE_CLIENT_SECRET),
        ];

        let resp = self.client
            .post(format!("{}/oauth/v2/token", self.sso_base))
            .form(&params)
            .send()
            .await?;

        if resp.status() == 401 || resp.status() == 400 {
            return Err(OperatorError::TokenRefreshFailed("refresh token rejected".into()));
        }

        let body: TokenResponse = resp.error_for_status()?.json().await?;
        self.access_token = Some(body.access_token);
        if let Some(rt) = body.refresh_token {
            self.refresh_token = Some(rt);
        }
        Ok(())
    }
}

impl Default for OrangeOperator {
    fn default() -> Self { Self::new() }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    expires_in: Option<u64>,
}

/// Shape of one item in the Orange channel list response.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrangeChannel {
    channel_id: String,
    name: String,
    #[serde(default)]
    logo_url: Option<String>,
    #[serde(default)]
    channel_number: Option<u32>,
    #[serde(default)]
    genre: Option<String>,
    hls_url: Option<String>,
    #[serde(default)]
    dash_url: Option<String>,
}

#[async_trait]
impl Operator for OrangeOperator {
    fn name(&self) -> &'static str { "Orange TV" }
    fn requires_auth(&self) -> bool { true }

    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        let params = [
            ("grant_type", "password"),
            ("username", username),
            ("password", password),
            ("client_id", ORANGE_CLIENT_ID),
            ("client_secret", ORANGE_CLIENT_SECRET),
        ];

        let resp = self.client
            .post(format!("{}/oauth/v2/token", self.sso_base))
            .form(&params)
            .send()
            .await?;

        if resp.status() == 401 || resp.status() == 400 {
            return Err(OperatorError::InvalidCredentials);
        }

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::UnexpectedResponse { status, body });
        }

        let body: TokenResponse = resp.json().await?;
        self.access_token = Some(body.access_token);
        self.refresh_token = body.refresh_token;
        Ok(())
    }

    async fn fetch_channels(&self) -> Result<Vec<Channel>> {
        let url = format!("{}/EPG/JSON/getChannelList", self.api_base);
        let resp = self.client.get(&url)
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

                let channels = channels_raw.into_iter().filter_map(|c| {
                    let url_str = c.hls_url.or(c.dash_url)?;
                    let url = url::Url::parse(&url_str).ok()?;
                    Some(Channel {
                        id: c.channel_id,
                        name: c.name,
                        logo_url: c.logo_url,
                        number: c.channel_number,
                        category: ChannelCategory::from_group_title(
                            c.genre.as_deref().unwrap_or(""),
                        ),
                        stream_template: StreamTemplate::Authenticated { base_url: url },
                    })
                }).collect();

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
        let token = self.access_token.as_deref().unwrap_or("");
        match &channel.stream_template {
            StreamTemplate::Direct(url) => Ok(StreamUrl::direct(url.clone())),
            StreamTemplate::Authenticated { base_url } => {
                Ok(StreamUrl::authenticated(base_url.clone(), token))
            }
        }
    }

    async fn fetch_epg(&self, _hours: u8) -> Result<Option<EpgData>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{MockServer, Mock, ResponseTemplate};
    use wiremock::matchers::{method, path, body_string_contains};
    use serde_json::json;

    #[tokio::test]
    async fn test_authenticate_success() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/oauth/v2/token"))
            .and(body_string_contains("grant_type=password"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "tok_abc123",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "refresh_token": "refresh_xyz"
                })),
            )
            .mount(&mock)
            .await;

        let mut op = OrangeOperator::new_with_bases(&mock.uri(), &mock.uri());
        op.authenticate("user@orange.fr", "pass1234").await.unwrap();
        assert_eq!(op.access_token.as_deref(), Some("tok_abc123"));
    }

    #[tokio::test]
    async fn test_authenticate_invalid_credentials() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/oauth/v2/token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "invalid_grant"
            })))
            .mount(&mock)
            .await;

        let mut op = OrangeOperator::new_with_bases(&mock.uri(), &mock.uri());
        let err = op.authenticate("bad@example.com", "wrong").await.unwrap_err();
        assert!(matches!(err, OperatorError::InvalidCredentials));
    }

    #[tokio::test]
    async fn test_fetch_channels_falls_back_on_api_error() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/EPG/JSON/getChannelList"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let op = OrangeOperator::new_with_bases(&mock.uri(), &mock.uri());
        let channels = op.fetch_channels().await.unwrap();
        // Fallback M3U must supply at least 1 channel
        assert!(!channels.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_channels_parses_api_response() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/EPG/JSON/getChannelList"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([
                    {
                        "channelId": "TF1",
                        "name": "TF1",
                        "logoUrl": "https://logos.example.com/tf1.png",
                        "channelNumber": 1,
                        "genre": "Généraliste",
                        "hlsUrl": "https://iptv.example.com/TF1/playlist.m3u8"
                    },
                    {
                        "channelId": "BFMTV",
                        "name": "BFM TV",
                        "channelNumber": 15,
                        "genre": "Info",
                        "hlsUrl": "https://iptv.example.com/BFMTV/playlist.m3u8"
                    }
                ])),
            )
            .mount(&mock)
            .await;

        let op = OrangeOperator::new_with_bases(&mock.uri(), &mock.uri());
        let channels = op.fetch_channels().await.unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].id, "TF1");
        assert_eq!(channels[0].category, ChannelCategory::Generalist);
        assert_eq!(channels[1].category, ChannelCategory::News);
    }

    #[tokio::test]
    async fn test_resolve_stream_authenticated() {
        let op = OrangeOperator {
            client: reqwest::Client::new(),
            api_base: "http://api".into(),
            sso_base: "http://sso".into(),
            access_token: Some("mytoken".into()),
            refresh_token: None,
        };

        let channel = Channel {
            id: "TF1".into(),
            name: "TF1".into(),
            logo_url: None,
            number: Some(1),
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Authenticated {
                base_url: url::Url::parse("https://iptv.example.com/TF1/playlist.m3u8").unwrap(),
            },
        };

        let stream = op.resolve_stream(&channel).await.unwrap();
        assert_eq!(stream.auth_header.as_deref(), Some("Bearer mytoken"));
    }
}
