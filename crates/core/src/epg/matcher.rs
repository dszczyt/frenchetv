//! Match operator channels to XMLTV feed channels.
//!
//! Feed ids (`TF1.fr`) have nothing to do with operator ids, so the two sides
//! are matched on name. Names disagree in predictable ways — accents, casing,
//! an `HD` suffix, punctuation — so both sides are normalised before comparing.
//!
//! Unmatched channels are reported rather than silently dropped: a mismatch and
//! "this feed has no schedule for that channel" look identical in the UI, and
//! without a report there is nothing to tell them apart.

use super::xmltv::XmltvChannel;
use crate::channel::Channel;
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct MatchReport {
    /// Feed channel id -> our channel id. Ready to hand to `parse_programs`.
    pub matched: HashMap<String, String>,
    /// Names of our channels with no counterpart in the feed.
    pub unmatched: Vec<String>,
}

impl MatchReport {
    pub fn matched_count(&self) -> usize {
        self.matched.len()
    }
}

/// Overrides for names that do not survive normalisation.
///
/// Keyed by our normalised channel name; the value is the normalised feed name
/// to accept as equivalent.
/// `normalize` drops `+`, so "Canal+" becomes "canal" while a feed spelling it
/// "Canal Plus" becomes "canalplus". These are the pairs that difference (and
/// a few other known spellings) produces.
const OVERRIDES: &[(&str, &str)] = &[
    ("canal", "canalplus"),
    ("canaldecale", "canalplusdecale"),
    ("canalcinema", "canalpluscinema"),
    ("canalsport", "canalplussport"),
];

/// Strip accents, casing, punctuation and broadcast-quality suffixes.
///
/// `"France 2 HD"`, `"france2"` and `"FRANCE 2"` all normalise to `"france2"`.
pub fn normalize(name: &str) -> String {
    let lowered: String = name
        .chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'ñ' => 'n',
            'ÿ' => 'y',
            other => other,
        })
        .collect::<String>()
        .to_lowercase();

    // Keep alphanumerics only; punctuation and spacing carry no signal here.
    let compact: String = lowered.chars().filter(|c| c.is_alphanumeric()).collect();

    // Drop a trailing quality marker. Only at the end — "hd" inside a name
    // (say a channel actually called "HDmovies") is part of the name.
    for suffix in ["uhd", "4k", "fullhd", "hd", "sd"] {
        if let Some(stripped) = compact.strip_suffix(suffix) {
            if !stripped.is_empty() {
                return stripped.to_string();
            }
        }
    }
    compact
}

/// Match `ours` against the feed's channels.
pub fn match_channels(ours: &[Channel], feed: &[XmltvChannel]) -> MatchReport {
    // Normalised feed name -> feed id. First writer wins, so an earlier
    // channel is not displaced by a later one sharing a normalised name.
    let mut by_name: HashMap<String, &str> = HashMap::new();
    for c in feed {
        for name in &c.display_names {
            by_name.entry(normalize(name)).or_insert(&c.id);
        }
        // Some feeds carry a usable name in the id itself ("TF1.fr").
        let id_stem = c.id.split('.').next().unwrap_or(&c.id);
        by_name.entry(normalize(id_stem)).or_insert(&c.id);
    }

    let overrides: HashMap<&str, &str> = OVERRIDES.iter().copied().collect();

    let mut report = MatchReport::default();
    for ch in ours {
        let key = normalize(&ch.name);
        let feed_id = by_name.get(key.as_str()).or_else(|| {
            overrides
                .get(key.as_str())
                .and_then(|alias| by_name.get(*alias))
        });

        match feed_id {
            Some(id) => {
                report.matched.insert((*id).to_string(), ch.id.clone());
            }
            None => report.unmatched.push(ch.name.clone()),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelCategory, StreamTemplate};

    fn chan(id: &str, name: &str) -> Channel {
        Channel {
            id: id.into(),
            name: name.into(),
            logo_url: None,
            number: None,
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Direct(
                "https://example.invalid/s.m3u8".parse().unwrap(),
            ),
            locked: false,
        }
    }

    fn feed_chan(id: &str, names: &[&str]) -> XmltvChannel {
        XmltvChannel {
            id: id.into(),
            display_names: names.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn normalizes_case_accents_spacing_and_quality() {
        assert_eq!(normalize("France 2"), "france2");
        assert_eq!(normalize("FRANCE 2"), "france2");
        assert_eq!(normalize("france2"), "france2");
        assert_eq!(normalize("France 2 HD"), "france2");
        assert_eq!(normalize("Arte"), "arte");
        assert_eq!(normalize("ARTE UHD"), "arte");
        assert_eq!(normalize("Canal+ Décalé"), "canaldecale");
        assert_eq!(normalize("Canal+"), "canal");
        assert_eq!(normalize("TF1 Séries Films"), "tf1seriesfilms");
        assert_eq!(normalize("M6"), "m6");
    }

    #[test]
    fn does_not_strip_hd_that_is_part_of_the_name() {
        // Stripping unconditionally would turn this into "movies".
        assert_eq!(normalize("HDmovies"), "hdmovies");
        // …but a bare suffix does go.
        assert_eq!(normalize("Foo HD"), "foo");
    }

    #[test]
    fn never_strips_a_name_down_to_nothing() {
        assert_eq!(normalize("HD"), "hd");
        assert_eq!(normalize("4K"), "4k");
    }

    #[test]
    fn matches_across_the_usual_spelling_differences() {
        let ours = vec![chan("c1", "France 2"), chan("c2", "TF1")];
        let feed = vec![
            feed_chan("France2.fr", &["FRANCE 2 HD"]),
            feed_chan("TF1.fr", &["tf1"]),
        ];
        let r = match_channels(&ours, &feed);
        assert_eq!(r.matched_count(), 2);
        assert_eq!(r.matched.get("France2.fr").unwrap(), "c1");
        assert_eq!(r.matched.get("TF1.fr").unwrap(), "c2");
        assert!(r.unmatched.is_empty());
    }

    #[test]
    fn falls_back_to_the_feed_id_stem() {
        let ours = vec![chan("c1", "M6")];
        // No display-name at all; the id carries the only usable name.
        let feed = vec![feed_chan("M6.fr", &[])];
        let r = match_channels(&ours, &feed);
        assert_eq!(r.matched.get("M6.fr").unwrap(), "c1");
    }

    #[test]
    fn reports_unmatched_channels_instead_of_dropping_them() {
        let ours = vec![chan("c1", "TF1"), chan("c2", "Chaîne Inconnue")];
        let feed = vec![feed_chan("TF1.fr", &["TF1"])];
        let r = match_channels(&ours, &feed);
        assert_eq!(r.matched_count(), 1);
        assert_eq!(r.unmatched, vec!["Chaîne Inconnue"]);
    }

    #[test]
    fn overrides_bridge_the_canal_plus_spelling() {
        let ours = vec![chan("c1", "Canal+")];
        let feed = vec![feed_chan("CanalPlus.fr", &["Canal Plus"])];
        let r = match_channels(&ours, &feed);
        assert_eq!(
            r.matched.get("CanalPlus.fr"),
            Some(&"c1".to_string()),
            "normalize drops '+', so this only matches via the override table"
        );
    }

    #[test]
    fn empty_inputs_are_not_an_error() {
        assert_eq!(match_channels(&[], &[]).matched_count(), 0);
        let r = match_channels(&[chan("c1", "TF1")], &[]);
        assert_eq!(r.unmatched.len(), 1);
    }
}
