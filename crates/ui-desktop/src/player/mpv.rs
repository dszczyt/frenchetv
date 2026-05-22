/// Lightweight wrapper that spawns mpv as a subprocess.
///
/// Embedded rendering (libmpv2 crate) is deferred to v0.2 — it requires
/// `libmpv-dev` to be installed at compile time and wgpu texture integration.
/// For the v0.1 scaffold, we spawn `mpv --force-window` in its own window.
pub struct MpvPlayer {
    handle: Option<std::process::Child>,
}

impl MpvPlayer {
    pub fn new() -> Self {
        Self { handle: None }
    }

    /// Start playing a stream URL (opens mpv in its own window).
    ///
    /// * `auth_header` — `Authorization: <value>` sent by mpv's own HTTP layer.
    /// * `extra_headers` — additional headers forwarded to FFmpeg's lavf demuxer
    ///   (needed for DASH segment requests: Origin, Referer, User-Agent …).
    pub fn play(
        &mut self,
        url: &str,
        auth_header: Option<&str>,
        extra_headers: &[(String, String)],
    ) {
        let _ = self.stop();
        let mut cmd = std::process::Command::new("mpv");
        cmd.arg("--force-window=yes");

        // Authorization header for mpv's own HTTP requests.
        if let Some(header) = auth_header {
            cmd.arg(format!("--http-header-fields=Authorization: {}", header));
        }

        // Extra headers.  mpv's --stream-lavf-o key-value parser splits on
        // spaces, so we can't use it for headers with URL values.  Instead:
        //   Referer   → --referrer=<url>
        //   User-Agent→ --user-agent=<ua>
        //   anything else → --http-header-fields=Name: value
        // --http-header-fields is a string-list (not key-value list), so
        // spaces in the value are fine.
        for (name, value) in extra_headers {
            match name.to_lowercase().as_str() {
                "referer" | "referrer" => {
                    cmd.arg(format!("--referrer={}", value));
                }
                "user-agent" => {
                    cmd.arg(format!("--user-agent={}", value));
                }
                _ => {
                    cmd.arg(format!("--http-header-fields={}: {}", name, value));
                }
            }
        }

        // --cdm-store only works with mpv compiled with --enable-cdm
        // (e.g. the mpv-widevine AUR package).  The standard Arch mpv does
        // not support it, so we skip the flag.  The CDM is already on disk
        // at widevine::cdm_path() for when a CDM-capable mpv is in use.

        cmd.arg(url);
        match cmd.spawn() {
            Ok(child) => self.handle = Some(child),
            Err(e) => tracing::error!("failed to spawn mpv: {}", e),
        }
    }

    /// Stop the current playback (kills the mpv process).
    pub fn stop(&mut self) -> std::io::Result<()> {
        if let Some(mut child) = self.handle.take() {
            child.kill()?;
            child.wait()?;
        }
        Ok(())
    }

    /// Returns true if mpv is still running.
    /// Used in v0.2 for player state polling.
    #[allow(dead_code)]
    pub fn is_playing(&mut self) -> bool {
        self.handle.as_mut().map_or(false, |c| {
            c.try_wait().map_or(false, |status| status.is_none())
        })
    }
}

impl Default for MpvPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
