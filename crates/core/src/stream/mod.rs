use url::Url;
use serde::{Deserialize, Serialize};

/// A resolved stream URL plus any HTTP headers the player must send with
/// every segment request (Origin, Referer, User-Agent, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamUrl {
    pub url: Url,
    pub auth_header: Option<String>,
    /// Extra headers forwarded to the media player for CDN authentication.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

impl StreamUrl {
    pub fn direct(url: Url) -> Self {
        Self { url, auth_header: None, headers: vec![] }
    }

    pub fn authenticated(url: Url, bearer_token: &str) -> Self {
        Self { url, auth_header: Some(format!("Bearer {}", bearer_token)), headers: vec![] }
    }

    /// Attach an extra (name, value) header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}
