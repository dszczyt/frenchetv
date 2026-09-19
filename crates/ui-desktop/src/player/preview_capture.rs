//! Desktop preview capture: one headless `mpv` process per frame.
//!
//! The guide wants a recent still of every visible row. Producing one means
//! decoding live DASH, which on a protected channel also means a Widevine
//! license exchange and a DRM proxy — the same machinery the real player uses,
//! for a 256x144 thumbnail nobody is watching. So the governing rule here is
//! not throughput, it is restraint: **capture yields to playback**. A preview
//! that costs the focused stream a dropped frame has made the application
//! worse, and no amount of thumbnail freshness buys that back.
//!
//! Three choices follow from that rule.
//!
//! **A separate process, not a second in-process libmpv.** `libmpv.rs` already
//! proves that rendering a frame into a plain CPU buffer is reachable from this
//! dependency set (`MPV_RENDER_API_TYPE_SW` via `libmpv2_sys`), and the GL/FBO
//! path the design sketch describes exists there too. Either could have been
//! reused if `libmpv.rs` were in scope to refactor. Neither is worth
//! duplicating here, because the decisive property is isolation, not API
//! elegance: a decoder in another process cannot deadlock the egui loop, does
//! not add demux and decode threads to a process whose render loop
//! `LibMpvPlayer`'s own notes already suspect of losing races with the DRM
//! proxy, and — the part that matters most — can be abandoned instantly by
//! killing it. In-process, abandoning means asking libmpv to stop and waiting
//! for `mpv_terminate_destroy` to agree.
//!
//! Be precise about how far that isolation goes: **only decode leaves this
//! process.** Resolving the stream, loading the CDM, the license exchange and
//! the DRM proxy serving the capture all still run here, on this module's own
//! single-threaded runtime. That bounds them; it does not banish them.
//!
//! **`--vo=image --frames=1`, not `screenshot-raw`.** `screenshot-raw` returns
//! its pixels in an `mpv_node`, which `libmpv2`'s safe `command()` cannot
//! deliver — it wraps `mpv_command_string` and returns `()`. Reading it needs
//! raw `mpv_command_node` FFI and manual node traversal. `vo=image` needs
//! none of that: mpv decodes one frame, writes one PNG, exits.
//!
//! **Downscaled by mpv, before the pixels ever exist.** The `--vf` chain scales
//! and letterboxes to exactly [`PREVIEW_WIDTH`]x[`PREVIEW_HEIGHT`] inside the
//! decoder process, so what crosses the process boundary is a ~4KB PNG rather
//! than a 1080p frame that this process would then have to resize.
//!
//! Previews are decoration. Every failure here is a dropped frame and a log
//! line, never an error the user sees.

// Nothing in the binary references this module until the guide screen wires it
// into `app.rs`. Without this the whole file reads as dead code and fails
// `-D warnings`.
#![allow(dead_code)]

use egui::ColorImage;
use frenchetv_core::{Channel, Operator, StreamUrl};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use ui_shared::preview::{PreviewCapture, PREVIEW_HEIGHT, PREVIEW_WIDTH};

/// Everything one capture is allowed to consume, wall clock, from the moment
/// the worker picks the request up.
///
/// It covers the lot: `resolve_stream`, CDM load, MPD fetch, license exchange,
/// proxy start, mpv launch, DASH buffering, decode. Measured against a real
/// operator that is expected to land somewhere around 4-7s, most of it in the
/// license exchange — uncomfortably close to this ceiling. If captures turn out
/// to time out routinely, the fix is the key caching the design document
/// already describes as a known optimisation, not a bigger number here: a
/// channel that cannot produce a frame in eight seconds is one the rotation is
/// better off skipping.
const CAPTURE_BUDGET: Duration = Duration::from_secs(8);

/// Handed to mpv so a stalled CDN cannot hold a capture open for mpv's own
/// 60s default. Deliberately well inside [`CAPTURE_BUDGET`] so mpv gives up
/// and reports before the budget has to kill it.
const NETWORK_TIMEOUT_SECS: u64 = 5;

/// The operator handle, shared with `app.rs`, which owns the authenticated
/// session. Capture borrows it to resolve a stream and holds it no longer than
/// that call.
pub type SharedOperator = Arc<tokio::sync::Mutex<Box<dyn Operator>>>;

// ── Public face ───────────────────────────────────────────────────────────────

/// [`PreviewCapture`] over a headless `mpv`.
///
/// One worker thread, one capture at a time. The worker owns a small tokio
/// runtime of its own rather than borrowing the application's, so that the
/// license exchange and the DRM proxy serving this capture are bounded to a
/// single extra thread no matter what the rest of the app is doing.
pub struct MpvPreviewCapture {
    engine: Arc<Engine>,
    results: std::sync::mpsc::Receiver<Outcome>,
    /// Drained out of `results` by [`Self::drain`]; split in two because the
    /// trait's `poll` returns only frames and must not swallow the failures.
    ready_frames: Vec<(String, ColorImage)>,
    ready_failures: Vec<String>,
    /// `None` when there is no usable `mpv` binary, or the worker refused to
    /// start. Requests then fail immediately instead of waiting out the budget.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl MpvPreviewCapture {
    /// Starts the capture worker.
    ///
    /// Returns a working value even when `mpv` is missing: previews are
    /// decoration, so an install without the binary loses thumbnails and
    /// nothing else. Every request is then reported as a failure straight
    /// away, which lets the scheduler's back-off do the right thing instead of
    /// retrying into a wall.
    pub fn new(operator: SharedOperator) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let engine = Arc::new(Engine::new());

        let worker = match locate_mpv() {
            Some(mpv_bin) => {
                let engine_for_worker = Arc::clone(&engine);
                std::thread::Builder::new()
                    .name("preview-capture".into())
                    .spawn(move || worker_main(engine_for_worker, operator, mpv_bin, tx))
                    .map_err(|e| tracing::warn!("preview: worker thread refused to start: {e}"))
                    .ok()
            }
            None => {
                tracing::info!(
                    "preview: no `mpv` executable on PATH — guide rows will show logos only. \
                     libmpv (which the player embeds) does not install one; the `mpv` package does."
                );
                None
            }
        };

        Self {
            engine,
            results: rx,
            ready_frames: Vec::new(),
            ready_failures: Vec::new(),
            worker,
        }
    }

    /// Channels whose capture gave up, since the last call.
    ///
    /// [`PreviewCapture`] has no failure channel — `poll` reports successes
    /// only — but `PreviewCache::mark_requested` puts a channel in flight and
    /// only a frame or a `mark_failed` takes it out again. Without this, a
    /// channel that times out stays in flight forever and the scheduler, which
    /// never targets an in-flight channel, skips it for the rest of the
    /// True when captures can actually run — i.e. an `mpv` binary was found.
    pub fn is_available(&self) -> bool {
        self.worker.is_some()
    }

    fn drain(&mut self) {
        while let Ok(outcome) = self.results.try_recv() {
            match outcome {
                Outcome::Frame { channel_id, image } => {
                    self.ready_frames.push((channel_id, image));
                }
                Outcome::Failed { channel_id } => self.ready_failures.push(channel_id),
            }
        }
    }
}

impl PreviewCapture for MpvPreviewCapture {
    fn request(&mut self, channel: &Channel) {
        if self.worker.is_none() {
            self.ready_failures.push(channel.id.clone());
            return;
        }
        self.engine.post(channel.clone());
    }

    fn poll(&mut self) -> Vec<(String, ColorImage)> {
        self.drain();
        std::mem::take(&mut self.ready_frames)
    }

    fn poll_failures(&mut self) -> Vec<String> {
        self.drain();
        std::mem::take(&mut self.ready_failures)
    }

    fn cancel(&mut self) {
        self.engine.cancel();
    }
}

impl Drop for MpvPreviewCapture {
    /// Tells the worker to stop and does **not** wait for it.
    ///
    /// Teardown has to kill mpv, drop a DRM proxy and shut down a runtime.
    /// That is bounded but not instant, and it happens on the way out of the
    /// guide — i.e. on the frame the user is watching. It runs on the worker
    /// thread, which owns everything that needs dropping, so leaving it to
    /// finish unobserved costs the UI nothing.
    fn drop(&mut self) {
        self.engine.shutdown();
    }
}

// ── Worker inbox and cancellation ────────────────────────────────────────────

struct Slot {
    pending: Option<Channel>,
    shutdown: bool,
}

/// The worker's inbox and its cancellation signal, behind one lock.
///
/// They share a mutex deliberately. `cancel()` has to be able to abandon a
/// capture the worker has *just* taken out of the slot, and that is only
/// well-defined if taking a request and bumping the generation cannot
/// interleave. The generation lives in a `watch` channel rather than a
/// `Notify` for the same reason: a `watch` receiver created while the request
/// was taken reports a change that happened before anyone awaited it, so a
/// cancel can never be lost in the gap between pick-up and the first `await`.
struct Engine {
    slot: Mutex<Slot>,
    ready: Condvar,
    generation: watch::Sender<u64>,
}

impl Engine {
    fn new() -> Self {
        Self {
            slot: Mutex::new(Slot {
                pending: None,
                shutdown: false,
            }),
            ready: Condvar::new(),
            generation: watch::channel(0).0,
        }
    }

    /// Queues `channel`, replacing anything already waiting.
    ///
    /// One slot, newest wins: a backlog would have the capture engine chasing
    /// rows the user scrolled past several seconds ago, which is exactly what
    /// the scheduler's focus debounce exists to prevent.
    fn post(&self, channel: Channel) {
        let mut slot = lock(&self.slot);
        if slot.shutdown {
            return;
        }
        slot.pending = Some(channel);
        self.ready.notify_one();
    }

    /// Abandons the queued request and whatever is in flight.
    fn cancel(&self) {
        let mut slot = lock(&self.slot);
        slot.pending = None;
        self.generation.send_modify(|g| *g += 1);
    }

    fn shutdown(&self) {
        let mut slot = lock(&self.slot);
        slot.pending = None;
        slot.shutdown = true;
        self.generation.send_modify(|g| *g += 1);
        self.ready.notify_all();
    }

    /// Blocks until there is a request, or until shutdown (`None`).
    ///
    /// The returned receiver is subscribed while the slot lock is held, so it
    /// observes every cancel from this moment on.
    fn take(&self) -> Option<(Channel, watch::Receiver<u64>)> {
        let mut slot = lock(&self.slot);
        loop {
            if slot.shutdown {
                return None;
            }
            if let Some(channel) = slot.pending.take() {
                let mut rx = self.generation.subscribe();
                rx.borrow_and_update();
                return Some((channel, rx));
            }
            slot = match self.ready.wait(slot) {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }
}

/// A poisoned inbox mutex means a previous capture panicked while holding it.
/// The slot holds a `Channel` and two flags; there is no invariant left to
/// violate, and refusing to serve previews forever because one capture panicked
/// is the worse outcome.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── Worker ────────────────────────────────────────────────────────────────────

enum Outcome {
    Frame {
        channel_id: String,
        image: ColorImage,
    },
    Failed {
        channel_id: String,
    },
}

#[derive(Debug)]
enum CaptureError {
    /// The guide took the decoder back. Not reported: nobody asked for a frame
    /// any more, and counting it as a failure would back off a channel that
    /// did nothing wrong.
    Cancelled,
    TimedOut,
    Failed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::TimedOut => write!(f, "timed out after {}s", CAPTURE_BUDGET.as_secs()),
            Self::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<anyhow::Error> for CaptureError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(format!("{e:#}"))
    }
}

fn worker_main(
    engine: Arc<Engine>,
    operator: SharedOperator,
    mpv_bin: PathBuf,
    results: std::sync::mpsc::Sender<Outcome>,
) {
    // One worker thread, and `enable_all` for the timer and the process reaper.
    // The DRM proxy this capture starts lives on this runtime too, so its
    // segment decryption is bounded to the same single thread — that is the
    // point. The focused player's proxy runs on the application's own runtime
    // and is untouched by anything here.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("preview-capture-rt")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!("preview: runtime build failed: {e} — previews disabled");
            return;
        }
    };

    while let Some((channel, mut generation)) = engine.take() {
        let channel_id = channel.id.clone();
        let outcome = runtime.block_on(under_budget(
            capture_frame(&operator, &channel, &mpv_bin),
            &mut generation,
            CAPTURE_BUDGET,
        ));

        let sent = match outcome {
            Ok(image) => {
                tracing::debug!("preview: captured {}", channel.name);
                results.send(Outcome::Frame { channel_id, image })
            }
            Err(CaptureError::Cancelled) => {
                tracing::trace!("preview: {} abandoned", channel.name);
                continue;
            }
            Err(e) => {
                tracing::debug!("preview: {} failed: {e}", channel.name);
                results.send(Outcome::Failed { channel_id })
            }
        };
        if sent.is_err() {
            break; // The guide is gone; so is any reason to keep capturing.
        }
    }

    // On this thread, never the UI's: dropping the runtime waits for the proxy
    // task and the reaper to wind down.
    runtime.shutdown_background();
}

/// Runs `work` under the two rules that let capture yield to playback: it stops
/// the moment the guide cancels, and it stops when the budget is gone.
///
/// `biased` so a cancel that lands in the same poll as a finished capture still
/// wins — delivering a frame the guide has already disowned is how a stale
/// thumbnail ends up on the wrong row.
///
/// Losing either race drops `work`, and dropping it is what actually abandons
/// the capture: the in-flight request, the DRM proxy, the temporary directory
/// and the mpv child (spawned with `kill_on_drop`) all go with it. That matters
/// because the child does not exist for most of a capture's life — the license
/// exchange happens first — so killing a process was never going to be enough
/// on its own.
async fn under_budget<F, T>(
    work: F,
    generation: &mut watch::Receiver<u64>,
    budget: Duration,
) -> Result<T, CaptureError>
where
    F: Future<Output = Result<T, CaptureError>>,
{
    tokio::select! {
        biased;
        _ = generation.changed() => Err(CaptureError::Cancelled),
        _ = tokio::time::sleep(budget) => Err(CaptureError::TimedOut),
        out = work => out,
    }
}

/// Resolve, bring up DRM if the channel needs it, run mpv, decode the PNG.
async fn capture_frame(
    operator: &SharedOperator,
    channel: &Channel,
    mpv_bin: &Path,
) -> Result<ColorImage, CaptureError> {
    let stream = {
        // Held only for the resolve call. The focused player's own
        // `resolve_stream` takes the same lock; tokio's mutex is fair, so a
        // user tuning in queues ahead of us rather than behind us.
        let operator = operator.lock().await;
        operator
            .resolve_stream(channel)
            .await
            .map_err(|e| CaptureError::Failed(format!("resolve: {e}")))?
    };

    // Bound to this scope so the proxy outlives mpv and dies with the capture.
    let _proxy;
    let (url, headers, auth) = if stream.protection.is_some() {
        let proxy = drm_bring_up(&stream).await?;
        let url = proxy.mpd_url.clone();
        _proxy = Some(proxy);
        // Exactly what `app.rs` hands the real player once its proxy is up: a
        // bare localhost URL. The proxy holds the CDN credentials; mpv never
        // sees them.
        (url, Vec::new(), None)
    } else {
        _proxy = None;
        (
            stream.url.to_string(),
            stream.headers.clone(),
            stream.auth_header.clone(),
        )
    };

    let workdir = WorkDir::new().map_err(|e| CaptureError::Failed(format!("workdir: {e}")))?;
    workdir
        .write_options(&header_config(&headers, auth.as_deref()))
        .map_err(|e| CaptureError::Failed(format!("workdir: {e}")))?;

    let mut child = tokio::process::Command::new(mpv_bin)
        .args(mpv_args(&url, workdir.frames_dir(), workdir.options_file()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        // mpv's diagnostics are dropped on purpose. Draining them would mean
        // either a pipe this code must keep reading or a log file, and at error
        // level mpv quotes the URL it failed on — which for an unprotected
        // channel is a signed CDN URL. A failed thumbnail is not worth a token
        // in a logfile.
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CaptureError::Failed(format!("spawn mpv: {e}")))?;

    let status = child
        .wait()
        .await
        .map_err(|e| CaptureError::Failed(format!("mpv wait: {e}")))?;

    // The file is the authority, not the exit code: mpv reports a non-zero
    // status for a live stream that ends right after the frame we asked for,
    // and that frame is perfectly good.
    let png = workdir
        .read_frame()
        .ok_or_else(|| CaptureError::Failed(format!("mpv produced no frame ({status})")))?;

    decode_preview(&png).map_err(CaptureError::from)
}

// ── DRM bring-up ──────────────────────────────────────────────────────────────

/// Stand up a private DRM proxy for one capture.
///
/// This repeats `app.rs::start_drm_proxy` — CDM, manifest, license, proxy —
/// because that function is wired into the application's message loop and
/// cannot be called from here. The duplication is real and should be resolved
/// by lifting the sequence into `drm/` as a shared `bring_up(&StreamUrl)`,
/// which is a change for whoever owns the DRM stack, not for this module.
///
/// One deliberate difference: `app.rs` first fetches an init segment to read
/// the PSSH mp4 box it carries, and falls back to the operator's PSSH after
/// that. Here the operator's PSSH comes first and the init-segment probe is
/// skipped. It costs an HTTP round trip this budget cannot spare, and the
/// comment that justifies the probe over there indicts the *MPD-derived* PSSH
/// (hand-built from `cenc:default_KID`), not the operator-supplied one, which
/// Orange does populate. **If Orange previews fail their license exchange, this
/// is the first thing to look at** — the fix is the init-segment probe, which
/// is another reason to share one implementation.
async fn drm_bring_up(stream: &StreamUrl) -> Result<crate::drm::DrmProxy, CaptureError> {
    use crate::drm::cdm::CdmHandle;
    use crate::drm::{license, proxy};

    let protection = stream
        .protection
        .as_ref()
        .ok_or_else(|| CaptureError::Failed("protected stream without protection data".into()))?;

    let cdm_path = crate::widevine::cdm_path().to_string_lossy().into_owned();
    let mut cdm = CdmHandle::open(&cdm_path).map_err(|e| err("CDM open", &e))?;
    cdm.initialize().map_err(|e| err("CDM init", &e))?;
    let cdm = Arc::new(std::sync::Mutex::new(cdm));

    // Cookie store on, and the same client handed to the proxy: Broadpeak sets
    // a session cookie during the manifest fetch that every segment request
    // afterwards has to carry.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(NETWORK_TIMEOUT_SECS))
        .cookie_store(true)
        .build()
        .map_err(|e| err("CDN client", &e))?;

    let mut request = client.get(stream.url.as_str());
    for (name, value) in &stream.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request.send().await.map_err(|e| err("MPD fetch", &e))?;
    // Read before consuming the body: reqwest follows redirects, and segment
    // paths resolve against wherever the manifest actually came from.
    let final_mpd_url = response.url().to_string();
    let mpd_text = response.text().await.map_err(|e| err("MPD read", &e))?;

    let pssh = protection
        .pssh
        .clone()
        .or_else(|| license::extract_pssh_from_mpd(&mpd_text))
        .ok_or_else(|| CaptureError::Failed("no PSSH in protection data or MPD".into()))?;

    license::acquire_license(&cdm, &pssh, &protection.la_url, &protection.license_headers)
        .await
        .map_err(|e| err("license", &e))?;

    proxy::start(cdm, mpd_text, final_mpd_url, stream.headers.clone(), client)
        .await
        .map_err(|e| err("proxy start", &e))
}

fn err(stage: &str, e: &impl std::fmt::Display) -> CaptureError {
    CaptureError::Failed(format!("{stage}: {e}"))
}

// ── mpv invocation ────────────────────────────────────────────────────────────

/// Look for an `mpv` executable once, at construction.
///
/// The desktop build links *libmpv*, which on Linux ships in `libmpv-dev` and
/// carries no executable. Capture needs the binary, so this can legitimately
/// come up empty on a machine where playback works perfectly.
fn locate_mpv() -> Option<PathBuf> {
    let bin = PathBuf::from("mpv");
    std::process::Command::new(&bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .filter(std::process::ExitStatus::success)
        .map(|_| bin)
}

/// The argument list for one capture.
///
/// Pure, so the shape of the command is a test rather than a thing discovered
/// on a user's machine.
fn mpv_args(url: &str, frames_dir: &Path, options_file: &Path) -> Vec<String> {
    vec![
        // The user's mpv.conf is not ours to inherit — `LibMpvPlayer` already
        // has to fight a `loop=yes` in it — but the options file written beside
        // the frame is, and `--include` survives `--no-config`.
        "--no-config".into(),
        format!("--include={}", options_file.display()),
        "--no-terminal".into(),
        // Load-bearing: the focused player holds the ALSA device. A capture
        // that opens an audio output can take it away mid-programme.
        "--no-audio".into(),
        "--no-sub".into(),
        // Same reason as the player: hardware decoders refuse to initialise
        // while DASH probing reports a pixel format of "none".
        "--hwdec=no".into(),
        // One decode thread. A thumbnail has no deadline; the stream being
        // watched does.
        "--vd-lavc-threads=1".into(),
        // The lowest representation mpv is willing to pick. Honoured for HLS
        // variants; DASH may ignore it, but for protected channels the proxy
        // has already dropped everything above 2.2 Mbps from the manifest, so
        // the choice is narrow either way.
        "--hls-bitrate=min".into(),
        format!("--network-timeout={NETWORK_TIMEOUT_SECS}"),
        // One frame is wanted, so read-ahead is bandwidth and CPU spent on
        // video that is thrown away.
        "--demuxer-readahead-secs=0".into(),
        "--vo=image".into(),
        "--vo-image-format=png".into(),
        format!("--vo-image-outdir={}", frames_dir.display()),
        "--frames=1".into(),
        // Scale inside mpv, letterboxed rather than stretched so a 4:3 channel
        // keeps its shape, and `setsar=1` so the PNG carries no aspect
        // correction for the cache to have to think about.
        format!(
            "--vf=lavfi=[scale={PREVIEW_WIDTH}:{PREVIEW_HEIGHT}:force_original_aspect_ratio=decrease,\
             pad={PREVIEW_WIDTH}:{PREVIEW_HEIGHT}:(ow-iw)/2:(oh-ih)/2,setsar=1]"
        ),
        // The URL can begin with a dash in principle; nothing after this is an
        // option.
        "--".into(),
        url.into(),
    ]
}

/// The contents of the per-capture options file.
///
/// Headers go through a file rather than the command line because an
/// `Authorization: Bearer …` in `argv` is readable out of `/proc` by anything
/// running as this user. The stream URL still goes on the command line — mpv
/// takes the file to play as an argument and nowhere else — so for an
/// unprotected channel a signed CDN URL remains visible there. Protected
/// channels, which is most of them, only ever get `http://127.0.0.1:PORT/…`.
///
/// `referer` and `user-agent` are mapped to their own options, matching
/// `LibMpvPlayer::play`, because mpv sets both itself and appending duplicates
/// to the header list would send each twice.
fn header_config(headers: &[(String, String)], auth: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(auth) = auth {
        out.push_str(&format!(
            "http-header-fields-append=Authorization: {auth}\n"
        ));
    }
    for (name, value) in headers {
        match name.to_lowercase().as_str() {
            "referer" | "referrer" => out.push_str(&format!("referrer={value}\n")),
            "user-agent" => out.push_str(&format!("user-agent={value}\n")),
            _ => out.push_str(&format!("http-header-fields-append={name}: {value}\n")),
        }
    }
    out
}

/// A private directory for one capture: the options file in, the PNG out.
///
/// Removed on drop, including when the drop is a cancel or a timeout tearing
/// the whole future down.
struct WorkDir {
    path: PathBuf,
    options: PathBuf,
}

impl WorkDir {
    fn new() -> std::io::Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "frenchetv-preview-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The options file can hold a session token, and /tmp is shared.
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        let options = path.join("options.conf");
        Ok(Self { path, options })
    }

    fn options_file(&self) -> &Path {
        &self.options
    }

    fn frames_dir(&self) -> &Path {
        &self.path
    }

    fn write_options(&self, contents: &str) -> std::io::Result<()> {
        std::fs::write(&self.options, contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.options, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// The frame mpv wrote, whatever it decided to call it.
    ///
    /// `vo=image` numbers its output (`00000001.png`); reading the directory
    /// rather than that name keeps this working if the numbering ever starts
    /// somewhere else.
    fn read_frame(&self) -> Option<Vec<u8>> {
        let entries = std::fs::read_dir(&self.path).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "png") {
                return std::fs::read(&path).ok();
            }
        }
        None
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// PNG bytes to the exact frame the cache expects.
///
/// mpv has already scaled, so the resize below is a guard rather than the
/// downscale: it only fires if a filter chain was rejected and mpv wrote a
/// full-resolution frame instead. `ColorImage::from_rgba_unmultiplied` panics
/// on a size mismatch, and a panic here would take the worker thread with it.
fn decode_preview(png: &[u8]) -> anyhow::Result<ColorImage> {
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)?;
    let decoded = if decoded.width() as usize != PREVIEW_WIDTH
        || decoded.height() as usize != PREVIEW_HEIGHT
    {
        tracing::debug!(
            "preview: mpv returned {}x{}, resizing",
            decoded.width(),
            decoded.height()
        );
        decoded.resize_exact(
            PREVIEW_WIDTH as u32,
            PREVIEW_HEIGHT as u32,
            image::imageops::FilterType::Triangle,
        )
    } else {
        decoded
    };
    let rgba = decoded.to_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied(
        [PREVIEW_WIDTH, PREVIEW_HEIGHT],
        rgba.as_raw(),
    ))
}

// ── Tests ─────────────────────────────────────────────────────────────────────
//
// Everything here runs without a decoder, a network or a stream: the inbox and
// its cancellation ordering, the budget, the command line, and the PNG decode.
// What is left untested is the part that needs a live channel, and no amount of
// mocking would make that test mean anything.

#[cfg(test)]
mod tests {
    use super::*;
    use frenchetv_core::channel::{ChannelCategory, StreamTemplate};

    fn channel(id: &str) -> Channel {
        Channel {
            id: id.into(),
            name: id.into(),
            logo_url: None,
            number: None,
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Direct(
                format!("http://example.test/{id}.mpd")
                    .parse()
                    .expect("test url"),
            ),
            locked: false,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    // ── inbox ────────────────────────────────────────────────────────────────

    #[test]
    fn newest_request_replaces_the_one_waiting() {
        let engine = Engine::new();
        engine.post(channel("tf1"));
        engine.post(channel("m6"));

        let (taken, _) = engine.take().expect("a request");
        assert_eq!(taken.id, "m6", "a stale target must not be captured first");
        engine.shutdown();
        assert!(engine.take().is_none(), "nothing else was queued");
    }

    #[test]
    fn cancel_discards_the_queued_request() {
        let engine = Engine::new();
        engine.post(channel("tf1"));
        engine.cancel();
        engine.shutdown();

        assert!(engine.take().is_none());
    }

    #[test]
    fn cancel_after_pickup_reaches_the_running_capture() {
        let engine = Engine::new();
        engine.post(channel("tf1"));
        let (_, generation) = engine.take().expect("a request");

        engine.cancel();

        assert!(
            generation.has_changed().expect("sender is alive"),
            "a cancel landing between pick-up and the first await must not be lost"
        );
    }

    #[test]
    fn cancel_before_a_request_does_not_abandon_it() {
        let engine = Engine::new();
        engine.cancel();
        engine.post(channel("tf1"));

        let (_, generation) = engine.take().expect("a request");

        assert!(
            !generation.has_changed().expect("sender is alive"),
            "the new request was posted after the cancel and is still wanted"
        );
    }

    #[test]
    fn shutdown_releases_a_waiting_worker() {
        let engine = Arc::new(Engine::new());
        let waiter = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || engine.take().is_none())
        };
        // The worker may not be blocked yet; shutdown must be observed either
        // way, which is the point of checking the flag inside the loop.
        engine.shutdown();
        assert!(waiter.join().expect("waiter thread"));
    }

    // ── budget and cancellation ──────────────────────────────────────────────

    #[test]
    fn work_that_finishes_inside_the_budget_is_delivered() {
        let engine = Engine::new();
        engine.post(channel("tf1"));
        let (_, mut generation) = engine.take().unwrap();

        let out = runtime().block_on(under_budget(
            async { Ok::<_, CaptureError>(7u8) },
            &mut generation,
            Duration::from_secs(5),
        ));

        assert!(matches!(out, Ok(7)));
    }

    #[test]
    fn work_that_overruns_the_budget_is_abandoned() {
        let engine = Engine::new();
        engine.post(channel("tf1"));
        let (_, mut generation) = engine.take().unwrap();

        let out = runtime().block_on(under_budget(
            std::future::pending::<Result<u8, CaptureError>>(),
            &mut generation,
            Duration::from_millis(20),
        ));

        assert!(matches!(out, Err(CaptureError::TimedOut)));
    }

    #[test]
    fn a_cancelled_capture_is_abandoned_even_when_it_has_a_frame_ready() {
        let engine = Engine::new();
        engine.post(channel("tf1"));
        let (_, mut generation) = engine.take().unwrap();
        engine.cancel();

        let out = runtime().block_on(under_budget(
            async { Ok::<_, CaptureError>(7u8) },
            &mut generation,
            Duration::from_secs(5),
        ));

        assert!(
            matches!(out, Err(CaptureError::Cancelled)),
            "a frame the guide already disowned must not be delivered"
        );
    }

    #[test]
    fn a_cancel_mid_capture_stops_it() {
        let engine = Arc::new(Engine::new());
        engine.post(channel("tf1"));
        let (_, mut generation) = engine.take().unwrap();

        let canceller = {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                engine.cancel();
            })
        };

        // Budget far beyond the test's patience: only the cancel can end this.
        let out = runtime().block_on(under_budget(
            std::future::pending::<Result<u8, CaptureError>>(),
            &mut generation,
            Duration::from_secs(60),
        ));

        canceller.join().expect("canceller thread");
        assert!(matches!(out, Err(CaptureError::Cancelled)));
    }

    // ── command line ─────────────────────────────────────────────────────────

    #[test]
    fn the_command_line_asks_for_one_silent_downscaled_frame() {
        let args = mpv_args(
            "http://127.0.0.1:9/manifest.mpd",
            Path::new("/tmp/cap"),
            Path::new("/tmp/cap/options.conf"),
        );

        assert!(args.contains(&"--frames=1".to_string()));
        assert!(args.contains(&"--no-audio".to_string()));
        assert!(args.contains(&"--no-config".to_string()));
        assert!(args.contains(&"--vo=image".to_string()));
        assert!(args.contains(&"--vo-image-outdir=/tmp/cap".to_string()));
        assert!(args.contains(&"--include=/tmp/cap/options.conf".to_string()));
        // Pinned exactly, not matched loosely: this string was verified against
        // mpv 0.41 by hand, and a stray space from a wrapped `format!` would
        // make mpv reject the whole filter chain and write a full-resolution
        // frame instead — which still "works", just slowly and wrongly.
        let expected_vf = concat!(
            "--vf=lavfi=[scale=256:144:force_original_aspect_ratio=decrease,",
            "pad=256:144:(ow-iw)/2:(oh-ih)/2,setsar=1]"
        );
        assert!(
            args.contains(&expected_vf.to_string()),
            "the frame must be downscaled by mpv, not after readback: {args:?}"
        );
    }

    #[test]
    fn the_url_is_the_last_argument_and_cannot_be_read_as_an_option() {
        let args = mpv_args(
            "-weird-url",
            Path::new("/tmp/cap"),
            Path::new("/tmp/o.conf"),
        );
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args[args.len() - 1], "-weird-url");
    }

    #[test]
    fn credentials_go_in_the_options_file_not_the_command_line() {
        let headers = vec![
            ("Referer".to_string(), "https://tv.example".to_string()),
            ("User-Agent".to_string(), "frenchetv".to_string()),
            ("X-Session".to_string(), "abc123".to_string()),
        ];
        let config = header_config(&headers, Some("Bearer secret"));

        assert!(config.contains("http-header-fields-append=Authorization: Bearer secret\n"));
        assert!(config.contains("referrer=https://tv.example\n"));
        assert!(config.contains("user-agent=frenchetv\n"));
        assert!(config.contains("http-header-fields-append=X-Session: abc123\n"));
        // Referer and User-Agent have dedicated options; appending them to the
        // header list as well would send each header twice.
        assert!(!config.contains("http-header-fields-append=Referer"));
        assert!(!config.contains("http-header-fields-append=User-Agent"));
    }

    #[test]
    fn a_proxied_stream_needs_no_options_at_all() {
        assert_eq!(header_config(&[], None), "");
    }

    // ── frame decode ─────────────────────────────────────────────────────────

    fn png_of(width: u32, height: u32) -> Vec<u8> {
        let image =
            image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, _| {
                image::Rgb([(x % 256) as u8, 0, 0])
            }));
        let mut out = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode");
        out.into_inner()
    }

    #[test]
    fn a_correctly_sized_frame_decodes_unchanged() {
        let frame =
            decode_preview(&png_of(PREVIEW_WIDTH as u32, PREVIEW_HEIGHT as u32)).expect("decode");
        assert_eq!(frame.size, [PREVIEW_WIDTH, PREVIEW_HEIGHT]);
    }

    #[test]
    fn an_unscaled_frame_is_brought_back_to_preview_size() {
        // The guard path: mpv rejected the filter chain and wrote full
        // resolution. The cache's texture upload would panic on any other size.
        let frame = decode_preview(&png_of(1280, 720)).expect("decode");
        assert_eq!(frame.size, [PREVIEW_WIDTH, PREVIEW_HEIGHT]);
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_panic() {
        assert!(decode_preview(b"not a png at all").is_err());
    }

    // ── work directory ───────────────────────────────────────────────────────

    #[test]
    fn the_work_directory_is_removed_even_when_a_frame_was_written() {
        let path = {
            let workdir = WorkDir::new().expect("workdir");
            workdir.write_options("user-agent=test\n").expect("options");
            std::fs::write(workdir.frames_dir().join("00000001.png"), b"x").expect("frame");
            assert!(workdir.read_frame().is_some());
            workdir.path.clone()
        };
        assert!(
            !path.exists(),
            "a cancelled capture must not leave files behind"
        );
    }

    #[test]
    fn no_frame_means_no_frame() {
        let workdir = WorkDir::new().expect("workdir");
        workdir.write_options("").expect("options");
        assert!(workdir.read_frame().is_none());
    }
}
