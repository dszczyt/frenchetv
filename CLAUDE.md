# CLAUDE.md
 
This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.
 
## Project
 
**frenchetv** — lightweight open-source IPTV client for French operators (Orange, Bouygues, SFR, Free), targeting Linux/Windows/macOS desktop and Android TV / FireTV. Rust workspace, no Electron, no JVM (except Android).
 
- GitHub: https://github.com/dszczyt/frenchetv (public from day one)
- License: MIT
- **MSRV: 1.97** — do not use language or standard library features introduced after Rust 1.97. (Bumped from a stale 1.75: a transitive dependency already required edition2024, i.e. Rust ≥1.85, so the old claim was unenforceable. CI's quality gate is pinned to 1.97.1 — see `.github/workflows/ci.yml`.)
---
 
## Build Commands
 
### Desktop (requires `libmpv-dev` on Linux, `mpv` via brew on macOS)
 
```bash
cargo build -p ui-desktop                  # debug
cargo build -p ui-desktop --release        # release
cargo run -p ui-desktop                    # run
```
 
### Core crate only
 
```bash
cargo build -p frenchetv-core
cargo test -p frenchetv-core               # unit tests (wiremock, no network)
cargo test -p frenchetv-core -- --ignored  # integration tests (requires network/VPN)
```
 
### All crates
 
```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all                            # format before committing
cargo clippy --workspace -- -D warnings
```
 
### Android TV / FireTV
 
```bash
# One-time setup
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk
 
# Build Rust .so files
cargo ndk -t arm64-v8a -t armeabi-v7a \
  -o crates/ui-android/android/app/src/main/jniLibs \
  build -p ui-android --release
 
# Build APK or AAB
cd crates/ui-android/android
./gradlew assembleRelease   # APK for sideloading / adb install
./gradlew bundleRelease     # AAB for Play Store / Amazon Appstore
```
 
---
 
## Architecture
 
### Crate layout
 
| Crate | `name` in Cargo.toml | Role |
|---|---|---|
| `crates/core` | `frenchetv-core` | All business logic — operator auth, channel list, stream resolution, EPG, config. No UI. |
| `crates/ui-desktop` | `ui-desktop` | egui/wgpu app for PC. Embeds libmpv for playback via `libmpv2` crate. |
| `crates/ui-android` | `ui-android` | `cdylib` — egui/wgpu app for Android TV/FireTV. Calls back into Kotlin via JNI for ExoPlayer. |
 
### Central abstraction: `Operator` trait
 
`crates/core/src/operator/traits.rs` — every operator implements:
 
```rust
#[async_trait::async_trait]
pub trait Operator: Send + Sync {
    fn name(&self) -> &'static str;
    fn requires_auth(&self) -> bool;
    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()>;
    async fn fetch_channels(&self) -> Result<Vec<Channel>>;
    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl>;
    async fn fetch_epg(&self, hours: u8) -> Result<Option<EpgData>>;
}
```
 
`OperatorKind` enum + `OperatorRegistry::all()` in `crates/core/src/operator/mod.rs` register all implementations. To add an operator: implement trait in `operator/<name>.rs`, add variant to enum, register in `all()`. See `CONTRIBUTING.md` for the full checklist.
 
### Screen flow
 
```
Setup ──(first launch)──► Channel List ──► Player
                               │
                               └──► EPG Grid
```
 
Screen structs live in `crates/ui-desktop/src/screens/` (and mirrored in `crates/ui-android/src/screens/`). Top-level app state in `app.rs`.
 
### Playback split
 
- **Desktop**: `crates/ui-desktop/src/player/mpv.rs` — renders into egui window via `mpv_render_context_render()` with a wgpu texture.
- **Android**: `crates/ui-android/src/player/exoplayer.rs` — JNI call into `PlayerActivity.kt`, which hosts ExoPlayer with auth header injected via `DefaultHttpDataSource.Factory`. Hardware decoder handles 4K HDR.
### Config & credentials
 
Config: `~/.config/frenchetv/config.toml` (Linux), `%APPDATA%\frenchetv\config.toml` (Windows), app internal storage (Android).
Credentials: OS keyring via `keyring` crate on desktop (`keyring::Entry::new("frenchetv", username)`), Android Keystore on Android. **Passwords must never be written to `config.toml`.**
 
---
 
## Key Implementation Rules
 
1. **M3U fallback is mandatory** — every operator must have a static fallback M3U in `assets/channels/`. The app detects API failure and silently falls back.
2. **Transparent token refresh** — wrap all API calls with a 401-retry that refreshes the token. Users must never see "session expired".
3. **Non-blocking logos** — fetch and cache channel logos asynchronously (LRU, 50 MB cap). Never block channel list render on logo fetches.
4. **Pre-resolve streams** — start `resolve_stream()` on channel focus, before the user confirms selection.
5. **No unsafe** except FFI boundaries (libmpv, JNI).
---
 
## Error Handling
 
- `anyhow::Result` at the UI boundary (screens, event handlers).
- `thiserror`-derived errors in core: `OperatorError`, `StreamError`, `EpgError`.
- Auth errors → redirect to Setup with inline message.
- Stream errors → overlay with auto-retry (3×, exponential backoff).
- EPG errors → degrade silently (hide Guide button).
- Timeouts: 10s auth, 5s channel list, 30s stream start.
---
 
## Testing
 
- Unit tests use `wiremock` to mock operator HTTP APIs; no real network needed.
- Fixture M3U files live in `tests/fixtures/`.
- Integration tests (real network/VPN) are `#[ignore]`-gated; run with `-- --ignored`.
- CI matrix: ubuntu-latest, windows-latest, macos-latest for desktop; build-only check for Android via `cargo ndk`.
---
 
## Contributing
 
This is an open-source project. Before opening a PR:
 
- Run `cargo fmt --all` and `cargo clippy --workspace -- -D warnings` — CI will reject anything that doesn't pass.
- Operator API changes (broken endpoints, new auth flow) are the most frequent contribution — document them in `docs/operators.md` alongside the code change.
- Do not commit real credentials or personal tokens anywhere, including test fixtures.
- See `CONTRIBUTING.md` for the full operator integration checklist and PR template.
