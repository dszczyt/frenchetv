use crate::error::ConfigError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
    fn default() -> Self {
        Self {
            language: "fr".into(),
            parental_lock: false,
            startup_channel: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub epg_ttl_minutes: u32,
    pub logo_ttl_hours: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            epg_ttl_minutes: 60,
            logo_ttl_hours: 24,
        }
    }
}

impl Config {
    /// Returns `~/.config/frenchetv/config.toml` (Linux/macOS) or
    /// `%APPDATA%\frenchetv\config.toml` (Windows).
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        let base = dirs::config_dir().ok_or(ConfigError::NoDirFound)?;
        Ok(base.join("frenchetv").join("config.toml"))
    }

    /// Load config from disk, returning `Config::default()` if the file doesn't exist.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)?;
        let cfg: Self = toml::from_str(&content)?;
        Ok(cfg)
    }

    /// Persist config to disk, creating the directory if needed.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_valid_config() {
        let toml_str = r#"
[operator]
kind = "orange"
username = "user@example.com"

[preferences]
language = "fr"
parental_lock = false

[cache]
epg_ttl_minutes = 60
logo_ttl_hours = 24
"#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.operator.kind, "orange");
        assert_eq!(cfg.operator.username, "user@example.com");
        assert_eq!(cfg.preferences.language, "fr");
        assert!(!cfg.preferences.parental_lock);
        assert_eq!(cfg.cache.epg_ttl_minutes, 60);
    }

    #[test]
    fn test_config_roundtrip() {
        let cfg = Config {
            operator: OperatorConfig {
                kind: "bouygues".into(),
                username: "bob@bbox.fr".into(),
            },
            preferences: Preferences {
                language: "fr".into(),
                parental_lock: false,
                startup_channel: Some("tf1".into()),
            },
            cache: CacheConfig::default(),
        };
        let serialized = toml::to_string(&cfg).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.operator.kind, "bouygues");
        assert_eq!(
            deserialized.preferences.startup_channel.as_deref(),
            Some("tf1")
        );
    }

    #[test]
    fn test_config_path_is_absolute() {
        let path = Config::config_path().unwrap();
        assert!(path.is_absolute());
        assert!(path.to_str().unwrap().contains("frenchetv"));
    }

    #[test]
    fn test_load_returns_default_when_no_file() {
        let cfg = Config::default();
        assert_eq!(cfg.operator.kind, "");
        assert_eq!(cfg.preferences.language, "fr");
        assert!(!cfg.preferences.parental_lock);
        assert_eq!(cfg.cache.epg_ttl_minutes, 60);
    }
}
