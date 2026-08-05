use thiserror::Error;

#[derive(Debug, Error)]
pub enum OperatorError {
    #[error("invalid credentials")]
    InvalidCredentials,
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("token refresh failed: {0}")]
    TokenRefreshFailed(String),
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("unexpected response: status {status}, body: {body}")]
    UnexpectedResponse { status: u16, body: String },
    #[error("channel list parse error: {0}")]
    ParseChannels(String),
}

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("stream resolution failed: {0}")]
    ResolutionFailed(String),
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
}

#[derive(Debug, Error)]
pub enum EpgError {
    #[error("EPG fetch failed: {0}")]
    FetchFailed(String),
    #[error("EPG parse error: {0}")]
    ParseError(String),
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config directory not found")]
    NoDirFound,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("toml serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Debug, Error)]
pub enum LogoCacheError {
    #[error("cache directory not found")]
    NoDirFound,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
}
