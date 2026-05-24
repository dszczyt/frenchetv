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
                    tracing::info!(
                        "widevine: PSSH built from default_KID {} → kid_bytes={} ({} bytes)",
                        kid_str,
                        kid.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
                        pssh.len()
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

/// Encode a minimal `WidevineCencHeader` protobuf for one KID.
///
/// Proto schema (relevant fields only):
/// ```
/// message WidevineCencHeader {
///   enum Algorithm { UNENCRYPTED=0; AESCTR=1; }
///   optional Algorithm algorithm = 1;   // wire: varint
///   repeated bytes    key_id     = 2;   // wire: length-delimited
/// }
/// ```
/// Encoded: `\x08\x01\x12\x10<16-byte KID>` = 20 bytes.
fn build_widevine_cenc_header(kid: &[u8; 16]) -> Vec<u8> {
    let mut pb = Vec::with_capacity(20);
    pb.push(0x08); // field 1, wire type 0 (varint) = algorithm
    pb.push(0x01); // AESCTR
    pb.push(0x12); // field 2, wire type 2 (bytes) = key_id
    pb.push(0x10); // length 16
    pb.extend_from_slice(kid);
    pb
}

/// Build a Widevine PSSH **version 0** box with a `WidevineCencHeader` payload.
///
/// Version 0 (data-carrying) is required by many Widevine license servers;
/// version 1 (key-list only, no data) causes servers to return 500 because
/// the challenge lacks the `content_id` / `algorithm` fields they expect.
///
/// Box layout:
/// ```
/// 4B  size
/// 4B  'pssh'
/// 1B  version (= 0)
/// 3B  flags   (= 0)
/// 16B system_id (Widevine UUID)
/// 4B  data_size (= 20)
/// 20B WidevineCencHeader protobuf
/// ```
/// Total: 52 bytes.
pub fn build_pssh_from_kid(kid: &[u8; 16]) -> Vec<u8> {
    let data = build_widevine_cenc_header(kid);
    let total = (4 + 4 + 1 + 3 + 16 + 4 + data.len()) as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());            // size
    out.extend_from_slice(b"pssh");                         // type
    out.push(0);                                            // version 0
    out.extend_from_slice(&[0u8; 3]);                       // flags
    out.extend_from_slice(&WV_SYSTEM_ID);                   // system_id
    out.extend_from_slice(&(data.len() as u32).to_be_bytes()); // data_size
    out.extend_from_slice(&data);                           // WidevineCencHeader
    out
}

/// Decode a full PSSH box and log every KID it carries at INFO level.
///
/// Handles both PSSH v0 (KID in WidevineCencHeader proto, field 2) and
/// v1 (explicit KID list in the box header).  No-ops silently if the box
/// is too short or malformed.
fn log_pssh_kids(pssh: &[u8]) {
    // Minimum: size(4)+fourcc(4)+version(1)+flags(3)+SystemID(16)+data_size(4) = 32 bytes.
    if pssh.len() < 32 {
        tracing::info!("widevine: license PSSH {} bytes (too short to decode KID)", pssh.len());
        return;
    }
    let version = pssh[8];
    if version == 1 {
        // v1: kid_count(4) at offset 28, then KIDs.
        let kid_count = u32::from_be_bytes([pssh[28], pssh[29], pssh[30], pssh[31]]) as usize;
        tracing::info!("widevine: license PSSH v1, {} KID(s)", kid_count);
        for i in 0..kid_count {
            let start = 32 + i * 16;
            if start + 16 <= pssh.len() {
                tracing::info!(
                    "widevine: license PSSH v1 KID[{}]: {}",
                    i,
                    pssh[start..start + 16].iter().map(|b| format!("{:02x}", b)).collect::<String>()
                );
            }
        }
    } else {
        // v0: data_size(4) at offset 28, then WidevineCencHeader protobuf.
        let data_size = u32::from_be_bytes([pssh[28], pssh[29], pssh[30], pssh[31]]) as usize;
        tracing::info!("widevine: license PSSH v0, proto {} bytes", data_size);
        if pssh.len() >= 32 + data_size {
            let proto = &pssh[32..32 + data_size];
            // Minimal protobuf scan: field 2 (key_id), wire type 2 (LEN), length 16.
            let mut pos = 0usize;
            while pos < proto.len() {
                let tag = proto[pos]; pos += 1;
                let wire = tag & 0x07;
                let field = (tag >> 3) as u32;
                match wire {
                    0 => { // varint — skip
                        while pos < proto.len() && proto[pos] & 0x80 != 0 { pos += 1; }
                        if pos < proto.len() { pos += 1; }
                    }
                    2 => { // LEN
                        if pos >= proto.len() { break; }
                        let len = proto[pos] as usize; pos += 1;
                        if field == 2 && len == 16 && pos + 16 <= proto.len() {
                            tracing::info!(
                                "widevine: license PSSH v0 key_id: {}",
                                proto[pos..pos + 16].iter().map(|b| format!("{:02x}", b)).collect::<String>()
                            );
                        }
                        if pos + len <= proto.len() { pos += len; } else { break; }
                    }
                    _ => break,
                }
            }
        }
    }
}

/// POST the Widevine challenge to the license server and return the raw license response.
///
/// Orange's mediation API (`mediation-tv.orange.fr/all/api-gw/license/v1/...`) accepts
/// standard POST with `Content-Type: application/octet-stream` and raw binary challenge.
/// Auth is via `tv_token: Bearer <token>` and `Cookie: wassup=<value>` headers.
///
/// If the server returns JSON, we attempt to extract a base64-encoded license from
/// well-known field names before falling back to the raw bytes.
async fn send_challenge(
    client: &reqwest::Client,
    la_url: &str,
    challenge: &[u8],
    license_headers: &[(String, String)],
) -> Result<Vec<u8>> {
    tracing::debug!("widevine: POST license to {}", la_url);

    let mut req = client
        .post(la_url)
        .header("Content-Type", "application/octet-stream")
        .body(challenge.to_vec());

    for (name, value) in license_headers {
        req = req.header(name.as_str(), value.as_str());
    }

    let resp = req.send().await.context("license POST")?;

    // Follow redirect while preserving POST (reqwest converts POST→GET on 302).
    let resp = if resp.status().is_redirection() {
        if let Some(loc) = resp.headers().get(reqwest::header::LOCATION) {
            let redirect_url = loc.to_str().unwrap_or("").to_string();
            tracing::info!("widevine: license redirect → {}", redirect_url);
            let mut req2 = client
                .post(&redirect_url)
                .header("Content-Type", "application/octet-stream")
                .body(challenge.to_vec());
            for (name, value) in license_headers {
                req2 = req2.header(name.as_str(), value.as_str());
            }
            req2.send().await.context("license POST (after redirect)")?
        } else {
            resp
        }
    } else {
        resp
    };

    if !resp.status().is_success() {
        let status = resp.status();
        tracing::debug!("widevine: license HTTP status = {}", status);
        for (k, v) in resp.headers() {
            tracing::debug!("widevine: license error header: {}: {}", k, v.to_str().unwrap_or("?"));
        }
        let body = resp.text().await.unwrap_or_default();
        tracing::debug!("widevine: license error body = {}", body);
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

    // If JSON, try to extract base64-encoded license from known field names.
    if content_type.contains("json") || raw.first() == Some(&b'{') {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&raw) {
            for field in &["license", "data", "rawLicenseResponse", "licenseResponse"] {
                if let Some(b64) = json.get(field).and_then(|v| v.as_str()) {
                    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64)
                        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64))
                    {
                        tracing::debug!("widevine: license from JSON field '{}'", field);
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
    // Decode PSSH box to log the KID(s) it carries, for comparison with the
    // decrypt KID logged by the proxy.
    //
    // Full PSSH box layout:
    //   size(4) + "pssh"(4) + version(1) + flags(3) + SystemID(16) = 32 bytes header
    //   v1: kid_count(4) + KID[0..N] (16 bytes each)
    //   v0: data_size(4) + WidevineCencHeader protobuf (field 2 = key_id, 16 bytes)
    log_pssh_kids(pssh);

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
