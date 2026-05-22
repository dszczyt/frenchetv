/// License exchange: extract PSSH from DASH MPD, obtain a Widevine license,
/// and feed it into the CDM so that `CdmHandle::decrypt` can be called.
use anyhow::{bail, Context, Result};
use base64::engine::Engine as _;
use std::sync::{Arc, Mutex};

use super::cdm::CdmHandle;

/// Widevine system UUID bytes (network byte order).
const WV_SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce,
    0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];

/// Extract the Widevine PSSH bytes from a DASH MPD XML string.
///
/// Strategy (tried in order):
/// 1. `<cenc:pssh>` or `<pssh>` element inside the Widevine ContentProtection block
///    (base64-encoded full PSSH box).
/// 2. `cenc:default_KID` attribute on any ContentProtection element → construct a
///    minimal PSSH v1 box with that KID.
/// 3. `default_KID` attribute (without namespace prefix) → same.
///
/// Returns `None` only if none of the above is found.
pub fn extract_pssh_from_mpd(mpd_text: &str) -> Option<Vec<u8>> {
    let lower = mpd_text.to_lowercase();

    // ── Strategy 1: look for <cenc:pssh> inside WV ContentProtection block ──────
    if let Some(wv_pos) = lower.find("edef8ba9") {
        let after_wv = &mpd_text[wv_pos..];
        // Skip to end of the ContentProtection opening tag.
        if let Some(tag_end) = after_wv.find('>') {
            let remaining = &after_wv[tag_end + 1..];
            for (open_tag, close_tag) in [("<cenc:pssh>", "</cenc:pssh>"), ("<pssh>", "</pssh>")] {
                if let Some(p_start) = remaining.find(open_tag) {
                    let content_start = p_start + open_tag.len();
                    if let Some(rel_end) = remaining[content_start..].find(close_tag) {
                        let b64 = remaining[content_start..content_start + rel_end].trim();
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64))
                        {
                            tracing::debug!("widevine: PSSH from <cenc:pssh> ({} bytes)", bytes.len());
                            return Some(bytes);
                        }
                    }
                }
            }
        }
    }

    // ── Strategy 2: extract KID from default_KID attribute ───────────────────────
    // Look for cenc:default_KID or default_KID (both upper and lower case in the original text).
    for attr_name in ["cenc:default_KID", "default_KID", "cenc:default_kid", "default_kid"] {
        let pattern = format!("{}=\"", attr_name);
        if let Some(pos) = mpd_text.find(&pattern) {
            let after = &mpd_text[pos + pattern.len()..];
            if let Some(end) = after.find('"') {
                let kid_str = &after[..end];
                if let Some(kid) = parse_uuid_to_bytes(kid_str) {
                    let pssh = build_pssh_from_kid(&kid);
                    tracing::debug!(
                        "widevine: PSSH built from default_KID {} ({} bytes)",
                        kid_str, pssh.len()
                    );
                    return Some(pssh);
                }
            }
        }
    }

    tracing::warn!("widevine: no PSSH or default_KID found in MPD (first 500 chars: {})",
        &mpd_text[..mpd_text.len().min(500)]);
    None
}

/// Parse a UUID string like `"12345678-1234-1234-1234-123456789abc"` into 16 bytes.
fn parse_uuid_to_bytes(uuid: &str) -> Option<[u8; 16]> {
    let hex: String = uuid.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 { return None; }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)? as u8;
        let lo = (chunk[1] as char).to_digit(16)? as u8;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

/// Build a minimal Widevine PSSH box (version 1) containing a single KID.
///
/// Box layout:
/// ```
/// 4B  size (= 52)
/// 4B  'pssh'
/// 1B  version (= 1)
/// 3B  flags (= 0)
/// 16B system_id (Widevine UUID)
/// 4B  key_id_count (= 1)
/// 16B key_id
/// 4B  data_size (= 0)
/// ```
/// Total: 52 bytes.
pub fn build_pssh_from_kid(kid: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(52);
    out.extend_from_slice(&52u32.to_be_bytes());   // size
    out.extend_from_slice(b"pssh");                 // type
    out.push(1);                                    // version 1
    out.extend_from_slice(&[0u8; 3]);               // flags
    out.extend_from_slice(&WV_SYSTEM_ID);           // system_id
    out.extend_from_slice(&1u32.to_be_bytes());     // key_id_count
    out.extend_from_slice(kid);                     // key_id[0]
    out.extend_from_slice(&0u32.to_be_bytes());     // data_size
    out
}

/// Send the Widevine challenge to a license endpoint.
///
/// Orange's CDN exposes a GET-only license endpoint.  The Widevine challenge
/// (binary protobuf) is base64url-encoded (no padding) and appended as the
/// `body` query parameter, matching the pattern used by many EME proxies.
///
/// If the CDN returns JSON, we try to extract the `license` / `data` /
/// `rawLicenseResponse` field (also base64url-decoded) before feeding it to
/// the CDM.  Raw binary responses are used directly.
async fn send_challenge(
    client: &reqwest::Client,
    la_url: &str,
    challenge: &[u8],
    license_headers: &[(String, String)],
) -> Result<Vec<u8>> {
    // Encode challenge as base64url without padding.
    let challenge_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge);

    // Append to URL as `body=` parameter (common Orange/OTT pattern).
    let url_with_challenge = if la_url.contains('?') {
        format!("{}&body={}", la_url, challenge_b64)
    } else {
        format!("{}?body={}", la_url, challenge_b64)
    };

    tracing::debug!("widevine: GET license URL length={}", url_with_challenge.len());

    let mut req = client.get(&url_with_challenge);
    for (name, value) in license_headers {
        req = req.header(name.as_str(), value.as_str());
    }

    let resp = req.send().await.context("license GET")?;

    if !resp.status().is_success() {
        let status = resp.status();
        for (k, v) in resp.headers() {
            tracing::debug!("widevine: license error header: {}: {}", k, v.to_str().unwrap_or("?"));
        }
        let body = resp.text().await.unwrap_or_default();
        bail!("license server returned {}: {}", status, body);
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let raw = resp.bytes().await.context("license response body")?.to_vec();
    tracing::info!("widevine: license response {} bytes (content-type: {})", raw.len(), content_type);

    // If response is JSON, extract the nested license bytes.
    if content_type.contains("json") || raw.first() == Some(&b'{') {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&raw) {
            for field in &["license", "data", "rawLicenseResponse", "licenseResponse"] {
                if let Some(b64) = json.get(field).and_then(|v| v.as_str()) {
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64)
                        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64))
                    {
                        tracing::debug!("widevine: extracted license from JSON field '{}'", field);
                        return Ok(decoded);
                    }
                }
            }
        }
    }

    Ok(raw)
}

/// Perform the full license exchange:
/// 1. CDM generates a challenge from `pssh`.
/// 2. Challenge is sent to `la_url` (Orange uses GET with base64url body param).
/// 3. Response is fed back into the CDM.
///
/// Returns the session ID.
pub async fn acquire_license(
    cdm: &Arc<Mutex<CdmHandle>>,
    pssh: &[u8],
    la_url: &str,
    license_headers: &[(String, String)],
) -> Result<String> {
    // Step 1: generate challenge (synchronous CDM call).
    let (session_id, challenge) = {
        let mut h = cdm.lock().unwrap();
        h.create_session(pssh).context("CDM create_session")?
    };

    tracing::info!(
        "widevine: license challenge generated ({} bytes) for session {}",
        challenge.len(),
        session_id
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("reqwest client")?;

    // Step 2: send challenge to license server.
    let license_response: Vec<u8> = send_challenge(&client, la_url, &challenge, license_headers).await?;

    // Step 3: feed response to CDM.
    {
        let mut h = cdm.lock().unwrap();
        h.update_session(&session_id, &license_response).context("CDM update_session")?;
    }

    tracing::info!("widevine: keys loaded for session {}", session_id);
    Ok(session_id)
}
