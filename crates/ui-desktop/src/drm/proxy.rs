use anyhow::{bail, Context, Result};
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
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::cdm::CdmHandle;
use super::fmp4::{self, InitInfo};

/// DIAGNOSTIC (audio-loop investigation): dumps every audio init/media segment
/// (both CDN-encrypted and decrypted forms) to a private per-run directory, so
/// they can be concatenated and decoded offline with `ffmpeg`/`ffprobe` —
/// isolating whether corruption is baked into the decrypted bytes themselves,
/// independent of mpv, the network, or playback timing entirely. Opt-in only
/// (`FRENCHETV_DUMP_SEGMENT=1`); files carry no auth tokens (media bytes only)
/// but still go under the user's home dir, created 0700, not `/tmp`.
fn dump_segment_diagnostic(kind: &str, cdn_path: &str, raw: &[u8], rebuilt: &[u8]) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    static DIR_READY: std::sync::Once = std::sync::Once::new();

    let Some(dir) = dirs::home_dir().map(|h| h.join("frenchetv-audio-dump")) else {
        return;
    };
    DIR_READY.call_once(|| {
        if std::fs::create_dir_all(&dir).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            }
        }
    });

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let stem = cdn_path.rsplit('/').next().unwrap_or(cdn_path);
    let stem: String = stem
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = dir.join(format!("{seq:05}_{kind}_{stem}"));
    let _ = std::fs::write(base.with_extension("raw.mp4"), raw);
    let _ = std::fs::write(base.with_extension("dec.mp4"), rebuilt);
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// A running DRM proxy.  Drop to stop (aborts the listener task).
pub struct DrmProxy {
    /// URL to pass to mpv: `http://127.0.0.1:PORT/manifest.mpd`
    pub mpd_url: String,
    _task: tokio::task::JoinHandle<()>,
}

impl Drop for DrmProxy {
    fn drop(&mut self) {
        self._task.abort();
    }
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
    tracing::info!(
        "DRM proxy: MPD encryption scheme={} ({})",
        mpd_scheme,
        if mpd_scheme == 2 {
            "CBCS/AES-CBC"
        } else {
            "CENC/AES-CTR"
        }
    );

    // Established once per audio representation from the first manifest that
    // lists it, then reused (and validated) on every subsequent refresh — see
    // `number_mappings` field doc. Created here, before `ProxyState` exists,
    // so the very first `rewrite_mpd` call (below) can populate it and later
    // refreshes in `fetch_live_mpd` keep extending the same store.
    let number_mappings: NumberMappings = Mutex::new(HashMap::new());

    // Build initial MPD as a fallback (used if the first CDN refresh fails).
    let (mpd_fallback, initial_hosts) =
        rewrite_mpd(&mpd_text, &mpd_base_url, port, &number_mappings);
    tracing::info!(
        "DRM proxy initial MPD:\n{}",
        &mpd_fallback[..mpd_fallback.len().min(4000)]
    );

    // The proxy forwards CDN auth headers (incl. the operator session cookie) on
    // every /cdn/<scheme>/<host>/<path> request. Since scheme+host come straight
    // from the request path, restrict them to CDN hosts the MPD itself actually
    // references — otherwise a local process, or a page in the user's browser
    // guessing this ephemeral port, could make the app leak the live session
    // cookie to an arbitrary attacker-controlled host.
    //
    // This is a *set*, not a single host: Orange is fronted by Broadpeak, whose
    // manifests carry multiple <BaseURL serviceLocation="..."> entries for
    // multi-CDN failover (see resolve_relative_base_urls below) — a single-host
    // allowlist would 502 every segment mpv pulls from the second CDN. It's
    // rebuilt from scratch on every `fetch_live_mpd` refresh (not just seeded
    // once here) so it tracks whatever the CDN's current manifest actually
    // serves, redirects included. Scheme is part of the key too, so a request
    // for a host the manifest only ever used over https can't be replayed as
    // plain http to leak the cookie in cleartext.
    let allowed_hosts = Mutex::new(initial_hosts);

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
        allowed_hosts,
        segment_cache: Mutex::new(HashMap::new()),
        number_mappings,
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
    Ok(DrmProxy {
        mpd_url,
        _task: task,
    })
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
    /// `"scheme://host"` entries the most recently (re)written manifest actually
    /// referenced. Only requests matching one of these may receive `cdn_headers`
    /// (session cookie, auth tokens) — see `start()`. Rebuilt on every
    /// `fetch_live_mpd` refresh, since the manifest can list multiple CDN hosts
    /// (multi-CDN failover) or redirect to a new one between refreshes.
    allowed_hosts: Mutex<HashSet<String>>,
    /// Short-TTL cache of already-decrypted segment bytes, keyed by `cdn_path`
    /// (the full `/cdn/...` request path, so distinct segments never collide).
    ///
    /// Cuts redundant CDN fetches + CENC decrypts when the same segment path
    /// is requested more than once. Originally added on a theory that this
    /// alone explained the "audio loop" bug; it doesn't — see
    /// `number_mappings` below for the actual root cause — but repeat
    /// requests for the same path are still real (mpv's dash demuxer retries
    /// while it's stuck) and this keeps every repeat past the first cheap.
    segment_cache: SegmentCache,
    /// Per-audio-representation `$Number$`→`$Time$` mapping, keyed by
    /// representation id. See `derive_and_rewrite_audio_number_mappings` for
    /// why this exists: ffmpeg's dash demuxer reliably desyncs on a live
    /// manifest's refreshed `<SegmentTimeline>`, reproduced independent of
    /// this proxy in `tools/dash-demuxer-repro/`. Persists across
    /// `fetch_live_mpd` refreshes — established once per representation from
    /// the first manifest that lists it, then held fixed.
    number_mappings: NumberMappings,
}

/// `cdn_path -> (cached_at, decrypted_bytes)`.
type SegmentCache = Mutex<HashMap<String, (std::time::Instant, Arc<Vec<u8>>)>>;

/// How long a decrypted segment stays cached. Deliberately short: live
/// segments are only ever requested within a few seconds of "now" anyway, so
/// this only needs to outlive the tight retry bursts it's meant to absorb,
/// not become a general segment store. Also bounds memory — combined with the
/// retention this implies (at most a handful of seconds of segments across
/// all representations), this stays far under any concerning footprint
/// without needing a separate byte-size cap.
const SEGMENT_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// Returns cached bytes for `cdn_path` if present and still fresh. Prunes
/// expired entries on every call so the map can't grow unbounded.
fn segment_cache_get(cache: &SegmentCache, cdn_path: &str) -> Option<Arc<Vec<u8>>> {
    let mut cache = cache.lock().unwrap();
    let now = std::time::Instant::now();
    cache.retain(|_, (cached_at, _)| now.duration_since(*cached_at) < SEGMENT_CACHE_TTL);
    cache.get(cdn_path).map(|(_, data)| Arc::clone(data))
}

fn segment_cache_put(cache: &SegmentCache, cdn_path: &str, data: Arc<Vec<u8>>) {
    cache
        .lock()
        .unwrap()
        .insert(cdn_path.to_string(), (std::time::Instant::now(), data));
}

/// Handles every request on one TCP connection, keeping it open (HTTP/1.1
/// keep-alive) instead of closing after each response.
///
/// mpv's DASH demuxer polls this proxy at up to several hundred requests/sec
/// near the live edge (see investigation history around the "audio loop"
/// fix). The previous `Connection: close` meant every one of those polls
/// paid for a fresh TCP handshake + teardown — real syscall/allocation
/// overhead competing for CPU with mpv's own demux/decode threads in the
/// same process. Reusing the connection removes that overhead entirely.
async fn handle_connection(stream: TcpStream, state: Arc<ProxyState>) -> Result<()> {
    let mut reader = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        let n = reader
            .read_line(&mut request_line)
            .await
            .context("read request line")?;
        if n == 0 {
            // Client closed the connection — not an error.
            return Ok(());
        }

        // Parse: "GET /path HTTP/1.1\r\n"
        let parts: Vec<&str> = request_line.trim().splitn(3, ' ').collect();
        if parts.len() < 2 {
            bail!("malformed request");
        }
        let path = parts[1].to_string();

        // Drain headers (we don't need them)
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            if line == "\r\n" || line == "\n" || line.is_empty() {
                break;
            }
        }

        tracing::debug!("DRM proxy request: {}", &path[..path.len().min(200)]);
        let (status, content_type, body) = dispatch(&path, &state).await;
        if status != "200 OK" {
            tracing::warn!("DRM proxy {} → {}", path, status);
        }

        // One write_all for headers+body instead of two — halves the syscalls
        // per response on top of the connection reuse above.
        let headers = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            status,
            content_type,
            body.len()
        );
        let mut response = Vec::with_capacity(headers.len() + body.len());
        response.extend_from_slice(headers.as_bytes());
        response.extend_from_slice(&body);
        // `BufReader<TcpStream>` forwards `AsyncWrite` straight to the
        // underlying socket, so this can write directly without splitting
        // the stream or re-wrapping it in a `BufWriter` per request.
        reader.write_all(&response).await?;
        reader.flush().await?;
    }
}

async fn dispatch(
    path: &str,
    state: &Arc<ProxyState>,
) -> (&'static str, &'static str, Arc<Vec<u8>>) {
    if path == "/manifest.mpd" || path.starts_with("/manifest.mpd?") {
        // Re-fetch from CDN every time so the SegmentTimeline stays current.
        // A live DASH stream has minimumUpdatePeriod="PT2S" and timeShiftBufferDepth="PT30S";
        // serving a stale MPD causes mpv to request already-expired segment timestamps → CDN 400.
        let mpd = fetch_live_mpd(state).await;
        return ("200 OK", "application/dash+xml", Arc::new(mpd.into_bytes()));
    }

    // `/cdnnum/<repid>/<number>` — the $Number$-based audio addressing
    // (see `derive_and_rewrite_audio_number_mappings`) translates back to a
    // real `/cdn/...`-shaped path here, then joins the exact same
    // cache/fetch/decrypt path below as any other segment request.
    let cdnnum_path = path
        .strip_prefix("/cdnnum/")
        .and_then(|rest| number_route_to_cdn_path(rest, &state.number_mappings));
    let cdn_path = cdnnum_path
        .as_deref()
        .or_else(|| path.strip_prefix("/cdn/"));

    if let Some(cdn_path) = cdn_path {
        if let Some(cached) = segment_cache_get(&state.segment_cache, cdn_path) {
            return ("200 OK", "video/mp4", cached);
        }
        match fetch_and_decrypt(cdn_path, state).await {
            Ok(data) => {
                let data = Arc::new(data);
                segment_cache_put(&state.segment_cache, cdn_path, Arc::clone(&data));
                return ("200 OK", "video/mp4", data);
            }
            Err(e) => {
                tracing::error!("DRM proxy segment error for {} ({}): {:#}", path, cdn_path, e);
                return (
                    "502 Bad Gateway",
                    "text/plain",
                    Arc::new(e.to_string().into_bytes()),
                );
            }
        }
    }

    (
        "404 Not Found",
        "text/plain",
        Arc::new(b"not found".to_vec()),
    )
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
                    tracing::debug!(
                        "DRM proxy: refreshed live MPD ({} bytes, url={})",
                        text.len(),
                        &final_url[..final_url.len().min(120)]
                    );
                    let (rewritten, hosts) =
                        rewrite_mpd(&text, &final_url, state.proxy_port, &state.number_mappings);
                    // Rebuild (not merge) the allowlist from what this manifest actually
                    // references — see `allowed_hosts` field doc for why this must be a
                    // set, and why it's rebuilt rather than seeded once.
                    *state.allowed_hosts.lock().unwrap() = hosts;
                    rewritten
                }
                Err(e) => {
                    tracing::warn!("DRM proxy: MPD body read failed ({}), using fallback", e);
                    state.mpd_fallback.clone()
                }
            }
        }
        Ok(resp) => {
            tracing::warn!(
                "DRM proxy: MPD refresh returned {} — using fallback",
                resp.status()
            );
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
    ensure_allowed_host(&real_url, &state.allowed_hosts.lock().unwrap())?;
    tracing::debug!("DRM proxy → {}", real_url);
    // Log URL after url-crate normalization so we can detect encoding changes.
    if let Ok(parsed) = url::Url::parse(&real_url) {
        if parsed.as_str() != real_url {
            tracing::warn!(
                "DRM proxy URL normalised: {} → {}",
                real_url,
                parsed.as_str()
            );
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
            tracing::info!(
                "DRM proxy CDN error header: {}: {}",
                k,
                v.to_str().unwrap_or("?")
            );
        }
        let body = resp.text().await.unwrap_or_default();
        tracing::info!("DRM proxy CDN error body: {}", &body[..body.len().min(500)]);
        bail!("CDN returned {}", status);
    }
    let data = resp.bytes().await.context("CDN body")?;

    // Detect whether this is an init segment or media segment by checking for moof.
    let is_media = fmp4::find_box(&data, b"moof").is_some();

    // DIAGNOSTIC (audio-loop investigation): `rebuild_segment` assumes exactly
    // one moof+mdat pair per segment. If a segment actually carries more than
    // one (multi-fragment/chunked CMAF), everything past the first mdat is
    // currently passed through untouched — still encrypted — by `rebuild_segment`.
    // Count top-level moof boxes to check whether that assumption holds here.
    if is_media {
        let moof_count = fmp4::boxes(&data).filter(|b| &b.fourcc == b"moof").count();
        let mdat_count = fmp4::boxes(&data).filter(|b| &b.fourcc == b"mdat").count();
        if moof_count != 1 || mdat_count != 1 {
            tracing::warn!(
                "DRM proxy: segment has {} moof / {} mdat boxes (expected 1/1) {}",
                moof_count,
                mdat_count,
                cdn_path.rsplit('/').next().unwrap_or(cdn_path)
            );
        }
    }

    if !is_media {
        // Init segment — extract CENC info, rewrite encv→avc1, return.
        match fmp4::parse_init_segment(&data, state.mpd_scheme) {
            Ok(Some(info)) => {
                tracing::info!(
                    "DRM proxy: init segment parsed ok (scheme={}, iv_size={}, kid={})",
                    if info.encryption_scheme == 2 {
                        "CBCS"
                    } else {
                        "CENC"
                    },
                    info.default_iv_size,
                    info.default_kid
                        .iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<String>()
                );
                *state.init_info.lock().unwrap() = Some(info);
            }
            Ok(None) => {
                tracing::warn!("DRM proxy: init segment has no tenc box — init_info stays None")
            }
            Err(e) => tracing::warn!("DRM proxy: init segment parse failed: {:#}", e),
        }
        let plain_init = fmp4::strip_encryption_from_init(&data);
        // Diagnostic: verify encv→avc1 transform and avcC presence.
        let has_encv = fmp4::find_box_in_init(&plain_init, b"encv").is_some();
        let has_avc1 = fmp4::find_box_in_init(&plain_init, b"avc1").is_some();
        let has_avcc = fmp4::find_box_in_init(&plain_init, b"avcC").is_some();
        tracing::info!(
            "DRM proxy: init stripped ({} → {} bytes) encv={} avc1={} avcC={}",
            data.len(),
            plain_init.len(),
            has_encv,
            has_avc1,
            has_avcc
        );
        if std::env::var_os("FRENCHETV_DUMP_SEGMENT").is_some() && cdn_path.contains("-audio") {
            dump_segment_diagnostic("init", cdn_path, &data, &plain_init);
        }
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
            tracing::warn!(
                "DRM proxy: cannot derive init URL from: {}",
                &real_url[..real_url.len().min(120)]
            );
        }
    }

    let init_info = state.init_info.lock().unwrap().clone();
    let iv_size = init_info.as_ref().map(|i| i.default_iv_size).unwrap_or(8);
    let default_kid = init_info
        .as_ref()
        .map(|i| i.default_kid)
        .unwrap_or([0u8; 16]);
    let encryption_scheme = init_info
        .as_ref()
        .map(|i| i.encryption_scheme)
        .unwrap_or(state.mpd_scheme);
    if init_info.is_none() {
        tracing::warn!("DRM proxy: init still None after auto-fetch — decrypt will fail (NoKey)");
    } else {
        tracing::debug!(
            "DRM proxy: decrypt scheme={} kid={}",
            if encryption_scheme == 2 {
                "CBCS"
            } else {
                "CENC"
            },
            default_kid
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        );
    }

    let parsed = fmp4::parse_media_segment(&data, iv_size).context("parse media segment")?;

    let mut decrypted_samples = Vec::with_capacity(parsed.samples.len());
    for sample in &parsed.samples {
        let raw = &parsed.mdat_payload[sample.mdat_offset..sample.mdat_offset + sample.size];
        if let Some(ref enc) = sample.enc {
            let subs: Vec<(u32, u32)> = enc.subsamples.clone();
            let decrypted = state
                .cdm
                .lock()
                .unwrap()
                .decrypt(
                    raw,
                    &default_kid,
                    &enc.iv,
                    &subs,
                    sample.decode_time as i64,
                    encryption_scheme,
                )
                .context("CDM decrypt")?;
            decrypted_samples.push(decrypted);
        } else {
            decrypted_samples.push(raw.to_vec());
        }
    }

    let result = fmp4::rebuild_segment(&data, &decrypted_samples, &parsed);

    if std::env::var_os("FRENCHETV_DUMP_SEGMENT").is_some() && cdn_path.contains("-audio") {
        dump_segment_diagnostic("media", cdn_path, &data, &result);
    }
    let elapsed = t0.elapsed();
    // Log every segment fetch; WARN if it took > 1 s (likely stall cause).
    // DIAGNOSTIC (audio-loop investigation): raw CDN body size + rebuilt output
    // size, to check whether repeated fetches of the same cdn_path are getting
    // progressively larger (growing/chunked CMAF segment) or byte-identical
    // (something else re-triggering ffmpeg's request).
    let seg_name = cdn_path.rsplit('/').next().unwrap_or(cdn_path);
    if elapsed.as_millis() > 1000 {
        tracing::warn!(
            "DRM proxy: slow segment {}ms cdn_bytes={} out_bytes={} {}",
            elapsed.as_millis(),
            data.len(),
            result.len(),
            seg_name
        );
    } else {
        tracing::debug!(
            "DRM proxy: segment {}ms cdn_bytes={} out_bytes={} {}",
            elapsed.as_millis(),
            data.len(),
            result.len(),
            seg_name
        );
    }
    Ok(result)
}

/// Refuse to dial hosts other than the CDN the MPD itself came from. Without
/// this, `/cdn/<scheme>/<host>/<path>` would let any caller of this
/// unauthenticated localhost listener redirect the proxy's outbound request
/// (with `cdn_headers`, incl. the operator session cookie) to an arbitrary host.
fn ensure_allowed_host(real_url: &str, allowed_hosts: &HashSet<String>) -> Result<()> {
    match scheme_host_of(real_url) {
        Some(h) if allowed_hosts.contains(&h) => Ok(()),
        Some(h) => bail!(
            "DRM proxy: refusing to proxy to disallowed host {} (expected one of {:?})",
            h,
            allowed_hosts
        ),
        None => bail!("DRM proxy: could not parse host from {}", real_url),
    }
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
///
/// Returns the rewritten text plus every `"scheme://host"` the manifest
/// actually referenced (post `<BaseURL>` resolution, pre proxy-URL rewrite) —
/// the caller uses this as the SSRF allowlist for `/cdn/<scheme>/<host>/...`
/// requests, since a manifest can list more than one CDN host.
fn rewrite_mpd(
    mpd: &str,
    mpd_base_url: &str,
    proxy_port: u16,
    number_mappings: &NumberMappings,
) -> (String, HashSet<String>) {
    let proxy_base = format!("http://127.0.0.1:{}/cdn/", proxy_port);

    // Step 1: Remove ContentProtection blocks.
    let mpd_no_drm = remove_content_protection(mpd);

    // Step 2: Resolve relative <BaseURL> elements against the MPD fetch URL.
    // e.g. <BaseURL>dash/</BaseURL> + MPD at https://cdn.host/live/ch1/manifest.mpd
    //      → <BaseURL>https://cdn.host/live/ch1/dash/</BaseURL>
    // Without this, mpv resolves segment paths relative to the proxy root (/dash/...)
    // which the proxy can't map to the CDN.
    let mpd_abs = resolve_relative_base_urls(&mpd_no_drm, mpd_base_url);

    // Collect the allowlist from the fully-resolved (but not yet proxy-rewritten)
    // text, so it reflects every CDN host the manifest names — including
    // multi-CDN <BaseURL serviceLocation="..."> failover entries — plus a
    // guaranteed fallback of the fetch URL's own host in case the manifest has
    // no absolute URLs at all (pure relative SegmentTemplate).
    let mut hosts = extract_cdn_hosts(&mpd_abs);
    if let Some(base_host) = scheme_host_of(mpd_base_url) {
        hosts.insert(base_host);
    }

    // Step 2.5: Replace audio <SegmentTemplate>s with $Number$-based
    // addressing — see `derive_and_rewrite_audio_number_mappings` doc.
    // Operates on `mpd_abs` (media/initialization patterns are still the
    // CDN's original relative strings here; BaseURL is already absolute)
    // before URL rewriting, since the replacement contains no `https://`/
    // `http://` substrings for step 3 to touch either way.
    let mpd_numbered = derive_and_rewrite_audio_number_mappings(&mpd_abs, number_mappings);

    // Step 3: Rewrite all https://... and http://... CDN URLs through the proxy.
    let mpd_rewritten = rewrite_cdn_urls(&mpd_numbered, &proxy_base, mpd_base_url);

    // Step 4: Strip the highest-bitrate video representation (3.2 Mbps) which
    // arrives in 1–6 s per segment on the CDN.  Allow up to 2.0 Mbps so mpv
    // can pick the 2.1 Mbps track (likely higher frame rate than 1.4 Mbps).
    // If 2.1 Mbps is also slow, logs will show slow-segment warnings for
    // ctv-video=2137600 and we can lower this threshold further.
    // 2137600 > 2_000_000 so the previous threshold accidentally kept filtering
    // the 2.1 Mbps track.  2_200_000 allows 2137600 through and only drops 3225200.
    let final_mpd = filter_high_bitrate_representations(&mpd_rewritten, 2_200_000);
    (final_mpd, hosts)
}

// ─── $Number$-based audio addressing ───────────────────────────────────────────
//
// ffmpeg's dash demuxer reliably desyncs its fragment-index tracking when a
// live (`type="dynamic"`) manifest's `<SegmentTimeline>` is refreshed
// periodically (`minimumUpdatePeriod`) — reproduced in isolation, independent
// of this proxy/CDN/decrypt path entirely, in `tools/dash-demuxer-repro/`.
// The break happens regardless of *how* the timeline changes between
// refreshes (a single entry sliding its own `t` forward, same as Orange's
// origin sends; or a stable multi-entry list only ever appended to) — only
// removing the `<SegmentTimeline>` element in favor of `$Number$`+fixed
// `duration` addressing avoids it, while the manifest keeps refreshing
// normally for a genuinely live stream. See `tools/dash-demuxer-repro/README.md`
// for the full test matrix.
//
// Only applied to audio `<AdaptationSet>`s: video has never shown this bug in
// any test, and there's no reason to add risk to what already works.

type NumberMappings = Mutex<HashMap<String, NumberMapping>>;

/// Per-audio-representation `$Number$` → `$Time$` translation, keyed by
/// representation id in `ProxyState::number_mappings`.
#[derive(Debug, Clone, PartialEq)]
struct NumberMapping {
    /// Segment duration in this representation's own `timescale` units.
    /// Established once from the first manifest that lists this
    /// representation, then held fixed — see `derive_and_rewrite_audio_number_mappings`.
    duration: u64,
    /// The `$Time$` value that corresponds to segment number 1.
    epoch_t: u64,
    /// Absolute media URL pattern (`<BaseURL>` + the CDN's original `media`
    /// attribute), still containing the literal `$RepresentationID$` and
    /// `$Time$` placeholders — substituted per-request in `number_to_cdn_path`.
    media_pattern: String,
}

impl NumberMapping {
    /// `$Time$` value for 1-based segment `number`.
    fn time_for_number(&self, number: u64) -> u64 {
        self.epoch_t + (number.saturating_sub(1)) * self.duration
    }

    /// 1-based segment number for a given `$Time$` value, rounding down to
    /// the segment that contains it — used to compute the advertised
    /// `startNumber` for the current live window from the CDN's own `t`.
    fn number_for_time(&self, t: u64) -> u64 {
        1 + t.saturating_sub(self.epoch_t) / self.duration
    }

    /// Builds the real (still `$RepresentationID$`/`$Time$`-templated
    /// nowhere — both substituted here) absolute CDN URL for `number`.
    fn resolve_url(&self, repid: &str, number: u64) -> String {
        let t = self.time_for_number(number);
        self.media_pattern
            .replace("$RepresentationID$", repid)
            .replace("$Time$", &t.to_string())
    }
}

/// Scratch state accumulated while scanning one `<AdaptationSet>`.
#[derive(Default)]
struct SetScratch {
    is_audio: bool,
    /// Byte span of the `<SegmentTemplate>...</SegmentTemplate>` element in
    /// the original text, once both ends have been seen.
    template_span: Option<(u64, u64)>,
    timescale: Option<u64>,
    /// Original `media` attribute, unescaped — used to build real URLs.
    media_raw: Option<String>,
    /// Original `initialization` attribute, still XML-escaped — reused
    /// verbatim in the replacement, since it's re-embedded as XML, not built
    /// into a URL directly.
    init_raw: Option<String>,
    /// `(t, d)` per `<S>` child seen. A qualifying timeline has exactly one,
    /// with `t` present — anything else falls back to passthrough.
    s_entries: Vec<(Option<u64>, u64)>,
    rep_ids: Vec<String>,
}

/// Rewrites every audio `<AdaptationSet>`'s `<SegmentTemplate>` from the CDN's
/// original `$Time$` + `<SegmentTimeline>` addressing to `$Number$` + fixed
/// `duration`, routed through `/cdnnum/<repid>/<number>` (translated back to
/// a real CDN fetch in `dispatch`). See the module doc above for why.
///
/// `mappings` persists across refreshes: a representation's `(epoch_t,
/// duration)` is established once, from the first manifest that lists it,
/// then held fixed. A later refresh reporting a different `duration` for an
/// already-known representation — which would happen if a segment's real
/// duration ever changes mid-stream, e.g. an SCTE-35 ad-break splice — means
/// the fixed mapping would silently produce the wrong `$Time$` from that
/// point on, so that representation instead falls back to the original
/// passthrough `<SegmentTemplate>`/`<SegmentTimeline>` for the rest of the
/// session. The same fallback applies whenever a set's timeline doesn't look
/// like a single, uniform-duration run (parse failure, multiple `<S>`
/// entries, missing attributes) — this function only ever *replaces* text it
/// has fully understood; anything it can't confidently parse is left
/// untouched.
fn derive_and_rewrite_audio_number_mappings(mpd_abs: &str, mappings: &NumberMappings) -> String {
    use quick_xml::events::{BytesStart, Event};
    use quick_xml::Reader;

    fn attr_raw(tag: &BytesStart, name: &[u8]) -> Option<String> {
        tag.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .map(|a| String::from_utf8_lossy(a.value.as_ref()).into_owned())
    }

    fn attr_unescaped(tag: &BytesStart, name: &[u8]) -> Option<String> {
        tag.attributes()
            .flatten()
            .find(|a| a.key.as_ref() == name)
            .and_then(|a| a.unescape_value().ok())
            .map(|c| c.into_owned())
    }

    let mut reader = Reader::from_str(mpd_abs);
    let mut out = String::with_capacity(mpd_abs.len());
    let mut last_copied: u64 = 0;
    let mut base_url: Option<String> = None;
    let mut capturing_base_url = false;
    let mut set = SetScratch::default();

    loop {
        let pos_before = reader.buffer_position();
        let event = match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(e) => e,
            // Malformed/unexpected XML — bail entirely, return the input
            // unmodified rather than risk emitting a half-rewritten manifest.
            Err(_) => return mpd_abs.to_string(),
        };

        match &event {
            Event::Start(e) | Event::Empty(e) if e.name().as_ref() == b"BaseURL" => {
                capturing_base_url = base_url.is_none() && matches!(event, Event::Start(_));
            }
            Event::Text(t) if capturing_base_url => {
                if let Ok(text) = t.unescape() {
                    let text = text.trim();
                    if !text.is_empty() {
                        base_url = Some(text.to_string());
                    }
                }
                capturing_base_url = false;
            }
            Event::Start(e) if e.name().as_ref() == b"AdaptationSet" => {
                let content_type = attr_raw(e, b"contentType").unwrap_or_default();
                let mime_type = attr_raw(e, b"mimeType").unwrap_or_default();
                set = SetScratch::default();
                set.is_audio =
                    content_type.eq_ignore_ascii_case("audio") || mime_type.starts_with("audio/");
            }
            Event::Start(e) if set.is_audio && e.name().as_ref() == b"SegmentTemplate" => {
                set.template_span = Some((pos_before, 0));
                set.timescale = attr_raw(e, b"timescale").and_then(|s| s.parse().ok());
                set.media_raw = attr_unescaped(e, b"media");
                set.init_raw = attr_raw(e, b"initialization");
            }
            Event::End(e) if set.is_audio && e.name().as_ref() == b"SegmentTemplate" => {
                if let Some((start, _)) = set.template_span {
                    set.template_span = Some((start, reader.buffer_position()));
                }
            }
            Event::Start(e) | Event::Empty(e) if set.is_audio && e.name().as_ref() == b"S" => {
                let t = attr_raw(e, b"t").and_then(|s| s.parse().ok());
                if let Some(d) = attr_raw(e, b"d").and_then(|s| s.parse().ok()) {
                    set.s_entries.push((t, d));
                }
            }
            Event::Start(e) | Event::Empty(e)
                if set.is_audio && e.name().as_ref() == b"Representation" =>
            {
                if let Some(id) = attr_raw(e, b"id") {
                    set.rep_ids.push(id);
                }
            }
            Event::End(e) if e.name().as_ref() == b"AdaptationSet" => {
                if set.is_audio {
                    if let Some(replacement) =
                        build_number_template(&set, base_url.as_deref(), mappings)
                    {
                        let (start, end) =
                            set.template_span.expect("checked in build_number_template");
                        out.push_str(&mpd_abs[last_copied as usize..start as usize]);
                        out.push_str(&replacement);
                        last_copied = end;
                    }
                }
                set = SetScratch::default();
            }
            _ => {}
        }
    }

    out.push_str(&mpd_abs[last_copied as usize..]);
    out
}

/// Validates one `<AdaptationSet>`'s captured scratch state and, if it fully
/// qualifies, establishes/validates its `NumberMapping`s and returns the
/// replacement `<SegmentTemplate>` XML. Returns `None` (leave the original
/// untouched) for anything not fully understood — see the fallback rules on
/// `derive_and_rewrite_audio_number_mappings`.
fn build_number_template(
    set: &SetScratch,
    base_url: Option<&str>,
    mappings: &NumberMappings,
) -> Option<String> {
    let (_, template_end) = set.template_span?;
    if template_end == 0 {
        return None; // saw <SegmentTemplate> but never its matching close tag
    }
    let timescale = set.timescale?;
    let media_raw = set.media_raw.as_deref()?;
    let init_raw = set.init_raw.as_deref()?;
    let base_url = base_url?;
    if set.rep_ids.is_empty() {
        return None;
    }
    // Exactly one <S t=.. d=..> — anything else (no entries, multiple runs,
    // missing t) means this isn't the simple sliding-single-entry shape this
    // rewrite is designed for.
    let [(Some(t), d)] = set.s_entries.as_slice() else {
        return None;
    };
    let (t, d) = (*t, *d);

    let media_pattern = format!("{base_url}{media_raw}");

    let mut mappings = mappings.lock().unwrap();
    for repid in &set.rep_ids {
        match mappings.get(repid) {
            Some(existing) if existing.duration != d || existing.media_pattern != media_pattern => {
                // Segment duration (or the underlying URL pattern) changed
                // since this representation's mapping was established —
                // don't risk translating $Number$ to the wrong $Time$ for
                // the rest of the session.
                tracing::warn!(
                    "DRM proxy: audio representation {} changed duration/pattern \
                     (mapped duration={} now={}) — falling back to passthrough",
                    repid,
                    existing.duration,
                    d
                );
                return None;
            }
            Some(_) => {} // matches what's stored — fine
            None => {
                mappings.insert(
                    repid.clone(),
                    NumberMapping {
                        duration: d,
                        epoch_t: t,
                        media_pattern: media_pattern.clone(),
                    },
                );
            }
        }
    }

    // All representations in this set share one mapping (by construction —
    // segmentAlignment="true" means one SegmentTimeline for the whole set),
    // so any of them gives the same start_number for the current window.
    let start_number = mappings[&set.rep_ids[0]].number_for_time(t);

    Some(format!(
        "<SegmentTemplate timescale=\"{timescale}\" initialization=\"{init_raw}\" \
         media=\"/cdnnum/$RepresentationID$/$Number$\" duration=\"{d}\" \
         startNumber=\"{start_number}\"/>"
    ))
}

/// Translates a `/cdnnum/<repid>/<number>` route (see
/// `derive_and_rewrite_audio_number_mappings`) back into the
/// `"<scheme>/<host>/<path>?<query>"` shape `fetch_and_decrypt` expects —
/// same as what's already behind `/cdn/`, so the rest of the pipeline
/// (host allowlisting, decrypt, caching) is unchanged. `None` if the path
/// doesn't parse or `repid` has no established mapping.
fn number_route_to_cdn_path(rest: &str, mappings: &NumberMappings) -> Option<String> {
    let (repid, number_str) = rest.split_once('/')?;
    let number: u64 = number_str.parse().ok()?;
    let mapping = mappings.lock().unwrap().get(repid)?.clone();
    let real_url = mapping.resolve_url(repid, number);
    Some(real_url.replacen("://", "/", 1))
}

/// Every `"scheme://host"` (lowercase) that `https://` or `http://` precedes in
/// `mpd`, matching exactly the substrings `rewrite_cdn_urls` rewrites.
fn extract_cdn_hosts(mpd: &str) -> HashSet<String> {
    let mut hosts = HashSet::new();
    for scheme in ["https://", "http://"] {
        let mut rest = mpd;
        while let Some(pos) = rest.find(scheme) {
            let after = &rest[pos + scheme.len()..];
            let end = after
                .find(|c: char| {
                    c == '/' || c == '"' || c == '\'' || c == '<' || c == '>' || c.is_whitespace()
                })
                .unwrap_or(after.len());
            let host = &after[..end];
            if !host.is_empty() {
                hosts.insert(format!("{}{}", scheme, host.to_ascii_lowercase()));
            }
            rest = &after[end..];
        }
    }
    hosts
}

/// `"scheme://host"` (lowercase) of `url`, or `None` if unparseable.
fn scheme_host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    Some(format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str()?.to_ascii_lowercase()
    ))
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
                            content,
                            mpd_dir,
                            content
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
    let mpd_replace_http = mpd_replace_https.replace("http://", &format!("{}http/", proxy_base));
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
    if !base.ends_with(".dash") {
        return None;
    }
    let stem = &base[..base.len() - 5]; // strip ".dash"
    let last_dash = stem.rfind('-')?;
    let after = &stem[last_dash + 1..];
    if after.is_empty() || !after.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let init_stem = &stem[..last_dash];
    Some(if query.is_empty() {
        format!("{}.dash", init_stem)
    } else {
        format!("{}.dash?{}", init_stem, query)
    })
}

/// Fetch the DASH init segment directly from the CDN and populate `state.init_info`.
async fn fetch_and_store_init(init_url: &str, state: &Arc<ProxyState>) -> Result<()> {
    tracing::info!(
        "DRM proxy: fetching init segment: {}",
        &init_url[..init_url.len().min(120)]
    );
    let mut req = state.client.get(init_url);
    for (name, value) in &state.cdn_headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let resp = req.send().await.context("init segment CDN fetch")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "init segment CDN returned {} (body: {})",
            status,
            &body[..body.len().min(200)]
        );
    }
    let data = resp.bytes().await.context("init segment CDN body")?;
    match fmp4::parse_init_segment(&data, state.mpd_scheme) {
        Ok(Some(info)) => {
            tracing::info!(
                "DRM proxy: init segment ok (scheme={}, iv_size={}, kid={})",
                if info.encryption_scheme == 2 {
                    "CBCS"
                } else {
                    "CENC"
                },
                info.default_iv_size,
                info.default_kid
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<String>()
            );
            *state.init_info.lock().unwrap() = Some(info);
            Ok(())
        }
        Ok(None) => {
            // Dump top-level box types so we can diagnose alternate encryption layouts.
            let top_boxes: Vec<String> = fmp4::boxes(&data)
                .map(|b| String::from_utf8_lossy(&b.fourcc).into_owned())
                .collect();
            tracing::warn!(
                "DRM proxy: init segment no tenc — top boxes: {:?} (data len={})",
                top_boxes,
                data.len()
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts(entries: &[&str]) -> HashSet<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_segment_cache_miss_then_hit() {
        let cache: SegmentCache = Mutex::new(HashMap::new());
        assert!(segment_cache_get(&cache, "p").is_none());
        segment_cache_put(&cache, "p", Arc::new(vec![1, 2, 3]));
        assert_eq!(
            segment_cache_get(&cache, "p").as_deref(),
            Some(&vec![1, 2, 3])
        );
    }

    #[test]
    fn test_segment_cache_distinct_paths_dont_collide() {
        let cache: SegmentCache = Mutex::new(HashMap::new());
        segment_cache_put(&cache, "a", Arc::new(vec![1]));
        segment_cache_put(&cache, "b", Arc::new(vec![2]));
        assert_eq!(segment_cache_get(&cache, "a").as_deref(), Some(&vec![1]));
        assert_eq!(segment_cache_get(&cache, "b").as_deref(), Some(&vec![2]));
    }

    #[test]
    fn test_segment_cache_expires_after_ttl() {
        let cache: SegmentCache = Mutex::new(HashMap::new());
        // Insert with a synthetic already-expired timestamp instead of sleeping.
        cache.lock().unwrap().insert(
            "p".to_string(),
            (
                std::time::Instant::now() - SEGMENT_CACHE_TTL - std::time::Duration::from_secs(1),
                Arc::new(vec![1]),
            ),
        );
        assert!(segment_cache_get(&cache, "p").is_none());
    }

    #[test]
    fn test_ensure_allowed_host_matches() {
        let real_url = "https://cdnfr.orange.fr/live/ch1/seg-1.dash";
        assert!(ensure_allowed_host(real_url, &hosts(&["https://cdnfr.orange.fr"])).is_ok());
    }

    #[test]
    fn test_ensure_allowed_host_case_insensitive() {
        let real_url = "https://CDNfr.orange.fr/live/ch1/seg-1.dash";
        assert!(ensure_allowed_host(real_url, &hosts(&["https://cdnfr.orange.fr"])).is_ok());
    }

    #[test]
    fn test_ensure_allowed_host_rejects_disallowed_host() {
        // This is the exploit path: a request-path-controlled host that
        // doesn't match any CDN host the MPD referenced must be refused, not
        // silently dialed with the session cookie attached.
        let real_url = "https://evil.example/steal";
        assert!(ensure_allowed_host(real_url, &hosts(&["https://cdnfr.orange.fr"])).is_err());
    }

    #[test]
    fn test_ensure_allowed_host_rejects_unparseable_url() {
        assert!(ensure_allowed_host("not a url", &hosts(&["https://cdnfr.orange.fr"])).is_err());
    }

    #[test]
    fn test_ensure_allowed_host_supports_multi_cdn_failover() {
        // Broadpeak-style multi-CDN manifests list more than one host; both
        // must be dialable, not just whichever one the fetch URL used.
        let allowed = hosts(&["https://cdnfr.orange.fr", "https://cdn2fr.orange.fr"]);
        assert!(ensure_allowed_host("https://cdnfr.orange.fr/a.dash", &allowed).is_ok());
        assert!(ensure_allowed_host("https://cdn2fr.orange.fr/b.dash", &allowed).is_ok());
    }

    #[test]
    fn test_ensure_allowed_host_pins_scheme() {
        // A host the manifest only ever referenced over https must not be
        // dialable over plain http — that would replay cdn_headers (the
        // session cookie) in cleartext.
        let allowed = hosts(&["https://cdnfr.orange.fr"]);
        assert!(ensure_allowed_host("http://cdnfr.orange.fr/a.dash", &allowed).is_err());
    }

    #[test]
    fn test_extract_cdn_hosts_multiple_base_urls() {
        let mpd = r#"<MPD>
            <BaseURL serviceLocation="cdn1">https://cdnfr.orange.fr/live/ch1/</BaseURL>
            <BaseURL serviceLocation="cdn2">https://cdn2fr.orange.fr/live/ch1/</BaseURL>
        </MPD>"#;
        let found = extract_cdn_hosts(mpd);
        assert_eq!(
            found,
            hosts(&["https://cdnfr.orange.fr", "https://cdn2fr.orange.fr"])
        );
    }

    #[test]
    fn test_rewrite_mpd_allowlist_falls_back_to_fetch_host_when_no_absolute_urls() {
        // A manifest with only relative SegmentTemplate paths has no
        // scheme://host substrings at all — the fetch URL's own host must
        // still end up in the allowlist, or every segment 403s.
        let mpd = r#"<MPD><SegmentTemplate media="seg-$Number$.dash" /></MPD>"#;
        let mappings = Mutex::new(HashMap::new());
        let (_, found) = rewrite_mpd(
            mpd,
            "https://cdnfr.orange.fr/live/ch1/manifest.mpd",
            12345,
            &mappings,
        );
        assert!(found.contains("https://cdnfr.orange.fr"));
    }

    // ── $Number$-based audio addressing ─────────────────────────────────────

    fn sample_mpd(audio_t: u64) -> String {
        format!(
            r#"<MPD>
  <BaseURL>https://cdn.example.fr/live/ch1/dash/</BaseURL>
  <Period>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate timescale="48000"
                        initialization="ch1-$RepresentationID$.dash?tok=abc"
                        media="ch1-$RepresentationID$-$Time$.dash?tok=abc">
        <SegmentTimeline>
          <S t="{audio_t}" d="92160" r="15"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="audio_fra=104000" bandwidth="104000"></Representation>
      <Representation id="audio_qaa=104000" bandwidth="104000"></Representation>
    </AdaptationSet>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="600"
                        initialization="ch1-$RepresentationID$.dash?tok=abc"
                        media="ch1-$RepresentationID$-$Time$.dash?tok=abc">
        <SegmentTimeline>
          <S t="1000000" d="1152" r="15"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="video=457600" bandwidth="457600"></Representation>
    </AdaptationSet>
  </Period>
</MPD>"#
        )
    }

    #[test]
    fn test_number_mapping_replaces_audio_segment_template_only() {
        let mappings = Mutex::new(HashMap::new());
        let out =
            derive_and_rewrite_audio_number_mappings(&sample_mpd(85_000_000_000_000), &mappings);

        assert!(out.contains("media=\"/cdnnum/$RepresentationID$/$Number$\""));
        // The audio <S> entry is gone (video's, checked below, legitimately
        // still has one — "SegmentTimeline" alone isn't a safe substring to
        // assert absent, since video keeps its own untouched).
        assert!(
            !out.contains(r#"<S t="85000000000000""#),
            "audio timeline entry should be gone: {out}"
        );
        // Video's SegmentTemplate/SegmentTimeline must be untouched.
        assert!(out.contains(r#"media="ch1-$RepresentationID$-$Time$.dash?tok=abc""#));
        assert!(out.contains(r#"<S t="1000000" d="1152" r="15"/>"#));
        // initialization stays as the original CDN pattern (unchanged, still
        // routed through the existing /cdn/ passthrough).
        assert!(out.contains(r#"initialization="ch1-$RepresentationID$.dash?tok=abc""#));
    }

    #[test]
    fn test_number_mapping_established_and_reused_across_refreshes() {
        let mappings = Mutex::new(HashMap::new());
        let epoch_t = 85_000_000_000_000u64;

        let first = derive_and_rewrite_audio_number_mappings(&sample_mpd(epoch_t), &mappings);
        assert!(
            first.contains(r#"startNumber="1""#),
            "first window: {first}"
        );

        // 2 segments later (t advances by 2 * 92160).
        let second =
            derive_and_rewrite_audio_number_mappings(&sample_mpd(epoch_t + 2 * 92160), &mappings);
        assert!(
            second.contains(r#"startNumber="3""#),
            "second window: {second}"
        );

        let stored = mappings.lock().unwrap();
        let m = stored.get("audio_fra=104000").expect("mapping established");
        assert_eq!(m.epoch_t, epoch_t, "epoch must stay fixed once established");
        assert_eq!(m.duration, 92160);
    }

    #[test]
    fn test_number_mapping_falls_back_when_duration_changes() {
        let mappings = Mutex::new(HashMap::new());
        let epoch_t = 85_000_000_000_000u64;
        let _ = derive_and_rewrite_audio_number_mappings(&sample_mpd(epoch_t), &mappings);

        // Simulate a duration change (e.g. an SCTE ad-break splice) on the
        // next refresh — same shape, different `d`.
        let changed = sample_mpd(epoch_t + 92160).replace(r#"d="92160""#, r#"d="48000""#);
        let out = derive_and_rewrite_audio_number_mappings(&changed, &mappings);

        // Falls back to passthrough: original SegmentTimeline preserved,
        // no /cdnnum/ rewrite for this refresh.
        assert!(!out.contains("/cdnnum/"), "should not rewrite: {out}");
        assert!(out.contains("SegmentTimeline"));

        // The stored mapping is untouched by the failed attempt.
        let stored = mappings.lock().unwrap();
        assert_eq!(stored.get("audio_fra=104000").unwrap().duration, 92160);
    }

    #[test]
    fn test_number_mapping_falls_back_on_multiple_s_entries() {
        let mappings = Mutex::new(HashMap::new());
        let mpd = sample_mpd(85_000_000_000_000).replace(
            r#"<S t="85000000000000" d="92160" r="15"/>"#,
            r#"<S t="85000000000000" d="92160" r="7"/><S t="85000000737280" d="92160" r="7"/>"#,
        );
        let out = derive_and_rewrite_audio_number_mappings(&mpd, &mappings);
        assert!(
            !out.contains("/cdnnum/"),
            "multi-run timeline should pass through: {out}"
        );
        assert!(mappings.lock().unwrap().is_empty());
    }

    #[test]
    fn test_number_mapping_round_trips_through_dispatch_route() {
        let mappings = Mutex::new(HashMap::new());
        let epoch_t = 85_000_000_000_000u64;
        derive_and_rewrite_audio_number_mappings(&sample_mpd(epoch_t), &mappings);

        // Segment number 3 → t = epoch_t + 2*duration.
        let cdn_path = number_route_to_cdn_path("audio_fra=104000/3", &mappings)
            .expect("mapping exists for this repid");
        assert_eq!(
            cdn_path,
            format!(
                "https/cdn.example.fr/live/ch1/dash/ch1-audio_fra=104000-{}.dash?tok=abc",
                epoch_t + 2 * 92160
            )
        );
    }

    #[test]
    fn test_number_route_to_cdn_path_unknown_repid_is_none() {
        let mappings: NumberMappings = Mutex::new(HashMap::new());
        assert!(number_route_to_cdn_path("nonexistent=1/1", &mappings).is_none());
    }

    #[test]
    fn test_number_mapping_time_and_number_round_trip() {
        let m = NumberMapping {
            duration: 92160,
            epoch_t: 1000,
            media_pattern: "https://x/$RepresentationID$-$Time$".to_string(),
        };
        assert_eq!(m.time_for_number(1), 1000);
        assert_eq!(m.time_for_number(2), 1000 + 92160);
        assert_eq!(m.number_for_time(1000), 1);
        assert_eq!(m.number_for_time(1000 + 92160), 2);
        assert_eq!(m.number_for_time(1000 + 92160 + 1), 2); // rounds down
        assert_eq!(
            m.resolve_url("audio_fra=104000", 3),
            format!("https://x/audio_fra=104000-{}", 1000 + 2 * 92160)
        );
    }
}
