use crate::error::ConfigError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    pub operator: OperatorConfig,
    pub preferences: Preferences,
    pub cache: CacheConfig,
    /// Defaulted so a config.toml written before this existed still loads.
    #[serde(default)]
    pub epg: EpgConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgConfig {
    /// XMLTV feed to pull the schedule from. Configurable because public feeds
    /// move and die; see docs/operators.md.
    pub feed_url: String,
}

impl Default for EpgConfig {
    fn default() -> Self {
        Self {
            feed_url: crate::epg::provider::DEFAULT_FEED_URL.to_string(),
        }
    }
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
        let base = crate::paths::config_dir().ok_or(ConfigError::NoDirFound)?;
        Ok(base.join("config.toml"))
    }

    /// Load config from disk, returning `Config::default()` if the file doesn't exist.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)?;
        let mut cfg: Self = toml::from_str(&content)?;
        cfg.migrate();
        Ok(cfg)
    }

    /// Repair settings that were valid when written but are not any more.
    fn migrate(&mut self) {
        if crate::epg::provider::RETIRED_FEED_URLS.contains(&self.epg.feed_url.as_str()) {
            tracing::info!(
                old = %self.epg.feed_url,
                "EPG feed URL is retired; falling back to the current default"
            );
            self.epg.feed_url = crate::epg::provider::DEFAULT_FEED_URL.to_string();
        }
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
            epg: EpgConfig::default(),
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
    fn a_retired_feed_url_migrates_to_the_current_default() {
        // A dead URL persists in config.toml once written, so changing the
        // constant alone would never reach an existing install.
        let toml_str = format!(
            "[operator]\nkind = \"orange\"\nusername = \"u\"\n\n\
             [preferences]\nlanguage = \"fr\"\nparental_lock = false\n\n\
             [cache]\nepg_ttl_minutes = 60\nlogo_ttl_hours = 24\n\n\
             [epg]\nfeed_url = \"{}\"\n",
            crate::epg::provider::RETIRED_FEED_URLS[0]
        );
        let mut cfg: Config = toml::from_str(&toml_str).unwrap();
        cfg.migrate();
        assert_eq!(cfg.epg.feed_url, crate::epg::provider::DEFAULT_FEED_URL);
    }

    #[test]
    fn a_feed_url_the_user_chose_is_left_alone() {
        let mut cfg = Config {
            epg: EpgConfig {
                feed_url: "https://example.invalid/mine.xml".into(),
            },
            ..Default::default()
        };
        cfg.migrate();
        assert_eq!(cfg.epg.feed_url, "https://example.invalid/mine.xml");
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
