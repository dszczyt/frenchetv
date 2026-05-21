use serde::{Deserialize, Serialize};
use crate::error::ConfigError;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    pub operator: OperatorConfig,
    pub preferences: Preferences,
    pub cache: CacheConfig,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct OperatorConfig {
    pub kind: String,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preferences {
    pub language: String,
    pub parental_lock: bool,
    pub startup_channel: Option<String>,
}

impl Default for Preferences {
    fn default() -> Self { Self { language: "fr".into(), parental_lock: false, startup_channel: None } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub epg_ttl_minutes: u32,
    pub logo_ttl_hours: u32,
}

impl Default for CacheConfig {
    fn default() -> Self { Self { epg_ttl_minutes: 60, logo_ttl_hours: 24 } }
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> { Ok(Self::default()) }
    pub fn save(&self) -> Result<(), ConfigError> { Ok(()) }
    pub fn config_path() -> Result<std::path::PathBuf, ConfigError> {
        Ok(std::path::PathBuf::from("/tmp/frenchetv/config.toml"))
    }
}
