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
    /// Feed channel id -> every one of our channels it serves.
    ///
    /// One-to-many on purpose: normalisation deliberately collapses variants,
    /// so "TCM CINEMA" and "TCM CINEMA (VO)" both resolve to the same feed
    /// entry. A one-to-one map silently dropped whichever was seen second,
    /// leaving that channel with no schedule at all.
    pub matched: HashMap<String, Vec<String>>,
    /// Names of our channels with no counterpart in the feed.
    pub unmatched: Vec<String>,
}

impl MatchReport {
    /// How many of *our* channels got a schedule — not how many feed entries
    /// were used, which is smaller whenever variants share one.
    pub fn matched_count(&self) -> usize {
        self.matched.values().map(Vec::len).sum()
    }
}

/// Overrides for names that survive normalisation differently on each side.
///
/// Keyed by our normalised channel name; the value is the normalised feed name
/// to accept as equivalent. Kept deliberately short — every entry here is a
/// rule `normalize` could not express, and the `+`-to-"plus" mapping already
/// removed the whole Canal+/Ciné+/Ligue 1+ family that used to live here.
const OVERRIDES: &[(&str, &str)] = &[
    ("francelatelevision", "franceinfo"),
    ("rmcstory", "rmcstoryhd"),
];

/// Strip accents, casing, punctuation and broadcast-quality suffixes.
///
/// `"France 2 HD"`, `"france2"` and `"FRANCE 2"` all normalise to `"france2"`.
pub fn normalize(name: &str) -> String {
    // Parenthetical qualifiers mark a variant of the same channel — "(VO)" is
    // the original-language feed of a channel the guide lists once.
    let without_parens = {
        let mut out = String::with_capacity(name.len());
        let mut depth = 0usize;
        for c in name.chars() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    };

    // Lowercase BEFORE folding accents. Folding first only ever matched the
    // lowercase forms, so an upper-case accented name — and French line-ups
    // are largely upper-case, "CINÉ+classic", "MATÉLÉ" — kept its accent and
    // never matched the feed's spelling.
    let lowered: String = without_parens
        .to_lowercase()
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
        // "Canal+" and a feed's "Canal Plus" have to land on the same string.
        // Dropping the '+' as punctuation made them differ, which is what the
        // override table used to paper over.
        .replace('+', "plus");

    // Keep alphanumerics only; punctuation and spacing carry no signal here.
    let compact: String = lowered.chars().filter(|c| c.is_alphanumeric()).collect();

    // Drop a trailing quality or language marker. Only at the end — "hd"
    // inside a name (say a channel actually called "HDmovies") is part of it.
    // Language suffixes matter here: a line-up lists "France 24 Français"
    // where the feed simply says "France 24".
    for suffix in [
        "uhd", "4k", "fullhd", "hd", "sd", "francais", "anglais", "espagnol", "arabe", "allemand",
        "vo", "vf",
    ] {
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
                report
                    .matched
                    .entry((*id).to_string())
                    .or_default()
                    .push(ch.id.clone());
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
    fn folds_accents_on_upper_case_names_too() {
        // Folding used to run before lowercasing, so upper-case accented names
        // kept their accent — and French line-ups are largely upper-case.
        assert_eq!(normalize("CINÉ+classic"), normalize("Ciné+ Classic"));
        assert_eq!(normalize("MATÉLÉ"), "matele");
        assert_eq!(normalize("ATHAQAFIA"), "athaqafia");
    }

    #[test]
    fn normalizes_case_accents_spacing_and_quality() {
        assert_eq!(normalize("France 2"), "france2");
        assert_eq!(normalize("FRANCE 2"), "france2");
        assert_eq!(normalize("france2"), "france2");
        assert_eq!(normalize("France 2 HD"), "france2");
        assert_eq!(normalize("Arte"), "arte");
        assert_eq!(normalize("ARTE UHD"), "arte");
        assert_eq!(normalize("Canal+ Décalé"), "canalplusdecale");
        assert_eq!(normalize("Canal+"), "canalplus");
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
        assert_eq!(
            r.matched.get("France2.fr").unwrap(),
            &vec!["c1".to_string()]
        );
        assert_eq!(r.matched.get("TF1.fr").unwrap(), &vec!["c2".to_string()]);
        assert!(r.unmatched.is_empty());
    }

    #[test]
    fn falls_back_to_the_feed_id_stem() {
        let ours = vec![chan("c1", "M6")];
        // No display-name at all; the id carries the only usable name.
        let feed = vec![feed_chan("M6.fr", &[])];
        let r = match_channels(&ours, &feed);
        assert_eq!(r.matched.get("M6.fr").unwrap(), &vec!["c1".to_string()]);
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
    fn plus_channels_match_whichever_way_the_feed_spells_them() {
        // Real unmatched names from a live run: the whole Canal+/Ciné+/Ligue 1+
        // family failed until '+' normalised to "plus" rather than being
        // dropped as punctuation.
        let ours = vec![
            chan("c1", "Canal+"),
            chan("c2", "CINÉ+classic"),
            chan("c3", "LIGUE 1+"),
        ];
        let feed = vec![
            feed_chan("CanalPlus.fr", &["Canal Plus"]),
            feed_chan("CinePlusClassic.fr", &["Ciné+ Classic"]),
            feed_chan("Ligue1Plus.fr", &["Ligue 1+"]),
        ];
        let r = match_channels(&ours, &feed);
        assert_eq!(r.matched_count(), 3, "unmatched: {:?}", r.unmatched);
    }

    #[test]
    fn language_and_version_qualifiers_are_stripped() {
        // Also from the live run: a line-up says "FRANCE 24 Français" and
        // "TCM CINEMA (VO)" where the feed just names the channel.
        assert_eq!(normalize("FRANCE 24 Français"), "france24");
        assert_eq!(normalize("AL JAZEERA Anglais"), "aljazeera");
        assert_eq!(normalize("TCM CINEMA (VO)"), "tcmcinema");
        assert_eq!(normalize("BOOMERANG (VO)"), "boomerang");
        // A name that is only a qualifier keeps it rather than vanishing.
        assert_eq!(normalize("(VO)"), "");
    }

    #[test]
    fn one_feed_channel_can_serve_several_of_ours() {
        // Normalisation collapses variants on purpose; a one-to-one map used to
        // drop whichever variant was seen second, leaving it scheduleless.
        let ours = vec![chan("c1", "TCM CINEMA"), chan("c2", "TCM CINEMA (VO)")];
        let feed = vec![feed_chan("TCM.fr", &["TCM Cinéma"])];
        let r = match_channels(&ours, &feed);
        assert_eq!(r.matched_count(), 2, "both variants keep a schedule");
        let served = r.matched.get("TCM.fr").unwrap();
        assert!(served.contains(&"c1".to_string()));
        assert!(served.contains(&"c2".to_string()));
    }

    #[test]
    fn empty_inputs_are_not_an_error() {
        assert_eq!(match_channels(&[], &[]).matched_count(), 0);
        let r = match_channels(&[chan("c1", "TF1")], &[]);
        assert_eq!(r.unmatched.len(), 1);
    }
}
