# Bouygues Native Live Playback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a click on a Bouygues channel resolve and play the real B.tv live stream natively via libmpv + the existing Widevine proxy — no browser in the shipped client.

**Architecture:** `resolve_stream` in `crates/core/src/operator/bouygues.rs` is rewritten to (1) exchange the stored Keycloak tokens for an *entitled* Kaltura session (KS), (2) call Kaltura `getPlaybackContext` to obtain the `dash_cenc/index.mpd` URL + Widevine license info, and (3) return a populated `StreamUrl` (manifest URL + `Basic` auth header + `ProtectionData`) that the existing desktop `drm/` Widevine proxy and mpv already know how to play (identical to the Orange path). The whole build is gated by a one-time reverse-engineering spike (Phase 0) that determines whether the `bt-api-int` `Basic` credential is a static app-level value (native feasible) or PFS-rotated per session (native blocked).

**Tech Stack:** Rust (MSRV 1.75), `reqwest`, `serde_json`, `async-trait`, `wiremock` (tests), Kaltura OTT API, Widevine CENC DASH. Spike uses Node + Playwright (system Chrome), not shipped.

**Spec:** `docs/superpowers/specs/2026-05-31-bouygues-native-playback-design.md`

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `tools/spike/bouygues-pfs-probe.mjs` | Create (not shipped) | Drive a logged-in B.tv session, capture entitled-KS login + getPlaybackContext + the `Basic` header; run the static-vs-rotated test. |
| `docs/operators.md` | Modify | Append spike findings: exact request shapes + static/rotated verdict + capture date. |
| `crates/core/src/operator/bouygues.rs` | Modify | New `entitled_login_url` / `playback_context_url` fields + builder; `kaltura_entitled_ks()`, `kaltura_playback_context()`; rewritten `resolve_stream`; new unit tests. |

All runtime code changes are confined to `bouygues.rs`. `crates/core/src/stream/mod.rs` and `crates/ui-desktop/src/drm/` are **read and reused, never modified** — the Orange path stays byte-for-byte identical.

---

## PHASE 0 — Reverse-engineering spike (GATE)

> This phase is an interactive investigation, not TDD. It requires a real logged-in
> B.tv account and an OTP. It produces a written verdict that gates Phase 1.
> **Do not start Phase 1 until Step 0.5 records a GREEN verdict.**

### Task 0: Determine if the `bt-api-int` Basic credential is static or PFS-rotated

**Files:**
- Create: `tools/spike/bouygues-pfs-probe.mjs`
- Modify: `docs/operators.md`

- [ ] **Step 0.1: Create the capture helper script**

Create `tools/spike/bouygues-pfs-probe.mjs`. It opens system Chrome via Playwright,
lets the human log in + complete OTP, then records every XHR/fetch to Kaltura and
`bt-api-int`, and writes them to `tools/spike/out/capture.json` (gitignored).

```javascript
// tools/spike/bouygues-pfs-probe.mjs
// One-time RE helper. NOT shipped. Requires: npm i -g playwright && npx playwright install chrome
import { chromium } from 'playwright';
import { writeFileSync, mkdirSync } from 'node:fs';

const OUT = new URL('./out/', import.meta.url);
mkdirSync(OUT, { recursive: true });

const browser = await chromium.launch({ channel: 'chrome', headless: false });
const page = await browser.newPage();
const captured = [];

page.on('requestfinished', async (req) => {
  const url = req.url();
  if (!/kaltura\.com|bt-api-int\.bouyguestelecom\.fr/.test(url)) return;
  const res = await req.response();
  let body = null;
  try { body = await res.text(); } catch {}
  captured.push({
    url,
    method: req.method(),
    reqHeaders: req.headers(),     // includes Authorization: Basic … on bt-api-int
    reqBody: req.postData(),
    status: res.status(),
    resBody: body && body.length < 200_000 ? body : `<${body?.length} bytes>`,
  });
  writeFileSync(new URL('capture.json', OUT), JSON.stringify(captured, null, 2));
});

console.log('Log in, complete OTP, then START PLAYBACK on one channel. Ctrl+C when done.');
await page.goto('https://www.bouyguestelecom.fr/tv-direct');
await new Promise(() => {}); // keep open until Ctrl+C
```

- [ ] **Step 0.2: Add spike output to gitignore**

Append to `.gitignore`:

```
/tools/spike/out/
```

Run: `git check-ignore tools/spike/out/capture.json`
Expected: prints the path (confirms it is ignored).

- [ ] **Step 0.3: Run the capture (interactive)**

Run:
```bash
cd tools/spike && node bouygues-pfs-probe.mjs
```
In the opened Chrome: log in, complete OTP, start playback on **one** channel, then Ctrl+C.

From `tools/spike/out/capture.json`, extract and note (locally, do NOT commit secrets):
1. The **entitled-KS** call: the Kaltura request that returns a `result.ks` *after* login (URL, method, body shape). This is the `ottUser/login` (or refresh) call.
2. The **getPlaybackContext** call: URL, request body (assetId + params), and `result.sources[]` shape — specifically the `dash_cenc` source `url` and its `drm[]` (Widevine `scheme`, `licenseURL`/`licenseServerURL`, license token).
3. The **`bt-api-int` `Authorization: Basic`** header value used to fetch `index.mpd`.

- [ ] **Step 0.4: The decisive static-vs-rotated test**

Using the captured `Basic` header from Step 0.3, run a fresh native fetch — a fresh
anonymous-or-entitled KS, the same `index.mpd` URL, the captured `Basic` header:

```bash
# Substitute the captured values. Run TWICE: now, and again after several hours / next day.
curl -s -o /dev/null -w "%{http_code}\n" \
  -H "Authorization: Basic <CAPTURED_BASIC>" \
  -H "User-Agent: Mozilla/5.0" \
  "<CAPTURED_INDEX_MPD_URL>"
```

Interpretation:
- **`200` on both runs (now + later)** → the `Basic` credential is **STATIC** → **GREEN. Proceed to Phase 1.**
- **`401`/`403`** (especially when re-run later) → **PFS-ROTATED** → **RED. STOP.** Native playback is not achievable under the no-browser constraint. Record the finding and end this plan here; porting the PFS WASM or a runtime browser bridge is a separate future decision.

- [ ] **Step 0.5: Record the verdict in docs/operators.md and commit**

Append a dated subsection to `docs/operators.md` under the Bouygues section with:
- The exact entitled-KS request (URL, method, body — **no tokens/credentials**).
- The exact `getPlaybackContext` request + sanitized `result.sources[]`/`drm[]` shape.
- The verdict: **STATIC** or **PFS-ROTATED**, with the capture date.
- If STATIC: note that the `Basic` value lives as a constant in `bouygues.rs` (app-level credential, not a user secret).

```bash
git add docs/operators.md tools/spike/bouygues-pfs-probe.mjs .gitignore
git commit -m "docs(bouygues): PFS spike findings — Basic cred is <STATIC|ROTATED>"
```

> **GATE:** If RED, stop. All tasks below assume GREEN and assume Step 0.3 produced
> the concrete values referenced as `<SPIKE: …>` placeholders. Substitute those
> captured constants/shapes where marked.

---

## PHASE 1 — Native resolve_stream (only if Phase 0 GREEN)

### Task 1: Add configurable playback endpoints + the static Basic credential constant

**Files:**
- Modify: `crates/core/src/operator/bouygues.rs` (struct fields, `new`, `new_with_urls`, new builder, new const)

- [ ] **Step 1.1: Add the static Basic credential constant**

Near the other consts (after `KALTURA_PARTNER_ID`, around line 28), add — value from spike Step 0.3:

```rust
/// `bt-api-int` gateway credential, `Basic base64(<32-char id>:<16-char secret>)`.
/// Determined to be a STATIC app-level credential (not a per-user secret) by the
/// 2026-05-31 PFS spike — see docs/operators.md. If Bouygues ever rotates it, the
/// manifest fetch returns 401, surfaced as a StreamError (never a logout).
const BT_API_BASIC: &str = "Basic <SPIKE: captured 32:16 base64 value>";

/// Kaltura `ottUser/login` — exchanges the Keycloak tokens for an *entitled* KS.
const KALTURA_ENTITLED_LOGIN_URL: &str =
    "https://api.bgp1.ott.kaltura.com/api_v3/service/ottUser/action/login";
/// Kaltura `asset/getPlaybackContext` — yields the playable manifest + DRM info.
const KALTURA_PLAYBACK_CONTEXT_URL: &str =
    "https://api.bgp1.ott.kaltura.com/api_v3/service/asset/action/getPlaybackContext";
```

- [ ] **Step 1.2: Add two struct fields**

In `pub struct BouyguesOperator` (after `channel_list_url`, line 60), add:

```rust
    /// Kaltura `ottUser/login` — exchanges Keycloak tokens for an entitled KS.
    entitled_login_url: String,
    /// Kaltura `asset/getPlaybackContext` — playable manifest + Widevine DRM.
    playback_context_url: String,
```

- [ ] **Step 1.3: Initialise the fields in `new_with_urls`**

In `new_with_urls`, in the struct literal (after `channel_list_url: channel_list_url.to_string(),`, line 123), add the real defaults:

```rust
            entitled_login_url: KALTURA_ENTITLED_LOGIN_URL.to_string(),
            playback_context_url: KALTURA_PLAYBACK_CONTEXT_URL.to_string(),
```

This keeps every existing 4-arg `new_with_urls` call site (e.g. `op_for`) compiling unchanged.

- [ ] **Step 1.4: Add a test-only builder to point the new endpoints at a mock**

Immediately after the `new_with_urls` function (around line 132), add:

```rust
    /// Override the playback endpoints (test-only; production uses the consts).
    #[cfg(test)]
    fn with_playback_urls(mut self, entitled_login_url: &str, playback_context_url: &str) -> Self {
        self.entitled_login_url = entitled_login_url.to_string();
        self.playback_context_url = playback_context_url.to_string();
        self
    }
```

- [ ] **Step 1.5: Verify it compiles**

Run: `cargo build -p frenchetv-core`
Expected: builds (warnings about unused `with_playback_urls`/new fields are fine until Task 2/3 use them).

- [ ] **Step 1.6: Commit**

```bash
git add crates/core/src/operator/bouygues.rs
git commit -m "feat(bouygues): add entitled-KS + playback-context endpoints and static bt-api-int cred"
```

---

### Task 2: `kaltura_entitled_ks()` — exchange Keycloak tokens for an entitled KS

**Files:**
- Modify: `crates/core/src/operator/bouygues.rs` (new method + test)

- [ ] **Step 2.1: Write the failing test**

Add to the `tests` module (after `op_for`, near line 825). The request body / token field
name marked `<SPIKE>` come from Step 0.3; the response shape is standard Kaltura.

```rust
    #[tokio::test]
    async fn test_entitled_ks_parsed_from_login() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/entitled-login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "objectType": "KalturaLoginSession", "ks": "entitled_ks_123" }
            })))
            .mount(&mock)
            .await;

        let mut op = op_for(&mock).with_playback_urls(
            &format!("{}/entitled-login", mock.uri()),
            &format!("{}/playback", mock.uri()),
        );
        op.set_tokens("acc_tok".into(), "id_tok".into());

        let ks = op.kaltura_entitled_ks().await.expect("entitled ks");
        assert_eq!(ks, "entitled_ks_123");
    }
```

- [ ] **Step 2.2: Run it to verify it fails**

Run: `cargo test -p frenchetv-core test_entitled_ks_parsed_from_login`
Expected: FAIL — `no method named kaltura_entitled_ks`.

- [ ] **Step 2.3: Implement `kaltura_entitled_ks`**

Add as a method on `impl BouyguesOperator` (right after `kaltura_anonymous_ks`, line 382).
The body shape (how the Keycloak token is passed) is the `<SPIKE>` value from Step 0.3 —
the example below passes the `id_token` as an external token, the most common Kaltura pattern:

```rust
    /// Exchange the stored Keycloak tokens for an *entitled* Kaltura KS (the one
    /// that unlocks `getPlaybackContext`). Requires a prior successful `authenticate`
    /// (or `restore_session`) so `id_token`/`access_token` are present.
    async fn kaltura_entitled_ks(&self) -> Result<String> {
        let id_token = self.id_token.as_deref().ok_or(OperatorError::InvalidCredentials)?;
        let resp = self
            .client
            .post(&self.entitled_login_url)
            .header("Accept", "application/json")
            // <SPIKE: exact body from capture>. Common Kaltura external-IDP shape:
            .json(&json!({
                "partnerId": KALTURA_PARTNER_ID,
                "apiVersion": "8.7.5",
                "extraParams": { "token": { "value": id_token } }
            }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        let status = resp.status();
        let v: serde_json::Value = resp.json().await.map_err(|_| {
            OperatorError::UnexpectedResponse { status: status.as_u16(), body: "entitled login: bad JSON".into() }
        })?;
        v.pointer("/result/ks")
            .or_else(|| v.pointer("/result/loginSession/ks"))
            .and_then(|k| k.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or(OperatorError::InvalidCredentials)
    }
```

- [ ] **Step 2.4: Run the test to verify it passes**

Run: `cargo test -p frenchetv-core test_entitled_ks_parsed_from_login`
Expected: PASS.

- [ ] **Step 2.5: Commit**

```bash
git add crates/core/src/operator/bouygues.rs
git commit -m "feat(bouygues): exchange Keycloak tokens for an entitled Kaltura KS"
```

---

### Task 3: `kaltura_playback_context()` — manifest URL + Widevine DRM info

**Files:**
- Modify: `crates/core/src/operator/bouygues.rs` (new struct + method + test)

- [ ] **Step 3.1: Write the failing test**

Add to the `tests` module. The `result.sources[]`/`drm[]` field names below are the
standard Kaltura OTT shape; replace any that differ per spike Step 0.3.

```rust
    #[tokio::test]
    async fn test_playback_context_extracts_mpd_and_drm() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/playback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "sources": [{
                    "type": "DASH",
                    "format": "dash_cenc",
                    "url": "https://bt-api-int.bouyguestelecom.fr/api/sessions/v1/bpk-tv/KEY/dash_cenc/index.mpd",
                    "drm": [{
                        "scheme": "WIDEVINE_CENC",
                        "licenseURL": "https://bt-api-int.bouyguestelecom.fr/api/licenses/v1/widevine?token=LICJWT"
                    }]
                }]}
            })))
            .mount(&mock)
            .await;

        let op = op_for(&mock).with_playback_urls(
            &format!("{}/entitled-login", mock.uri()),
            &format!("{}/playback", mock.uri()),
        );
        let ctx = op.kaltura_playback_context("entitled_ks_123", "555").await.expect("ctx");
        assert_eq!(
            ctx.mpd_url,
            "https://bt-api-int.bouyguestelecom.fr/api/sessions/v1/bpk-tv/KEY/dash_cenc/index.mpd"
        );
        assert_eq!(
            ctx.license_url,
            "https://bt-api-int.bouyguestelecom.fr/api/licenses/v1/widevine?token=LICJWT"
        );
    }
```

- [ ] **Step 3.2: Run it to verify it fails**

Run: `cargo test -p frenchetv-core test_playback_context_extracts_mpd_and_drm`
Expected: FAIL — `PlaybackContext` / `kaltura_playback_context` not found.

- [ ] **Step 3.3: Add the `PlaybackContext` struct**

Add above `impl BouyguesOperator` (near line 80):

```rust
/// Parsed result of Kaltura `getPlaybackContext`: the playable DASH-CENC manifest
/// plus the Widevine license server URL.
struct PlaybackContext {
    mpd_url: String,
    license_url: String,
}
```

- [ ] **Step 3.4: Implement `kaltura_playback_context`**

Add as a method on `impl BouyguesOperator` (after `kaltura_entitled_ks`):

```rust
    /// Call Kaltura `getPlaybackContext` for a live asset and pick the DASH-CENC
    /// source + its Widevine license URL.
    async fn kaltura_playback_context(&self, ks: &str, asset_id: &str) -> Result<PlaybackContext> {
        let resp = self
            .client
            .post(&self.playback_context_url)
            .header("Accept", "application/json")
            .json(&json!({
                "ks": ks,
                "assetId": asset_id,
                "assetType": "media",
                "contextDataParams": {
                    "objectType": "KalturaPlaybackContextOptions",
                    "context": "PLAYBACK",
                    "adapterData": { "playbackType": "LIVE" }
                }
            }))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let v: serde_json::Value = resp.json().await.map_err(|_| {
            OperatorError::UnexpectedResponse { status, body: "playbackContext: bad JSON".into() }
        })?;

        let sources = v
            .pointer("/result/sources")
            .and_then(|s| s.as_array())
            .ok_or_else(|| OperatorError::UnexpectedResponse {
                status,
                body: "playbackContext: no sources".into(),
            })?;

        // Prefer a dash_cenc / DASH source that carries a Widevine drm entry.
        let pick = sources.iter().find(|s| {
            let is_dash = s.get("format").and_then(|f| f.as_str()).map(|f| f.contains("dash")).unwrap_or(false)
                || s.get("type").and_then(|t| t.as_str()).map(|t| t.eq_ignore_ascii_case("DASH")).unwrap_or(false);
            let has_wv = s.get("drm").and_then(|d| d.as_array()).map(|arr| {
                arr.iter().any(|e| {
                    e.get("scheme").and_then(|x| x.as_str()).map(|x| x.to_ascii_uppercase().contains("WIDEVINE")).unwrap_or(false)
                })
            }).unwrap_or(false);
            is_dash && has_wv
        }).or_else(|| sources.first());

        let src = pick.ok_or_else(|| OperatorError::UnexpectedResponse {
            status,
            body: "playbackContext: no usable source".into(),
        })?;

        let mpd_url = src.get("url").and_then(|u| u.as_str())
            .ok_or_else(|| OperatorError::UnexpectedResponse { status, body: "playbackContext: source has no url".into() })?
            .to_string();

        let license_url = src.get("drm").and_then(|d| d.as_array())
            .and_then(|arr| arr.iter().find_map(|e| {
                e.get("licenseURL").or_else(|| e.get("licenseServerURL")).and_then(|x| x.as_str())
            }))
            .ok_or_else(|| OperatorError::UnexpectedResponse { status, body: "playbackContext: no widevine licenseURL".into() })?
            .to_string();

        Ok(PlaybackContext { mpd_url, license_url })
    }
```

- [ ] **Step 3.5: Run the test to verify it passes**

Run: `cargo test -p frenchetv-core test_playback_context_extracts_mpd_and_drm`
Expected: PASS.

- [ ] **Step 3.6: Commit**

```bash
git add crates/core/src/operator/bouygues.rs
git commit -m "feat(bouygues): parse getPlaybackContext into manifest + widevine license"
```

---

### Task 4: Rewrite `resolve_stream` to return a playable StreamUrl

**Files:**
- Modify: `crates/core/src/operator/bouygues.rs` (`resolve_stream` body + import + tests)

- [ ] **Step 4.1: Add the ProtectionData import**

Change the import at line 9 from:

```rust
use crate::stream::StreamUrl;
```
to:
```rust
use crate::stream::{ProtectionData, StreamUrl};
```

- [ ] **Step 4.2: Write the failing test (live channel → populated StreamUrl)**

Add to the `tests` module:

```rust
    #[tokio::test]
    async fn test_resolve_stream_live_returns_playable_url() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/entitled-login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "ks": "entitled_ks_123" }
            })))
            .mount(&mock).await;
        Mock::given(method("POST"))
            .and(path("/playback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "sources": [{
                    "type": "DASH", "format": "dash_cenc",
                    "url": "https://bt-api-int.bouyguestelecom.fr/api/sessions/v1/bpk-tv/K/dash_cenc/index.mpd",
                    "drm": [{ "scheme": "WIDEVINE_CENC",
                              "licenseURL": "https://bt-api-int.bouyguestelecom.fr/api/licenses/v1/widevine?token=LIC" }]
                }]}
            })))
            .mount(&mock).await;

        let mut op = op_for(&mock).with_playback_urls(
            &format!("{}/entitled-login", mock.uri()),
            &format!("{}/playback", mock.uri()),
        );
        op.set_tokens("acc".into(), "id".into());

        let channel = Channel {
            id: "555".into(), name: "TF1".into(), logo_url: None, number: Some(1),
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Direct(url::Url::parse(PLACEHOLDER_URL).unwrap()),
            locked: false,
        };
        let stream = op.resolve_stream(&channel).await.expect("stream");
        assert_eq!(stream.url.as_str(),
            "https://bt-api-int.bouyguestelecom.fr/api/sessions/v1/bpk-tv/K/dash_cenc/index.mpd");
        assert!(stream.auth_header.as_deref().unwrap().starts_with("Basic "));
        let prot = stream.protection.expect("protection");
        assert_eq!(prot.la_url, "https://bt-api-int.bouyguestelecom.fr/api/licenses/v1/widevine?token=LIC");
    }
```

- [ ] **Step 4.3: Write the failing test (manifest 401 → StreamError, not logout)**

This is the regression guard for the session-expired-logout bug fixed 2026-05-31.

```rust
    #[tokio::test]
    async fn test_resolve_stream_playback_401_is_stream_error_not_logout() {
        let mock = MockServer::start().await;
        Mock::given(method("POST")).and(path("/entitled-login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "result": { "ks": "k" } })))
            .mount(&mock).await;
        Mock::given(method("POST")).and(path("/playback"))
            .respond_with(ResponseTemplate::new(401).set_body_string("denied"))
            .mount(&mock).await;

        let mut op = op_for(&mock).with_playback_urls(
            &format!("{}/entitled-login", mock.uri()),
            &format!("{}/playback", mock.uri()),
        );
        op.set_tokens("acc".into(), "id".into());
        let channel = Channel {
            id: "555".into(), name: "TF1".into(), logo_url: None, number: Some(1),
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Direct(url::Url::parse(PLACEHOLDER_URL).unwrap()),
            locked: false,
        };
        let err = op.resolve_stream(&channel).await.unwrap_err();
        assert!(!matches!(err, OperatorError::InvalidCredentials));
    }
```

- [ ] **Step 4.4: Update the existing `test_resolve_stream_live_unsupported_keeps_session` test**

That test (line 1035) asserted the live path errors. The live path now *succeeds* given a
session, but with **no tokens set** it must still error WITHOUT `InvalidCredentials`. Replace
its body's final assertions — it already calls `BouyguesOperator::new()` (no tokens), so
`kaltura_entitled_ks` returns `InvalidCredentials`... which WOULD trip the guard. To keep the
"no logout on live failure" contract, `resolve_stream` must map a *missing-session* entitled-KS
failure to a `StreamError`, not propagate `InvalidCredentials`. Keep the test as-is (it asserts
`!InvalidCredentials`); Step 4.5's implementation satisfies it by converting the entitled-KS
error into `UnexpectedResponse` inside `resolve_stream`.

- [ ] **Step 4.5: Run the new tests to verify they fail**

Run: `cargo test -p frenchetv-core resolve_stream`
Expected: the two new tests FAIL (old `resolve_stream` returns hard 501), and `test_resolve_stream_live_unsupported_keeps_session` still passes.

- [ ] **Step 4.6: Rewrite `resolve_stream`**

Replace the whole `resolve_stream` body (lines 728-747) with:

```rust
    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl> {
        // Channels from the fallback M3U carry their real URL directly.
        if let StreamTemplate::Direct(url) = &channel.stream_template {
            if url.as_str() != PLACEHOLDER_URL {
                return Ok(StreamUrl::direct(url.clone()));
            }
        }

        // Live Kaltura channel: entitled KS -> getPlaybackContext -> playable MPD.
        // Any failure here is a STREAM error, never InvalidCredentials — a live
        // resolution failure must not log the user out (the M3U fallback channels
        // and the rest of the session stay valid).
        let to_stream_err = |status: u16, msg: &str| OperatorError::UnexpectedResponse {
            status,
            body: format!("Lecture Bouygues indisponible: {msg}"),
        };

        let ks = self
            .kaltura_entitled_ks()
            .await
            .map_err(|_| to_stream_err(403, "session TV non autorisée"))?;
        let ctx = self
            .kaltura_playback_context(&ks, &channel.id)
            .await
            .map_err(|e| to_stream_err(502, &e.to_string()))?;

        let url = url::Url::parse(&ctx.mpd_url)
            .map_err(|_| to_stream_err(502, "URL de manifeste invalide"))?;

        // The bt-api-int CDN and the Widevine license endpoint both require the
        // static Basic credential plus browser-like Origin/Referer/UA headers.
        let mut stream = StreamUrl::direct(url);
        stream.auth_header = Some(BT_API_BASIC.to_string());
        stream.headers = vec![
            ("Origin".into(), "https://www.bouyguestelecom.fr".into()),
            ("Referer".into(), "https://www.bouyguestelecom.fr/".into()),
            ("User-Agent".into(), USER_AGENT.into()),
        ];
        stream.protection = Some(ProtectionData {
            la_url: ctx.license_url,
            pssh: None, // proxy extracts PSSH from the MPD (same as Orange)
            license_headers: vec![
                ("Authorization".into(), BT_API_BASIC.to_string()),
                ("Origin".into(), "https://www.bouyguestelecom.fr".into()),
                ("Referer".into(), "https://www.bouyguestelecom.fr/".into()),
                ("User-Agent".into(), USER_AGENT.into()),
            ],
        });
        Ok(stream)
    }
```

- [ ] **Step 4.7: Run all bouygues tests to verify they pass**

Run: `cargo test -p frenchetv-core operator::bouygues`
Expected: PASS — including both new tests, the updated keeps-session test, and the existing M3U-direct test.

- [ ] **Step 4.8: Commit**

```bash
git add crates/core/src/operator/bouygues.rs
git commit -m "feat(bouygues): resolve live streams natively (entitled KS + DASH-CENC + Widevine)"
```

---

### Task 5: Update the doc comment + remove the stale "blocked" note

**Files:**
- Modify: `crates/core/src/operator/bouygues.rs` (struct doc comment, lines 44-48)
- Modify: `docs/operators.md` (Bouygues playback section)

- [ ] **Step 5.1: Update the struct doc comment**

Replace the `NOTE:` paragraph (lines 44-48) with:

```rust
/// NOTE: auth, the channel list, AND live playback all work. `fetch_channels`
/// reads the Kaltura lineup via an anonymous KS. `resolve_stream` exchanges the
/// Keycloak tokens for an entitled KS, calls `getPlaybackContext`, and returns a
/// Widevine DASH-CENC `StreamUrl` the desktop DRM proxy plays. The `bt-api-int`
/// `Basic` credential is a static app-level value (PFS spike 2026-05-31); see
/// `docs/operators.md`.
```

- [ ] **Step 5.2: Flip the docs/operators.md playback section to "implemented"**

In `docs/operators.md`, change the heading at line 68 and the "NOT implemented (blocked)" block
to reflect that playback is implemented via the entitled-KS + getPlaybackContext + static Basic
path, keeping the PFS history as background.

- [ ] **Step 5.3: Commit**

```bash
git add crates/core/src/operator/bouygues.rs docs/operators.md
git commit -m "docs(bouygues): playback now implemented (entitled KS + static bt-api-int cred)"
```

---

## PHASE 2 — Verification

### Task 6: Full workspace gate

**Files:** none (verification only)

- [ ] **Step 6.1: Run the whole core test suite**

Run: `cargo test -p frenchetv-core`
Expected: PASS — all Bouygues tests + all Orange tests green (Orange untouched).

- [ ] **Step 6.2: Clippy + fmt (CI gate)**

Run:
```bash
cargo fmt --all
cargo clippy --workspace -- -D warnings
```
Expected: no warnings, no diff after fmt.

- [ ] **Step 6.3: Build the desktop app**

Run: `cargo build -p ui-desktop`
Expected: builds (confirms the reused `StreamUrl`/`ProtectionData`/`drm` proxy contract is intact).

- [ ] **Step 6.4: Manual smoke test (real account, desktop)**

Run: `cargo run -p ui-desktop`
- Log in to Bouygues, complete OTP.
- Focus, then click a major channel (e.g. TF1).
- Expected: the Widevine proxy starts and mpv plays the live stream within ~30s. On
  failure, an auto-retrying stream overlay appears — **not** a logout to Setup.

- [ ] **Step 6.5: Commit any fmt-only changes**

```bash
git add -A
git commit -m "style: cargo fmt" || echo "nothing to format"
```

---

## Self-Review Notes

- **Spec coverage:** Phase 0 spike (gate) → Task 0. Entitled KS → Task 2. getPlaybackContext → Task 3. `resolve_stream` populated StreamUrl + M3U-direct preserved → Task 4. Error handling (401 → StreamError not logout) → Task 4 Steps 4.3/4.6. Tests (login, playback-context, resolve, M3U regression, 401 regression) → Tasks 2-4. Orange-untouched → Task 6 Step 6.1. Docs → Tasks 0.5 + 5.
- **Spike-dependent substitutions (unavoidable for RE):** `BT_API_BASIC` value (Step 1.1), the entitled-login request body shape (Step 2.3), and any differing `sources[]`/`drm[]` field names (Step 3.x) are marked `<SPIKE>` and come from Step 0.3 capture. These are not lazy placeholders — they are values that physically cannot be known before the capture, and the spike produces concrete values to drop in.
- **Type consistency:** `PlaybackContext { mpd_url, license_url }` defined in Task 3, used in Task 4. `kaltura_entitled_ks() -> Result<String>` (Task 2) feeds `kaltura_playback_context(&self, ks, asset_id)` (Task 3). `with_playback_urls` defined in Task 1.4, used in Tasks 2-4. `BT_API_BASIC`/`KALTURA_ENTITLED_LOGIN_URL`/`KALTURA_PLAYBACK_CONTEXT_URL` defined Task 1.1.
- **YAGNI:** no entitled-KS caching (recomputed per resolve — `resolve_stream` is `&self`, avoids interior mutability), no Android, no EPG.
