use async_trait::async_trait;
use serde::Deserialize;
use tracing::warn;

use crate::channel::{Channel, ChannelCategory, StreamTemplate};
use crate::channel::m3u::parse_m3u;
use crate::epg::EpgData;
use crate::error::OperatorError;
use crate::stream::StreamUrl;
use super::traits::{Operator, Result};

const FALLBACK_M3U: &str = include_str!("../../../../assets/channels/bouygues.m3u");

pub struct BouyguesOperator {
    client: reqwest::Client,
    base_url: String,
    pub(crate) access_token: Option<String>,
}

impl BouyguesOperator {
    pub fn new() -> Self {
        Self::new_with_base("https://api.bbox.fr")
    }

    pub fn new_with_base(base_url: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .cookie_store(true)
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client build"),
            base_url: base_url.to_string(),
            access_token: None,
        }
    }
}

impl Default for BouyguesOperator {
    fn default() -> Self { Self::new() }
}

#[derive(Deserialize)]
struct LoginResponse {
    #[serde(rename = "token", alias = "access_token")]
    token: Option<String>,
}

#[derive(Deserialize)]
struct BouyguesChannel {
    id: u64,
    #[serde(rename = "label")]
    name: String,
    #[serde(default)]
    logo: Option<String>,
    #[serde(rename = "position", default)]
    number: Option<u32>,
    #[serde(rename = "category", default)]
    category: Option<String>,
    #[serde(rename = "hls_url", default)]
    hls_url: Option<String>,
}

#[async_trait]
impl Operator for BouyguesOperator {
    fn name(&self) -> &'static str { "Bouygues Bbox" }
    fn requires_auth(&self) -> bool { true }

    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        use serde_json::json;

        let resp = self.client
            .post(format!("{}/api/v1/login", self.base_url))
            .json(&json!({ "login": username, "password": password }))
            .send()
            .await?;

        if resp.status() == 401 || resp.status() == 403 {
            return Err(OperatorError::InvalidCredentials);
        }

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::UnexpectedResponse { status, body });
        }

        // Bbox may return the token in the body or rely on cookies alone.
        let body: LoginResponse = resp.json().await
            .unwrap_or(LoginResponse { token: None });
        self.access_token = body.token;
        Ok(())
    }

    async fn fetch_channels(&self) -> Result<Vec<Channel>> {
        let url = format!("{}/api/v1/bouyguestv/channels", self.base_url);
        let mut req = self.client.get(&url)
            .timeout(std::time::Duration::from_secs(5));
        if let Some(token) = &self.access_token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let raw: Vec<BouyguesChannel> = match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Bouygues: channel parse error: {}, using fallback", e);
                        return Ok(parse_m3u(FALLBACK_M3U));
                    }
                };

                let channels = raw.into_iter().filter_map(|c| {
                    let hls = c.hls_url?;
                    let url = url::Url::parse(&hls).ok()?;
                    Some(Channel {
                        id: c.id.to_string(),
                        name: c.name,
                        logo_url: c.logo,
                        number: c.number,
                        category: ChannelCategory::from_group_title(
                            c.category.as_deref().unwrap_or(""),
                        ),
                        stream_template: StreamTemplate::Authenticated { base_url: url },
                        locked: false,
                    })
                }).collect();

                Ok(channels)
            }
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED
                   || r.status() == reqwest::StatusCode::FORBIDDEN => {
                Err(OperatorError::InvalidCredentials)
            }
            Ok(r) => {
                warn!("Bouygues: channels returned {}, using fallback", r.status());
                Ok(parse_m3u(FALLBACK_M3U))
            }
            Err(e) => {
                warn!("Bouygues: channels network error: {}, using fallback", e);
                Ok(parse_m3u(FALLBACK_M3U))
            }
        }
    }

    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl> {
        let token = self.access_token.as_deref().unwrap_or("");
        match &channel.stream_template {
            StreamTemplate::Direct(url) => Ok(StreamUrl::direct(url.clone())),
            StreamTemplate::Authenticated { base_url } => {
                if token.is_empty() {
                    Ok(StreamUrl::direct(base_url.clone()))
                } else {
                    let mut url = base_url.clone();
                    url.query_pairs_mut().append_pair("access_token", token);
                    Ok(StreamUrl::direct(url))
                }
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
    use wiremock::matchers::{method, path};
    use serde_json::json;

    #[tokio::test]
    async fn test_authenticate_success() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/login"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "token": "bbox_tok_abc",
                    "expires": 3600
                })),
            )
            .mount(&mock)
            .await;

        let mut op = BouyguesOperator::new_with_base(&mock.uri());
        op.authenticate("user@bbox.fr", "pass").await.unwrap();
        assert_eq!(op.access_token.as_deref(), Some("bbox_tok_abc"));
    }

    #[tokio::test]
    async fn test_authenticate_invalid_credentials() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/login"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock)
            .await;

        let mut op = BouyguesOperator::new_with_base(&mock.uri());
        let err = op.authenticate("bad@bbox.fr", "wrong").await.unwrap_err();
        assert!(matches!(err, OperatorError::InvalidCredentials));
    }

    #[tokio::test]
    async fn test_fetch_channels_parses_api() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/bouyguestv/channels"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([
                    {
                        "id": 1,
                        "label": "TF1",
                        "logo": "https://logos.example.com/tf1.png",
                        "position": 1,
                        "category": "Généraliste",
                        "hls_url": "https://bbox.example.com/tf1/index.m3u8"
                    },
                    {
                        "id": 44,
                        "label": "Eurosport 1",
                        "position": 44,
                        "category": "Sport",
                        "hls_url": "https://bbox.example.com/eurosport/index.m3u8"
                    }
                ])),
            )
            .mount(&mock)
            .await;

        let op = BouyguesOperator::new_with_base(&mock.uri());
        let channels = op.fetch_channels().await.unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name, "TF1");
        assert_eq!(channels[0].category, ChannelCategory::Generalist);
        assert_eq!(channels[1].category, ChannelCategory::Sports);
    }

    #[tokio::test]
    async fn test_fetch_channels_fallback_on_500() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v1/bouyguestv/channels"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let op = BouyguesOperator::new_with_base(&mock.uri());
        let channels = op.fetch_channels().await.unwrap();
        assert!(!channels.is_empty()); // fallback M3U
    }

    #[tokio::test]
    async fn test_resolve_stream_appends_token() {
        let op = BouyguesOperator {
            client: reqwest::Client::new(),
            base_url: "http://api".into(),
            access_token: Some("mytoken".into()),
        };

        let channel = Channel {
            id: "1".into(),
            name: "TF1".into(),
            logo_url: None,
            number: Some(1),
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Authenticated {
                base_url: url::Url::parse("https://bbox.example.com/tf1/index.m3u8").unwrap(),
            },
            locked: false,
        };

        let stream = op.resolve_stream(&channel).await.unwrap();
        assert!(stream.url.as_str().contains("access_token=mytoken"));
        assert!(stream.auth_header.is_none());
    }
}
