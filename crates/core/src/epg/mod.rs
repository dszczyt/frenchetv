pub mod matcher;
pub mod provider;
pub mod source;
pub mod xmltv;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpgProgram {
    pub channel_id: String,
    pub title: String,
    pub start: DateTime<Utc>,
    pub stop: DateTime<Utc>,
    pub description: Option<String>,
}

/// Programmes indexed per channel, each channel's list sorted by start time.
///
/// The guide draws several rows over a multi-hour window every frame. A flat
/// `Vec` with a linear scan made that O(programmes) per row per frame, which at
/// a realistic ~100k programmes is unusable; a hash plus a binary search is
/// not.
#[derive(Debug, Clone, Default)]
pub struct EpgData {
    by_channel: HashMap<String, Vec<EpgProgram>>,
}

impl EpgData {
    pub fn from_programs(programs: Vec<EpgProgram>) -> Self {
        let mut by_channel: HashMap<String, Vec<EpgProgram>> = HashMap::new();
        for p in programs {
            by_channel.entry(p.channel_id.clone()).or_default().push(p);
        }
        for list in by_channel.values_mut() {
            list.sort_by_key(|p| p.start);
        }
        Self { by_channel }
    }

    pub fn is_empty(&self) -> bool {
        self.by_channel.values().all(|v| v.is_empty())
    }

    /// Total number of programmes across all channels.
    pub fn len(&self) -> usize {
        self.by_channel.values().map(Vec::len).sum()
    }

    pub fn channels(&self) -> impl Iterator<Item = &str> {
        self.by_channel.keys().map(String::as_str)
    }

    /// All programmes for `channel_id`, sorted by start time.
    pub fn channel_programs(&self, channel_id: &str) -> &[EpgProgram] {
        self.by_channel
            .get(channel_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Programmes overlapping `[from, to)`, in order.
    ///
    /// Includes a programme already in progress at `from` — that is the one the
    /// guide most needs to draw, and it starts before the window.
    pub fn programs_in_window(
        &self,
        channel_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> &[EpgProgram] {
        let progs = self.channel_programs(channel_id);
        if progs.is_empty() || to <= from {
            return &[];
        }

        // First programme starting at or after `from` …
        let i = progs.partition_point(|p| p.start < from);
        // … then step back one if the preceding programme is still running.
        let begin = if i > 0 && progs[i - 1].stop > from {
            i - 1
        } else {
            i
        };
        let end = progs.partition_point(|p| p.start < to);
        if end <= begin {
            return &[];
        }
        &progs[begin..end]
    }

    pub fn current_program(&self, channel_id: &str) -> Option<&EpgProgram> {
        self.program_at(channel_id, Utc::now())
    }

    pub fn program_at(&self, channel_id: &str, at: DateTime<Utc>) -> Option<&EpgProgram> {
        let progs = self.channel_programs(channel_id);
        let i = progs.partition_point(|p| p.start <= at);
        progs.get(i.checked_sub(1)?).filter(|p| p.stop > at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 19, h, 0, 0).unwrap()
    }

    fn prog(ch: &str, title: &str, start: u32, stop: u32) -> EpgProgram {
        EpgProgram {
            channel_id: ch.into(),
            title: title.into(),
            start: at(start),
            stop: at(stop),
            description: None,
        }
    }

    fn sample() -> EpgData {
        // Deliberately out of order — from_programs must sort.
        EpgData::from_programs(vec![
            prog("tf1", "Film", 21, 23),
            prog("tf1", "Journal", 20, 21),
            prog("f2", "Docu", 20, 22),
        ])
    }

    #[test]
    fn indexes_and_sorts_by_channel() {
        let d = sample();
        assert_eq!(d.len(), 3);
        let tf1 = d.channel_programs("tf1");
        assert_eq!(tf1.len(), 2);
        assert_eq!(tf1[0].title, "Journal");
        assert_eq!(tf1[1].title, "Film");
        assert!(d.channel_programs("unknown").is_empty());
    }

    #[test]
    fn window_includes_a_programme_already_running() {
        let d = sample();
        // Window opens mid-Journal; Journal must still be returned.
        let w = d.programs_in_window("tf1", at(20) + chrono::Duration::minutes(30), at(22));
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].title, "Journal");
        assert_eq!(w[1].title, "Film");
    }

    #[test]
    fn window_excludes_programmes_outside_it() {
        let d = sample();
        let w = d.programs_in_window("tf1", at(23), at(23) + chrono::Duration::hours(1));
        assert!(w.is_empty(), "nothing starts at or after 23:00");

        let w = d.programs_in_window("tf1", at(20), at(21));
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].title, "Journal");
    }

    #[test]
    fn empty_window_and_unknown_channel_are_empty() {
        let d = sample();
        assert!(d.programs_in_window("tf1", at(21), at(21)).is_empty());
        assert!(d.programs_in_window("nope", at(20), at(23)).is_empty());
    }

    #[test]
    fn program_at_respects_boundaries() {
        let d = sample();
        assert_eq!(d.program_at("tf1", at(20)).unwrap().title, "Journal");
        // A programme's stop is exclusive: 21:00 belongs to Film, not Journal.
        assert_eq!(d.program_at("tf1", at(21)).unwrap().title, "Film");
        assert!(d.program_at("tf1", at(23)).is_none(), "stop is exclusive");
        assert!(d.program_at("tf1", at(19)).is_none());
    }
}
