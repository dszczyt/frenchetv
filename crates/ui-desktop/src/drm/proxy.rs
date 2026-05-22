/// Local HTTP proxy that pre-decrypts CENC segments for mpv.
///
/// ## URL scheme
/// * `GET /manifest.mpd` → returns the rewritten DASH MPD
/// * `GET /cdn/https/HOST/PATH?QUERY` → fetches `https://HOST/PATH?QUERY` from CDN,
///   CENC-decrypts if encrypted, returns plain fMP4
/// * `GET /cdn/http/HOST/PATH?QUERY` → same for plain-HTTP CDN URLs
///
/// The MPD is rewritten so every `https://HOST/...` CDN URL becomes
/// `/cdn/https/HOST/...` and `ContentProtection` elements are stripped.
use std::sync::{Arc, Mutex};
use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::cdm::CdmHandle;
use super::fmp4::{self, InitInfo};

// ─── Public API ───────────────────────────────────────────────────────────────

/// A running DRM proxy.  Drop to stop (aborts the listener task).
pub struct DrmProxy {
    /// URL to pass to mpv: `http://127.0.0.1:PORT/manifest.mpd`
    pub mpd_url: String,
    _task: tokio::task::JoinHandle<()>,
}

impl Drop for DrmProxy {
    fn drop(&mut self) { self._task.abort(); }
}

/// Start the DRM proxy.
///
/// * `cdm` — initialised CDM with keys already loaded.
/// * `mpd_text` — the (potentially already fetched) raw DASH MPD XML text.
/// * `mpd_base_url` — absolute URL from which the MPD was fetched, used to
///   resolve relative segment template paths.
/// * `cdn_headers` — headers to send to the CDN on every segment request.
/// * `init_info` — CENC parameters from the init segment (may be None until
///   the init segment is first fetched through the proxy).
pub async fn start(
    cdm: Arc<Mutex<CdmHandle>>,
    mpd_text: String,
    mpd_base_url: String,
    cdn_headers: Vec<(String, String)>,
) -> Result<DrmProxy> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("DRM proxy: bind")?;
    let addr = listener.local_addr()?;
    let port = addr.port();

    let rewritten_mpd = rewrite_mpd(&mpd_text, &mpd_base_url, port);
    tracing::debug!("DRM proxy rewritten MPD (first 2000 chars):\n{}", &rewritten_mpd[..rewritten_mpd.len().min(2000)]);

    let state = Arc::new(ProxyState {
        cdm,
        mpd: rewritten_mpd,
        cdn_headers,
        init_info: Mutex::new(None),
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default(),
    });

    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let s = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, s).await {
                            tracing::debug!("DRM proxy connection error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("DRM proxy accept error: {}", e);
                    break;
                }
            }
        }
    });

    let mpd_url = format!("http://127.0.0.1:{}/manifest.mpd", port);
    tracing::info!("DRM proxy started at {}", mpd_url);
    Ok(DrmProxy { mpd_url, _task: task })
}

// ─── Proxy internals ──────────────────────────────────────────────────────────

struct ProxyState {
    cdm: Arc<Mutex<CdmHandle>>,
    mpd: String,
    cdn_headers: Vec<(String, String)>,
    init_info: Mutex<Option<InitInfo>>,
    client: reqwest::Client,
}

async fn handle_connection(stream: TcpStream, state: Arc<ProxyState>) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await.context("read request line")?;

    // Parse: "GET /path HTTP/1.1\r\n"
    let parts: Vec<&str> = request_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 { bail!("malformed request"); }
    let path = parts[1];

    // Drain headers (we don't need them)
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line == "\r\n" || line == "\n" || line.is_empty() { break; }
    }

    tracing::debug!("DRM proxy request: {}", path);
    let (status, content_type, body) = dispatch(path, &state).await;
    if status != "200 OK" {
        tracing::warn!("DRM proxy {} → {}", path, status);
    }

    let stream = reader.into_inner();
    let mut stream = tokio::io::BufWriter::new(stream);
    let headers = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await?;
    Ok(())
}

async fn dispatch(path: &str, state: &Arc<ProxyState>) -> (&'static str, &'static str, Vec<u8>) {
    if path == "/manifest.mpd" || path.starts_with("/manifest.mpd?") {
        return ("200 OK", "application/dash+xml", state.mpd.as_bytes().to_vec());
    }

    if let Some(cdn_path) = path.strip_prefix("/cdn/") {
        match fetch_and_decrypt(cdn_path, state).await {
            Ok(data) => return ("200 OK", "video/mp4", data),
            Err(e) => {
                tracing::error!("DRM proxy segment error: {}", e);
                return ("502 Bad Gateway", "text/plain", e.to_string().into_bytes());
            }
        }
    }

    ("404 Not Found", "text/plain", b"not found".to_vec())
}

async fn fetch_and_decrypt(cdn_path: &str, state: &Arc<ProxyState>) -> Result<Vec<u8>> {
    // cdn_path is "https/HOST/PATH?QUERY" or "http/HOST/PATH?QUERY"
    let real_url = cdn_path_to_url(cdn_path)?;
    tracing::debug!("DRM proxy → {}", real_url);

    // Orange's CDN uses Broadpeak signed URLs (token embedded in path).
    // Sending Origin/Referer headers from the browser triggers CDN CORS validation
    // against the signature and returns 400.  We send only User-Agent (for browser
    // recognition) plus any session Cookie captured from the MPD response — Broadpeak
    // may use cookies for session continuity in addition to the signed URL token.
    let user_agent = state.cdn_headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("Mozilla/5.0");
    let mut req = state.client.get(&real_url).header("User-Agent", user_agent);
    if let Some((_, cookie)) = state.cdn_headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("cookie")) {
        tracing::debug!("DRM proxy CDN request: forwarding Cookie header");
        req = req.header("Cookie", cookie.as_str());
    }
    let resp = req.send().await.context("CDN fetch")?;
    if !resp.status().is_success() {
        let status = resp.status();
        for (k, v) in resp.headers() {
            tracing::debug!("DRM proxy CDN error header: {}: {}", k, v.to_str().unwrap_or("?"));
        }
        let body = resp.text().await.unwrap_or_default();
        tracing::debug!("DRM proxy CDN error body: {}", &body[..body.len().min(500)]);
        bail!("CDN returned {}", status);
    }
    let data = resp.bytes().await.context("CDN body")?;

    // Detect whether this is an init segment or media segment by checking for moof.
    let is_media = fmp4::find_box(&data, b"moof").is_some();

    if !is_media {
        // Init segment — extract CENC info, rewrite encv→avc1, return.
        if let Ok(Some(info)) = fmp4::parse_init_segment(&data) {
            *state.init_info.lock().unwrap() = Some(info);
            tracing::debug!("DRM proxy: init segment parsed (iv_size={})", {
                state.init_info.lock().unwrap().as_ref().map(|i| i.default_iv_size).unwrap_or(0)
            });
        }
        let plain_init = fmp4::strip_encryption_from_init(&data);
        return Ok(plain_init);
    }

    // Media segment — decrypt CENC samples.
    let init_info = state.init_info.lock().unwrap().clone();
    let iv_size = init_info.as_ref().map(|i| i.default_iv_size).unwrap_or(8);
    let default_kid = init_info.as_ref().map(|i| i.default_kid).unwrap_or([0u8; 16]);

    let parsed = fmp4::parse_media_segment(&data, iv_size)
        .context("parse media segment")?;

    let mut decrypted_samples = Vec::with_capacity(parsed.samples.len());
    for sample in &parsed.samples {
        let raw = &parsed.mdat_payload[sample.mdat_offset..sample.mdat_offset + sample.size];
        if let Some(ref enc) = sample.enc {
            let subs: Vec<(u32, u32)> = enc.subsamples.clone();
            let decrypted = state
                .cdm
                .lock()
                .unwrap()
                .decrypt(raw, &default_kid, &enc.iv, &subs, sample.decode_time as i64)
                .context("CDM decrypt")?;
            decrypted_samples.push(decrypted);
        } else {
            decrypted_samples.push(raw.to_vec());
        }
    }

    Ok(fmp4::rebuild_segment(&data, &decrypted_samples, &parsed))
}

fn cdn_path_to_url(cdn_path: &str) -> Result<String> {
    // cdn_path: "https/HOST/PATH?QUERY" or "http/HOST/PATH?QUERY"
    if let Some(rest) = cdn_path.strip_prefix("https/") {
        Ok(format!("https://{}", rest))
    } else if let Some(rest) = cdn_path.strip_prefix("http/") {
        Ok(format!("http://{}", rest))
    } else {
        bail!("unrecognised cdn_path format: {}", cdn_path)
    }
}

// ─── MPD rewriting ────────────────────────────────────────────────────────────

/// Rewrite a DASH MPD text so that:
/// 1. All `https://HOST/...` and `http://HOST/...` CDN URLs are rewritten to
///    `http://127.0.0.1:PORT/cdn/https/HOST/...` (or `http/`).
/// 2. All `<ContentProtection ...>...</ContentProtection>` elements are removed.
///
/// Template variables (`$RepresentationID$`, `$Number$`, `$Time$`) are preserved
/// because the URL prefix is encoded up to the `$`, and mpv resolves the template
/// by appending to the base URL.
///
/// The `BaseURL` in the MPD (if present) is the primary CDN base; we rewrite it
/// directly.  SegmentTemplate `initialization` / `media` attributes that are
/// absolute URLs are also rewritten.
fn rewrite_mpd(mpd: &str, mpd_base_url: &str, proxy_port: u16) -> String {
    let proxy_base = format!("http://127.0.0.1:{}/cdn/", proxy_port);

    // Step 1: Remove ContentProtection blocks.
    let mpd_no_drm = remove_content_protection(mpd);

    // Step 2: Resolve relative <BaseURL> elements against the MPD fetch URL.
    // e.g. <BaseURL>dash/</BaseURL> + MPD at https://cdn.host/live/ch1/manifest.mpd
    //      → <BaseURL>https://cdn.host/live/ch1/dash/</BaseURL>
    // Without this, mpv resolves segment paths relative to the proxy root (/dash/...)
    // which the proxy can't map to the CDN.
    let mpd_abs = resolve_relative_base_urls(&mpd_no_drm, mpd_base_url);

    // Step 3: Rewrite all https://... and http://... CDN URLs through the proxy.
    rewrite_cdn_urls(&mpd_abs, &proxy_base, mpd_base_url)
}

/// Resolve all relative `<BaseURL>` element contents against the MPD's own URL.
///
/// The MPD directory is everything up to (and including) the last `/` in
/// `mpd_base_url`.  A relative value like `dash/` becomes
/// `https://cdn.host/live/ch1/dash/`.
fn resolve_relative_base_urls(mpd: &str, mpd_base_url: &str) -> String {
    // Directory of the MPD URL.
    let mpd_dir = if let Some(pos) = mpd_base_url.rfind('/') {
        &mpd_base_url[..pos + 1]
    } else {
        mpd_base_url
    };

    let mut out = String::with_capacity(mpd.len() + 128);
    let mut remaining = mpd;

    while !remaining.is_empty() {
        if let Some(tag_start) = remaining.find("<BaseURL") {
            out.push_str(&remaining[..tag_start]);
            remaining = &remaining[tag_start..];

            // Find end of opening tag (may have attributes like serviceLocation="…").
            if let Some(tag_end) = remaining.find('>') {
                let open_tag = &remaining[..tag_end + 1];
                out.push_str(open_tag);
                remaining = &remaining[tag_end + 1..];

                if let Some(close) = remaining.find("</BaseURL>") {
                    let content = remaining[..close].trim();
                    // Only resolve truly relative values (not empty, not already absolute).
                    if !content.is_empty()
                        && !content.starts_with("http://")
                        && !content.starts_with("https://")
                    {
                        out.push_str(mpd_dir);
                        tracing::debug!(
                            "DRM proxy: resolved relative BaseURL {:?} → {}{}",
                            content, mpd_dir, content
                        );
                    }
                    out.push_str(content);
                    out.push_str("</BaseURL>");
                    remaining = &remaining[close + "</BaseURL>".len()..];
                } else {
                    out.push_str(remaining);
                    break;
                }
            } else {
                out.push_str(remaining);
                break;
            }
        } else {
            out.push_str(remaining);
            break;
        }
    }
    out
}

fn remove_content_protection(mpd: &str) -> String {
    let mut out = String::with_capacity(mpd.len());
    let mut remaining = mpd;
    while !remaining.is_empty() {
        // Find next <ContentProtection (case-insensitive is complex; assume lowercase)
        if let Some(start) = remaining.find("<ContentProtection") {
            out.push_str(&remaining[..start]);
            remaining = &remaining[start + "<ContentProtection".len()..];
            // Find the closing tag: either self-closing "/> " or "</ContentProtection>"
            if let Some(self_close) = remaining.find("/>") {
                // Check no nested element before this
                let close_tag = remaining.find("</ContentProtection>");
                if close_tag.map(|p| p < self_close).unwrap_or(false) {
                    // Nested close tag comes first
                    let end = close_tag.unwrap() + "</ContentProtection>".len();
                    remaining = &remaining[end..];
                } else {
                    remaining = &remaining[self_close + 2..];
                }
            } else if let Some(close) = remaining.find("</ContentProtection>") {
                remaining = &remaining[close + "</ContentProtection>".len()..];
            } else {
                break;
            }
        } else {
            out.push_str(remaining);
            break;
        }
    }
    out
}

fn rewrite_cdn_urls(mpd: &str, proxy_base: &str, _mpd_base_url: &str) -> String {
    // Replace https:// → <proxy_base>https/ and http:// → <proxy_base>http/
    // inside attribute values (between quotes).
    // We do a simple pass: scan for `https://` or `http://` and replace.
    // We skip replacing the proxy_base itself (already local).
    let mpd_replace_https = mpd.replace("https://", &format!("{}https/", proxy_base));
    let mpd_replace_http  = mpd_replace_https.replace("http://", &format!("{}http/", proxy_base));
    // But we may have accidentally rewritten our own proxy URLs — fix that.
    // Pattern: <proxy_base>http/<proxy_base>http/... should not happen because
    // the original MPD has real CDN URLs, not proxy URLs.
    // Also fix: http://127.0.0.1 got rewritten to <proxy_base>http/127.0.0.1
    // We need to restore it.
    let local_addr = "127.0.0.1";
    let broken = format!("{}http/{}", proxy_base, local_addr);
    let fixed = format!("http://{}", local_addr);
    mpd_replace_http.replace(&broken, &fixed)
}
