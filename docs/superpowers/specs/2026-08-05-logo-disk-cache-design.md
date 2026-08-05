# Channel Logo Disk Cache Design

**Date:** 2026-08-05
**Status:** Approved

## Goal

Channel logos are currently re-downloaded from the network on every app launch (an in-memory-only `HashMap<url, TextureHandle>` populated by `start_fetch_logos()` in both `ui-desktop` and `ui-android`). Persist decoded logo bytes to disk so a restart within the cache TTL skips the network fetch entirely. TTL is configurable via the existing (currently unused) `Config.cache.logo_ttl_hours` field, default 24 (1 day).

## Architecture

### Placement: shared function in `frenchetv-core`

Per `CLAUDE.md`, `crates/core` holds business logic, UI crates hold rendering. Disk caching (fetch bytes, check mtime, write to disk) is not UI-specific — only decoding bytes into an `egui::TextureHandle` is. A single `frenchetv_core::logo_cache` module avoids duplicating cache logic across `ui-desktop` and `ui-android`, which today independently implement identical `start_fetch_logos()` bodies.

### `crates/core/src/logo_cache.rs`

```rust
pub async fn fetch_logo(
    client: &reqwest::Client,
    url: &str,
    ttl_hours: u32,
) -> Result<bytes::Bytes, LogoCacheError>;
```

Behavior:

1. Compute cache file path: `logo_cache_dir()?.join(sha1_hex(url))` — no extension; the file is opaque, read back only through this module.
2. `logo_cache_dir()` = `dirs::cache_dir().ok_or(LogoCacheError::NoDirFound)?.join("frenchetv").join("logos")`.
3. If the file exists and `now - mtime < ttl_hours` → read and return its bytes. No network call.
4. Otherwise, GET the URL via the passed-in `client`.
   - On success: write bytes to a temp file in the same directory, then rename over the target (atomic replace, avoids partial-file corruption on crash/interrupt). Return the bytes.
   - On failure: if a stale file still exists on disk (expired but present), return its bytes instead of erroring — logo stays visible even when the network blips, consistent with the project's existing degrade-gracefully patterns (silent EPG degradation, mandatory M3U fallback). Only return `LogoCacheError::Network` when there is no cached copy at all.
5. `ttl_hours = 0` is treated as "always stale" (always attempt redownload before falling back), not "cache forever."

No LRU / size eviction in this pass — `CLAUDE.md` calls for a 50 MB LRU cap eventually, but that's a separate follow-up; this cache only bounds itself by TTL.

### `LogoCacheError`

New enum in `crates/core/src/error.rs`, following the existing `EpgError`/`ConfigError` shape:

```rust
#[derive(Debug, Error)]
pub enum LogoCacheError {
    #[error("cache directory not found")]
    NoDirFound,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
}
```

Exported from `crates/core/src/lib.rs` alongside the other error types; `logo_cache` module made `pub mod logo_cache;`.

### New dependency

`sha1 = { version = "0.10", default-features = false }` added to `crates/core/Cargo.toml` — same crate/version already used in `ui-desktop/src/widevine.rs`, just relocated to where it's now needed for cache-key hashing.

### UI integration (`ui-desktop` and `ui-android`, both `app.rs`)

`start_fetch_logos()` currently does, per channel URL:

```rust
let bytes = client.get(&url).send().await.ok()?.bytes().await.ok()?;
```

This becomes:

```rust
let bytes = frenchetv_core::logo_cache::fetch_logo(&client, &url, logo_ttl_hours).await.ok()?;
```

Everything downstream (image decode, `egui::ColorImage`, `ctx.load_texture`, insert into `LogoCache`) is unchanged.

`logo_ttl_hours` comes from `Config.cache.logo_ttl_hours` (already defined, default 24, already user-editable in `config.toml` — no config schema change). `App` does not currently hold a `Config` past `App::new()`; `Config::load()` is already called once in `App::new()` — that value (or just the one field) needs to be threaded into `start_fetch_logos()`, e.g. stored as an `App` field (`logo_ttl_hours: u32`) set at construction time alongside `logos`.

## Testing

Unit tests in `crates/core/src/logo_cache.rs` using `wiremock` (already a dev-dependency), each using a temp dir for the cache root:

1. No cached file → fetch hits network, returns bytes, writes file to disk.
2. Fresh cached file (mtime within TTL) → returns disk bytes, no network call made (assert via wiremock's expected-request-count).
3. Stale cached file (mtime past TTL), network reachable → redownloads, overwrites file, returns new bytes.
4. Stale cached file, network fails (mock returns 500 / connection error) → returns the stale disk bytes instead of erroring.
5. No cached file, network fails → returns `LogoCacheError::Network`.

## Out of scope

- 50 MB LRU eviction cap (tracked as a follow-up against the existing `CLAUDE.md` rule).
- Cache invalidation UI / manual "clear logo cache" action.
- Changing the config file format — `logo_ttl_hours` already exists and is reused as-is.
