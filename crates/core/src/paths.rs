//! Where this app keeps config and cache files.
//!
//! Everything used to call `dirs::config_dir()` / `dirs::cache_dir()` directly.
//! That works on desktop and returns `None` on Android — `dirs-sys` has no
//! home directory to fall back to there, and the device sets no
//! `XDG_CONFIG_HOME`. `logo_cache` already documented that for the cache half.
//!
//! The consequence was quieter than it looks: on Android `Config::save` failed
//! with `NoDirFound` and `session::save_session` took its "cannot determine
//! path" branch and did nothing, so **no session or config has ever persisted
//! on the Fire TV**. It silently logged a warning and carried on.
//!
//! Platforms that know their own storage call [`set_app_dir`] once at startup
//! — Android passes `AndroidApp::internal_data_path()` — and every path below
//! is taken relative to it. Desktop sets nothing and keeps the `dirs`
//! behaviour, so its file locations do not move.

use std::path::PathBuf;
use std::sync::RwLock;

static APP_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Override the base directory for config and cache.
///
/// Call once, before anything reads config. Idempotent and safe to call from
/// any thread; a poisoned lock is ignored rather than panicking, since losing
/// a preference is recoverable and crashing at startup is not.
pub fn set_app_dir(dir: PathBuf) {
    if let Ok(mut guard) = APP_DIR.write() {
        *guard = Some(dir);
    }
}

fn app_dir() -> Option<PathBuf> {
    APP_DIR.read().ok().and_then(|g| g.clone())
}

/// Directory for durable files: config, saved sessions.
pub fn config_dir() -> Option<PathBuf> {
    match app_dir() {
        Some(base) => Some(base.join("config")),
        None => Some(dirs::config_dir()?.join("frenchetv")),
    }
}

/// Directory for regenerable files: logos, EPG.
///
/// Separate from config so clearing the cache can never take a session with
/// it.
pub fn cache_dir() -> Option<PathBuf> {
    match app_dir() {
        Some(base) => Some(base.join("cache")),
        None => Some(dirs::cache_dir()?.join("frenchetv")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, not several: `APP_DIR` is process-global, so separate tests
    /// would race each other through it.
    #[test]
    fn override_redirects_both_dirs_and_keeps_them_separate() {
        // Default (no override) resolves through `dirs` on this host.
        assert!(config_dir().is_some());

        set_app_dir(PathBuf::from("/data/data/com.frenchetv/files"));
        let cfg = config_dir().unwrap();
        let cache = cache_dir().unwrap();

        assert!(cfg.starts_with("/data/data/com.frenchetv/files"));
        assert!(cache.starts_with("/data/data/com.frenchetv/files"));
        assert_ne!(cfg, cache, "clearing the cache must not remove sessions");

        // Leave the global clean for any test that runs after this one.
        if let Ok(mut g) = APP_DIR.write() {
            *g = None;
        }
    }
}
