/// Lightweight wrapper that spawns mpv as a subprocess.
///
/// Embedded rendering (libmpv2 crate) is deferred to v0.2 — it requires
/// `libmpv-dev` to be installed at compile time and wgpu texture integration.
/// For the v0.1 scaffold, we spawn `mpv --force-window` in its own window.
pub struct MpvPlayer {
    handle: Option<std::process::Child>,
    /// true when the installed mpv was compiled with --enable-cdm (e.g. mpv-widevine AUR).
    has_cdm_support: bool,
}

impl MpvPlayer {
    pub fn new() -> Self {
        // Probe once whether this mpv binary was compiled with --enable-cdm.
        // mpv-widevine (AUR) adds the --cdm-store option; stock Arch mpv does not.
        let has_cdm_support = std::process::Command::new("mpv")
            .arg("--list-options")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("cdm-store"))
            .unwrap_or(false);
        if has_cdm_support {
            tracing::info!("mpv: CDM support detected (--cdm-store available)");
        } else {
            tracing::debug!("mpv: no CDM support (standard build); DRM streams require mpv-widevine");
        }
        Self { handle: None, has_cdm_support }
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
        // (e.g. the mpv-widevine AUR package).  Skip on standard Arch mpv.
        if self.has_cdm_support {
            cmd.arg(format!("--cdm-store={}", crate::widevine::dir().display()));
        }

        // Live DASH: lavf's DASH demuxer doesn't invoke the codec decoder during
        // probing, so pixel format stays "none" until the first IDR frame.
        // Hardware decoders (vaapi/vdpau) refuse to init with pixel_format=none;
        // force software decode so mpv can open the stream and resolve pix_fmt
        // from the first decoded IDR.
        cmd.arg("--hwdec=no");
        cmd.arg("--demuxer-lavf-analyzeduration=1");

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
