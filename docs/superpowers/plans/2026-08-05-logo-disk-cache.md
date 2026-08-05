# Channel Logo Disk Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist decoded channel logo bytes to disk so a restart within the configured TTL skips the network fetch, using the existing (currently unused) `Config.cache.logo_ttl_hours` field (default 24h).

**Architecture:** A single `frenchetv_core::logo_cache::fetch_logo()` function does disk-cache-then-network-fetch-then-write, shared by both `ui-desktop` and `ui-android`. UI crates keep decoding bytes into `egui::TextureHandle` themselves — only the byte-fetch call in each `start_fetch_logos()` changes.

**Tech Stack:** Rust, `reqwest` (already a dep), `sha1` (new dep, mirrors existing use in `ui-desktop/src/widevine.rs`), `std::fs` (matches existing sync-fs convention in `crates/core/src/session.rs` and `crates/core/src/config/mod.rs`), `wiremock` for tests (already a dev-dep).

## Global Constraints

- MSRV 1.97 — no language/stdlib features newer than Rust 1.97. (`std::fs::File::set_times`/`FileTimes`, used in tests, stabilized 1.75 — safe.)
- No `unsafe` except FFI boundaries (not touched by this work).
- Run `cargo fmt --all` and `cargo clippy --workspace -- -D warnings` before considering any task done.
- Passwords/credentials are out of scope here — not touched.
- Unit tests use `wiremock` to mock HTTP; no real network in tests.
- `crates/core` holds business logic only, no UI dependencies — `logo_cache.rs` must not depend on `egui` or `image`.

---

### Task 1: `logo_cache` module in `frenchetv-core`

**Files:**
- Modify: `crates/core/Cargo.toml` (add `sha1` runtime dep, `tempfile` dev-dep)
- Modify: `crates/core/src/error.rs` (add `LogoCacheError`)
- Modify: `crates/core/src/lib.rs` (register `logo_cache` module, export `LogoCacheError`)
- Create: `crates/core/src/logo_cache.rs`

**Interfaces:**
- Produces: `pub async fn frenchetv_core::logo_cache::fetch_logo(client: &reqwest::Client, url: &str, ttl_hours: u32) -> Result<bytes::Bytes, LogoCacheError>` — used by Task 2 and Task 3.
- Produces: `pub enum frenchetv_core::LogoCacheError { NoDirFound, Io(std::io::Error), Network(reqwest::Error) }`.

- [ ] **Step 1: Add dependencies**

Edit `crates/core/Cargo.toml`. Add to `[dependencies]` (after the existing `bytes = "1"` line):

```toml
sha1 = { version = "0.10", default-features = false }
```

Add a `[dev-dependencies]` entry (the section already exists with `tokio`, `wiremock`, `serde_json`):

```toml
tempfile = "3"
```

- [ ] **Step 2: Add `LogoCacheError`**

Edit `crates/core/src/error.rs`. Append at the end of the file:

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

- [ ] **Step 3: Register the module**

Edit `crates/core/src/lib.rs`. Add `pub mod logo_cache;` to the `pub mod` block (alphabetical order, between `epg` and `operator`):

```rust
pub mod channel;
pub mod config;
pub mod epg;
pub mod error;
pub mod logo_cache;
pub mod operator;
pub mod session;
pub mod stream;
```

Add `LogoCacheError` to the error re-export line:

```rust
pub use error::{ConfigError, EpgError, LogoCacheError, OperatorError, StreamError};
```

- [ ] **Step 4: Create the module — implementation and tests together**

This is a brand-new module with no prior code to make red/green against; the usual write-test-then-stub-then-implement cycle doesn't apply cleanly to a from-scratch file (a stub returning `todo!()` would violate the no-placeholders rule). Create `crates/core/src/logo_cache.rs` with the full implementation and its tests in one step:

```rust
use crate::error::LogoCacheError;
use bytes::Bytes;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn logo_cache_dir() -> Result<PathBuf, LogoCacheError> {
    Ok(dirs::cache_dir()
        .ok_or(LogoCacheError::NoDirFound)?
        .join("frenchetv")
        .join("logos"))
}

fn cache_key(url: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(url.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_fresh(path: &Path, ttl_hours: u32) -> bool {
    if ttl_hours == 0 {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age < Duration::from_secs(u64::from(ttl_hours) * 3600)
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Bytes, LogoCacheError> {
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?)
}

fn write_cache_file(dir: &Path, path: &Path, bytes: &Bytes) -> Result<(), LogoCacheError> {
    std::fs::create_dir_all(dir)?;
    let tmp_path = path.with_extension("tmp");
    std::fs::write(&tmp_path, bytes)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

async fn fetch_logo_in(
    dir: &Path,
    client: &reqwest::Client,
    url: &str,
    ttl_hours: u32,
) -> Result<Bytes, LogoCacheError> {
    let path = dir.join(cache_key(url));

    if is_fresh(&path, ttl_hours) {
        if let Ok(bytes) = std::fs::read(&path) {
            return Ok(Bytes::from(bytes));
        }
    }

    match download(client, url).await {
        Ok(bytes) => {
            let _ = write_cache_file(dir, &path, &bytes);
            Ok(bytes)
        }
        Err(e) => {
            if let Ok(stale) = std::fs::read(&path) {
                Ok(Bytes::from(stale))
            } else {
                Err(e)
            }
        }
    }
}

/// Fetches logo bytes for `url`, using a TTL-based disk cache under the OS
/// cache directory. Falls back to a stale cached copy if the network fetch
/// fails and no fresh copy is available; errors only when there is no
/// cached copy at all.
pub async fn fetch_logo(
    client: &reqwest::Client,
    url: &str,
    ttl_hours: u32,
) -> Result<Bytes, LogoCacheError> {
    let dir = logo_cache_dir()?;
    fetch_logo_in(&dir, client, url, ttl_hours).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_with_mtime(path: &Path, bytes: &[u8], age: Duration) {
        std::fs::write(path, bytes).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        let old_time = SystemTime::now() - age;
        file.set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
    }

    #[tokio::test]
    async fn no_cached_file_downloads_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"FRESH".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());

        let bytes = fetch_logo_in(dir.path(), &client, &url, 24).await.unwrap();

        assert_eq!(bytes.as_ref(), b"FRESH");
        let cached = std::fs::read(dir.path().join(cache_key(&url))).unwrap();
        assert_eq!(cached, b"FRESH");
        server.verify().await;
    }

    #[tokio::test]
    async fn fresh_cached_file_skips_network() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"SHOULD_NOT_BE_FETCHED".to_vec()))
            .expect(0)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());
        let cache_path = dir.path().join(cache_key(&url));
        write_with_mtime(&cache_path, b"CACHED", Duration::from_secs(60));

        let bytes = fetch_logo_in(dir.path(), &client, &url, 24).await.unwrap();

        assert_eq!(bytes.as_ref(), b"CACHED");
        server.verify().await;
    }

    #[tokio::test]
    async fn stale_cached_file_redownloads_when_network_ok() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"NEW".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());
        let cache_path = dir.path().join(cache_key(&url));
        write_with_mtime(&cache_path, b"OLD", Duration::from_secs(25 * 3600));

        let bytes = fetch_logo_in(dir.path(), &client, &url, 24).await.unwrap();

        assert_eq!(bytes.as_ref(), b"NEW");
        let cached = std::fs::read(&cache_path).unwrap();
        assert_eq!(cached, b"NEW");
        server.verify().await;
    }

    #[tokio::test]
    async fn stale_cached_file_falls_back_when_network_fails() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());
        let cache_path = dir.path().join(cache_key(&url));
        write_with_mtime(&cache_path, b"STALE_BUT_USABLE", Duration::from_secs(25 * 3600));

        let bytes = fetch_logo_in(dir.path(), &client, &url, 24).await.unwrap();

        assert_eq!(bytes.as_ref(), b"STALE_BUT_USABLE");
    }

    #[tokio::test]
    async fn no_cached_file_and_network_fails_errors() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());

        let result = fetch_logo_in(dir.path(), &client, &url, 24).await;

        assert!(matches!(result, Err(LogoCacheError::Network(_))));
    }

    #[tokio::test]
    async fn zero_ttl_always_attempts_network() {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/logo.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"REFETCHED".to_vec()))
            .expect(1)
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let url = format!("{}/logo.png", server.uri());
        let cache_path = dir.path().join(cache_key(&url));
        write_with_mtime(&cache_path, b"JUST_WRITTEN", Duration::from_secs(1));

        let bytes = fetch_logo_in(dir.path(), &client, &url, 0).await.unwrap();

        assert_eq!(bytes.as_ref(), b"REFETCHED");
        server.verify().await;
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p frenchetv-core logo_cache`
Expected: all 6 tests pass (`no_cached_file_downloads_and_writes`, `fresh_cached_file_skips_network`, `stale_cached_file_redownloads_when_network_ok`, `stale_cached_file_falls_back_when_network_fails`, `no_cached_file_and_network_fails_errors`, `zero_ttl_always_attempts_network`).

- [ ] **Step 6: Full core test suite + lint**

Run: `cargo test -p frenchetv-core && cargo fmt --all -- --check && cargo clippy -p frenchetv-core -- -D warnings`
Expected: all pass, no warnings. If `cargo fmt --all -- --check` fails, run `cargo fmt --all` and re-check.

- [ ] **Step 7: Commit**

```bash
git add crates/core/Cargo.toml crates/core/src/error.rs crates/core/src/lib.rs crates/core/src/logo_cache.rs
git commit -m "feat(core): add TTL-based disk cache for channel logos"
```

---

### Task 2: Wire `ui-desktop` to the disk cache

**Files:**
- Modify: `crates/ui-desktop/src/app.rs:78` (add field), `:93-94` (capture ttl), `:118-135` (Self construction), `:157-202` (`start_fetch_logos`)

**Interfaces:**
- Consumes: `frenchetv_core::logo_cache::fetch_logo(client: &reqwest::Client, url: &str, ttl_hours: u32) -> Result<bytes::Bytes, LogoCacheError>` (Task 1).
- Consumes: `Config.cache.logo_ttl_hours: u32` (already exists, `crates/core/src/config/mod.rs:38`).

- [ ] **Step 1: Add `logo_ttl_hours` field to `App`**

In `crates/ui-desktop/src/app.rs`, in the `App` struct (around line 78), change:

```rust
    /// Decoded channel logos, populated asynchronously after channel list loads.
    logos: LogoCache,
```

to:

```rust
    /// Decoded channel logos, populated asynchronously after channel list loads.
    logos: LogoCache,
    /// TTL (hours) for the on-disk logo cache — from `Config.cache.logo_ttl_hours`.
    logo_ttl_hours: u32,
```

- [ ] **Step 2: Capture the TTL in `App::new`**

In the same file, around lines 93-94, change:

```rust
        let logos: LogoCache = Arc::new(Mutex::new(HashMap::new()));
        let config = Config::load().unwrap_or_default();
```

to:

```rust
        let logos: LogoCache = Arc::new(Mutex::new(HashMap::new()));
        let config = Config::load().unwrap_or_default();
        let logo_ttl_hours = config.cache.logo_ttl_hours;
```

- [ ] **Step 3: Set the field in the `Self { ... }` construction**

Around lines 118-135, change:

```rust
            channels: Vec::new(),
            current_operator: None,
            current_session: None,
            logos,
```

to:

```rust
            channels: Vec::new(),
            current_operator: None,
            current_session: None,
            logos,
            logo_ttl_hours,
```

- [ ] **Step 4: Use the cache in `start_fetch_logos`**

Around lines 157-202, inside `fn start_fetch_logos`, change:

```rust
    fn start_fetch_logos(&self, channels: Vec<Channel>) {
        let logos = Arc::clone(&self.logos);
        let ctx = self.egui_ctx.clone();
        self.rt.spawn(async move {
```

to:

```rust
    fn start_fetch_logos(&self, channels: Vec<Channel>) {
        let logos = Arc::clone(&self.logos);
        let ctx = self.egui_ctx.clone();
        let logo_ttl_hours = self.logo_ttl_hours;
        self.rt.spawn(async move {
```

Then, further down in the same function, change:

```rust
                    set.spawn(async move {
                        let _permit = sem.acquire().await.ok()?;
                        let bytes = client.get(&url).send().await.ok()?.bytes().await.ok()?;
                        let img = image::load_from_memory(&bytes).ok()?;
```

to:

```rust
                    set.spawn(async move {
                        let _permit = sem.acquire().await.ok()?;
                        let bytes = frenchetv_core::logo_cache::fetch_logo(&client, &url, logo_ttl_hours)
                            .await
                            .ok()?;
                        let img = image::load_from_memory(&bytes).ok()?;
```

Note `logo_ttl_hours` is `Copy` (`u32`) — it's captured by the outer `async move` block and then implicitly copied into each inner `set.spawn(async move { ... })` closure per iteration, same as `client`/`logos`/`ctx` are `.clone()`d per iteration. No extra clone needed for a `Copy` type.

- [ ] **Step 5: Build and lint**

Run: `cargo build -p ui-desktop && cargo clippy -p ui-desktop -- -D warnings && cargo fmt --all -- --check`
Expected: builds clean, no clippy warnings, formatting already correct (fix with `cargo fmt --all` if not).

- [ ] **Step 6: Manual smoke test**

Run: `cargo run -p ui-desktop`, log in, let the channel list load with logos visible. Then check the cache directory was populated:

```bash
ls -la ~/.cache/frenchetv/logos/ | head
```

Expected: one file per distinct logo URL (filenames are 40-character sha1 hex strings, no extension). Quit the app, delete `debug.log` if noisy, and relaunch — logos should appear without the usual brief blank/placeholder flash, since they load from disk instead of network. (This is a qualitative check — timing differences aren't asserted, just confirm no errors and logos still render.)

- [ ] **Step 7: Commit**

```bash
git add crates/ui-desktop/src/app.rs
git commit -m "feat(ui-desktop): use disk-cached logo fetch"
```

---

### Task 3: Wire `ui-android` to the disk cache

**Files:**
- Modify: `crates/ui-android/src/app.rs:109` (add field), `:122-123` (capture ttl), `:125-137` (Self construction), `:157-202` (`start_fetch_logos`)

**Interfaces:**
- Consumes: same `frenchetv_core::logo_cache::fetch_logo` and `Config.cache.logo_ttl_hours` as Task 2.

This mirrors Task 2 exactly, applied to the Android app's near-identical `App` struct and `start_fetch_logos`.

- [ ] **Step 1: Add `logo_ttl_hours` field to `App`**

In `crates/ui-android/src/app.rs`, in the `App` struct (around line 109), change:

```rust
    pending_channel: Option<Channel>,
    logos: LogoCache,
```

to:

```rust
    pending_channel: Option<Channel>,
    logos: LogoCache,
    /// TTL (hours) for the on-disk logo cache — from `Config.cache.logo_ttl_hours`.
    logo_ttl_hours: u32,
```

- [ ] **Step 2: Capture the TTL in `App::new`**

Around lines 122-123, change:

```rust
        let logos: LogoCache = Arc::new(Mutex::new(HashMap::new()));
        let config = Config::load().unwrap_or_default();
```

to:

```rust
        let logos: LogoCache = Arc::new(Mutex::new(HashMap::new()));
        let config = Config::load().unwrap_or_default();
        let logo_ttl_hours = config.cache.logo_ttl_hours;
```

- [ ] **Step 3: Set the field in the `Self { ... }` construction**

Around lines 125-137, change:

```rust
            pending_channel: None,
            logos,
            tx,
```

to:

```rust
            pending_channel: None,
            logos,
            logo_ttl_hours,
            tx,
```

- [ ] **Step 4: Use the cache in `start_fetch_logos`**

Around lines 157-202, apply the identical change as Task 2 Step 4: add `let logo_ttl_hours = self.logo_ttl_hours;` right after `let ctx = self.egui_ctx.clone();` inside `fn start_fetch_logos`, and replace:

```rust
                        let bytes = client.get(&url).send().await.ok()?.bytes().await.ok()?;
```

with:

```rust
                        let bytes = frenchetv_core::logo_cache::fetch_logo(&client, &url, logo_ttl_hours)
                            .await
                            .ok()?;
```

- [ ] **Step 5: Verify as far as this environment allows**

This crate only compiles for `target_os = "android"` (all its UI/JNI dependencies in `crates/ui-android/Cargo.toml` are gated under `[target.'cfg(target_os = "android")'.dependencies]`) and requires the Android NDK's `clang` toolchain to link native deps. If `cargo-ndk` and the NDK are installed:

```bash
cargo ndk -t arm64-v8a -o /tmp/frenchetv-jniLibs build -p ui-android --release
```

Expected: builds clean.

If the NDK is not available (confirmed not installed in this sandbox — `cargo check -p ui-android --target aarch64-linux-android` fails with `failed to find tool "aarch64-linux-android-clang++"`), skip the build and instead diff this task's edit against Task 2's already-verified edit to confirm they're mechanically identical modulo file layout — the two `start_fetch_logos` bodies and `App` structs were already near-identical before this change (both use the same `LogoCache` type, same `Channel`/`Config` types from `frenchetv_core`). Flag in the PR description that the Android build must be verified in CI (which already runs `cargo ndk` per `CLAUDE.md`) before merge.

- [ ] **Step 6: Commit**

```bash
git add crates/ui-android/src/app.rs
git commit -m "feat(ui-android): use disk-cached logo fetch"
```

---

### Task 4: Final workspace-wide check

**Files:** none (verification only)

- [ ] **Step 1: Full workspace format and lint**

Run: `cargo fmt --all -- --check`
Expected: clean.

- [ ] **Step 2: Full test suite for crates that build on this host**

Run: `cargo test -p frenchetv-core && cargo build -p ui-desktop && cargo clippy -p ui-desktop -- -D warnings`
Expected: all pass. (`ui-android` excluded per Task 3 Step 5's environment constraint.)

- [ ] **Step 3: Confirm no stray debug artifacts**

Run: `git status`
Expected: only the files touched by Tasks 1-3 are modified/new; no accidental changes to `Cargo.lock` beyond the new `sha1`/`tempfile` entries pulled in by Task 1, no leftover files under `~/.cache/frenchetv/logos/` committed (that directory is outside the repo, but double-check nothing was written under the repo root during manual testing).
