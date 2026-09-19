//! Streaming XMLTV parser.
//!
//! Deliberately event-based rather than DOM: a French feed covering ~200
//! channels over a week is tens of megabytes, and this also runs on a 32-bit
//! Fire TV with roughly a gigabyte of RAM. Nothing is materialised except the
//! channel table (small) and the programmes the caller asked for.

use super::{EpgData, EpgProgram};
use crate::error::EpgError;
use chrono::{DateTime, FixedOffset, Utc};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

/// A `<channel>` entry: its feed-local id and the names it advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmltvChannel {
    pub id: String,
    pub display_names: Vec<String>,
}

/// Parse XMLTV timestamps: `20260919203000 +0200`, or without the offset, in
/// which case UTC is assumed (the spec says local time, but a feed that omits
/// the offset gives us nothing better to go on).
pub fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    let (stamp, offset) = match raw.split_once(' ') {
        Some((s, o)) => (s, Some(o)),
        None => (raw, None),
    };
    if stamp.len() < 14 {
        return None;
    }
    match offset {
        Some(off) => {
            let joined = format!("{} {}", &stamp[..14], off);
            DateTime::parse_from_str(&joined, "%Y%m%d%H%M%S %z")
                .ok()
                .map(|d: DateTime<FixedOffset>| d.with_timezone(&Utc))
        }
        None => chrono::NaiveDateTime::parse_from_str(&stamp[..14], "%Y%m%d%H%M%S")
            .ok()
            .map(|n| n.and_utc()),
    }
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        if a.key.as_ref() == key {
            String::from_utf8(a.value.into_owned()).ok()
        } else {
            None
        }
    })
}

/// Read every `<channel>` declaration. Stops as soon as programmes begin —
/// XMLTV puts all channels first, so there is no need to walk the rest.
pub fn parse_channels(xml: &[u8]) -> Result<Vec<XmltvChannel>, EpgError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut out: Vec<XmltvChannel> = Vec::new();
    let mut current: Option<XmltvChannel> = None;
    let mut in_display_name = false;
    let mut buf = Vec::new();

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| EpgError::ParseError(e.to_string()))?
        {
            Event::Start(e) => match e.name().as_ref() {
                b"channel" => {
                    current = Some(XmltvChannel {
                        id: attr(&e, b"id").unwrap_or_default(),
                        display_names: Vec::new(),
                    });
                }
                b"display-name" => in_display_name = true,
                b"programme" => break,
                _ => {}
            },
            Event::Text(t) if in_display_name => {
                if let Some(c) = current.as_mut() {
                    if let Ok(s) = t.unescape() {
                        let s = s.trim();
                        if !s.is_empty() {
                            c.display_names.push(s.to_string());
                        }
                    }
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"display-name" => in_display_name = false,
                b"channel" => {
                    if let Some(c) = current.take() {
                        if !c.id.is_empty() {
                            out.push(c);
                        }
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

/// Read `<programme>` entries, keeping only those whose feed channel id appears
/// in `wanted` and which overlap `window` (when given).
///
/// `wanted` maps a feed channel id to *our* channel id, so the programmes come
/// back already addressed by the id the rest of the app uses. Filtering here
/// rather than afterwards is the point: everything else is skipped without
/// allocating.
pub fn parse_programs(
    xml: &[u8],
    wanted: &HashMap<String, String>,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Result<Vec<EpgProgram>, EpgError> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();

    // Set when inside a <programme> we intend to keep.
    let mut pending: Option<(String, DateTime<Utc>, DateTime<Utc>)> = None;
    let mut title: Option<String> = None;
    let mut desc: Option<String> = None;
    let mut in_title = false;
    let mut in_desc = false;

    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| EpgError::ParseError(e.to_string()))?
        {
            Event::Start(e) => match e.name().as_ref() {
                b"programme" => {
                    pending = None;
                    title = None;
                    desc = None;

                    let feed_id = attr(&e, b"channel").unwrap_or_default();
                    let Some(our_id) = wanted.get(&feed_id) else {
                        continue;
                    };
                    let (Some(start), Some(stop)) = (
                        attr(&e, b"start").as_deref().and_then(parse_timestamp),
                        attr(&e, b"stop").as_deref().and_then(parse_timestamp),
                    ) else {
                        continue;
                    };
                    if let Some((from, to)) = window {
                        if stop <= from || start >= to {
                            continue;
                        }
                    }
                    pending = Some((our_id.clone(), start, stop));
                }
                b"title" if pending.is_some() => in_title = true,
                b"desc" if pending.is_some() => in_desc = true,
                _ => {}
            },
            Event::Text(t) => {
                if in_title && title.is_none() {
                    title = t.unescape().ok().map(|s| s.trim().to_string());
                } else if in_desc && desc.is_none() {
                    desc = t.unescape().ok().map(|s| s.trim().to_string());
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"title" => in_title = false,
                b"desc" => in_desc = false,
                b"programme" => {
                    if let Some((channel_id, start, stop)) = pending.take() {
                        out.push(EpgProgram {
                            channel_id,
                            title: title.take().unwrap_or_else(|| "—".to_string()),
                            start,
                            stop,
                            description: desc.take().filter(|d| !d.is_empty()),
                        });
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(out)
}

/// Parse a whole feed for the given channels.
pub fn parse_guide(
    xml: &[u8],
    wanted: &HashMap<String, String>,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Result<EpgData, EpgError> {
    Ok(EpgData::from_programs(parse_programs(xml, wanted, window)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FEED: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<tv>
  <channel id="TF1.fr">
    <display-name>TF1</display-name>
    <display-name>TF1 HD</display-name>
  </channel>
  <channel id="France2.fr">
    <display-name>France 2</display-name>
  </channel>
  <programme start="20260919200000 +0200" stop="20260919210000 +0200" channel="TF1.fr">
    <title>Journal</title>
    <desc>Les titres</desc>
  </programme>
  <programme start="20260919210000 +0200" stop="20260919230000 +0200" channel="TF1.fr">
    <title>Les Visiteurs</title>
  </programme>
  <programme start="20260919200000 +0200" stop="20260919220000 +0200" channel="Unwanted.fr">
    <title>Ignore me</title>
  </programme>
</tv>"#;

    fn wanted() -> HashMap<String, String> {
        HashMap::from([("TF1.fr".to_string(), "tf1".to_string())])
    }

    #[test]
    fn parses_channels_with_all_display_names() {
        let chans = parse_channels(FEED).unwrap();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans[0].id, "TF1.fr");
        assert_eq!(chans[0].display_names, vec!["TF1", "TF1 HD"]);
        assert_eq!(chans[1].display_names, vec!["France 2"]);
    }

    #[test]
    fn keeps_only_wanted_channels_and_maps_to_our_ids() {
        let progs = parse_programs(FEED, &wanted(), None).unwrap();
        assert_eq!(progs.len(), 2, "Unwanted.fr and France2.fr are dropped");
        assert!(progs.iter().all(|p| p.channel_id == "tf1"));
        assert_eq!(progs[0].title, "Journal");
        assert_eq!(progs[0].description.as_deref(), Some("Les titres"));
        assert_eq!(progs[1].title, "Les Visiteurs");
        assert_eq!(progs[1].description, None, "missing <desc> stays None");
    }

    #[test]
    fn applies_the_window_filter() {
        // 22:00–23:00 Paris == 20:00–21:00 UTC; only "Les Visiteurs" overlaps.
        let from = Utc.with_ymd_and_hms(2026, 9, 19, 20, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 9, 19, 21, 0, 0).unwrap();
        let progs = parse_programs(FEED, &wanted(), Some((from, to))).unwrap();
        assert_eq!(progs.len(), 1);
        assert_eq!(progs[0].title, "Les Visiteurs");
    }

    #[test]
    fn timestamps_honour_the_offset() {
        let t = parse_timestamp("20260919203000 +0200").unwrap();
        assert_eq!(t, Utc.with_ymd_and_hms(2026, 9, 19, 18, 30, 0).unwrap());
        // Without an offset the stamp is taken as UTC.
        let t = parse_timestamp("20260919203000").unwrap();
        assert_eq!(t, Utc.with_ymd_and_hms(2026, 9, 19, 20, 30, 0).unwrap());
        assert!(parse_timestamp("nonsense").is_none());
        assert!(parse_timestamp("2026").is_none());
    }

    #[test]
    fn a_programme_missing_times_is_skipped_not_fatal() {
        let feed = br#"<tv>
          <programme channel="TF1.fr"><title>No times</title></programme>
          <programme start="20260919200000 +0200" stop="20260919210000 +0200" channel="TF1.fr">
            <title>Good</title>
          </programme>
        </tv>"#;
        let progs = parse_programs(feed, &wanted(), None).unwrap();
        assert_eq!(progs.len(), 1);
        assert_eq!(progs[0].title, "Good");
    }

    #[test]
    fn mismatched_tags_are_an_error_not_a_panic() {
        // quick-xml checks end-tag names by default, so this is a real error.
        assert!(parse_programs(b"<tv><programme></wrong></tv>", &wanted(), None).is_err());
    }

    #[test]
    fn garbage_input_yields_nothing_rather_than_panicking() {
        // quick-xml is lenient about a lot; the property that matters is that
        // junk never panics and never invents programmes.
        for junk in [
            b"<tv><<<>".as_slice(),
            b"not xml at all".as_slice(),
            b"".as_slice(),
        ] {
            let got = parse_programs(junk, &wanted(), None);
            assert!(got.map(|p| p.is_empty()).unwrap_or(true));
        }
        assert!(parse_channels(b"<tv><channel id='x'>").is_ok());
    }

    #[test]
    fn parse_guide_indexes_by_channel() {
        let data = parse_guide(FEED, &wanted(), None).unwrap();
        assert_eq!(data.channel_programs("tf1").len(), 2);
    }
}
