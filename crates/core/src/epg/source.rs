//! Fetch an XMLTV feed, with a TTL disk cache.
//!
//! Mirrors `logo_cache`: sha1-keyed files under the OS cache directory, a stale
//! copy served when the network fails, and no caching at all when the platform
//! has no cache directory (Android) rather than failing.
//!
//! One deviation, because the payloads are far larger than a logo: the parsed
//! programmes are cached alongside the raw feed. Re-parsing tens of megabytes
//! on every cold start is the cost this avoids, and it is the difference
//! between a guide that appears instantly and one that stalls on a Fire TV.

use crate::epg::EpgProgram;
use crate::error::EpgError;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn cache_dir() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("frenchetv").join("epg"))
}

fn cache_key(parts: &[&str]) -> String {
    let mut hasher = Sha1::new();
    for p in parts {
        hasher.update(p.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_fresh(path: &Path, ttl_minutes: u32) -> bool {
    if ttl_minutes == 0 {
        return false;
    }
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age < Duration::from_secs(u64::from(ttl_minutes) * 60)
}

fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, EpgError> {
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

/// Raw feed bytes for `url`, cached for `ttl_minutes`.
///
/// Falls back to a stale cached copy when the fetch fails, and only errors when
/// there is neither a working network nor anything cached.
pub async fn fetch_feed(
    client: &reqwest::Client,
    url: &str,
    ttl_minutes: u32,
) -> Result<Vec<u8>, EpgError> {
    let Some(dir) = cache_dir() else {
        tracing::debug!("no EPG cache dir; fetching without cache");
        return download(client, url).await;
    };
    let path = dir.join(format!("{}.xml", cache_key(&[url])));

    if is_fresh(&path, ttl_minutes) {
        if let Ok(bytes) = std::fs::read(&path) {
            tracing::debug!("EPG: using cached feed");
            return Ok(bytes);
        }
    }

    match download(client, url).await {
        Ok(bytes) => {
            if let Err(e) = write_atomic(&dir, &path, &bytes) {
                tracing::warn!(error = %e, "EPG: failed to write feed cache");
            }
            Ok(bytes)
        }
        Err(e) => match std::fs::read(&path) {
            Ok(stale) => {
                tracing::warn!(error = %e, "EPG: fetch failed, serving stale feed");
                Ok(stale)
            }
            Err(_) => Err(e),
        },
    }
}

/// Programmes previously parsed for this feed and channel set, if still fresh.
///
/// Keyed by the channel ids as well as the URL: the same feed parsed for a
/// different line-up is a different result.
pub fn load_parsed(url: &str, channel_key: &str, ttl_minutes: u32) -> Option<Vec<EpgProgram>> {
    let path = cache_dir()?.join(format!("{}.json", cache_key(&[url, channel_key])));
    if !is_fresh(&path, ttl_minutes) {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn store_parsed(url: &str, channel_key: &str, programs: &[EpgProgram]) {
    let Some(dir) = cache_dir() else { return };
    let path = dir.join(format!("{}.json", cache_key(&[url, channel_key])));
    match serde_json::to_vec(programs) {
        Ok(bytes) => {
            if let Err(e) = write_atomic(&dir, &path, &bytes) {
                tracing::warn!(error = %e, "EPG: failed to write parsed cache");
            }
        }
        Err(e) => tracing::warn!(error = %e, "EPG: failed to serialize parsed cache"),
    }
}

/// A stable key for a set of channel ids, order-independent.
pub fn channel_set_key(channel_ids: &mut [String]) -> String {
    channel_ids.sort();
    cache_key(&channel_ids.iter().map(String::as_str).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetches_a_feed_over_http() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/guide.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<tv></tv>"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/guide.xml", server.uri());
        // ttl 0 => never consider the cache fresh, so this exercises the network.
        let bytes = fetch_feed(&client, &url, 0).await.unwrap();
        assert_eq!(bytes, b"<tv></tv>");
    }

    #[tokio::test]
    async fn a_failing_fetch_with_no_cache_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/missing.xml"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/missing.xml", server.uri());
        assert!(fetch_feed(&client, &url, 0).await.is_err());
    }

    #[test]
    fn channel_set_key_ignores_order() {
        let mut a = vec!["b".to_string(), "a".to_string()];
        let mut b = vec!["a".to_string(), "b".to_string()];
        assert_eq!(channel_set_key(&mut a), channel_set_key(&mut b));

        let mut c = vec!["a".to_string(), "c".to_string()];
        assert_ne!(channel_set_key(&mut b), channel_set_key(&mut c));
    }

    #[test]
    fn a_zero_ttl_is_never_fresh() {
        let dir = std::env::temp_dir();
        let path = dir.join("frenchetv-epg-freshness-test");
        std::fs::write(&path, b"x").unwrap();
        assert!(!is_fresh(&path, 0));
        assert!(is_fresh(&path, 60));
        let _ = std::fs::remove_file(&path);
    }
}
