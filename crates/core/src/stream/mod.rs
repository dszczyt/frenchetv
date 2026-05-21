use url::Url;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamUrl {
    pub url: Url,
    pub auth_header: Option<String>,
}

impl StreamUrl {
    pub fn direct(url: Url) -> Self { Self { url, auth_header: None } }
    pub fn authenticated(url: Url, bearer_token: &str) -> Self {
        Self { url, auth_header: Some(format!("Bearer {}", bearer_token)) }
    }
}
