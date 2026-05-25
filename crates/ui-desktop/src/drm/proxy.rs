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
    // The reqwest client that was used to fetch the MPD.  Must be the *same*
    // instance so that any Broadpeak session cookies set by the CDN during the
    // manifest request are present in its cookie jar and automatically included
    // in subsequent segment requests.
    cdn_client: reqwest::Client,
) -> Result<DrmProxy> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("DRM proxy: bind")?;
    let addr = listener.local_addr()?;
    let port = addr.port();

    // Extract encryption scheme from MPD BEFORE stripping ContentProtection elements.
    // Orange DASH MPD uses value="cenc" or value="cbcs" on ContentProtection.
    let mpd_scheme = extract_mpd_scheme(&mpd_text);
    tracing::info!("DRM proxy: MPD encryption scheme={} ({})", mpd_scheme,
        if mpd_scheme == 2 { "CBCS/AES-CBC" } else { "CENC/AES-CTR" });

    // Build initial MPD as a fallback (used if the first CDN refresh fails).
    let mpd_fallback = rewrite_mpd(&mpd_text, &mpd_base_url, port);
    tracing::info!("DRM proxy initial MPD:\n{}", &mpd_fallback[..mpd_fallback.len().min(4000)]);

    let state = Arc::new(ProxyState {
        cdm,
        mpd_fallback,
        mpd_cdn_url: mpd_base_url.clone(),
        proxy_port: port,
        cdn_headers,
        init_info: Mutex::new(None),
        client: cdn_client,
        mpd_scheme,
        mpd_cache: Mutex::new(None),
    });

    let task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let s = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, s).await {
                            tracing::warn!("DRM proxy connection error: {:#}", e);
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
    /// Cached rewritten MPD from initial fetch — used as fallback if CDN refresh fails.
    mpd_fallback: String,
    /// CDN URL to re-fetch when the cache expires.
    mpd_cdn_url: String,
    proxy_port: u16,
    cdn_headers: Vec<(String, String)>,
    init_info: Mutex<Option<InitInfo>>,
    client: reqwest::Client,
    /// Encryption scheme extracted from the original MPD's `ContentProtection value` attribute
    /// before stripping.  Used as `scheme_hint` when parsing CMAF-style init segments that have
    /// no `sinf/schm` box.  1 = CENC (AES-128-CTR), 2 = CBCS (AES-128-CBC).
    mpd_scheme: u32,
    /// Short-lived MPD cache: (fetch_time, rewritten_mpd).
    ///
    /// mpv polls `/manifest.mpd` far more often than `minimumUpdatePeriod` (2 s).
    /// Without caching, each poll re-fetches from CDN and returns a slightly
    /// different SegmentTimeline.  mpv recalculates its live-edge position on every
    /// change, producing the audio jitter / backward-replay symptom.
    /// Serving the same rewritten MPD for 2 s stabilises the timeline.
    mpd_cache: Mutex<Option<(std::time::Instant, String)>>,
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

    tracing::debug!("DRM proxy request: {}", &path[..path.len().min(200)]);
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
        // Re-fetch from CDN every time so the SegmentTimeline stays current.
        // A live DASH stream has minimumUpdatePeriod="PT2S" and timeShiftBufferDepth="PT30S";
        // serving a stale MPD causes mpv to request already-expired segment timestamps → CDN 400.
        let mpd = fetch_live_mpd(state).await;
        return ("200 OK", "application/dash+xml", mpd.into_bytes());
    }

    if let Some(cdn_path) = path.strip_prefix("/cdn/") {
        match fetch_and_decrypt(cdn_path, state).await {
            Ok(data) => return ("200 OK", "video/mp4", data),
            Err(e) => {
                tracing::error!("DRM proxy segment error: {:#}", e);
                return ("502 Bad Gateway", "text/plain", e.to_string().into_bytes());
            }
        }
    }

    ("404 Not Found", "text/plain", b"not found".to_vec())
}

/// Serve the live MPD, re-fetching from CDN at most once per `MPD_TTL`.
///
/// mpv polls this endpoint much more often than `minimumUpdatePeriod`.
/// Serving the same rewritten MPD for `MPD_TTL` prevents mpv from
/// recalculating its live-edge position on every poll, which was the
/// source of the audio backward-replay / jitter symptom.
const MPD_TTL: std::time::Duration = std::time::Duration::from_secs(2);

async fn fetch_live_mpd(state: &Arc<ProxyState>) -> String {
    // Return cached MPD if still fresh.
    {
        let cache = state.mpd_cache.lock().unwrap();
        if let Some((fetched_at, ref cached)) = *cache {
            if fetched_at.elapsed() < MPD_TTL {
                return cached.clone();
            }
        }
    }

    // Cache expired — fetch a fresh one from CDN.
    let mut req = state.client.get(&state.mpd_cdn_url);
    for (name, value) in &state.cdn_headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let fresh = match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let final_url = resp.url().to_string();
            match resp.text().await {
                Ok(text) => {
                    tracing::debug!("DRM proxy: refreshed live MPD ({} bytes, url={})", text.len(), &final_url[..final_url.len().min(120)]);
                    rewrite_mpd(&text, &final_url, state.proxy_port)
                }
                Err(e) => {
                    tracing::warn!("DRM proxy: MPD body read failed ({}), using fallback", e);
                    state.mpd_fallback.clone()
                }
            }
        }
        Ok(resp) => {
            tracing::warn!("DRM proxy: MPD refresh returned {} — using fallback", resp.status());
            state.mpd_fallback.clone()
        }
        Err(e) => {
            tracing::warn!("DRM proxy: MPD refresh failed ({}), using fallback", e);
            state.mpd_fallback.clone()
        }
    };

    *state.mpd_cache.lock().unwrap() = Some((std::time::Instant::now(), fresh.clone()));
    fresh
}

async fn fetch_and_decrypt(cdn_path: &str, state: &Arc<ProxyState>) -> Result<Vec<u8>> {
    let t0 = std::time::Instant::now();
    // cdn_path is "https/HOST/PATH?QUERY" or "http/HOST/PATH?QUERY"
    let real_url = cdn_path_to_url(cdn_path)?;
    tracing::debug!("DRM proxy → {}", real_url);
    // Log URL after url-crate normalization so we can detect encoding changes.
    if let Ok(parsed) = url::Url::parse(&real_url) {
        if parsed.as_str() != real_url {
            tracing::warn!("DRM proxy URL normalised: {} → {}", real_url, parsed.as_str());
        }
    }

    // Forward the same headers used for the manifest fetch so Orange's CDN layer
    // (cdnfr.orange.fr) can validate the request before routing to Broadpeak.
    // The CDN validates Origin/Referer for CORS, User-Agent for client detection,
    // and the wassup session cookie for authentication — all are required.
    // The shared cookie jar (populated during MPD fetch) also applies automatically.
    tracing::debug!("DRM proxy → CDN: {}", &real_url[..real_url.len().min(120)]);
    let mut req = state.client.get(&real_url);
    for (name, value) in &state.cdn_headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let resp = req.send().await.context("CDN fetch")?;
    if !resp.status().is_success() {
        let status = resp.status();
        for (k, v) in resp.headers() {
            tracing::info!("DRM proxy CDN error header: {}: {}", k, v.to_str().unwrap_or("?"));
        }
        let body = resp.text().await.unwrap_or_default();
        tracing::info!("DRM proxy CDN error body: {}", &body[..body.len().min(500)]);
        bail!("CDN returned {}", status);
    }
    let data = resp.bytes().await.context("CDN body")?;

    // Detect whether this is an init segment or media segment by checking for moof.
    let is_media = fmp4::find_box(&data, b"moof").is_some();

    if !is_media {
        // Init segment — extract CENC info, rewrite encv→avc1, return.
        match fmp4::parse_init_segment(&data, state.mpd_scheme) {
            Ok(Some(info)) => {
                tracing::info!(
                    "DRM proxy: init segment parsed ok (scheme={}, iv_size={}, kid={})",
                    if info.encryption_scheme == 2 { "CBCS" } else { "CENC" },
                    info.default_iv_size,
                    info.default_kid.iter().map(|b| format!("{:02x}", b)).collect::<String>()
                );
                *state.init_info.lock().unwrap() = Some(info);
            }
            Ok(None) => tracing::warn!("DRM proxy: init segment has no tenc box — init_info stays None"),
            Err(e) => tracing::warn!("DRM proxy: init segment parse failed: {:#}", e),
        }
        let plain_init = fmp4::strip_encryption_from_init(&data);
        // Diagnostic: verify encv→avc1 transform and avcC presence.
        let has_encv = fmp4::find_box_in_init(&plain_init, b"encv").is_some();
        let has_avc1 = fmp4::find_box_in_init(&plain_init, b"avc1").is_some();
        let has_avcc = fmp4::find_box_in_init(&plain_init, b"avcC").is_some();
        tracing::info!(
            "DRM proxy: init stripped ({} → {} bytes) encv={} avc1={} avcC={}",
            data.len(), plain_init.len(), has_encv, has_avc1, has_avcc
        );
        return Ok(plain_init);
    }

    // Media segment — ensure init_info is populated before decrypting.
    if state.init_info.lock().unwrap().is_none() {
        tracing::warn!("DRM proxy: media segment arrived before init segment — auto-fetching init");
        if let Some(init_url) = derive_init_url(&real_url) {
            if let Err(e) = fetch_and_store_init(&init_url, state).await {
                tracing::warn!("DRM proxy: init auto-fetch failed: {:#}", e);
            }
        } else {
            tracing::warn!("DRM proxy: cannot derive init URL from: {}", &real_url[..real_url.len().min(120)]);
        }
    }

    let init_info = state.init_info.lock().unwrap().clone();
    let iv_size = init_info.as_ref().map(|i| i.default_iv_size).unwrap_or(8);
    let default_kid = init_info.as_ref().map(|i| i.default_kid).unwrap_or([0u8; 16]);
    let encryption_scheme = init_info.as_ref().map(|i| i.encryption_scheme).unwrap_or(state.mpd_scheme);
    if init_info.is_none() {
        tracing::warn!("DRM proxy: init still None after auto-fetch — decrypt will fail (NoKey)");
    } else {
        tracing::debug!("DRM proxy: decrypt scheme={} kid={}",
            if encryption_scheme == 2 { "CBCS" } else { "CENC" },
            default_kid.iter().map(|b| format!("{:02x}", b)).collect::<String>());
    }

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
                .decrypt(raw, &default_kid, &enc.iv, &subs, sample.decode_time as i64, encryption_scheme)
                .context("CDM decrypt")?;
            decrypted_samples.push(decrypted);
        } else {
            decrypted_samples.push(raw.to_vec());
        }
    }

    let result = fmp4::rebuild_segment(&data, &decrypted_samples, &parsed);
    let elapsed = t0.elapsed();
    // Log every segment fetch; WARN if it took > 1 s (likely stall cause).
    let seg_name = cdn_path.rsplit('/').next().unwrap_or(cdn_path);
    if elapsed.as_millis() > 1000 {
        tracing::warn!("DRM proxy: slow segment {}ms {}", elapsed.as_millis(), seg_name);
    } else {
        tracing::debug!("DRM proxy: segment {}ms {}", elapsed.as_millis(), seg_name);
    }
    Ok(result)
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

// ─── MPD bandwidth filter ─────────────────────────────────────────────────────

/// Extract the numeric value of `bandwidth="N"` from an opening tag string.
fn bandwidth_attr(opening_tag: &str) -> Option<u64> {
    let key = "bandwidth=\"";
    let start = opening_tag.find(key)?;
    let after = &opening_tag[start + key.len()..];
    let end = after.find('"')?;
    after[..end].parse().ok()
}

/// Remove every `<Representation>` block (or self-closing `<Representation .../>`)
/// whose `bandwidth` attribute exceeds `max_bps`.
fn filter_high_bitrate_representations(mpd: &str, max_bps: u64) -> String {
    const OPEN: &str = "<Representation";
    const CLOSE: &str = "</Representation>";

    let mut out = String::with_capacity(mpd.len());
    let mut rest = mpd;

    while let Some(pos) = rest.find(OPEN) {
        // Everything before this tag passes through unchanged.
        out.push_str(&rest[..pos]);
        let chunk = &rest[pos..];

        // Find the end of the opening tag (first `>`).
        let tag_end = match chunk.find('>') {
            Some(p) => p,
            None => {
                // Malformed XML — pass through verbatim.
                out.push_str(chunk);
                return out;
            }
        };

        let opening = &chunk[..tag_end + 1];
        let bw = bandwidth_attr(opening).unwrap_or(0);

        if bw > max_bps {
            // Drop this representation entirely.
            if opening.ends_with("/>") {
                // Self-closing tag.
                rest = &rest[pos + tag_end + 1..];
            } else if let Some(close_pos) = chunk.find(CLOSE) {
                rest = &rest[pos + close_pos + CLOSE.len()..];
            } else {
                // No closing tag found — pass through to avoid data loss.
                out.push_str(chunk);
                return out;
            }
            tracing::warn!("MPD filter: dropped Representation bandwidth={}", bw);
        } else {
            // Keep this representation — emit the opening tag and advance past it.
            out.push_str(opening);
            rest = &rest[pos + tag_end + 1..];
        }
    }

    out.push_str(rest);
    out
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
    let mpd_rewritten = rewrite_cdn_urls(&mpd_abs, &proxy_base, mpd_base_url);

    // Step 4: Strip high-bitrate video representations that the CDN cannot
    // deliver within one segment duration (~1.15 s for Orange live DASH).
    // Segments > ~1.5 Mbps arrive in 1–6 s, causing mpv to stall and loop
    // through its decoded buffer.  Force mpv to the lower-bitrate track.
    filter_high_bitrate_representations(&mpd_rewritten, 1_500_000)
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

/// Extract the encryption scheme from an MPD's `ContentProtection` elements.
///
/// Looks for `value="cbcs"` (case-insensitive) to detect CBCS (AES-128-CBC pattern).
/// Falls back to 1 (CENC/AES-128-CTR) if no explicit scheme is indicated.
fn extract_mpd_scheme(mpd: &str) -> u32 {
    let lower = mpd.to_lowercase();
    // Common patterns: value="cbcs" or value='cbcs'
    if lower.contains("value=\"cbcs\"") || lower.contains("value='cbcs'") {
        return 2;
    }
    1
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

/// Derive the DASH init segment URL from a media segment URL.
///
/// Pattern: `…/stream-repid-TIMESTAMP.dash?q` → `…/stream-repid.dash?q`
///
/// Returns `None` if the URL doesn't end in `-<all-digits>.dash`.
fn derive_init_url(media_url: &str) -> Option<String> {
    let (base, query) = if let Some(pos) = media_url.find('?') {
        (&media_url[..pos], &media_url[pos + 1..])
    } else {
        (media_url, "")
    };
    if !base.ends_with(".dash") { return None; }
    let stem = &base[..base.len() - 5]; // strip ".dash"
    let last_dash = stem.rfind('-')?;
    let after = &stem[last_dash + 1..];
    if after.is_empty() || !after.chars().all(|c| c.is_ascii_digit()) { return None; }
    let init_stem = &stem[..last_dash];
    Some(if query.is_empty() {
        format!("{}.dash", init_stem)
    } else {
        format!("{}.dash?{}", init_stem, query)
    })
}

/// Fetch the DASH init segment directly from the CDN and populate `state.init_info`.
async fn fetch_and_store_init(init_url: &str, state: &Arc<ProxyState>) -> Result<()> {
    tracing::info!("DRM proxy: fetching init segment: {}", &init_url[..init_url.len().min(120)]);
    let mut req = state.client.get(init_url);
    for (name, value) in &state.cdn_headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let resp = req.send().await.context("init segment CDN fetch")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("init segment CDN returned {} (body: {})", status, &body[..body.len().min(200)]);
    }
    let data = resp.bytes().await.context("init segment CDN body")?;
    match fmp4::parse_init_segment(&data, state.mpd_scheme) {
        Ok(Some(info)) => {
            tracing::info!(
                "DRM proxy: init segment ok (scheme={}, iv_size={}, kid={})",
                if info.encryption_scheme == 2 { "CBCS" } else { "CENC" },
                info.default_iv_size,
                info.default_kid.iter().map(|b| format!("{:02x}", b)).collect::<String>()
            );
            *state.init_info.lock().unwrap() = Some(info);
            Ok(())
        }
        Ok(None) => {
            // Dump top-level box types so we can diagnose alternate encryption layouts.
            let top_boxes: Vec<String> = fmp4::boxes(&data)
                .map(|b| String::from_utf8_lossy(&b.fourcc).into_owned())
                .collect();
            tracing::warn!("DRM proxy: init segment no tenc — top boxes: {:?} (data len={})", top_boxes, data.len());
            if let Some(moov) = fmp4::find_box(&data, b"moov") {
                let moov_children: Vec<String> = fmp4::boxes(moov.payload)
                    .map(|b| String::from_utf8_lossy(&b.fourcc).into_owned())
                    .collect();
                tracing::warn!("DRM proxy: moov children: {:?}", moov_children);
            }
            bail!("init segment has no tenc box")
        }
        Err(e) => Err(e).context("init segment parse"),
    }
}
