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
    pub fn play(&mut self, url: &str, auth_header: Option<&str>) {
        let _ = self.stop();
        let mut cmd = std::process::Command::new("mpv");
        cmd.arg("--no-terminal").arg("--force-window=yes");
        if let Some(header) = auth_header {
            cmd.arg(format!("--http-header-fields=Authorization: {}", header));
        }
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
