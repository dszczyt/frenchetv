//! Where schedule data comes from.
//!
//! Operator first, public XMLTV feed second — the same shape as the mandatory
//! M3U fallback for channel lists, rather than a second unrelated pattern.
//! Today both operators return `None` from `fetch_epg`, so the feed is always
//! what answers; the day one is reverse-engineered it takes precedence with no
//! structural change.
//!
//! Every failure degrades to an empty guide. Per CLAUDE.md, EPG errors are
//! silent: the channel list must keep working when the schedule does not.

use super::{matcher, source, xmltv, EpgData};
use crate::channel::Channel;
use crate::operator::Operator;
use chrono::{DateTime, Duration, Utc};

/// Default feed. Overridable via config — feeds move and die, so this is a
/// starting point rather than a constant of nature.
pub const DEFAULT_FEED_URL: &str = "https://xmltv.ch/xmltv/xmltv-tnt-fr.xml";

pub struct EpgProvider {
    client: reqwest::Client,
    feed_url: String,
    ttl_minutes: u32,
}

/// What a fetch produced, including why channels have no schedule.
#[derive(Debug, Default)]
pub struct EpgFetch {
    pub data: EpgData,
    /// Channels with no counterpart in the feed. Empty when the operator's own
    /// EPG answered, since no matching was needed.
    pub unmatched: Vec<String>,
    pub from_operator: bool,
}

impl EpgProvider {
    pub fn new(client: reqwest::Client, feed_url: impl Into<String>, ttl_minutes: u32) -> Self {
        Self {
            client,
            feed_url: feed_url.into(),
            ttl_minutes,
        }
    }

    /// Schedule for `channels` covering the next `hours`.
    ///
    /// Never fails: an unreachable feed, a malformed one, or a line-up that
    /// matches nothing all yield an empty `EpgData`.
    pub async fn fetch(
        &self,
        operator: &dyn Operator,
        channels: &[Channel],
        hours: u8,
    ) -> EpgFetch {
        if let Ok(Some(data)) = operator.fetch_epg(hours).await {
            tracing::debug!("EPG: served by the operator");
            return EpgFetch {
                data,
                unmatched: Vec::new(),
                from_operator: true,
            };
        }

        let now = Utc::now();
        let window = (
            now - Duration::hours(1),
            now + Duration::hours(hours.into()),
        );
        match self.fetch_from_feed(channels, window).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "EPG: feed unavailable, continuing without a schedule");
                EpgFetch::default()
            }
        }
    }

    async fn fetch_from_feed(
        &self,
        channels: &[Channel],
        window: (DateTime<Utc>, DateTime<Utc>),
    ) -> Result<EpgFetch, crate::error::EpgError> {
        let mut ids: Vec<String> = channels.iter().map(|c| c.id.clone()).collect();
        let channel_key = source::channel_set_key(&mut ids);

        // A previous parse for this same feed and line-up saves re-reading tens
        // of megabytes of XML on a cold start.
        if let Some(programs) = source::load_parsed(&self.feed_url, &channel_key, self.ttl_minutes)
        {
            tracing::debug!(count = programs.len(), "EPG: using cached parse");
            return Ok(EpgFetch {
                data: EpgData::from_programs(programs),
                unmatched: Vec::new(),
                from_operator: false,
            });
        }

        let xml = source::fetch_feed(&self.client, &self.feed_url, self.ttl_minutes).await?;
        let feed_channels = xmltv::parse_channels(&xml)?;
        let report = matcher::match_channels(channels, &feed_channels);

        if !report.unmatched.is_empty() {
            tracing::info!(
                matched = report.matched_count(),
                unmatched = report.unmatched.len(),
                "EPG: some channels have no feed counterpart: {}",
                report.unmatched.join(", ")
            );
        }

        let programs = xmltv::parse_programs(&xml, &report.matched, Some(window))?;
        tracing::info!(count = programs.len(), "EPG: parsed feed");
        source::store_parsed(&self.feed_url, &channel_key, &programs);

        Ok(EpgFetch {
            data: EpgData::from_programs(programs),
            unmatched: report.unmatched,
            from_operator: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ChannelCategory, StreamTemplate};
    use crate::epg::{EpgData, EpgProgram};
    use crate::error::OperatorError;
    use crate::stream::StreamUrl;
    use async_trait::async_trait;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FEED: &str = r#"<tv>
      <channel id="TF1.fr"><display-name>TF1</display-name></channel>
      <programme start="20260919200000 +0000" stop="20260919210000 +0000" channel="TF1.fr">
        <title>Journal</title>
      </programme>
    </tv>"#;

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

    /// Operator returning no EPG of its own, like both real ones today.
    struct SilentOperator;

    #[async_trait]
    impl Operator for SilentOperator {
        fn name(&self) -> &'static str {
            "silent"
        }
        fn requires_auth(&self) -> bool {
            false
        }
        async fn authenticate(&mut self, _u: &str, _p: &str) -> Result<(), OperatorError> {
            Ok(())
        }
        async fn fetch_channels(&self) -> Result<Vec<Channel>, OperatorError> {
            Ok(vec![])
        }
        async fn resolve_stream(&self, _c: &Channel) -> Result<StreamUrl, OperatorError> {
            Err(OperatorError::AuthFailed("no".into()))
        }
        async fn fetch_epg(&self, _hours: u8) -> Result<Option<EpgData>, OperatorError> {
            Ok(None)
        }
    }

    /// Operator with its own EPG, which must win over the feed.
    struct TalkativeOperator;

    #[async_trait]
    impl Operator for TalkativeOperator {
        fn name(&self) -> &'static str {
            "talkative"
        }
        fn requires_auth(&self) -> bool {
            false
        }
        async fn authenticate(&mut self, _u: &str, _p: &str) -> Result<(), OperatorError> {
            Ok(())
        }
        async fn fetch_channels(&self) -> Result<Vec<Channel>, OperatorError> {
            Ok(vec![])
        }
        async fn resolve_stream(&self, _c: &Channel) -> Result<StreamUrl, OperatorError> {
            Err(OperatorError::AuthFailed("no".into()))
        }
        async fn fetch_epg(&self, _hours: u8) -> Result<Option<EpgData>, OperatorError> {
            Ok(Some(EpgData::from_programs(vec![EpgProgram {
                channel_id: "c1".into(),
                title: "From the operator".into(),
                start: Utc::now(),
                stop: Utc::now() + Duration::hours(1),
                description: None,
            }])))
        }
    }

    #[tokio::test]
    async fn prefers_the_operator_over_the_feed() {
        let provider = EpgProvider::new(reqwest::Client::new(), "http://unused.invalid", 0);
        let got = provider
            .fetch(&TalkativeOperator, &[chan("c1", "TF1")], 3)
            .await;
        assert!(got.from_operator);
        assert_eq!(
            got.data.channel_programs("c1")[0].title,
            "From the operator"
        );
    }

    #[tokio::test]
    async fn falls_back_to_the_feed_and_reports_unmatched() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/g.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FEED))
            .mount(&server)
            .await;

        let provider = EpgProvider::new(
            reqwest::Client::new(),
            format!("{}/g.xml", server.uri()),
            0, // never use the cache, so the test is about the feed
        );
        let channels = vec![chan("c1", "TF1"), chan("c2", "Chaîne Absente")];
        let got = provider.fetch(&SilentOperator, &channels, 24).await;

        assert!(!got.from_operator);
        assert_eq!(got.unmatched, vec!["Chaîne Absente"]);
    }

    #[tokio::test]
    async fn an_unreachable_feed_yields_an_empty_guide_not_an_error() {
        let provider = EpgProvider::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1/nothing.xml".to_string(),
            0,
        );
        let got = provider
            .fetch(&SilentOperator, &[chan("c1", "TF1")], 3)
            .await;
        assert!(got.data.is_empty());
        assert!(!got.from_operator);
    }

    #[tokio::test]
    async fn a_malformed_feed_yields_an_empty_guide_not_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/bad.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<tv><<<broken"))
            .mount(&server)
            .await;

        let provider = EpgProvider::new(
            reqwest::Client::new(),
            format!("{}/bad.xml", server.uri()),
            0,
        );
        let got = provider
            .fetch(&SilentOperator, &[chan("c1", "TF1")], 3)
            .await;
        assert!(got.data.is_empty());
    }
}
