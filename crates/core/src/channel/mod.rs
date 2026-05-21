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
    pub fn from_group_title(s: &str) -> Self { Self::Other(s.to_string()) }
    pub fn label(&self) -> &str { "Unknown" }
    pub fn fixed() -> &'static [ChannelCategory] { &[] }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StreamTemplate {
    Direct(Url),
    Authenticated { base_url: Url },
}
