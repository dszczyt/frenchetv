# Bouygues native live playback — design

**Date:** 2026-05-31
**Status:** approved (brainstorming) — pending spike result before implementation
**Scope:** Make a click on a Bouygues channel in the channel list actually play the
live stream, natively via libmpv + the existing Widevine DRM proxy, with **no
browser in the shipped client**.

## Problem

`fetch_channels` works: ~444 channels are read from the Kaltura OTT lineup via an
anonymous Kaltura session. But `resolve_stream` (`crates/core/src/operator/bouygues.rs`)
returns a hard `501` error for every live channel:

> "La lecture en direct Bouygues n'est pas disponible (protection DRM/PFS non supportée)."

So clicking a Bouygues channel shows a DRM-unavailable overlay and never plays.

Live playback is gated by a **PFS security WASM module** (`wasm_comm_module.wasm`,
~4.3 MB C++/OpenSSL, talks to `iptv.pfs.bouyguesbox.fr`). The B.tv web app runs it
to mint, client-side, both:

1. an **entitled** Kaltura session (KS), and
2. the `bt-api-int` gateway credential `Authorization: Basic base64(<32-char id>:<16-char secret>)`

that are required to fetch `…/dash_cenc/index.mpd` and its Widevine license. See
`docs/operators.md` → "Channel list: implemented (Kaltura). Playback: blocked by
the PFS WASM."

## Goal & constraints

- Click channel → live stream plays, native libmpv, like the Orange path.
- **No Chrome/headless browser in the shipped product.** (A one-time browser
  capture for reverse-engineering / mapping is fine — it is not shipped.)
- **Do not break Orange.** All code changes are confined to `bouygues.rs` and docs.
  Shared types (`StreamUrl`, `ProtectionData`) and the desktop `drm/` proxy are
  read and reused, never modified.
- Reuse the existing Orange Widevine DASH proxy (`crates/ui-desktop/src/drm/`).
- MSRV 1.75; `wiremock` unit tests, no network in CI.

## Decision rejected

- **Headless-browser harvest at runtime** (drive Chrome to let its own WASM mint
  creds, intercept the manifest). Delivers the real stream and survives Bouygues
  security updates, but requires Chrome in the product. **Rejected** by explicit
  user constraint (native libmpv only).
- **Public/free HLS substitute lineup.** Not the real Bouygues stream; partial
  coverage; legally grey. Rejected.

## Key reused interface — no changes required

`crates/core/src/stream/mod.rs` already models everything the proxy needs:

```rust
pub struct StreamUrl {
    pub url: Url,                       // the index.mpd
    pub auth_header: Option<String>,    // "Basic <cred>" for bt-api-int
    pub headers: Vec<(String, String)>, // segment headers (Origin/Referer/UA)
    pub protection: Option<ProtectionData>,
}
pub struct ProtectionData {
    pub la_url: String,                 // Widevine licenseServerURL
    pub pssh: Option<Vec<u8>>,          // None → proxy extracts from MPD
    pub license_headers: Vec<(String, String)>, // license JWT + Basic + Origin/Referer/UA
}
```

The desktop `drm/` proxy + mpv already consume a populated `StreamUrl.protection`
(this is exactly how Orange plays). Bouygues only has to fill the same struct.

---

## Phase 0 — RE spike (gate, one-time, browser allowed for mapping only)

**Single question that decides feasibility:** *is the `bt-api-int` Basic credential
STATIC (one fixed app-level value, reusable across users/sessions/days) or
PFS-ROTATED (minted fresh per session by the WASM)?*

Against a logged-in B.tv web session, captured with the existing Playwright/CDP
harness (not shipped):

1. **Entitled KS** — POST Kaltura `ottUser/login` (or `/anonymousLogin` then a
   `/login`/refresh) using the Keycloak `access_token`/`id_token` as the external
   token → entitled `result.ks`. Record the exact request shape.
2. **getPlaybackContext** — POST `asset/action/getPlaybackContext` with the
   entitled KS + a lineup `assetId` + `LIVE` params → capture `result.sources[].url`
   (`…/dash_cenc/index.mpd`) and `result.sources[].drm[]` (Widevine
   `licenseServerURL` + license JWT).
3. **Decisive test** — fetch that `index.mpd` natively (reqwest) with the
   **previously captured** `Authorization: Basic` header + a **fresh** KS. Repeat
   after several hours / the next day.
   - 200 across sessions/days → **STATIC → green light → implement Phase 1.**
   - 401/403 → **PFS-ROTATED → RED. Native is blocked.** Stop. Report. Porting
     the PFS WASM or a runtime browser bridge becomes a separate decision, not
     part of this work.

**Output:** a findings note appended to `docs/operators.md` (request shapes, the
static/rotated verdict, capture date). No credentials or tokens committed.

---

## Phase 1 — Native `resolve_stream` (implemented ONLY if Phase 0 is green)

All in `crates/core/src/operator/bouygues.rs`.

**1a. `kaltura_entitled_ks()`** — new method mirroring `kaltura_anonymous_ks()`.
Uses stored Keycloak tokens (`self.access_token` / `self.id_token`) → POST
`ottUser/login` → entitled `result.ks`. Cached on the operator; refreshed on 401.

**1b. `kaltura_playback_context(asset_id)`** — new method:
- POST `asset/action/getPlaybackContext` with the entitled KS + `assetId`
  (`channel.id` from the lineup, already stored) + `LIVE` params.
- Pick the `dash_cenc` source → `index.mpd` URL.
- Parse `drm[]` → Widevine `licenseServerURL` + license token.

**1c. Rewrite `resolve_stream`** — replace the `501` branch. For a Kaltura live
channel:
- `ks = entitled_ks()`; `ctx = playback_context(channel.id)`.
- Build `StreamUrl`:
  - `url` = `index.mpd`
  - `auth_header` = `Basic <static cred>` (const from spike — see storage note)
  - `headers` = required segment headers (Origin/Referer/UA from spike)
  - `protection = ProtectionData { la_url: <licenseServerURL>, pssh: None,
    license_headers: [license-JWT header, Basic, Origin/Referer/UA] }`
- The existing M3U-direct branch (non-`PLACEHOLDER_URL` URL → `StreamUrl::direct`)
  is left **unchanged**.

**1d. Wiring** — none. Desktop `drm/` proxy + mpv already consume
`StreamUrl.protection`. Channel focus pre-resolve (CLAUDE.md rule 4) starts the
proxy; click plays.

**Static Basic cred storage** — store as a `const` in `bouygues.rs` with a comment
citing the spike date and `docs/operators.md`. It is an app-level credential, not a
user secret, so it is not subject to the keyring rule. If it ever rotates in
practice, the 401 path (below) surfaces a clean `StreamError`, never a logout.

---

## Phase 2 — Error handling & testing

### Error handling (per CLAUDE.md)

- Entitled-KS 401 → transparent KS refresh once, then retry. Still 401 **and**
  Keycloak session genuinely dead → `OperatorError::InvalidCredentials` → Setup
  redirect.
- `getPlaybackContext` / manifest non-200 → `StreamError` (overlay + auto-retry
  3× exponential backoff). **Never** `InvalidCredentials`.
- Basic-cred 401 (unexpected rotation) → `StreamError` with a clear message + warn
  log. **No logout.** (Guards the session-expired logout bug fixed 2026-05-31.)
- Any live-resolution failure leaves the existing static M3U fallback channels
  playable (their `resolve_stream` branch is untouched).
- 30s stream-start timeout (existing behaviour).

### Testing (wiremock, no network)

- `ottUser/login` mock → asserts entitled KS parsed from `result.ks`.
- `getPlaybackContext` mock (sanitized captured JSON shape) → asserts `index.mpd`
  URL + Widevine `la_url` + license token land in `StreamUrl.protection`.
- `resolve_stream` Kaltura live channel → asserts populated `StreamUrl` (url,
  `Basic` `auth_header`, `protection`).
- `resolve_stream` M3U-direct channel → still returns `StreamUrl::direct`
  (regression guard).
- Playback 401 → asserts `StreamError`, **not** `InvalidCredentials` (regression
  guard for the logout bug).
- Gate: `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`,
  `cargo fmt --all`. Orange tests must stay green.

## Out of scope (YAGNI)

- Android playback (desktop-first; Android reuses `resolve_stream` later via
  ExoPlayer).
- EPG.
- Porting the PFS WASM or any runtime browser bridge (only revisited if Phase 0
  is red — separate decision).

## Risk

Primary risk is Phase 0 returning RED (Basic cred is PFS-rotated), in which case
native playback is not achievable under the no-browser constraint and the work
stops at the spike with a documented finding. This is accepted and explicit.
