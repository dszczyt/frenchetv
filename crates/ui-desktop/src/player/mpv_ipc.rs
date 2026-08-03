//! Diagnostic-only mpv player: separate OS process, controlled over its
//! `--input-ipc-server` JSON socket — NOT embedded into the app window.
//!
//! Exists to test one specific hypothesis in the audio-loop investigation
//! (see `LibMpvPlayer`'s doc comment): that the bug is specific to the
//! in-process render-API embedding (`mpv_render_context_render()` sharing a
//! process with the DRM proxy's tokio runtime and CENC decrypt work), not to
//! mpv or the proxy themselves. Standalone `mpv`/`ffmpeg` processes pointed
//! at this app's own live proxy never duplicated a single segment request in
//! direct testing; this type is that same test wired into the app's actual
//! channel-switching path instead of a one-off CLI invocation.
//!
//! mpv draws into its own top-level window here — there is no embedding yet.
//! `render_frame` always returns `None`, so `PlayerScreen` shows its loading
//! spinner for the session; that's expected and fine for this test. Toggle
//! with `FRENCHETV_MPV_SUBPROCESS=1`. If this confirms the fix, embedding
//! (via `--wid` under forced XWayland) replaces the spinner-only render path;
//! if it doesn't, this file and the render-API hypothesis both get dropped.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

pub struct MpvIpcPlayer {
    child: Child,
    socket_path: PathBuf,
    ipc: Option<UnixStream>,
}

impl MpvIpcPlayer {
    pub fn new() -> Self {
        let has_cdm_support = Command::new("mpv")
            .arg("--list-options")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("cdm-store"))
            .unwrap_or(false);

        // XDG_RUNTIME_DIR is per-user and 0700 by default; the socket only
        // ever carries playback-control commands (loadfile URL, headers), but
        // there's no reason to put it somewhere world-readable when a private
        // directory is available.
        let socket_path = dirs::runtime_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("frenchetv-mpv-{}.sock", std::process::id()));
        // mpv binds this itself; clear out anything left behind by a crashed run.
        let _ = std::fs::remove_file(&socket_path);

        let mut cmd = Command::new("mpv");
        cmd.args([
            "--idle=yes",
            "--no-config",
            "--force-window=yes",
            "--title=frenchetv (mpv subprocess diagnostic)",
            "--ao=alsa,pulse,pipewire",
            "--hwdec=no",
            "--cache=yes",
            "--demuxer-readahead-secs=3",
            "--cache-pause=no",
            "--audio-buffer=2.5",
            "--audio-stream-silence=yes",
            "--demuxer-lavf-o=multiple_requests=1",
            "--loop-file=no",
            "--loop-playlist=no",
        ])
        .arg(format!("--input-ipc-server={}", socket_path.display()));

        if has_cdm_support {
            let cdm_path = crate::widevine::dir().to_string_lossy().into_owned();
            cmd.arg(format!("--cdm-store={}", cdm_path));
        }

        let child = cmd.spawn().expect("failed to spawn mpv subprocess");

        let ipc = Self::connect_with_retry(&socket_path);
        if let Some(stream) = &ipc {
            Self::spawn_event_reader(stream);
        }

        Self {
            child,
            socket_path,
            ipc,
        }
    }

    /// mpv creates the IPC socket asynchronously after startup, so the first
    /// connect attempts are expected to fail — poll briefly instead of
    /// treating that as an error.
    fn connect_with_retry(path: &PathBuf) -> Option<UnixStream> {
        for _ in 0..50 {
            if let Ok(stream) = UnixStream::connect(path) {
                let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                return Some(stream);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        tracing::error!("mpv IPC: failed to connect to {:?} after 1s", path);
        None
    }

    /// Logs mpv's IPC event stream at WARN for seek/end-file/errors — mirrors
    /// the event handling `LibMpvPlayer::render_frame` does today, so a debug
    /// run of this backend is directly comparable in `debug.log`.
    fn spawn_event_reader(stream: &UnixStream) {
        let reader_stream = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("mpv IPC: failed to clone socket for event reader: {}", e);
                return;
            }
        };
        std::thread::spawn(move || {
            let reader = BufReader::new(reader_stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                match value.get("event").and_then(|e| e.as_str()) {
                    Some("seek") => tracing::warn!("mpv IPC: seek event"),
                    Some("end-file") => tracing::warn!("mpv IPC: end-file event: {}", value),
                    Some(_) => {}
                    None => {
                        if value.get("error").and_then(|e| e.as_str()) != Some("success") {
                            tracing::warn!("mpv IPC: command error: {}", value);
                        }
                    }
                }
            }
        });
    }

    fn send_command(&mut self, args: &[serde_json::Value]) {
        let Some(stream) = &mut self.ipc else {
            return;
        };
        let mut line = serde_json::json!({ "command": args }).to_string();
        line.push('\n');
        if let Err(e) = stream.write_all(line.as_bytes()) {
            tracing::warn!("mpv IPC: write failed: {}", e);
        }
    }

    /// Start playing a stream URL. Same header-handling semantics as
    /// `LibMpvPlayer::play` — kept in lockstep so this stays a fair test.
    pub fn play(
        &mut self,
        url: &str,
        auth_header: Option<&str>,
        extra_headers: &[(String, String)],
    ) {
        self.send_command(&[serde_json::json!("stop")]);
        self.send_command(&[
            serde_json::json!("change-list"),
            serde_json::json!("http-header-fields"),
            serde_json::json!("clr"),
            serde_json::json!(""),
        ]);

        if let Some(auth) = auth_header {
            self.send_command(&[
                serde_json::json!("change-list"),
                serde_json::json!("http-header-fields"),
                serde_json::json!("append"),
                serde_json::json!(format!("Authorization: {}", auth)),
            ]);
        }

        for (name, value) in extra_headers {
            match name.to_lowercase().as_str() {
                "referer" | "referrer" => {
                    self.send_command(&[
                        serde_json::json!("set_property"),
                        serde_json::json!("referrer"),
                        serde_json::json!(value),
                    ]);
                }
                "user-agent" => {
                    self.send_command(&[
                        serde_json::json!("set_property"),
                        serde_json::json!("user-agent"),
                        serde_json::json!(value),
                    ]);
                }
                _ => {
                    self.send_command(&[
                        serde_json::json!("change-list"),
                        serde_json::json!("http-header-fields"),
                        serde_json::json!("append"),
                        serde_json::json!(format!("{}: {}", name, value)),
                    ]);
                }
            }
        }

        self.send_command(&[
            serde_json::json!("loadfile"),
            serde_json::json!(url),
            serde_json::json!("replace"),
        ]);
    }

    pub fn stop(&mut self) {
        self.send_command(&[serde_json::json!("stop")]);
    }

    /// No embedding yet — mpv owns its own window. Always `None`.
    pub fn render_frame(
        &mut self,
        _ctx: &egui::Context,
        _width: u32,
        _height: u32,
    ) -> Option<egui::load::SizedTexture> {
        None
    }
}

impl Drop for MpvIpcPlayer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
