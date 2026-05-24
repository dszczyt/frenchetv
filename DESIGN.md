# DESIGN.md — frenchetv
 
## Project Vision
 
A lightweight, blazing-fast IPTV client for French TV operators (Orange, Bouygues Télécom, SFR, Free), targeting PC (Linux/Windows/macOS), Android TV, and Amazon FireTV.
 
The philosophy is radical simplicity: pick your operator once, enter credentials once, watch TV. That's it.
 
Think Kodi stripped of every unnecessary layer — no plugins, no XML skins, no Lua scripts. A polished, native-feeling TV app that works reliably on old hardware, maintained by its community.
 
---
 
## Core Principles
 
1. **One-time setup** — operator selection + credentials stored locally, never asked again.
2. **Zero bloat** — no Electron, no JVM, no web engine unless strictly necessary.
3. **Speed first** — target sub-100ms UI response on a Raspberry Pi 4 class device.
4. **Portability** — single Rust codebase shared across all targets via a clean platform abstraction layer.
5. **Graceful degradation** — works on 720p screens, remote controls, and slow networks.
6. **Community-maintained** — operator integrations are the most fragile part; the open-source model is the only sustainable way to keep them working long-term.
---
 
## Open Source Model & Sustainability
 
### Why open source
 
French operators don't document their APIs. Everything in this project comes from community reverse engineering. Keeping this knowledge locked in a closed codebase would be both legally riskier and practically unsustainable — one person can't track API changes across Orange, Bouygues, SFR, and Free simultaneously.
 
Open source means:
- Contributors with each operator report and fix breakage as it happens
- The project survives if the original author has less time
- Legal exposure is distributed and lower-profile than a commercial closed app
### License
 
**MIT** — permissive, no copyleft friction for contributors.
 
### Patreon
 
A Patreon funds ongoing maintenance without gating the software itself. The app stays free for everyone.
 
**Tier structure (suggested):**
 
| Tier | Price | Perks |
|---|---|---|
| Supporter | 2€/month | Name in CONTRIBUTORS, warm feelings |
| Builder | 5€/month | Access to private Discord, early builds before releases |
| Patron | 10€/month | Vote on roadmap priorities (next operator, next feature) |
 
**What Patreon funds:**
- Time spent tracking operator API changes
- CI/CD infrastructure (GitHub Actions minutes, build runners)
- Apple Developer account (if macOS notarization is needed)
- Amazon Developer account (Appstore publishing)
**What Patreon does NOT do:**
- Gate features
- Create a paid vs free version split
- Change the license
### Communication cadence
 
A monthly devlog post on Patreon (cross-posted to GitHub Discussions) is the minimum. The most engaging posts are technical: "Orange changed their auth flow again, here's how we fixed it." This kind of transparency builds trust and drives subscriptions better than feature announcements.
 
### When the project is ready to open Patreon
 
Not before a working v0.1 with at least Orange + Bouygues functional. People don't fund promises on an empty repo — they fund a working thing they already use.
 
---
 
## Language & Runtime
 
- **Language**: Rust (stable toolchain, MSRV 1.75+)
- **Async runtime**: Tokio (multi-threaded, full features)
- **No unsafe** unless strictly required for FFI with native media APIs
---
 
## Workspace Layout
 
```
frenchetv/
├── Cargo.toml                  # workspace root
├── CLAUDE.md                   # Claude Code operational guide
├── DESIGN.md                   # this file
├── CONTRIBUTING.md             # operator integration checklist, PR template
├── CHANGELOG.md                # user-facing changelog, updated each release
│
├── crates/
│   ├── core/                   # business logic, no UI
│   │   └── src/
│   │       ├── operator/       # Operator trait + implementations
│   │       │   ├── traits.rs
│   │       │   ├── orange.rs
│   │       │   └── bouygues.rs
│   │       ├── channel/        # channel list, logos, M3U parsing
│   │       ├── epg/            # XMLTV parser
│   │       ├── stream/         # stream URL resolution, token refresh
│   │       └── config/         # persistent settings
│   │
│   ├── ui-desktop/             # Linux / Windows / macOS
│   │   └── src/
│   │       ├── screens/        # setup, channel_list, player, epg
│   │       └── player/mpv.rs   # libmpv integration
│   │
│   └── ui-android/             # Android TV + FireTV
│       ├── src/
│       │   ├── screens/
│       │   └── player/exoplayer.rs   # JNI bridge
│       └── android/            # Gradle project, MainActivity.kt, PlayerActivity.kt
│
├── assets/
│   ├── logos/                  # fallback channel logos (SVG)
│   ├── fonts/                  # embedded UI fonts
│   └── channels/
│       ├── orange.m3u          # static fallback
│       └── bouygues.m3u
│
└── docs/
    ├── operators.md            # per-operator API notes (updated by contributors)
    └── building.md             # platform build instructions
```
 
---
 
## Key Dependencies
 
### Core crate
```toml
tokio            = { version = "1", features = ["full"] }
reqwest          = { version = "0.12", features = ["json", "cookies", "stream"] }
serde            = { version = "1", features = ["derive"] }
serde_json       = "1"
m3u8-rs          = "6"
quick-xml        = "0.36"
chrono           = { version = "0.4", features = ["serde"] }
thiserror        = "2"
anyhow           = "1"
tracing          = "0.1"
tracing-subscriber = "0.3"
keyring          = "3"
dirs             = "5"
toml             = "0.8"
bytes            = "1"
```
 
### ui-desktop crate
```toml
eframe      = { version = "0.30", features = ["wgpu"] }
egui        = "0.30"
egui_extras = { version = "0.30", features = ["image"] }
libmpv2     = "3"
```
 
### ui-android crate
```toml
android-activity = { version = "0.6", features = ["game-activity"] }
winit            = { version = "0.30", features = ["android-game-activity"] }
eframe           = { version = "0.30", features = ["wgpu"] }
jni              = "0.21"
```
 
---
 
## Operator Abstraction
 
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
 
```rust
pub struct Channel {
    pub id: String,
    pub name: String,
    pub logo_url: Option<String>,
    pub number: Option<u32>,
    pub category: ChannelCategory,
    pub stream_template: StreamTemplate,
}
 
pub enum StreamTemplate {
    Direct(Url),
    Authenticated { base_url: Url },
}
```
 
---
 
## Operator Implementation Notes
 
### Orange TV
 
**Auth flow:**
1. `POST https://sso.orange.fr/oauth/v2/token` → Bearer token (~1h TTL) + refresh_token
2. Channel list: `GET https://rp-iptv.orange.fr/EPG/JSON/getChannelList` (public, no auth)
3. Stream resolution: inject `Authorization: Bearer <token>` into HLS requests
**EPG:** `https://rp-iptv.orange.fr/EPG/XML/epg_date_YYYYMMDD.xml.gz` — gzip XMLTV, decompress with `flate2`.
 
**Auto-discovery:** Yes (JSON channel list endpoint).
 
**Fallback M3U:** `assets/channels/orange.m3u` — ~100 main channels.
 
---
 
### Bouygues Télécom (Bbox)
 
**Auth flow:**
1. `POST https://api.bbox.fr/api/v1/login` → session cookie + token
2. Channel list: `GET https://api.bbox.fr/api/v1/bouyguestv/channels` → JSON with `hls_url` per channel
3. Premium channels: append `?access_token=<token>` to HLS URL
**EPG:** `GET https://api.bbox.fr/api/v1/bouyguestv/epg?period=<YYYYMMDD>&channel_id=<id>`
 
**Auto-discovery:** Yes.
 
**Fallback M3U:** `assets/channels/bouygues.m3u` — ~80 channels.
 
---
 
### Adding a New Operator
 
1. Create `crates/core/src/operator/<name>.rs`, implement the `Operator` trait
2. Add variant to `OperatorKind` enum, register in `OperatorRegistry::all()`
3. Add static fallback M3U to `assets/channels/`
4. Document the API endpoints in `docs/operators.md`
5. Add unit tests with `wiremock` fixtures
6. Open a PR — the checklist in `CONTRIBUTING.md` must be fully checked
---
 
## UI Structure & Navigation
 
4 screens, no more:
 
```
Setup ──(first launch / reset)──► Channel List ──► Player
                                       │
                                       └──► EPG Grid
```
 
### Setup Screen
- Operator picker (card grid, one tap)
- Username + password fields if `requires_auth()`
- "Watch TV" CTA → `authenticate()` + `fetch_channels()` → Channel List
- Inline error handling (no modals)
### Channel List
- Scrollable grid: logo + name + number per tile
- Filter tabs: All / News / Sports / Entertainment / Kids
- Search bar (name or number)
- Full D-pad navigation (remote control friendly)
- `resolve_stream()` starts on focus, before selection is confirmed
### Player
- Desktop: libmpv embedded in egui window
- Android/FireTV: fullscreen ExoPlayer Activity (JNI handoff)
- Overlay: channel name + current show title, auto-hides after 3s
- D-pad left/right: channel ±1 | up/down: volume | OK: toggle overlay | Back: Channel List
### EPG Grid
- 2D grid: channels × 30min time slots
- Current time column highlighted
- Clicking current show → opens player for that channel
---
 
## Video Playback
 
### Desktop (libmpv)
```rust
mpv.command("loadfile", &[stream_url.as_str(), "replace"])?;
mpv.set_property("http-header-fields", "Authorization: Bearer <token>")?;
```
Rendered via `mpv_render_context_render()` into a wgpu texture embedded in the egui window.
 
### Android TV / FireTV (ExoPlayer via JNI)
```kotlin
fun playStream(url: String, title: String, authHeader: String?) {
    val intent = Intent(context, PlayerActivity::class.java).apply {
        putExtra("url", url)
        putExtra("title", title)
        putExtra("auth_header", authHeader)
    }
    context.startActivity(intent)
}
```
ExoPlayer with `DefaultHttpDataSource.Factory` + auth header. Platform hardware decoder → 4K HDR at no extra cost.
 
---
 
## Config & Credentials
 
```toml
# ~/.config/frenchetv/config.toml
[operator]
kind = "orange"
username = "user@example.com"
# password → OS keyring, never here
 
[preferences]
language = "fr"
parental_lock = false
startup_channel = "tf1"
 
[cache]
epg_ttl_minutes = 60
logo_ttl_hours = 24
```
 
Credentials: `keyring` crate on desktop, Android Keystore on Android. **Plaintext passwords must never reach disk.**
 
---
 
## UI Design Guidelines
 
- **Palette**: Dark background `#0D0F14`, accent electric blue `#0A84FF`, white text — cinema feel, dark room friendly.
- **Focus ring**: 3px accent border on focused element — non-negotiable for remote control UX.
- **Font sizes**: 18sp minimum body, 24sp channel names (TV viewing distance).
- **Animations**: channel tile scale on focus (1.02×), screen cross-fade (100ms max). Nothing that adds perceived latency.
- **D-pad only**: on Android TV/FireTV, the entire app must be fully navigable without a pointer. No hover states.
- **No modals**: inline errors and toast notifications only.
---
 
## Error Handling
 
| Layer | Error type | Behavior |
|---|---|---|
| UI boundary | `anyhow::Result` | Catch-all, log and show inline message |
| Core crate | `thiserror` (`OperatorError`, `StreamError`, `EpgError`) | Typed, matchable |
| Auth failure | `OperatorError::AuthFailed` | Redirect to Setup |
| Stream failure | `StreamError` | Overlay + auto-retry 3×, exponential backoff |
| EPG failure | `EpgError` | Silent degradation, hide Guide button |
 
Timeouts: 10s auth · 5s channel list · 30s stream start.
 
---
 
## Roadmap
 
### v0.1 — MVP (prerequisite for opening Patreon)
- [ ] Orange: auth + channel list + HLS playback
- [ ] Bouygues: auth + channel list + HLS playback
- [ ] Setup screen
- [ ] Channel list with category filter
- [ ] Desktop player (libmpv)
- [ ] Android TV / FireTV build + ExoPlayer
- [ ] GitHub repo public, README, LICENSE, CONTRIBUTING.md
### v0.2 — Polish
- [ ] EPG grid screen
- [ ] Channel logo disk cache
- [ ] Startup channel preference
- [ ] SFR operator
### v0.3 — Growth
- [ ] Free (Freebox) operator
- [ ] Stream quality selector (SD / HD / 4K)
- [ ] Parental lock
- [ ] Catchup TV (if operator supports it)
- [ ] Chromecast / AirPlay discovery
### v1.0 — Distribution
- [ ] Publish to Amazon Appstore
- [ ] Auto-update mechanism (desktop)
- [ ] Opt-in crash reporting (no usage telemetry)
- [ ] Patreon milestone: fund Apple Developer account for macOS notarization
