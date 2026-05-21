# CLAUDE.md — FrenchTV (working title)
 
## Project Vision
 
A lightweight, blazing-fast IPTV client for French TV operators (Orange, Bouygues Télécom, and others), targeting PC (Linux/Windows/macOS), Android TV, and Amazon FireTV. The philosophy is radical simplicity: pick your operator once, enter credentials once, watch TV. That's it.
 
Think Kodi stripped of every unnecessary layer — no plugins, no XML skins, no Lua scripts. Just a polished, native-feeling TV app that works reliably on old hardware.
 
---
 
## Core Principles
 
1. **One-time setup** — operator selection + credentials stored locally, never asked again.
2. **Zero bloat** — no Electron, no JVM, no web engine unless strictly necessary.
3. **Speed first** — target sub-100ms UI response on a Raspberry Pi 4 class device.
4. **Portability** — single Rust codebase shared across all targets via a clean platform abstraction layer.
5. **Graceful degradation** — works fine on 720p screens, remote controls, and slow networks.
---
 
## Language & Runtime
 
- **Language**: Rust (stable toolchain, MSRV 1.75+)
- **Async runtime**: Tokio (multi-threaded, full features)
- **No unsafe unless strictly required** for FFI with native media APIs
---
 
## Workspace Layout
 
```
frenchetv/
├── Cargo.toml                  # workspace root
├── CLAUDE.md
│
├── crates/
│   ├── core/                   # business logic, no UI
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── operator/       # operator abstraction + implementations
│   │   │   │   ├── mod.rs
│   │   │   │   ├── traits.rs   # Operator trait
│   │   │   │   ├── orange.rs
│   │   │   │   └── bouygues.rs
│   │   │   ├── channel/        # channel list, logos, M3U parsing
│   │   │   │   ├── mod.rs
│   │   │   │   └── m3u.rs
│   │   │   ├── epg/            # Electronic Program Guide
│   │   │   │   ├── mod.rs
│   │   │   │   └── xmltv.rs    # XMLTV format parser
│   │   │   ├── stream/         # stream URL resolution, auth token refresh
│   │   │   │   └── mod.rs
│   │   │   └── config/         # persistent settings (TOML/JSON)
│   │   │       └── mod.rs
│   │   └── Cargo.toml
│   │
│   ├── ui-desktop/             # PC target (Linux, Windows, macOS)
│   │   ├── src/
│   │   │   ├── main.rs
│   │   │   ├── app.rs          # top-level egui app
│   │   │   ├── screens/
│   │   │   │   ├── setup.rs    # operator + credential setup
│   │   │   │   ├── channel_list.rs
│   │   │   │   ├── player.rs
│   │   │   │   └── epg.rs
│   │   │   └── player/
│   │   │       └── mpv.rs      # libmpv integration
│   │   └── Cargo.toml
│   │
│   └── ui-android/             # Android TV + FireTV target
│       ├── src/
│       │   ├── lib.rs          # cdylib entry point
│       │   ├── app.rs
│       │   ├── screens/        # same screen structure as desktop
│       │   └── player/
│       │       └── exoplayer.rs  # JNI bridge to ExoPlayer
│       ├── android/
│       │   ├── app/
│       │   │   ├── build.gradle
│       │   │   └── src/main/
│       │   │       ├── AndroidManifest.xml
│       │   │       └── java/tv/frenche/
│       │   │           ├── MainActivity.kt
│       │   │           └── PlayerActivity.kt  # ExoPlayer host
│       │   └── build.gradle
│       └── Cargo.toml
│
├── assets/
│   ├── logos/                  # fallback channel logos (SVG)
│   ├── fonts/                  # embedded UI fonts
│   └── channels/
│       ├── orange.m3u          # static fallback if autodiscovery fails
│       └── bouygues.m3u        # static fallback if autodiscovery fails
│
└── docs/
    ├── operators.md            # API notes per operator
    └── building.md             # platform build instructions
```
 
---
 
## Key Dependencies
 
### Core crate
```toml
[dependencies]
tokio       = { version = "1", features = ["full"] }
reqwest     = { version = "0.12", features = ["json", "cookies", "stream"] }
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
serde_urlencoded = "0.7"
m3u8-rs     = "6"               # M3U/M3U8 playlist parser
quick-xml   = "0.36"            # XMLTV EPG parsing
chrono      = { version = "0.4", features = ["serde"] }
url         = "2"
thiserror   = "2"
anyhow      = "1"
tracing     = "0.1"
tracing-subscriber = "0.3"
keyring     = "3"               # OS credential store (desktop only, feature-gated)
dirs        = "5"               # XDG / standard config paths
toml        = "0.8"
tokio-stream = "0.1"
bytes       = "1"
```
 
### ui-desktop crate
```toml
[dependencies]
frenchetv-core = { path = "../core" }
eframe  = { version = "0.30", features = ["wgpu"] }  # egui + wgpu renderer
egui    = "0.30"
egui_extras = { version = "0.30", features = ["image"] }  # channel logo display
image   = { version = "0.25", default-features = false, features = ["png", "jpeg"] }
libmpv2 = "3"                   # safe libmpv bindings for video playback
tokio   = { version = "1", features = ["full"] }
```
 
### ui-android crate
```toml
[lib]
crate-type = ["cdylib"]
 
[dependencies]
frenchetv-core = { path = "../core" }
android-activity = { version = "0.6", features = ["game-activity"] }
winit   = { version = "0.30", features = ["android-game-activity"] }
eframe  = { version = "0.30", features = ["wgpu"] }
egui    = "0.30"
jni     = "0.21"                # JNI bridge to Kotlin/ExoPlayer
tokio   = { version = "1", features = ["full"] }
```
 
---
 
## Operator Abstraction
 
The `Operator` trait is the heart of the core crate. Every operator implements it:
 
```rust
// crates/core/src/operator/traits.rs
 
#[async_trait::async_trait]
pub trait Operator: Send + Sync {
    /// Human-readable name shown in the setup screen
    fn name(&self) -> &'static str;
 
    /// Whether this operator requires credentials
    fn requires_auth(&self) -> bool;
 
    /// Authenticate and store a session token internally
    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()>;
 
    /// Return authenticated channel list (streams + metadata)
    async fn fetch_channels(&self) -> Result<Vec<Channel>>;
 
    /// Resolve the final playable stream URL for a channel (may refresh token)
    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl>;
 
    /// Fetch EPG data for the next `hours` hours (None if unsupported)
    async fn fetch_epg(&self, hours: u8) -> Result<Option<EpgData>>;
}
```
 
### `Channel` model
 
```rust
pub struct Channel {
    pub id: String,
    pub name: String,
    pub logo_url: Option<String>,
    pub number: Option<u32>,
    pub category: ChannelCategory,   // News, Sports, Entertainment, Kids, ...
    pub stream_template: StreamTemplate,
}
 
pub enum StreamTemplate {
    /// Direct HLS/DASH URL, no further resolution needed
    Direct(Url),
    /// Needs operator-specific resolution (auth header injection, token swap, etc.)
    Authenticated { base_url: Url },
}
```
 
---
 
## Operator Implementation Notes
 
### Orange TV
 
Orange provides access to live streams through its multicast IPTV infrastructure when on its network, and through HTTP streams (HLS/DASH) when on internet.
 
**Authentication flow:**
1. `POST https://sso.orange.fr/oauth/v2/token` with client credentials → Bearer token
2. Token is valid ~1h, refresh automatically with stored refresh_token
3. Channel list: `GET https://rp-iptv.orange.fr/EPG/JSON/getChannelList` (no auth needed for the list, auth for stream resolution)
4. Stream resolution: inject `Authorization: Bearer <token>` header into HLS requests, or swap a signed token in the URL depending on the channel
**Auto-discovery**: Yes — Orange exposes a JSON channel list endpoint. Parse it to build `Vec<Channel>`.
 
**EPG**: Orange provides XMLTV-compatible EPG at `https://rp-iptv.orange.fr/EPG/XML/epg_date_YYYYMMDD.xml.gz`. Decompress with `flate2`, parse with `quick-xml`.
 
**Fallback M3U** (`assets/channels/orange.m3u`): Provide a curated static M3U with ~100 main channels for use without credentials or when API is unreachable.
 
---
 
### Bouygues Télécom (Bbox)
 
**Authentication flow:**
1. `POST https://api.bbox.fr/api/v1/login` with `{ login, password }` → session cookie + token
2. Channel list: `GET https://api.bbox.fr/api/v1/bouyguestv/channels` → JSON array
3. Stream resolution: Each channel has an `hls_url` field; for premium channels, append `?access_token=<token>`
**Auto-discovery**: Yes — the channels API returns a full list including logos, channel numbers, and HLS URLs.
 
**EPG**: `GET https://api.bbox.fr/api/v1/bouyguestv/epg?period=<YYYYMMDD>&channel_id=<id>` — query per channel or in batch.
 
**Fallback M3U** (`assets/channels/bouygues.m3u`): Static fallback for ~80 channels.
 
---
 
### Adding a New Operator (guide for future contributors)
 
1. Create `crates/core/src/operator/<name>.rs`
2. Implement the `Operator` trait
3. Add the variant to `OperatorKind` enum in `operator/mod.rs`
4. Register it in `OperatorRegistry::all()`
5. If credentials-free streams exist, add a static M3U to `assets/channels/`
6. Document the API endpoints in `docs/operators.md`
---
 
## Config & Persistence
 
Config file location:
- Linux: `~/.config/frenchetv/config.toml`
- Windows: `%APPDATA%\frenchetv\config.toml`
- Android/FireTV: app internal storage `/data/data/tv.frenche/config.toml`
```toml
# config.toml schema
[operator]
kind = "orange"          # "orange" | "bouygues" | ...
username = "user@example.com"
# password is NOT stored here — stored in OS keyring on desktop,
# in Android Keystore on Android
 
[preferences]
language = "fr"
parental_lock = false
startup_channel = "tf1"  # channel id
 
[cache]
epg_ttl_minutes = 60
logo_ttl_hours = 24
```
 
Credentials on desktop: use `keyring` crate (`keyring::Entry::new("frenchetv", username)`).
On Android: use Android Keystore via JNI helper in `PlayerActivity.kt`.
 
**Never store plaintext passwords in the config file.**
 
---
 
## UI Structure & Navigation
 
The app has exactly **4 screens**:
 
```
Setup Screen  ──(first launch or reset)──►  Channel List  ──►  Player
                                                │
                                                └──►  EPG Grid  (optional, 'guide' button)
```
 
### Setup Screen
- Operator picker (card grid, one tap to select)
- Username + password fields (if `requires_auth()`)
- "Watch TV" button — triggers `authenticate()` then `fetch_channels()`, transitions to Channel List
- Error handling inline (bad credentials, network error)
### Channel List
- Vertical scrollable grid of channel tiles (logo + name + number)
- Filter tabs: All / News / Sports / Entertainment / Kids
- Search bar (filter by name, number)
- Focus/cursor navigation for remote controls (D-pad friendly)
- Selecting a channel: calls `resolve_stream()`, then launches player
### Player Screen (Desktop: embedded mpv; Android: fullscreen ExoPlayer Activity)
- Channel name + current show title (from EPG if available) overlay, auto-hide after 3s
- Left/right D-pad: switch channel ±1
- Up/Down: volume
- OK/Enter: show/hide info bar
- Back: return to Channel List
- 'Guide' button: open EPG
### EPG Grid Screen
- 2D grid: channels (rows) × time (columns, 30min slots)
- Horizontally scrollable, current time highlighted
- Show title + duration in each cell
- Clicking a past show: nothing (or replay if supported)
- Clicking current show: open player for that channel
---
 
## Video Playback Architecture
 
### Desktop (libmpv)
 
Use the `libmpv2` crate. Open streams with:
 
```rust
mpv.command("loadfile", &[stream_url.as_str(), "replace"])?;
// For authenticated HLS, pass headers:
mpv.set_property("http-header-fields", "Authorization: Bearer <token>")?;
```
 
Embed the mpv render surface into the egui window using `mpv_render_context_render()` with a custom OpenGL/wgpu texture.
 
### Android TV / FireTV (ExoPlayer via JNI)
 
The Rust UI layer calls back into Kotlin to start `PlayerActivity` with an intent:
 
```kotlin
// MainActivity.kt — JNI-exposed function
@JvmStatic
fun playStream(url: String, title: String, authHeader: String?) {
    val intent = Intent(context, PlayerActivity::class.java).apply {
        putExtra("url", url)
        putExtra("title", title)
        putExtra("auth_header", authHeader)
    }
    context.startActivity(intent)
}
```
 
`PlayerActivity.kt` hosts an ExoPlayer instance with `DefaultHttpDataSource.Factory` configured with the auth header.
 
This approach means video decoding on Android is handled entirely by the platform's hardware decoder via ExoPlayer — maximum compatibility, including 4K HDR on capable devices.
 
---
 
## Build Instructions
 
### Desktop
 
```bash
# Install libmpv (system dependency)
# Ubuntu/Debian: sudo apt install libmpv-dev
# macOS: brew install mpv
# Windows: download mpv dev package, set MPV_LIB_DIR env var
 
cargo build -p ui-desktop --release
```
 
### Android TV / FireTV
 
Requirements: Android SDK 33+, NDK r26+, Kotlin 1.9+
 
```bash
# Install rust android targets
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
 
# Install cargo-ndk
cargo install cargo-ndk
 
# Build the Rust shared library
cargo ndk -t arm64-v8a -t armeabi-v7a -o crates/ui-android/android/app/src/main/jniLibs build -p ui-android --release
 
# Build the APK / AAB
cd crates/ui-android/android
./gradlew assembleRelease          # APK for sideloading
./gradlew bundleRelease            # AAB for Play Store / Amazon Appstore
```
 
Target FireTV: same APK, sideload via `adb install` or publish to Amazon Appstore. FireTV SDK is standard Android API level — no special handling needed.
 
---
 
## UI Design Guidelines
 
- **Color palette**: Dark background (`#0D0F14`), accent electric blue (`#0A84FF`), white text — cinema-like, easy on eyes in a dark living room.
- **Typography**: Use embedded `Inter` or `DM Sans` for UI labels; large channel numbers in a condensed bold font.
- **Focus ring**: Clear 3px accent-colored border on focused element — essential for remote control UX.
- **Animations**: Keep it to essentials — channel tile hover scale (1.02), screen cross-fade (100ms). No decorative animations that add latency.
- **Font sizes**: Minimum 18sp for body text (TV viewing distance), 24sp for channel names.
- **Touch vs remote**: On Android TV/FireTV, disable all hover states, increase tap targets to 48dp minimum. All navigation must be fully functional with D-pad alone.
- **No modal dialogs** — use inline error states and toast-style notifications.
---
 
## Error Handling Strategy
 
- `anyhow::Result` at the application boundary (screens, event handlers)
- `thiserror`-derived typed errors in the core crate (`OperatorError`, `StreamError`, `EpgError`)
- Authentication errors → redirect to Setup Screen with message
- Stream resolution failure → show error overlay on player, auto-retry 3× with exponential backoff
- EPG fetch failure → degrade gracefully (hide EPG button, don't crash)
- Network timeout: 10s for auth, 5s for channel list, 30s for stream start
---
 
## Testing Strategy
 
```
crates/core/src/
└── operator/
    ├── orange.rs          + orange_test.rs   (mock HTTP with wiremock)
    └── bouygues.rs        + bouygues_test.rs
 
Integration tests: tests/
└── channel_list.rs        (requires VPN/network, gated behind --ignored)
```
 
- Use `wiremock` for mocking operator HTTP APIs in unit tests
- Channel list parsing tests against real M3U fixture files in `tests/fixtures/`
- CI: GitHub Actions, test on ubuntu-latest + windows-latest + macos-latest for desktop crate
- Android: build-only CI check via `cargo ndk`
---
 
## Important Implementation Notes
 
1. **M3U fallback is mandatory** — operator APIs go down, change, or block VPN/CI IPs. Always ship static M3U files as fallback. The app should detect API failure and transparently fall back to the static list.
2. **Token refresh must be transparent** — wrap every API call in a retry loop that refreshes the auth token on 401 and retries once. The user should never see a "session expired" error mid-session.
3. **Channel logos** — fetch from operator API or logo URL in M3U. Cache on disk (LRU, 50MB limit). Display a placeholder SVG while loading. Never block the channel list render on logo fetches.
4. **HLS stream start latency** — pre-buffer the stream URL resolution while the user is still in the channel list (on hover/focus). By the time they select a channel, the URL may already be ready.
5. **Remote control key mapping** — Android TV remote: D-pad maps to egui's `Key::ArrowUp/Down/Left/Right`, Select → `Key::Enter`, Back → `Key::Escape`. Test with `adb shell input keyevent`.
6. **Legal note** — This project only accesses streams and APIs that operators make available to their own subscribers. It does not circumvent DRM, re-stream content, or access content the user hasn't paid for. Each operator's Terms of Service apply.
---
 
## Roadmap
 
### MVP (v0.1)
- [ ] Orange operator: auth + channel list + HLS playback
- [ ] Bouygues operator: auth + channel list + HLS playback  
- [ ] Setup screen
- [ ] Channel list with category filter
- [ ] Desktop player (libmpv embedded)
- [ ] Android TV build + ExoPlayer
### v0.2
- [ ] EPG grid screen
- [ ] Channel logo cache
- [ ] Startup channel preference
- [ ] SFR operator
### v0.3
- [ ] Free (Freebox) operator
- [ ] Stream quality selector (SD/HD/4K)
- [ ] Parental lock
- [ ] Catchup TV (if operator supports it)
- [ ] Chromecast / AirPlay discovery
### v1.0
- [ ] Publish to Amazon Appstore
- [ ] Auto-update mechanism
- [ ] Telemetry opt-in (crash reports only, no usage data)
