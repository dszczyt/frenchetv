use serde::{Deserialize, Serialize};
use url::Url;

/// Widevine DRM protection parameters extracted from the stream API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtectionData {
    /// Widevine license server URL.
    pub la_url: String,
    /// Raw PSSH box bytes (Widevine system), base64-decoded from the operator API
    /// or extracted from the DASH MPD ContentProtection element.
    /// If None the proxy will fetch the MPD and extract it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pssh: Option<Vec<u8>>,
    /// HTTP headers to attach to the license POST request (e.g., tv_token).
    #[serde(default)]
    pub license_headers: Vec<(String, String)>,
}

/// A resolved stream URL plus any HTTP headers the player must send with
/// every segment request (Origin, Referer, User-Agent, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamUrl {
    pub url: Url,
    pub auth_header: Option<String>,
    /// Extra headers forwarded to the media player for CDN authentication.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// DRM protection data. `Some` if stream is CENC/Widevine encrypted.
    /// When set, the desktop UI starts a local DRM proxy before passing the
    /// URL to mpv so that segments are pre-decrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protection: Option<ProtectionData>,
}

impl StreamUrl {
    pub fn direct(url: Url) -> Self {
        Self {
            url,
            auth_header: None,
            headers: vec![],
            protection: None,
        }
    }

    pub fn authenticated(url: Url, bearer_token: &str) -> Self {
        Self {
            url,
            auth_header: Some(format!("Bearer {}", bearer_token)),
            headers: vec![],
            protection: None,
        }
    }

    /// Attach an extra (name, value) header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}
