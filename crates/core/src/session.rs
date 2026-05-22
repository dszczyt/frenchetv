/// Persist and restore operator session tokens via the OS keyring.
///
/// Key scheme: service = "frenchetv", account = "<operator>:<username>"
/// Only the session token (e.g. `wassup`) is stored here — never the password.

const KEYRING_SERVICE: &str = "frenchetv";

fn account_key(operator: &str, username: &str) -> String {
    format!("{}:{}", operator, username)
}

/// Store `token` for the given operator + username in the OS keyring.
/// Silently ignores errors (keyring may be unavailable in headless envs).
pub fn save_session(operator: &str, username: &str, token: &str) {
    let account = account_key(operator, username);
    match keyring::Entry::new(KEYRING_SERVICE, &account) {
        Ok(entry) => {
            if let Err(e) = entry.set_password(token) {
                tracing::warn!("keyring: failed to save session for {}: {}", account, e);
            } else {
                tracing::debug!("keyring: session saved for {}", account);
            }
        }
        Err(e) => tracing::warn!("keyring: entry creation failed: {}", e),
    }
}

/// Load a previously saved session token from the OS keyring.
/// Returns `None` if no token is stored or keyring is unavailable.
pub fn load_session(operator: &str, username: &str) -> Option<String> {
    let account = account_key(operator, username);
    match keyring::Entry::new(KEYRING_SERVICE, &account) {
        Ok(entry) => match entry.get_password() {
            Ok(token) if !token.is_empty() => {
                tracing::debug!("keyring: session loaded for {}", account);
                Some(token)
            }
            Ok(_) => None,
            Err(keyring::Error::NoEntry) => None,
            Err(e) => {
                tracing::warn!("keyring: failed to load session for {}: {}", account, e);
                None
            }
        },
        Err(e) => {
            tracing::warn!("keyring: entry creation failed: {}", e);
            None
        }
    }
}

/// Delete the stored session (e.g. on logout or auth failure).
pub fn clear_session(operator: &str, username: &str) {
    let account = account_key(operator, username);
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &account) {
        let _ = entry.delete_credential();
        tracing::debug!("keyring: session cleared for {}", account);
    }
}
