/// Persist and restore operator session tokens in a JSON file alongside config.
///
/// File: `~/.config/frenchetv/sessions.json`  (Linux/macOS)
///       `%APPDATA%\frenchetv\sessions.json`   (Windows)
///
/// Format: `{"orange:user@example.com": "<token>", ...}`
///
/// Session tokens (e.g. `wassup` cookie) are not passwords — treating them like
/// browser cookies and storing on disk is appropriate.

use std::collections::HashMap;
use std::path::PathBuf;

fn sessions_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("frenchetv").join("sessions.json"))
}

fn account_key(operator: &str, username: &str) -> String {
    format!("{}:{}", operator, username)
}

fn load_map() -> HashMap<String, String> {
    let path = match sessions_path() {
        Some(p) => p,
        None => return HashMap::new(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_map(map: &HashMap<String, String>) {
    let path = match sessions_path() {
        Some(p) => p,
        None => {
            tracing::warn!("session: cannot determine sessions file path");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("session: failed to create dir {}: {}", parent.display(), e);
            return;
        }
    }
    match serde_json::to_string_pretty(map) {
        Ok(content) => {
            if let Err(e) = std::fs::write(&path, content) {
                tracing::warn!("session: failed to write {}: {}", path.display(), e);
            } else {
                tracing::debug!("session: saved to {}", path.display());
            }
        }
        Err(e) => tracing::warn!("session: serialize error: {}", e),
    }
}

/// Store `token` for the given operator + username.
pub fn save_session(operator: &str, username: &str, token: &str) {
    let mut map = load_map();
    map.insert(account_key(operator, username), token.to_string());
    save_map(&map);
    tracing::debug!("session: saved for {}:{}", operator, username);
}

/// Load a previously saved session token. Returns `None` if not stored.
pub fn load_session(operator: &str, username: &str) -> Option<String> {
    let map = load_map();
    let token = map.get(&account_key(operator, username)).cloned();
    if token.is_some() {
        tracing::debug!("session: loaded for {}:{}", operator, username);
    }
    token
}

/// Delete the stored session (e.g. on logout or auth failure).
pub fn clear_session(operator: &str, username: &str) {
    let mut map = load_map();
    if map.remove(&account_key(operator, username)).is_some() {
        save_map(&map);
        tracing::debug!("session: cleared for {}:{}", operator, username);
    }
}
