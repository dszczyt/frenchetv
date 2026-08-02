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
use std::sync::{Arc, Mutex};
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

    // Build initial MPD as a fallback (used if the first CDN refresh fails).
    let (mpd_fallback, initial_hosts) = rewrite_mpd(&mpd_text, &mpd_base_url, port);
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
        repeat_tracker: Mutex::new(HashMap::new()),
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
    /// FIX (audio-loop): `cdn_path -> (last_seen_at, confirmed_offset)` for
    /// `/cdn/` segment requests.
    ///
    /// Orange's live packager serves media segments before its own manifest
    /// lists them (confirmed by tracing) — the DASH demuxer computes and
    /// requests that not-yet-listed segment correctly, but has no dedup for
    /// identical `tfdt`, so it re-appends the same ~2 s of audio to its decode
    /// queue on every retry (confirmed via mpv's own trace log: same `pts`,
    /// `append packet to audio` called on every repeat). Once the manifest
    /// catches up, both would resolve, but that takes ~2 s per stall — this
    /// tracker lets `dispatch` recognise a same-path retry and hand back the
    /// next segment (extrapolated forward — see `extrapolated_cdn_path`)
    /// instead of the byte-identical one, so the retry becomes progress
    /// instead of a loop.
    ///
    /// `confirmed_offset` only advances when an extrapolated fetch actually
    /// succeeds (via `commit_repeat_offset`) — a run of CDN 404s keeps retrying
    /// the *same* next candidate rather than speculatively jumping further
    /// out, so this can never overshoot further than the CDN has actually
    /// published. Entries older than `REPEAT_WINDOW` are pruned on each call
    /// so this stays bounded.
    repeat_tracker: RepeatTracker,
}

async fn handle_connection(stream: TcpStream, state: Arc<ProxyState>) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .await
        .context("read request line")?;

    // Parse: "GET /path HTTP/1.1\r\n"
    let parts: Vec<&str> = request_line.trim().splitn(3, ' ').collect();
    if parts.len() < 2 {
        bail!("malformed request");
    }
    let path = parts[1];

    // Drain headers (we don't need them)
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
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
        // FIX (audio-loop): on a same-path retry, try to hand back the next
        // segment instead of re-serving identical bytes — see `repeat_tracker`
        // field doc and `extrapolated_cdn_path`.
        //
        // Audio only. Video was never the broken track — every original
        // trace showed it fetched exactly once per segment — and applying
        // this to video too caused its own regression: video re-opens the
        // same segment for reasons unrelated to the manifest stall (observed:
        // most video segments now fetched 2-3x), and its fallback-to-earlier-
        // offset path then visibly rewinds frames. Gating on mime keeps the
        // fix scoped to the track that's actually stuck.
        let is_audio = mime_hint_from_cdn_path(cdn_path) == Some("audio");
        let (is_repeat, served_offset) = if is_audio {
            record_repeat(&state.repeat_tracker, cdn_path)
        } else {
            (false, 0)
        };
        let candidate_offset = served_offset + 1;
        let duration = is_repeat
            .then(|| segment_duration_for(state, "audio"))
            .flatten();
        let extrapolated =
            duration.and_then(|d| extrapolated_cdn_path(cdn_path, candidate_offset, d));
        let effective_path = extrapolated.as_deref().unwrap_or(cdn_path);
        if effective_path != cdn_path {
            tracing::debug!(
                "DRM proxy: segment repeat (confirmed +{}) — trying +{} instead of re-serving identical bytes",
                served_offset,
                candidate_offset
            );
        }

        match fetch_and_decrypt(effective_path, state).await {
            Ok(data) => {
                if effective_path != cdn_path {
                    // Confirmed available — the *next* repeat should try one
                    // further, not retry this same offset again.
                    commit_repeat_offset(&state.repeat_tracker, cdn_path, candidate_offset);
                }
                return ("200 OK", "video/mp4", data);
            }
            Err(mut e) if effective_path != cdn_path => {
                // Not on the CDN yet. ffmpeg only issues its next request after
                // this one's response arrives (confirmed via mpv trace — retry
                // spacing tracks fetch latency, not a fixed timer), so holding
                // this response a little longer directly throttles the retry
                // storm instead of racing it: wait and retry the SAME candidate
                // (not jump further) a few times before giving up — the
                // manifest itself typically catches up within ~2s (see
                // `MPD_TTL` history), so most stalls resolve inside this wait.
                let deadline = std::time::Instant::now() + EXTRAPOLATE_MAX_WAIT;
                let mut retried_ok = None;
                while std::time::Instant::now() < deadline {
                    tokio::time::sleep(EXTRAPOLATE_RETRY_DELAY).await;
                    match fetch_and_decrypt(effective_path, state).await {
                        Ok(data) => {
                            retried_ok = Some(data);
                            break;
                        }
                        Err(e2) => e = e2,
                    }
                }
                if let Some(data) = retried_ok {
                    commit_repeat_offset(&state.repeat_tracker, cdn_path, candidate_offset);
                    return ("200 OK", "video/mp4", data);
                }

                // Still not available — fall back to the last segment we know
                // is good: if a previous repeat already confirmed offset N,
                // that's the segment ffmpeg most recently decoded, and it's the
                // same content it would decode again if this response were
                // simply dropped. Falling all the way back to the *original*
                // (offset 0) instead would hand back content ffmpeg already
                // played, moving dts backward and reproducing exactly the loop
                // this exists to fix.
                tracing::warn!(
                    "DRM proxy: extrapolated segment still unavailable after waiting ({:#}), falling back to last confirmed offset (+{})",
                    e,
                    served_offset
                );
                let fallback = if served_offset > 0 {
                    duration.and_then(|d| extrapolated_cdn_path(cdn_path, served_offset, d))
                } else {
                    None
                };
                let fallback_path = fallback.as_deref().unwrap_or(cdn_path);
                return match fetch_and_decrypt(fallback_path, state).await {
                    Ok(data) => ("200 OK", "video/mp4", data),
                    Err(e) => {
                        tracing::error!("DRM proxy segment error: {:#}", e);
                        ("502 Bad Gateway", "text/plain", e.to_string().into_bytes())
                    }
                };
            }
            Err(e) => {
                tracing::error!("DRM proxy segment error: {:#}", e);
                return ("502 Bad Gateway", "text/plain", e.to_string().into_bytes());
            }
        }
    }

    ("404 Not Found", "text/plain", b"not found".to_vec())
}

/// FIX (audio-loop): repeats decay after `REPEAT_WINDOW` so only requests that
/// arrive back-to-back for the same path (the retry-storm case, not a later
/// legitimate re-request) accumulate a nonzero index.
///
/// Must stay comfortably larger than `EXTRAPOLATE_MAX_WAIT`: while a repeat is
/// being held (see below), ffmpeg's *next* request for the same path can't
/// arrive until this one returns — confirmed via mpv trace, it only issues
/// its next read after the previous one completes. If this window were
/// shorter than (or too close to) the hold time, the tracker entry could age
/// out and get pruned right as the held response finally arrives, silently
/// resetting confirmed progress back to offset 0.
const REPEAT_WINDOW: std::time::Duration = std::time::Duration::from_millis(4_000);

/// FIX (audio-loop): how long to wait between retries of a not-yet-available
/// extrapolated segment, and the hard ceiling on total time spent waiting for
/// one.
///
/// This is a direct tradeoff with no value that removes it: every fallback
/// response (see `dispatch`) hands ffmpeg already-decoded content, which
/// either replays as a stutter (frequent, small) or a rewind (rare, more
/// noticeable) depending on how long `EXTRAPOLATE_MAX_WAIT` is — a longer
/// wait resolves more stalls with genuinely new content instead of a
/// fallback, but makes the fallback more jarring on the stalls it doesn't
/// resolve, since fewer/rarer fallbacks are individually easier to notice
/// than many small ones. 900ms is a middle point between the two extremes
/// tried (500ms: frequent small stutters; 2200ms, close to mpv's own
/// `audio-buffer=2.5s` in `libmpv.rs`: rare but distinct rewinds). This trades
/// audible duplicate segments (the original bug) for the CDN's own publish
/// latency: instead of giving up quickly and re-serving already-played
/// content (which ffmpeg re-decodes — confirmed via mpv trace, no dedup),
/// holding the response until genuinely new content exists means most stalls
/// resolve with zero duplicates. The tradeoff is that a stall longer than the
/// buffer can absorb becomes a brief audio stutter instead of a repeat — a
/// different symptom, not a guarantee this eliminates all cases. Both
/// constants are well under mpv's own 60 s read timeout.
const EXTRAPOLATE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(150);
const EXTRAPOLATE_MAX_WAIT: std::time::Duration = std::time::Duration::from_millis(900);

/// Returns `(is_repeat, confirmed_offset)` for `cdn_path`: whether it's been
/// seen within `REPEAT_WINDOW` before, and how many segment-durations beyond
/// the originally requested time have been confirmed available so far (see
/// `ProxyState::repeat_tracker`). Records this request's timestamp but does
/// NOT itself advance `confirmed_offset` — only `commit_repeat_offset` does,
/// after a fetch at that offset actually succeeds.
type RepeatTracker = Mutex<HashMap<String, (std::time::Instant, u32)>>;

fn record_repeat(tracker: &RepeatTracker, cdn_path: &str) -> (bool, u32) {
    let mut tracker = tracker.lock().unwrap();
    let now = std::time::Instant::now();
    tracker.retain(|_, (seen, _)| now.duration_since(*seen) < REPEAT_WINDOW);
    match tracker.get_mut(cdn_path) {
        Some((seen, offset)) => {
            *seen = now;
            (true, *offset)
        }
        None => {
            tracker.insert(cdn_path.to_string(), (now, 0));
            (false, 0)
        }
    }
}

/// Records that segments up to `offset` segment-durations beyond `cdn_path`'s
/// originally requested time are confirmed available (a fetch at `offset` just
/// succeeded), so the next repeat tries one further instead of retrying the
/// same offset again. No-op if the entry aged out between the fetch starting
/// and completing — nothing to update.
fn commit_repeat_offset(tracker: &RepeatTracker, cdn_path: &str, offset: u32) {
    if let Some(entry) = tracker.lock().unwrap().get_mut(cdn_path) {
        entry.1 = entry.1.max(offset);
    }
}

/// `"audio"` or `"video"` inferred from a `/cdn/` segment path's own naming
/// convention (e.g. `...-audio_104130_fra=...` / `...-video=...`), so the
/// caller can look up that representation's segment duration. `None` for
/// anything else (init segments, unrecognised naming) — extrapolation is
/// skipped rather than guessed at.
fn mime_hint_from_cdn_path(cdn_path: &str) -> Option<&'static str> {
    if cdn_path.contains("-audio") {
        Some("audio")
    } else if cdn_path.contains("-video") {
        Some("video")
    } else {
        None
    }
}

/// The `d` (duration, in the representation's own timescale units — the same
/// units as the `$Time$` value in segment URLs) of the most recent
/// `SegmentTimeline` entry for `mime_prefix`, read from whatever manifest is
/// currently cached. `None` if no manifest has been fetched yet or the
/// timeline can't be parsed.
fn segment_duration_for(state: &Arc<ProxyState>, mime_prefix: &str) -> Option<u64> {
    let cache = state.mpd_cache.lock().unwrap();
    let (_, mpd) = cache.as_ref()?;
    let (first, _, _) = segment_timeline_summary(mpd, mime_prefix)?;
    parse_attr_u64(&first, "d")
}

/// The integer value of `attr="..."` inside a raw `<S .../>` tag's text.
fn parse_attr_u64(tag: &str, attr: &str) -> Option<u64> {
    let needle = format!("{attr}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = tag.get(start..)?;
    let end = rest.find('"')?;
    rest[..end].parse().ok()
}

/// FIX (audio-loop): `cdn_path` with its trailing `-<time>.dash` timestamp
/// advanced by `offset * duration`, so a repeat asks for a genuinely later
/// segment instead of reproducing the identical request.
///
/// `offset` is `confirmed_offset + 1` (see `ProxyState::repeat_tracker`), not
/// a raw retry count — it only grows when a previous extrapolated fetch
/// actually succeeded, so this never speculates further than one segment past
/// what the CDN has already confirmed it has. `MAX_EXTRAPOLATE_SEGMENTS` is
/// therefore just a generous backstop against a runaway, not the mechanism
/// that keeps this from overshooting the live edge — evidence from the actual
/// stalls (see commit history) shows the manifest catches up within 1-2
/// confirmed segments. `None` if `cdn_path` doesn't end in the expected
/// `-<digits>.dash` shape (e.g. an init segment) or the backstop is hit.
const MAX_EXTRAPOLATE_SEGMENTS: u32 = 8;

fn extrapolated_cdn_path(cdn_path: &str, offset: u32, duration: u64) -> Option<String> {
    if offset == 0 || offset > MAX_EXTRAPOLATE_SEGMENTS || duration == 0 {
        return None;
    }
    let (base, query) = match cdn_path.find('?') {
        Some(pos) => (&cdn_path[..pos], &cdn_path[pos..]),
        None => (cdn_path, ""),
    };
    if !base.ends_with(".dash") {
        return None;
    }
    let stem = &base[..base.len() - 5];
    let last_dash = stem.rfind('-')?;
    let (prefix, digits) = (&stem[..=last_dash], &stem[last_dash + 1..]);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let time: u64 = digits.parse().ok()?;
    let next_time = time.checked_add(duration.checked_mul(u64::from(offset))?)?;
    Some(format!("{prefix}{next_time}.dash{query}"))
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
                // DIAGNOSTIC (audio-loop investigation) — see `segment_timeline_summary`.
                tracing::debug!(
                    "DRM proxy: manifest cache-hit (age={:.3}s) audio={:?} video={:?}",
                    fetched_at.elapsed().as_secs_f64(),
                    segment_timeline_summary(cached, "audio"),
                    segment_timeline_summary(cached, "video"),
                );
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
                    let (rewritten, hosts) = rewrite_mpd(&text, &final_url, state.proxy_port);
                    // Rebuild (not merge) the allowlist from what this manifest actually
                    // references — see `allowed_hosts` field doc for why this must be a
                    // set, and why it's rebuilt rather than seeded once.
                    *state.allowed_hosts.lock().unwrap() = hosts;
                    // DIAGNOSTIC (audio-loop investigation) — see `segment_timeline_summary`.
                    tracing::debug!(
                        "DRM proxy: manifest fresh-fetch audio={:?} video={:?}",
                        segment_timeline_summary(&rewritten, "audio"),
                        segment_timeline_summary(&rewritten, "video"),
                    );
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
    let elapsed = t0.elapsed();
    // Log every segment fetch; WARN if it took > 1 s (likely stall cause).
    let seg_name = cdn_path.rsplit('/').next().unwrap_or(cdn_path);
    if elapsed.as_millis() > 1000 {
        tracing::warn!(
            "DRM proxy: slow segment {}ms {}",
            elapsed.as_millis(),
            seg_name
        );
    } else {
        tracing::debug!("DRM proxy: segment {}ms {}", elapsed.as_millis(), seg_name);
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
fn rewrite_mpd(mpd: &str, mpd_base_url: &str, proxy_port: u16) -> (String, HashSet<String>) {
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

    // Step 3: Rewrite all https://... and http://... CDN URLs through the proxy.
    let mpd_rewritten = rewrite_cdn_urls(&mpd_abs, &proxy_base, mpd_base_url);

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

/// DIAGNOSTIC (audio-loop investigation): the inner text of the `SegmentTimeline`
/// belonging to the `AdaptationSet` whose `mimeType` starts with `mime_prefix`
/// (e.g. `"audio"` or `"video"`).
fn find_segment_timeline<'a>(mpd: &'a str, mime_prefix: &str) -> Option<&'a str> {
    let mut rest = mpd;
    loop {
        let pos = rest.find("<AdaptationSet")?;
        let after = &rest[pos..];
        let next_offset = after[1..].find("<AdaptationSet").map(|p| p + 1);
        let chunk = match next_offset {
            Some(p) => &after[..p],
            None => after,
        };

        let tag_end = chunk.find('>').unwrap_or(chunk.len());
        let opening_tag = &chunk[..tag_end];
        let matches_mime = opening_tag
            .find("mimeType=\"")
            .and_then(|mp| opening_tag.get(mp + "mimeType=\"".len()..))
            .map(|s| s.starts_with(mime_prefix))
            .unwrap_or(false);

        if matches_mime {
            if let Some(tl_start) = chunk.find("<SegmentTimeline") {
                if let Some(tl_end) = chunk[tl_start..].find("</SegmentTimeline>") {
                    return Some(&chunk[tl_start..tl_start + tl_end]);
                }
            }
            return None;
        }

        rest = &after[next_offset?..];
    }
}

/// DIAGNOSTIC (audio-loop investigation): `(first <S> tag text, last <S> tag
/// text, total <S> element count)` for the `SegmentTimeline` of the
/// `AdaptationSet` whose `mimeType` starts with `mime_prefix`.
///
/// The first `<S>` carries the explicit `t=` (later entries only have
/// implicit start times derived by accumulating `d`/`r`); the count lets a
/// caller tell "timeline genuinely unchanged" (tail *and* count identical
/// across polls) apart from "tail text coincidentally repeats while new
/// entries were appended earlier" (count grows). Only `t`/`d`/`r` timing
/// attributes are returned — never a CDN URL or token — so this is safe to
/// log at debug level. Remove once root cause of the "audio loops, video ok"
/// bug is confirmed.
fn segment_timeline_summary(mpd: &str, mime_prefix: &str) -> Option<(String, String, usize)> {
    let timeline = find_segment_timeline(mpd, mime_prefix)?;
    let s_tag_at = |start: usize| -> String {
        let s_tag = &timeline[start..];
        let end = s_tag.find('>').map(|p| p + 1).unwrap_or(s_tag.len());
        s_tag[..end].trim().to_string()
    };
    let first = s_tag_at(timeline.find("<S ")?);
    let last = s_tag_at(timeline.rfind("<S ")?);
    let count = timeline.matches("<S ").count();
    Some((first, last, count))
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
    fn test_record_repeat_first_request_is_not_a_repeat() {
        let tracker: RepeatTracker = Mutex::new(HashMap::new());
        assert_eq!(record_repeat(&tracker, "p"), (false, 0));
    }

    #[test]
    fn test_record_repeat_offset_only_advances_on_commit() {
        let tracker: RepeatTracker = Mutex::new(HashMap::new());
        record_repeat(&tracker, "p");
        // A repeat with no commit in between must keep reporting offset 0 —
        // this is the exact bug: a bare repeat count (not a confirmed
        // advance) would report 1, 2, 3... regardless of whether any of
        // those candidate segments actually existed on the CDN, so once the
        // raw count passed a small cap every subsequent retry fell straight
        // back to re-serving the original (identical) segment again.
        assert_eq!(record_repeat(&tracker, "p"), (true, 0));
        assert_eq!(record_repeat(&tracker, "p"), (true, 0));

        commit_repeat_offset(&tracker, "p", 1);
        assert_eq!(record_repeat(&tracker, "p"), (true, 1));

        // A run of failed extrapolation attempts (no commit calls) must not
        // move the offset — the next repeat retries the same +1 candidate,
        // not jump further out on every single retry the way the old
        // `repeat_index`-scaled version did.
        assert_eq!(record_repeat(&tracker, "p"), (true, 1));
        assert_eq!(record_repeat(&tracker, "p"), (true, 1));
    }

    #[test]
    fn test_record_repeat_survives_many_retries_without_a_commit() {
        // Regression: a stall of 40+ raw retries (observed in production
        // traces) must not exhaust anything — the offset simply stays at
        // whatever was last confirmed, indefinitely.
        let tracker: RepeatTracker = Mutex::new(HashMap::new());
        record_repeat(&tracker, "p");
        for _ in 0..50 {
            assert_eq!(record_repeat(&tracker, "p"), (true, 0));
        }
    }

    #[test]
    fn test_commit_repeat_offset_never_goes_backward() {
        let tracker: RepeatTracker = Mutex::new(HashMap::new());
        record_repeat(&tracker, "p");
        commit_repeat_offset(&tracker, "p", 3);
        // A late-arriving commit for an already-superseded lower offset must
        // not regress progress already made.
        commit_repeat_offset(&tracker, "p", 1);
        assert_eq!(record_repeat(&tracker, "p"), (true, 3));
    }

    #[test]
    fn test_extrapolated_cdn_path_advances_by_repeat_index_times_duration() {
        let path = "https/cdn.example/live/ch1-audio_104130_fra=104000-1000.dash?a=1";
        assert_eq!(
            extrapolated_cdn_path(path, 1, 92160).as_deref(),
            Some("https/cdn.example/live/ch1-audio_104130_fra=104000-93160.dash?a=1")
        );
        assert_eq!(
            extrapolated_cdn_path(path, 2, 92160).as_deref(),
            Some("https/cdn.example/live/ch1-audio_104130_fra=104000-185320.dash?a=1")
        );
    }

    #[test]
    fn test_extrapolated_cdn_path_none_for_repeat_index_zero() {
        let path = "https/cdn.example/live/ch1-audio_104130_fra=104000-1000.dash";
        assert_eq!(extrapolated_cdn_path(path, 0, 92160), None);
    }

    #[test]
    fn test_extrapolated_cdn_path_none_past_cap() {
        let path = "https/cdn.example/live/ch1-audio_104130_fra=104000-1000.dash";
        assert_eq!(
            extrapolated_cdn_path(path, MAX_EXTRAPOLATE_SEGMENTS + 1, 92160),
            None
        );
    }

    #[test]
    fn test_extrapolated_cdn_path_none_for_non_timestamped_path() {
        // Init segments (no trailing `-<digits>.dash`) must not be extrapolated.
        let path = "https/cdn.example/live/ch1-audio_104130_fra=104000.dash";
        assert_eq!(extrapolated_cdn_path(path, 1, 92160), None);
    }

    #[test]
    fn test_mime_hint_from_cdn_path() {
        assert_eq!(
            mime_hint_from_cdn_path("https/cdn.example/ch1-audio_104130_fra=104000-1000.dash"),
            Some("audio")
        );
        assert_eq!(
            mime_hint_from_cdn_path("https/cdn.example/ch1-video=930000-1000.dash"),
            Some("video")
        );
        assert_eq!(
            mime_hint_from_cdn_path("https/cdn.example/ch1-video=930000.dash"),
            Some("video")
        );
        assert_eq!(
            mime_hint_from_cdn_path("https/cdn.example/manifest.mpd"),
            None
        );
    }

    #[test]
    fn test_parse_attr_u64() {
        assert_eq!(
            parse_attr_u64("<S t=\"85711577105083\" d=\"92160\" r=\"15\"/>", "d"),
            Some(92160)
        );
        assert_eq!(
            parse_attr_u64("<S t=\"85711577105083\" d=\"92160\" r=\"15\"/>", "r"),
            Some(15)
        );
        assert_eq!(parse_attr_u64("<S d=\"92160\"/>", "t"), None);
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
        let (_, found) = rewrite_mpd(mpd, "https://cdnfr.orange.fr/live/ch1/manifest.mpd", 12345);
        assert!(found.contains("https://cdnfr.orange.fr"));
    }
}
