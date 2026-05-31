use async_trait::async_trait;
use serde_json::json;

use super::traits::{AuthPhase, Operator, Result};
use crate::channel::m3u::parse_m3u;
use crate::channel::{Channel, ChannelCategory, StreamTemplate};
use crate::epg::EpgData;
use crate::error::OperatorError;
use crate::stream::StreamUrl;

const FALLBACK_M3U: &str = include_str!("../../../../assets/channels/bouygues.m3u");

/// Placeholder used for channels resolved through the live stream API.
/// (Channels coming from the fallback M3U carry their real URL instead.)
const PLACEHOLDER_URL: &str = "https://placeholder.invalid/";

/// OAuth2 client parameters for the Bouygues "a360" web portal. The portal still
/// issues `id_token token` (implicit) to `redirect_uri#…`, but auth is now
/// brokered through a Keycloak CIAM realm (`ciam.bouyguestelecom.fr`) that
/// front-ends an Apereo CAS login form with mandatory MFA OTP (`mfa-otp-bytel`).
const OAUTH2_CLIENT_ID: &str = "a360.bouyguestelecom.fr";
const OAUTH2_RESPONSE_TYPE: &str = "id_token token";
const OAUTH2_REDIRECT_URI: &str = "https://www.bouyguestelecom.fr/mon-compte/";

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:84.0) Gecko/20100101 Firefox/84.0";

/// Kaltura OTT partner id for Bouygues B.tv (from the web app config).
const KALTURA_PARTNER_ID: u32 = 3199;

/// Bouygues Telecom B.tv (PCTV) operator. **Phased** auth — the live flow as of
/// 2026 is a Keycloak-brokered CAS login with a one-time-code second factor:
///
/// 1. `begin_auth` — `POST {oauth2_url}` and follow the redirect chain
///    (oauth2 → Keycloak realm `aegis` → CAS) to the login form; scrape its
///    single-use `execution` token and other hidden inputs.
/// 2. `complete_auth_password` — POST username + password back to the form. On
///    success Bouygues returns an MFA form → phase `Otp`; if no MFA, it redirects
///    straight to `redirect_uri#access_token=…&id_token=…` → phase `Done`.
/// 3. `submit_otp` — POST the user's one-time code; redirects to the tokens.
///
/// The redirect to `redirect_uri` is halted (see `client`) so the token-bearing
/// fragment survives.
///
/// NOTE: auth + the channel list work. `fetch_channels` reads the Kaltura lineup
/// via an anonymous Kaltura session (no PFS needed). Live *playback* is NOT
/// implemented: it needs an entitled session + a `bt-api-int` Basic credential
/// minted by a PFS security WASM module that a native client can't reproduce.
/// See `docs/operators.md` for the full reverse-engineered map.
pub struct BouyguesOperator {
    /// Follows redirects, EXCEPT it stops at any redirect whose target starts
    /// with `redirect_uri` — that final hop carries the OAuth2 tokens in its URL
    /// fragment, which reqwest would otherwise strip while following. Stopping
    /// lets us read the raw `Location` header instead.
    client: reqwest::Client,
    oauth2_url: String,
    /// Kaltura `ottUser/anonymousLogin` — yields a KS that can read the lineup
    /// (the channel list is not gated by the PFS credential, only playback is).
    kaltura_login_url: String,
    /// Kaltura `lineup/get` — the channel referentiel.
    channel_list_url: String,
    redirect_uri: String,
    /// Account holder's last name — kept in case the live CAS form asks for it.
    lastname: Option<String>,
    /// Username captured in `begin_auth`, submitted with the password.
    username: Option<String>,
    /// URL to POST the next webflow form to (CAS posts forms to their own URL).
    pending_form_url: Option<String>,
    /// Hidden inputs scraped from the current form (execution, conversationId, …)
    /// to echo back, minus the username/password fields we fill ourselves.
    pending_hidden: Vec<(String, String)>,
    /// Name of the OTP code input on the MFA form, once detected.
    otp_field: Option<String>,
    pub(crate) access_token: Option<String>,
    pub(crate) id_token: Option<String>,
    /// Persisted session handle: "access_token\nid_token". Kept as an owned
    /// String so `session_token()` can hand back a `&str`.
    session_blob: Option<String>,
}

impl BouyguesOperator {
    pub fn new() -> Self {
        Self::new_with_urls(
            "https://oauth2.bouyguestelecom.fr/authorize",
            "https://api.bgp1.ott.kaltura.com/api_v3/service/ottUser/action/anonymousLogin",
            "https://cache.bgp1.ott.kaltura.com/api_v3/service/lineup/action/get/partnerid/3199",
            OAUTH2_REDIRECT_URI,
        )
    }

    /// Construct with custom endpoint URLs — used in tests.
    pub fn new_with_urls(
        oauth2_url: &str,
        kaltura_login_url: &str,
        channel_list_url: &str,
        redirect_uri: &str,
    ) -> Self {
        // Stop following redirects at the token-bearing hop to `redirect_uri` so
        // its fragment survives; follow everything else (the CAS/Keycloak chain).
        let redirect_match = redirect_uri.trim_end_matches('/').to_string();
        Self {
            client: reqwest::Client::builder()
                .cookie_store(true)
                .user_agent(USER_AGENT)
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt
                        .url()
                        .as_str()
                        .trim_end_matches('/')
                        .starts_with(&redirect_match)
                    {
                        attempt.stop()
                    } else if attempt.previous().len() >= 20 {
                        attempt.error("too many redirects")
                    } else {
                        attempt.follow()
                    }
                }))
                .build()
                .expect("reqwest client build"),
            oauth2_url: oauth2_url.to_string(),
            kaltura_login_url: kaltura_login_url.to_string(),
            channel_list_url: channel_list_url.to_string(),
            redirect_uri: redirect_uri.to_string(),
            lastname: None,
            username: None,
            pending_form_url: None,
            pending_hidden: Vec::new(),
            otp_field: None,
            access_token: None,
            id_token: None,
            session_blob: None,
        }
    }

    /// Scrape `<input type="hidden" name="…" value="…">` pairs from the CAS login
    /// HTML. CAS embeds a single-use `execution` token that must be echoed back.
    fn parse_hidden_inputs(html: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = html;
        while let Some(pos) = rest.find("<input") {
            rest = &rest[pos..];
            let end = rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            let tag = &rest[..end];
            rest = &rest[end..];

            if !tag.contains("type=\"hidden\"") {
                continue;
            }
            if let (Some(name), Some(value)) = (attr_value(tag, "name"), attr_value(tag, "value")) {
                out.push((name, value));
            }
        }
        out
    }

    /// Decode a JWT payload (middle segment) without verifying the signature.
    fn jwt_payload(token: &str) -> Option<serde_json::Value> {
        use base64::engine::Engine as _;
        let payload_b64 = token.split('.').nth(1)?;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// All `<input>` tags as (name, type) pairs.
    fn parse_inputs(html: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = html;
        while let Some(pos) = rest.find("<input") {
            rest = &rest[pos..];
            let end = rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            let tag = &rest[..end];
            rest = &rest[end..];
            if let Some(name) = attr_value(tag, "name") {
                let kind = attr_value(tag, "type").unwrap_or_default();
                out.push((name, kind));
            }
        }
        out
    }

    /// Detect the OTP code field on an MFA form. Bouygues' Picasso `OtpInput`
    /// page hides the real code input (`id="codeOtp"`, usually `name="token"`)
    /// and fills it via JS, so first look for that explicitly; otherwise fall
    /// back to the first non-hidden input that isn't a known login/webflow field.
    fn find_otp_field(html: &str) -> Option<String> {
        // Picasso OTP entry page: the code input is the one with id="codeOtp".
        if html.contains("OtpInput") || html.contains("id=\"codeOtp\"") {
            if let Some(name) = Self::input_name_by_id(html, "codeOtp") {
                return Some(name);
            }
            return Some("token".to_string());
        }

        const KNOWN: &[&str] = &[
            "username",
            "password",
            "lastname",
            "execution",
            "_eventId",
            "rememberMe",
            "geolocation",
            "conversationId",
        ];
        Self::parse_inputs(html)
            .into_iter()
            .find_map(|(name, kind)| {
                let k = kind.to_lowercase();
                if k == "hidden" || k == "submit" || k == "button" || k == "checkbox" {
                    return None;
                }
                if KNOWN.contains(&name.as_str()) {
                    return None;
                }
                Some(name)
            })
    }

    /// Find the `name` of the `<input>` whose `id` matches `id`.
    fn input_name_by_id(html: &str, id: &str) -> Option<String> {
        let id_attr = format!("id=\"{}\"", id);
        let mut rest = html;
        while let Some(pos) = rest.find("<input") {
            rest = &rest[pos..];
            let end = rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            let tag = &rest[..end];
            rest = &rest[end..];
            if tag.contains(&id_attr) {
                return attr_value(tag, "name");
            }
        }
        None
    }

    /// All `<input>` fields as (name, value) pairs (value defaults to ""),
    /// excluding submit/button controls. Used to echo a webflow form back —
    /// including empty-valued fields like `geolocation` that the page expects.
    fn parse_field_values(html: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = html;
        while let Some(pos) = rest.find("<input") {
            rest = &rest[pos..];
            let end = rest.find('>').map(|e| e + 1).unwrap_or(rest.len());
            let tag = &rest[..end];
            rest = &rest[end..];
            let kind = attr_value(tag, "type").unwrap_or_default().to_lowercase();
            if kind == "submit" || kind == "button" {
                continue;
            }
            if let Some(name) = attr_value(tag, "name") {
                let value = attr_value(tag, "value").unwrap_or_default();
                out.push((name, value));
            }
        }
        out
    }

    /// Extract the masked OTP destination from the contact-selection page's
    /// `window.LOGIN_CONFIG.OtpMethod` block (e.g. `tel: '06 ** ** ** 76'`).
    /// Prefer SMS (`tel`), fall back to `email`. The masked string is exactly
    /// what the page's JS writes into the `maskedValue` field before submitting.
    fn extract_otp_contact(html: &str) -> Option<String> {
        for key in ["tel", "email"] {
            let marker = format!("{}: '", key);
            if let Some(start) = html.find(&marker) {
                let after = &html[start + marker.len()..];
                if let Some(end) = after.find('\'') {
                    let val = after[..end].trim();
                    if !val.is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
        None
    }

    /// Parse the Kaltura `lineup/get` response (`result.objects[]`) into channels.
    /// Each object: `description` = name, `lcn` = channel number, `id` = Kaltura
    /// asset id (the playback key), `images[]` = logos. Also tolerates a few other
    /// array locations / field spellings (and the `{body:[…]}` shape in tests).
    fn parse_channels_json(json: &serde_json::Value) -> Vec<Channel> {
        fn pick_str(o: &serde_json::Value, keys: &[&str]) -> Option<String> {
            keys.iter()
                .find_map(|k| o.get(k).and_then(|v| v.as_str()).map(str::to_string))
        }
        fn pick_num(o: &serde_json::Value, keys: &[&str]) -> Option<u64> {
            keys.iter().find_map(|k| {
                o.get(k).and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                })
            })
        }
        // Kaltura square/landscape logo: first image with an http url.
        fn pick_image(o: &serde_json::Value) -> Option<String> {
            o.get("images")?.as_array()?.iter().find_map(|i| {
                i.get("url")
                    .and_then(|v| v.as_str())
                    .filter(|u| u.starts_with("http"))
                    .map(str::to_string)
            })
        }

        let arr: Vec<serde_json::Value> = if let Some(a) = json.as_array() {
            a.clone()
        } else {
            // Kaltura nests the list under result.objects.
            json.get("result")
                .and_then(|r| r.get("objects"))
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| {
                    [
                        "objects", "channels", "items", "data", "results", "list", "body",
                    ]
                    .iter()
                    .find_map(|k| json.get(*k).and_then(|v| v.as_array()).cloned())
                })
                .unwrap_or_default()
        };

        let placeholder = url::Url::parse(PLACEHOLDER_URL).expect("placeholder URL is valid");
        arr.iter()
            .filter_map(|c| {
                let name = pick_str(c, &["description", "name", "title", "label", "channelName"])?;
                let id = pick_str(c, &["StreamURL", "streamUrl", "externalId", "channelId"])
                    .or_else(|| pick_num(c, &["id", "epgChannelNumber"]).map(|n| n.to_string()))?;
                let number = pick_num(
                    c,
                    &[
                        "lcn",
                        "number",
                        "channelNumber",
                        "epgChannelNumber",
                        "position",
                    ],
                )
                .map(|n| n as u32);
                let logo_url = pick_str(c, &["logoUrl", "logo", "image", "picto", "img"])
                    .filter(|p| p.starts_with("http"))
                    .or_else(|| pick_image(c));
                let category = pick_str(c, &["category", "genre", "theme"]).unwrap_or_default();
                Some(Channel {
                    id,
                    name,
                    logo_url,
                    number,
                    category: ChannelCategory::from_group_title(&category),
                    stream_template: StreamTemplate::Direct(placeholder.clone()),
                    locked: false,
                })
            })
            .collect()
    }

    /// Obtain an anonymous Kaltura KS (session token) for partner 3199. This needs
    /// no user credentials and is enough to read the public channel lineup.
    async fn kaltura_anonymous_ks(&self) -> Option<String> {
        let resp = self
            .client
            .post(&self.kaltura_login_url)
            .header("Accept", "application/json")
            .json(&json!({ "partnerId": KALTURA_PARTNER_ID, "apiVersion": "8.7.5" }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: serde_json::Value = resp.json().await.ok()?;
        // result.ks, or result.loginSession.ks
        v.pointer("/result/ks")
            .or_else(|| v.pointer("/result/loginSession/ks"))
            .and_then(|k| k.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    fn set_tokens(&mut self, access: String, id: String) {
        self.session_blob = Some(format!("{}\n{}", access, id));
        self.access_token = Some(access);
        self.id_token = Some(id);
    }

    /// If `resp` is the redirect that the policy halted at `redirect_uri`, pull
    /// `access_token`/`id_token` from the Location header's fragment (or query).
    fn extract_tokens_from_redirect(&self, resp: &reqwest::Response) -> Option<(String, String)> {
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())?;
        let url = resp.url().join(loc).ok()?;
        let raw = url
            .fragment()
            .filter(|f| !f.is_empty())
            .or_else(|| url.query().filter(|q| !q.is_empty()))?;
        let pairs: std::collections::HashMap<_, _> = url::form_urlencoded::parse(raw.as_bytes())
            .into_owned()
            .collect();
        let access = pairs.get("access_token").filter(|s| !s.is_empty())?.clone();
        let id = pairs.get("id_token").filter(|s| !s.is_empty())?.clone();
        Some((access, id))
    }

    /// Build the POST body for a CAS webflow form: echo the scraped hidden inputs
    /// (minus the fields we override) and append `extra` (e.g. username/password
    /// or the OTP code). Guarantees `_eventId=submit` is present.
    fn build_form_body(&self, extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let override_keys: Vec<&str> = extra.iter().map(|(k, _)| *k).collect();
        let mut form: Vec<(String, String)> = self
            .pending_hidden
            .iter()
            .filter(|(k, _)| !override_keys.contains(&k.as_str()))
            .cloned()
            .collect();
        for (k, v) in extra {
            form.push((k.to_string(), v.to_string()));
        }
        if !form.iter().any(|(k, _)| k == "_eventId") {
            form.push(("_eventId".to_string(), "submit".to_string()));
        }
        form
    }

    /// Inspect a CAS webflow POST response and decide the next phase. Tokens →
    /// `Done`; 401/403 → `InvalidCredentials`; an OTP entry form → `Otp`. The MFA
    /// flow inserts intermediate forms (e.g. `contactSelectionForm`, which picks
    /// the destination for the code) that carry no real input — those are
    /// auto-submitted (echo hidden inputs + `_eventId=submit`) to advance.
    async fn handle_form_response(&mut self, resp: reqwest::Response) -> Result<AuthPhase> {
        let mut resp = resp;
        let mut auto_submits = 0;
        loop {
            // Success: the policy halted at the token redirect to `redirect_uri`.
            if let Some((access, id)) = self.extract_tokens_from_redirect(&resp) {
                tracing::info!("Bouygues: tokens received (access len={})", access.len());
                self.set_tokens(access, id);
                return Ok(AuthPhase::Done);
            }

            let status = resp.status();
            if status == 401 || status == 403 {
                return Err(OperatorError::InvalidCredentials);
            }

            let form_url = resp.url().to_string();
            let html = resp.text().await.unwrap_or_default();
            let all_inputs = Self::parse_inputs(&html);
            tracing::debug!(
                "Bouygues: MFA form status={} fields={:?}",
                status,
                all_inputs
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            );

            // The OTP entry form: capture all its fields (incl. empty ones) and
            // the code field name.
            if let Some(field) = Self::find_otp_field(&html) {
                tracing::info!("Bouygues: OTP challenge detected, code field={:?}", field);
                self.pending_form_url = Some(form_url);
                self.pending_hidden = Self::parse_field_values(&html);
                self.otp_field = Some(field);
                return Ok(AuthPhase::Otp);
            }

            // Contact-selection step: choose where the OTP is sent. The page's JS
            // copies the masked destination into `maskedValue` and clicks the
            // `_eventId_submit` button. Replicate that to trigger the code send.
            if html.contains("contactSelectionForm") && auto_submits < 1 {
                auto_submits += 1;
                let contact = Self::extract_otp_contact(&html).ok_or_else(|| {
                    OperatorError::AuthFailed("OTP contact (tel/email) not found on page".into())
                })?;
                tracing::info!("Bouygues: selecting OTP contact {:?}", contact);

                let values = Self::parse_hidden_inputs(&html);
                let mut form: Vec<(String, String)> = vec![("maskedValue".into(), contact)];
                for (name, kind) in &all_inputs {
                    if kind == "hidden" && name != "maskedValue" {
                        let v = values
                            .iter()
                            .find(|(k, _)| k == name)
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default();
                        form.push((name.clone(), v));
                    }
                }
                // CAS fires the webflow transition via the submit button's name.
                form.push(("_eventId_submit".into(), String::new()));
                resp = self.client.post(&form_url).form(&form).send().await?;
                continue;
            }

            // Other intermediate hidden-only webflow form: echo ALL its hidden
            // inputs (including empty-valued ones like `geolocation`) and submit.
            let hidden_only =
                !all_inputs.is_empty() && all_inputs.iter().all(|(_, k)| k == "hidden");
            if hidden_only && auto_submits < 1 {
                auto_submits += 1;
                let values = Self::parse_hidden_inputs(&html);
                let form: Vec<(String, String)> = all_inputs
                    .iter()
                    .map(|(name, _)| {
                        let v = values
                            .iter()
                            .find(|(k, _)| k == name)
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default();
                        (name.clone(), v)
                    })
                    .collect();
                let mut form = form;
                if !form.iter().any(|(k, _)| k == "_eventId") {
                    form.push(("_eventId".to_string(), "submit".to_string()));
                }
                tracing::debug!("Bouygues: auto-submitting intermediate MFA form");
                resp = self.client.post(&form_url).form(&form).send().await?;
                continue;
            }

            tracing::warn!(
                "Bouygues: unrecognized MFA page (HTTP {}, fields={:?})",
                status,
                all_inputs
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            );
            return Err(OperatorError::AuthFailed(
                "authentication could not be completed (unexpected MFA step)".into(),
            ));
        }
    }
}

impl Default for BouyguesOperator {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the value of `attr="…"` inside a single HTML tag string.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=\"", attr);
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

#[async_trait]
impl Operator for BouyguesOperator {
    fn name(&self) -> &'static str {
        "Bouygues Bbox"
    }
    fn requires_auth(&self) -> bool {
        true
    }

    fn set_extra_credential(&mut self, value: &str) {
        self.lastname = Some(value.to_string());
    }

    fn uses_phased_auth(&self) -> bool {
        true
    }

    /// Phase 1: kick off the OAuth2 flow and follow the redirect chain to the
    /// Keycloak-brokered CAS login form. Scrape its hidden inputs so the password
    /// POST can echo the single-use `execution` token.
    async fn begin_auth(&mut self, username: &str) -> Result<AuthPhase> {
        self.username = Some(username.to_string());

        let resp = self
            .client
            .post(&self.oauth2_url)
            .form(&[
                ("client_id", OAUTH2_CLIENT_ID),
                ("response_type", OAUTH2_RESPONSE_TYPE),
                ("redirect_uri", self.redirect_uri.as_str()),
            ])
            .send()
            .await?;

        let status = resp.status();
        let form_url = resp.url().to_string();
        let html = resp.text().await.unwrap_or_default();
        let hidden = Self::parse_hidden_inputs(&html);
        tracing::debug!(
            "Bouygues: login form status={} fields={:?}",
            status,
            hidden.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        );

        if hidden.is_empty() {
            return Err(OperatorError::AuthFailed(format!(
                "Bouygues login form not found (HTTP {}). The login flow may have \
                 changed or is blocked.",
                status
            )));
        }

        self.pending_form_url = Some(form_url);
        self.pending_hidden = hidden;
        Ok(AuthPhase::Password)
    }

    /// Phase 2a: submit username + password to the CAS form. Returns `Otp` when
    /// Bouygues then demands a one-time code, or `Done` if it lets us straight
    /// through to the tokens.
    async fn complete_auth_password(&mut self, password: &str) -> Result<AuthPhase> {
        let form_url = self.pending_form_url.clone().ok_or_else(|| {
            OperatorError::AuthFailed("no pending login form; call begin_auth first".into())
        })?;
        let username = self.username.clone().unwrap_or_default();
        let lastname = self.lastname.clone().unwrap_or_default();

        let mut extra = vec![("username", username.as_str()), ("password", password)];
        // Echo lastname only if the live form actually carries that field.
        if self.pending_hidden.iter().any(|(k, _)| k == "lastname") {
            extra.push(("lastname", lastname.as_str()));
        }
        let form = self.build_form_body(&extra);

        let resp = self.client.post(&form_url).form(&form).send().await?;
        self.handle_form_response(resp).await
    }

    /// Phase 2c: submit the one-time code the user received (SMS/app).
    async fn submit_otp(&mut self, code: &str) -> Result<AuthPhase> {
        let form_url = self.pending_form_url.clone().ok_or_else(|| {
            OperatorError::AuthFailed(
                "no pending OTP form; complete the password step first".into(),
            )
        })?;
        let field = self
            .otp_field
            .clone()
            .ok_or_else(|| OperatorError::AuthFailed("OTP field unknown".into()))?;

        // Echo every captured field, fill the code, and fire the validate event
        // via the submit button name (`_eventId_submit`) — the Picasso OTP form
        // has no hidden `_eventId`, only `_eventId_submit`/`_eventId_regenerate`.
        let mut form: Vec<(String, String)> = self
            .pending_hidden
            .iter()
            .filter(|(k, _)| k != &field)
            .cloned()
            .collect();
        form.push((field.clone(), code.to_string()));
        form.push(("_eventId_submit".to_string(), String::new()));

        let resp = self.client.post(&form_url).form(&form).send().await?;
        match self.handle_form_response(resp).await {
            // A second OTP form usually means the code was wrong/expired.
            Ok(AuthPhase::Otp) => Err(OperatorError::InvalidCredentials),
            other => other,
        }
    }

    /// Convenience single-call auth (tests / password-only accounts). Errors if
    /// the account requires OTP — use the phased methods for that.
    async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        match self.begin_auth(username).await? {
            AuthPhase::Password => match self.complete_auth_password(password).await? {
                AuthPhase::Done => Ok(()),
                AuthPhase::Otp => Err(OperatorError::AuthFailed(
                    "account requires a one-time code; use the phased auth flow".into(),
                )),
                _ => Err(OperatorError::AuthFailed("unexpected auth phase".into())),
            },
            _ => Err(OperatorError::AuthFailed("unexpected auth phase".into())),
        }
    }

    async fn fetch_channels(&self) -> Result<Vec<Channel>> {
        // The channel list lives on Kaltura OTT and only needs an ANONYMOUS
        // Kaltura session — it is not gated by the PFS credential (only playback
        // is). Step 1: anonymousLogin → KS. Step 2: lineup/get → channels.
        let ks = match self.kaltura_anonymous_ks().await {
            Some(ks) => ks,
            None => {
                tracing::warn!("Bouygues: Kaltura anonymousLogin failed — using M3U fallback");
                return Ok(parse_m3u(FALLBACK_M3U));
            }
        };

        let resp = self
            .client
            .get(&self.channel_list_url)
            .bearer_auth(&ks)
            .header("Accept", "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::warn!("Bouygues: lineup HTTP {} — using M3U fallback", r.status());
                return Ok(parse_m3u(FALLBACK_M3U));
            }
            Err(e) => {
                tracing::warn!("Bouygues: lineup network error: {} — using M3U fallback", e);
                return Ok(parse_m3u(FALLBACK_M3U));
            }
        };

        let body_text = resp.text().await.unwrap_or_default();
        let channels = serde_json::from_str(&body_text)
            .ok()
            .map(|v: serde_json::Value| Self::parse_channels_json(&v))
            .unwrap_or_default();
        if channels.is_empty() {
            tracing::warn!("Bouygues: empty/unparseable lineup — using M3U fallback");
            return Ok(parse_m3u(FALLBACK_M3U));
        }

        tracing::info!("Bouygues: {} channels from Kaltura lineup", channels.len());
        Ok(channels)
    }

    async fn resolve_stream(&self, channel: &Channel) -> Result<StreamUrl> {
        // Channels from the fallback M3U carry their real URL directly.
        if let StreamTemplate::Direct(url) = &channel.stream_template {
            if url.as_str() != PLACEHOLDER_URL {
                return Ok(StreamUrl::direct(url.clone()));
            }
        }

        // Live channels come from the Kaltura lineup. Resolving a playable stream
        // requires an entitled session + the PFS-WASM-minted `bt-api-int` Basic
        // credential, which a native client cannot reproduce (see
        // docs/operators.md). Return a plain stream error — NOT InvalidCredentials,
        // which the UI treats as "session expired" and would log the user out.
        Err(OperatorError::UnexpectedResponse {
            status: 501,
            body: "La lecture en direct Bouygues n'est pas disponible \
                   (protection DRM/PFS non supportée)."
                .into(),
        })
    }

    async fn fetch_epg(&self, _hours: u8) -> Result<Option<EpgData>> {
        Ok(None)
    }

    fn session_token(&self) -> Option<&str> {
        // Persisted handle is "access_token\nid_token" (see `restore_session`).
        self.session_blob.as_deref()
    }

    async fn restore_session(&mut self, token: &str) -> Result<()> {
        // Persisted form is "access_token\nid_token".
        let mut parts = token.splitn(2, '\n');
        let access = parts.next().unwrap_or("");
        let id = parts.next().unwrap_or("");
        if access.is_empty() || id.is_empty() {
            return Err(OperatorError::InvalidCredentials);
        }

        // Validate the id_token is not expired (we cannot silently refresh without
        // the password, which is never persisted).
        if let Some(payload) = Self::jwt_payload(id) {
            if let Some(exp) = payload.get("exp").and_then(|v| v.as_i64()) {
                let now = chrono::Utc::now().timestamp();
                if exp <= now {
                    return Err(OperatorError::InvalidCredentials);
                }
            }
        }

        self.access_token = Some(access.to_string());
        self.id_token = Some(id.to_string());
        self.session_blob = Some(token.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A throwaway JWT (`{"alg":"none"}.{"exp":…,"id_personne":"42"}.`) with the
    /// given expiry, used to exercise id_token parsing without a real signature.
    fn fake_jwt(exp: i64) -> String {
        use base64::engine::Engine as _;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(b"{\"alg\":\"none\",\"typ\":\"JWT\"}");
        let payload_json = format!("{{\"exp\":{},\"id_personne\":\"42\"}}", exp);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        format!("{}.{}.", header, payload)
    }

    #[test]
    fn test_parse_hidden_inputs() {
        let html = r#"
            <form>
              <input type="hidden" name="execution" value="abc123=="/>
              <input type="text" name="username" value="x"/>
              <input type="hidden" name="_eventId" value="submit">
            </form>"#;
        let inputs = BouyguesOperator::parse_hidden_inputs(html);
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0], ("execution".into(), "abc123==".into()));
        assert_eq!(inputs[1], ("_eventId".into(), "submit".into()));
    }

    fn op_for(mock: &MockServer) -> BouyguesOperator {
        BouyguesOperator::new_with_urls(
            &format!("{}/authorize", mock.uri()),
            &format!("{}/kaltura-login", mock.uri()),
            &format!("{}/lineup", mock.uri()),
            &format!("{}/mon-compte/", mock.uri()),
        )
    }

    const LOGIN_FORM: &str = r#"<form id="fm1" method="post">
        <input type="hidden" name="execution" value="exec-token-1"/>
        <input type="hidden" name="_eventId" value="submit"/>
        <input type="hidden" name="conversationId" value="conv-1"/>
        <input type="hidden" id="username" name="username" value=""/>
        <input type="hidden" id="password" name="password" value=""/>
        </form>"#;

    /// Mount the redirect chain entry: POST /authorize → 302 → GET /login-form.
    async fn mount_authorize_to_form(mock: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/authorize"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/login-form", mock.uri()).as_str()),
            )
            .mount(mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/login-form"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LOGIN_FORM))
            .mount(mock)
            .await;
    }

    #[tokio::test]
    async fn test_authenticate_success_no_otp() {
        let mock = MockServer::start().await;
        let at = "bbox_access_token";
        let it = fake_jwt(chrono::Utc::now().timestamp() + 3600);
        mount_authorize_to_form(&mock).await;

        // Password POST → token redirect to redirect_uri (policy stops here).
        Mock::given(method("POST"))
            .and(path("/login-form"))
            .and(body_string_contains("password"))
            .respond_with(
                ResponseTemplate::new(302).insert_header(
                    "Location",
                    format!(
                        "{}/mon-compte/#access_token={}&id_token={}",
                        mock.uri(),
                        at,
                        it
                    )
                    .as_str(),
                ),
            )
            .mount(&mock)
            .await;

        let mut op = op_for(&mock);
        op.set_extra_credential("Dupont");
        op.authenticate("user@bbox.fr", "pass").await.unwrap();
        assert_eq!(op.access_token.as_deref(), Some(at));
        assert_eq!(op.id_token.as_deref(), Some(it.as_str()));
    }

    #[tokio::test]
    async fn test_authenticate_invalid_credentials() {
        let mock = MockServer::start().await;
        mount_authorize_to_form(&mock).await;
        // Wrong password → 401 + re-rendered login form.
        Mock::given(method("POST"))
            .and(path("/login-form"))
            .respond_with(ResponseTemplate::new(401).set_body_string(LOGIN_FORM))
            .mount(&mock)
            .await;

        let mut op = op_for(&mock);
        let phase = op.begin_auth("bad@bbox.fr").await.unwrap();
        assert_eq!(phase, AuthPhase::Password);
        let err = op.complete_auth_password("wrong").await.unwrap_err();
        assert!(matches!(err, OperatorError::InvalidCredentials));
    }

    #[tokio::test]
    async fn test_otp_flow() {
        let mock = MockServer::start().await;
        let at = "otp_access_token";
        let it = fake_jwt(chrono::Utc::now().timestamp() + 3600);
        mount_authorize_to_form(&mock).await;

        let otp_form = r#"<form id="fm1" method="post">
            <input type="hidden" name="execution" value="exec-token-2"/>
            <input type="hidden" name="_eventId" value="submit"/>
            <input type="hidden" name="conversationId" value="conv-2"/>
            <input type="text" name="otp" value=""/>
            </form>"#;

        // Password POST → MFA OTP form.
        Mock::given(method("POST"))
            .and(path("/login-form"))
            .and(body_string_contains("password"))
            .respond_with(ResponseTemplate::new(200).set_body_string(otp_form))
            .mount(&mock)
            .await;
        // OTP POST → token redirect.
        Mock::given(method("POST"))
            .and(path("/login-form"))
            .and(body_string_contains("otp="))
            .respond_with(
                ResponseTemplate::new(302).insert_header(
                    "Location",
                    format!(
                        "{}/mon-compte/#access_token={}&id_token={}",
                        mock.uri(),
                        at,
                        it
                    )
                    .as_str(),
                ),
            )
            .mount(&mock)
            .await;

        let mut op = op_for(&mock);
        assert_eq!(
            op.begin_auth("user@bbox.fr").await.unwrap(),
            AuthPhase::Password
        );
        assert_eq!(
            op.complete_auth_password("pass").await.unwrap(),
            AuthPhase::Otp
        );
        assert_eq!(op.otp_field.as_deref(), Some("otp"));
        assert_eq!(op.submit_otp("123456").await.unwrap(), AuthPhase::Done);
        assert_eq!(op.access_token.as_deref(), Some(at));
    }

    #[tokio::test]
    async fn test_fetch_channels_parses_kaltura_lineup() {
        let mock = MockServer::start().await;

        // Anonymous Kaltura login → KS.
        Mock::given(method("POST"))
            .and(path("/kaltura-login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "objectType": "KalturaLoginSession", "ks": "anon_ks" }
            })))
            .mount(&mock)
            .await;
        // lineup/get → channels.
        Mock::given(method("GET"))
            .and(path("/lineup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "result": { "objects": [
                    {
                        "id": 2781003, "description": "TF1", "externalId": "TF1", "lcn": 1,
                        "images": [{"imageTypeName": "PIC_SQUARE_DARK",
                                    "url": "https://images.example.com/tf1.png"}]
                    },
                    { "id": 2781050, "description": "Eurosport 1", "externalId": "EUROSPORT1", "lcn": 44 }
                ] }
            })))
            .mount(&mock)
            .await;

        let op = op_for(&mock);
        let channels = op.fetch_channels().await.unwrap();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name, "TF1");
        assert_eq!(channels[0].id, "TF1"); // externalId preferred as the id
        assert_eq!(channels[0].number, Some(1));
        assert_eq!(
            channels[0].logo_url.as_deref(),
            Some("https://images.example.com/tf1.png")
        );
        assert_eq!(channels[1].number, Some(44));
    }

    #[tokio::test]
    async fn test_fetch_channels_fallback_on_lineup_error() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/kaltura-login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result": {"ks": "k"}})))
            .mount(&mock)
            .await;
        Mock::given(method("GET"))
            .and(path("/lineup"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let op = op_for(&mock);
        let channels = op.fetch_channels().await.unwrap();
        assert!(!channels.is_empty()); // fallback M3U
    }

    #[tokio::test]
    async fn test_resolve_stream_direct_url_plays() {
        // A fallback-M3U channel carries a real URL → returned as-is.
        let op = BouyguesOperator::new();
        let real = url::Url::parse("https://cdn.example.com/tf1/index.m3u8").unwrap();
        let channel = Channel {
            id: "tf1".into(),
            name: "TF1".into(),
            logo_url: None,
            number: Some(1),
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Direct(real.clone()),
            locked: false,
        };
        let stream = op.resolve_stream(&channel).await.unwrap();
        assert_eq!(stream.url, real);
    }

    #[tokio::test]
    async fn test_resolve_stream_live_unsupported_keeps_session() {
        // A Kaltura live channel (placeholder URL) is unplayable (PFS) — it must
        // error WITHOUT InvalidCredentials, so the UI does not log the user out.
        let op = BouyguesOperator::new();
        let channel = Channel {
            id: "TF1".into(),
            name: "TF1".into(),
            logo_url: None,
            number: Some(1),
            category: ChannelCategory::Generalist,
            stream_template: StreamTemplate::Direct(url::Url::parse(PLACEHOLDER_URL).unwrap()),
            locked: false,
        };
        let err = op.resolve_stream(&channel).await.unwrap_err();
        assert!(!matches!(err, OperatorError::InvalidCredentials));
    }

    #[tokio::test]
    async fn test_restore_session_expired_token() {
        let mut op = BouyguesOperator::new();
        let expired = fake_jwt(0); // exp in 1970
        let token = format!("access_tok\n{}", expired);
        let err = op.restore_session(&token).await.unwrap_err();
        assert!(matches!(err, OperatorError::InvalidCredentials));
    }

    #[tokio::test]
    async fn test_restore_session_valid_token() {
        let mut op = BouyguesOperator::new();
        let valid = fake_jwt(chrono::Utc::now().timestamp() + 3600);
        let token = format!("access_tok\n{}", valid);
        op.restore_session(&token).await.unwrap();
        assert_eq!(op.access_token.as_deref(), Some("access_tok"));
    }
}
