pub mod xmltv;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgProgram {
    pub channel_id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub stop: DateTime<Utc>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct EpgData {
    pub programs: Vec<EpgProgram>,
}

impl EpgData {
    pub fn current_program(&self, channel_id: &str) -> Option<&EpgProgram> {
        let now = chrono::Utc::now();
        self.programs.iter().find(|p| {
            p.channel_id == channel_id && p.start <= now && p.stop > now
        })
    }
}
