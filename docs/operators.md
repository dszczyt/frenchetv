# Operator API notes

Reverse-engineered details for each operator. Update this file alongside any
code change to an operator's auth, channel list, or stream resolution.

## Orange TV

- Phased auth (`uses_phased_auth() == true`): `/api/access` → `/api/login` →
  `/api/password` or app-push (AOM) polling. Session handle is the `wassup`
  cookie; `tv_token` is scraped from the homepage HTML and refreshed every
  ~25 min.
- Channel list: `pds/v1/live/ew` JSON (`{"channels":[…]}`).
- Streams are Widevine-protected DASH → the desktop UI runs a local DRM proxy
  (`crates/ui-desktop/src/drm/`) before handing the MPD to mpv.

## Bouygues Bbox / B.tv

Implemented in `crates/core/src/operator/bouygues.rs`. Flow reverse-engineered
from the community Kodi addon
[`plugin.video.bouyguestv`](https://github.com/FoxXav/plugin.video.bouyguestv)
(melmorabity, GPL-2.0) — the Bouygues analog of `plugin.video.orange.fr`.

**Credentials:** Bouygues requires **three** fields — last name, identifier,
password. The last name is surfaced via `OperatorKind::extra_credential_label()`
and passed through `Operator::set_extra_credential()` before `authenticate()`.

**Auth (phased — Keycloak CIAM + CAS + mandatory MFA OTP).** The 2020 addon's
direct `/cas/login` POST is dead: that URL now 302s to a SPA. The live flow
(observed 2026-05) is brokered through Keycloak and forces a one-time code:

1. `begin_auth`: `POST https://oauth2.bouyguestelecom.fr/authorize`
   (`client_id=a360.bouyguestelecom.fr`, `response_type=id_token token`,
   `redirect_uri=https://www.bouyguestelecom.fr/mon-compte/`) and **follow the
   redirect chain**: oauth2 → `ciam.bouyguestelecom.fr` realm `aegis`
   (`kc_idp_hint=picasso`, `authn_method=mfa-otp-bytel`) → back to
   `www.mon-compte.bouyguestelecom.fr/cas/login?service=…`, which serves the
   Apereo CAS form (`id=fm1`, posts to its own URL). Scrape its hidden inputs:
   single-use `execution`, `_eventId=submit`, `conversationId`, …
2. `complete_auth_password`: POST `username` + `password` + the scraped hidden
   inputs back to the form URL. Wrong password → **401** + re-rendered form
   (`InvalidCredentials`). Correct password → the MFA flow. `handle_form_response`
   then drives the Picasso webflow:
   - **Contact selection** (`<form id="contactSelectionForm">`): the page's JS
     reads `window.LOGIN_CONFIG.OtpMethod.{tel,email}` (masked, e.g.
     `06 ** ** ** 76`) into the hidden `maskedValue` and clicks the
     `_eventId_submit` button. We replicate that (prefer `tel`) — this triggers
     the SMS send and advances to the code form. → phase `Otp`.
   - **OTP entry** (`window.LOGIN_CONFIG.OtpInput`, `<input id="codeOtp"
     name="token" type="hidden">`): the code field is `token`.
3. `submit_otp`: echo the form's fields, set `token=<code>`, fire the
   `_eventId_submit` button → 302 chain → `redirect_uri#access_token=…&id_token=…`.

⚠️ reqwest **drops the URL fragment** while following redirects (Python's
`requests` keeps it). The client uses a redirect policy that *stops* at any hop
whose target starts with `redirect_uri`, then reads the raw `Location` header —
fragment intact. The chain to the login form never transits `redirect_uri`, so
it is followed fully; only the final token hop is halted.

The `id_token` is a JWT; we decode (no signature check) its `exp` and
`id_personne` claims. Session is persisted as `"access_token\nid_token"`;
`restore_session()` rejects an expired `id_token` (we can't silently refresh
without the password, which is never persisted, and OTP is interactive).

> **Auth status:** ✅ verified end-to-end against a live account (2026-05-31),
> including the SMS OTP. A trusted-device cookie (`MFATRUSTED`) is set, so repeat
> logins may skip the OTP.

### Channel list: implemented (Kaltura). Playback: blocked by the PFS WASM.

The old 2020 endpoints are dead (`list-chaines.json` / the get-url Lambda). The
current B.tv stack is a **Kaltura OTT** backend (partner `3199`); the channel
list is reachable, but live *playback* is gated by a **PFS security WASM module**.

**Channel list — works (`fetch_channels`):**
1. `POST api.bgp1.ott.kaltura.com/api_v3/service/ottUser/action/anonymousLogin`
   `{partnerId:3199}` → `result.ks` (an **anonymous** Kaltura session — no user
   credentials needed; the lineup is not behind the PFS credential).
2. `GET cache.bgp1.ott.kaltura.com/api_v3/service/lineup/action/get/partnerid/3199`
   with `Authorization: Bearer <ks>` → `result.objects[]` (~444 channels). Each:
   `id` (Kaltura asset id), `description` (name), `externalId`, `lcn` (channel
   number), `images[]` (logos). Falls back to the static M3U on any failure.

**Playback — NOT implemented (blocked):**
- `POST api.bgp1.ott.kaltura.com/api_v3/service/asset/action/getPlaybackContext`
  `{assetId:<lineup id>, assetType:"media", contextDataParams:{context:"PLAYBACK",
  adapterData:{playbackType:"LIVE"}}, ks:<KS>}` → `result.sources[].url =
  https://bt-api-int.bouyguestelecom.fr/api/sessions/v1/bpk-tv/<key>/dash_cenc/index.mpd`
  plus `drm[]` (Widevine/PlayReady CENC + license JWT); license POSTed to
  `https://bt-api-int.bouyguestelecom.fr/api/licenses/v1/widevine`. Widevine DRM
  DASH → would reuse the Orange proxy (`crates/ui-desktop/src/drm/`).
- BUT `getPlaybackContext` needs a **user-entitled** KS, and the
  `bt-api-int` gateway uses `Authorization: Basic base64(<32-char id>:<16-char
  secret>)`. Neither the entitled KS nor that Basic credential is produced by any
  documented HTTP call — the web app's `/tv-direct/wasm/wasm_comm_module.wasm` (a
  **4.3 MB C++/OpenSSL "PFS" security module**: exports `pfsproxy_getTokenState`,
  `pfsproxy_resetToken`; talks to `iptv.pfs.bouyguesbox.fr`) derives them
  client-side. Porting it is impractical and fragile (anti-piracy; changes on
  their updates). So a native lightweight client can list channels but cannot
  resolve a playable live stream. Revisiting playback needs a runtime browser/WASM
  bridge.

> **How this was mapped:** a Playwright-driven instrumented Chrome (system
> `google-chrome-stable`, headed) captured the authenticated session's XHR/fetch
> traffic while a real user completed login + OTP. No credentials or tokens are
> stored in the repo. Unit tests exercise the auth flow against `wiremock` mocks.
