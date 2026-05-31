use super::{Channel, ChannelCategory, StreamTemplate};
use url::Url;

/// Parse an extended M3U (EXTINF format) channel list.
/// Each channel entry is:
///   #EXTINF:-1 [attributes],Display Name
///   http://stream-url
///
/// Supported attributes: tvg-id, tvg-name, tvg-logo, tvg-chno, group-title
pub fn parse_m3u(content: &str) -> Vec<Channel> {
    let mut channels = Vec::new();
    let mut lines = content.lines().peekable();

    // Skip the #EXTM3U header if present
    if let Some(&first) = lines.peek() {
        if first.trim_start().starts_with("#EXTM3U") {
            lines.next();
        }
    }

    while let Some(line) = lines.next() {
        let line = line.trim();
        if !line.starts_with("#EXTINF:") {
            continue;
        }

        // Parse the EXTINF line: #EXTINF:<duration> [key="value" ...],Display Name
        let (attrs_str, display_name) = match line.find(',') {
            Some(comma_pos) => (&line[..comma_pos], line[comma_pos + 1..].trim()),
            None => continue,
        };

        let tvg_id = extract_attr(attrs_str, "tvg-id").unwrap_or_default();
        let tvg_name =
            extract_attr(attrs_str, "tvg-name").unwrap_or_else(|| display_name.to_string());
        let tvg_logo = extract_attr(attrs_str, "tvg-logo");
        let tvg_chno = extract_attr(attrs_str, "tvg-chno").and_then(|s| s.parse::<u32>().ok());
        let group_title = extract_attr(attrs_str, "group-title").unwrap_or_default();

        // Next non-empty, non-comment line is the stream URL
        let url_line = loop {
            match lines.next() {
                Some(l) if !l.trim().is_empty() && !l.trim().starts_with('#') => {
                    break l.trim().to_string();
                }
                Some(_) => continue,
                None => break String::new(),
            }
        };

        if url_line.is_empty() {
            continue;
        }

        let url = match Url::parse(&url_line) {
            Ok(u) => u,
            Err(_) => continue,
        };

        let id = if !tvg_id.is_empty() {
            tvg_id
        } else {
            // Fallback: slugify the display name
            display_name.to_lowercase().replace(' ', "_")
        };

        channels.push(Channel {
            id,
            name: if !tvg_name.is_empty() {
                tvg_name
            } else {
                display_name.to_string()
            },
            logo_url: tvg_logo.filter(|s| !s.is_empty()),
            number: tvg_chno,
            category: ChannelCategory::from_group_title(&group_title),
            stream_template: StreamTemplate::Direct(url),
            locked: false,
        });
    }

    channels
}

/// Extract a key="value" attribute from an EXTINF attributes string.
fn extract_attr(attrs: &str, key: &str) -> Option<String> {
    let search = format!("{}=\"", key);
    let start = attrs.find(search.as_str())? + search.len();
    let rest = &attrs[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORANGE_SAMPLE: &str = include_str!("../../tests/fixtures/orange_sample.m3u");
    const BOUYGUES_SAMPLE: &str = include_str!("../../tests/fixtures/bouygues_sample.m3u");

    #[test]
    fn test_parse_orange_m3u_channel_count() {
        let channels = parse_m3u(ORANGE_SAMPLE);
        assert_eq!(channels.len(), 4);
    }

    #[test]
    fn test_parse_orange_m3u_first_channel() {
        let channels = parse_m3u(ORANGE_SAMPLE);
        let tf1 = &channels[0];
        assert_eq!(tf1.id, "TF1.fr");
        assert_eq!(tf1.name, "TF1");
        assert_eq!(
            tf1.logo_url.as_deref(),
            Some("https://logos.example.com/tf1.png")
        );
        assert_eq!(tf1.category, ChannelCategory::Generalist);
    }

    #[test]
    fn test_parse_orange_m3u_stream_url() {
        let channels = parse_m3u(ORANGE_SAMPLE);
        let tf1 = &channels[0];
        match &tf1.stream_template {
            StreamTemplate::Direct(url) => {
                assert_eq!(url.as_str(), "http://iptv.example.com/TF1/playlist.m3u8");
            }
            _ => panic!("expected Direct stream template"),
        }
    }

    #[test]
    fn test_parse_orange_m3u_news_category() {
        let channels = parse_m3u(ORANGE_SAMPLE);
        let bfm = channels.iter().find(|c| c.id == "BFMTV.fr").unwrap();
        assert_eq!(bfm.category, ChannelCategory::News);
    }

    #[test]
    fn test_parse_bouygues_channel_number() {
        let channels = parse_m3u(BOUYGUES_SAMPLE);
        let tf1 = channels.iter().find(|c| c.id == "tf1").unwrap();
        assert_eq!(tf1.number, Some(1));
        let m6 = channels.iter().find(|c| c.id == "m6").unwrap();
        assert_eq!(m6.number, Some(6));
    }

    #[test]
    fn test_parse_empty_logo_becomes_none() {
        let channels = parse_m3u(ORANGE_SAMPLE);
        let canal = channels.iter().find(|c| c.name == "Canal+").unwrap();
        assert!(canal.logo_url.is_none());
    }

    #[test]
    fn test_parse_missing_tvg_id_uses_display_name_slug() {
        let m3u = "#EXTM3U\n#EXTINF:-1,My Channel\nhttp://example.com/stream\n";
        let channels = parse_m3u(m3u);
        assert_eq!(channels[0].id, "my_channel");
    }

    #[test]
    fn test_parse_invalid_url_skipped() {
        let m3u = "#EXTM3U\n#EXTINF:-1 tvg-id=\"x\",Test\nnot_a_url\n";
        let channels = parse_m3u(m3u);
        assert!(channels.is_empty());
    }
}
