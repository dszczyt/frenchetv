pub mod m3u;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub logo_url: Option<String>,
    pub number: Option<u32>,
    pub category: ChannelCategory,
    pub stream_template: StreamTemplate,
    /// True when the channel is not available on this platform (e.g. W_PC absent
    /// from allowedDeviceCategories). Hidden by default; shown via toggle.
    #[serde(default)]
    pub locked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ChannelCategory {
    Generalist,
    News,
    Sports,
    Entertainment,
    Kids,
    Documentary,
    Music,
    Other(String),
}

impl ChannelCategory {
    /// Parse a group-title string (from M3U or operator API) into a category.
    pub fn from_group_title(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "généraliste" | "generaliste" | "general" => Self::Generalist,
            "info" | "news" | "actualités" | "actualites" => Self::News,
            "sport" | "sports" => Self::Sports,
            "divertissement" | "entertainment" => Self::Entertainment,
            "enfants" | "kids" | "jeunesse" => Self::Kids,
            "documentaire" | "documentary" | "culture" => Self::Documentary,
            "musique" | "music" => Self::Music,
            other => Self::Other(other.to_string()),
        }
    }

    /// Human-readable label for the filter UI.
    pub fn label(&self) -> &str {
        match self {
            Self::Generalist => "Généraliste",
            Self::News => "Info",
            Self::Sports => "Sport",
            Self::Entertainment => "Divertissement",
            Self::Kids => "Enfants",
            Self::Documentary => "Docs",
            Self::Music => "Musique",
            Self::Other(s) => s.as_str(),
        }
    }

    /// All fixed categories (for filter tab rendering).
    pub fn fixed() -> &'static [ChannelCategory] {
        static FIXED: [ChannelCategory; 7] = [
            ChannelCategory::Generalist,
            ChannelCategory::News,
            ChannelCategory::Sports,
            ChannelCategory::Entertainment,
            ChannelCategory::Kids,
            ChannelCategory::Documentary,
            ChannelCategory::Music,
        ];
        &FIXED
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StreamTemplate {
    /// Direct HLS/DASH URL — no further resolution needed.
    Direct(Url),
    /// Requires operator-specific resolution (auth header injection, token swap).
    Authenticated { base_url: Url },
}
