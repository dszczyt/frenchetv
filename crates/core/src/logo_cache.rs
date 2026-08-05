use crate::error::LogoCacheError;
use bytes::Bytes;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn logo_cache_dir() -> Result<PathBuf, LogoCacheError> {
    Ok(dirs::cache_dir()
        .ok_or(LogoCacheError::NoDirFound)?
        .join("frenchetv")
        .join("logos"))
}

fn cache_key(url: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(url.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_fresh(path: &Path, ttl_hours: u32) -> bool {
    if ttl_hours == 0 {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age < Duration::from_secs(u64::from(ttl_hours) * 3600)
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Bytes, LogoCacheError> {
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?)
}

fn write_cache_file(dir: &Path, path: &Path, bytes: &Bytes) -> Result<(), LogoCacheError> {
    std::fs::create_dir_all(dir)?;
    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

async fn fetch_logo_in(
    dir: &Path,
    client: &reqwest::Client,
    url: &str,
    ttl_hours: u32,
) -> Result<Bytes, LogoCacheError> {
    let path = dir.join(cache_key(url));

    if is_fresh(&path, ttl_hours) {
        if let Ok(bytes) = std::fs::read(&path) {
            return Ok(Bytes::from(bytes));
        }
    }

    match download(client, url).await {
        Ok(bytes) => {
            if let Err(e) = write_cache_file(dir, &path, &bytes) {
                tracing::warn!(error = %e, "failed to write logo cache");
            }
            Ok(bytes)
        }
        Err(e) => {
            if let Ok(stale) = std::fs::read(&path) {
                Ok(Bytes::from(stale))
            } else {
                Err(e)
            }
        }
    }
}

/// Fetches logo bytes for `url`, using a TTL-based disk cache under the OS
/// cache directory. Falls back to a stale cached copy if the network fetch
/// fails and no fresh copy is available; errors only when the network fetch
/// fails and no cached copy exists.
///
/// If no cache directory can be resolved for the current platform (e.g. on
/// Android, where `dirs::cache_dir()` returns `None`), this falls back to a
/// direct network fetch with no caching at all, rather than failing outright.
pub async fn fetch_logo(
    client: &reqwest::Client,
    url: &str,
    ttl_hours: u32,
) -> Result<Bytes, LogoCacheError> {
    match logo_cache_dir() {
        Ok(dir) => fetch_logo_in(&dir, client, url, ttl_hours).await,
        Err(_) => {
            tracing::debug!("no logo cache dir available; fetching without cache");
            download(client, url).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_with_mtime(path: &Path, bytes: &[u8], age: Duration) {
        std::fs::write(path, bytes).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let old_time = SystemTime::now() - age;
        file.set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
    }

    #[tokio::test]
    async fn no_cached_file_downloads_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"FRESH".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());

        let bytes = fetch_logo_in(dir.path(), &client, &url, 24).await.unwrap();

        assert_eq!(bytes.as_ref(), b"FRESH");
        let cached = std::fs::read(dir.path().join(cache_key(&url))).unwrap();
        assert_eq!(cached, b"FRESH");
        server.verify().await;
    }

    #[tokio::test]
    async fn fresh_cached_file_skips_network() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"SHOULD_NOT_BE_FETCHED".to_vec()),
            )
            .expect(0)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());
        let cache_path = dir.path().join(cache_key(&url));
        write_with_mtime(&cache_path, b"CACHED", Duration::from_secs(60));

        let bytes = fetch_logo_in(dir.path(), &client, &url, 24).await.unwrap();

        assert_eq!(bytes.as_ref(), b"CACHED");
        server.verify().await;
    }

    #[tokio::test]
    async fn stale_cached_file_redownloads_when_network_ok() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"NEW".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());
        let cache_path = dir.path().join(cache_key(&url));
        write_with_mtime(&cache_path, b"OLD", Duration::from_secs(25 * 3600));

        let bytes = fetch_logo_in(dir.path(), &client, &url, 24).await.unwrap();

        assert_eq!(bytes.as_ref(), b"NEW");
        let cached = std::fs::read(&cache_path).unwrap();
        assert_eq!(cached, b"NEW");
        server.verify().await;
    }

    #[tokio::test]
    async fn stale_cached_file_falls_back_when_network_fails() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());
        let cache_path = dir.path().join(cache_key(&url));
        write_with_mtime(
            &cache_path,
            b"STALE_BUT_USABLE",
            Duration::from_secs(25 * 3600),
        );

        let bytes = fetch_logo_in(dir.path(), &client, &url, 24).await.unwrap();

        assert_eq!(bytes.as_ref(), b"STALE_BUT_USABLE");
    }

    #[tokio::test]
    async fn no_cached_file_and_network_fails_errors() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());

        let result = fetch_logo_in(dir.path(), &client, &url, 24).await;

        assert!(matches!(result, Err(LogoCacheError::Network(_))));
    }

    #[tokio::test]
    async fn zero_ttl_always_attempts_network() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"REFETCHED".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());
        let cache_path = dir.path().join(cache_key(&url));
        write_with_mtime(&cache_path, b"JUST_WRITTEN", Duration::from_secs(1));

        let bytes = fetch_logo_in(dir.path(), &client, &url, 0).await.unwrap();

        assert_eq!(bytes.as_ref(), b"REFETCHED");
        server.verify().await;
    }
}
