/// License exchange: extract PSSH from DASH MPD, obtain a Widevine license,
/// and feed it into the CDM so that `CdmHandle::decrypt` can be called.
use anyhow::{bail, Context, Result};
use base64::engine::Engine as _;
use std::sync::{Arc, Mutex};

use super::cdm::CdmHandle;

/// Extract the Widevine PSSH box from a DASH MPD XML string.
///
/// Looks for `<ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed">`
/// and reads its `<cenc:pssh>` child element (base64-encoded raw PSSH box).
pub fn extract_pssh_from_mpd(mpd_text: &str) -> Option<Vec<u8>> {
    // Simple state-machine scan — avoids pulling in a full XML dep for this one task.
    let lower = mpd_text.to_lowercase();
    let wv_pos = lower.find("edef8ba9")?;

    // Walk backwards from wv_pos to find the enclosing ContentProtection element.
    // Then walk forwards to find <cenc:pssh>...</cenc:pssh>.
    let after_wv = &mpd_text[wv_pos..];
    // Find the closing '>' of the ContentProtection opening tag.
    let tag_end = after_wv.find('>')?;
    let remaining = &after_wv[tag_end + 1..];

    // Find <cenc:pssh> or <pssh>
    let pssh_start = remaining
        .find("<cenc:pssh>")
        .map(|p| (p, "<cenc:pssh>".len(), "</cenc:pssh>"))
        .or_else(|| {
            remaining
                .find("<pssh>")
                .map(|p| (p, "<pssh>".len(), "</pssh>"))
        })?;

    let (p_start, tag_len, close_tag) = pssh_start;
    let content_start = p_start + tag_len;
    let content_end = remaining[content_start..].find(close_tag)? + content_start;
    let b64 = remaining[content_start..content_end].trim();

    base64::engine::general_purpose::STANDARD.decode(b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64))
        .ok()
}

/// Perform the full license exchange:
/// 1. CDM generates a challenge from `pssh`.
/// 2. Challenge is POSTed to `la_url` (with `license_headers`).
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

    // Step 2: POST challenge to license server.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("reqwest client")?;

    let mut req = client
        .post(la_url)
        .header("Content-Type", "application/octet-stream")
        .body(challenge);

    for (name, value) in license_headers {
        req = req.header(name.as_str(), value.as_str());
    }

    let resp = req.send().await.context("license POST")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("license server returned {}: {}", status, body);
    }
    let license_response = resp.bytes().await.context("license response body")?;
    tracing::info!("widevine: license response {} bytes", license_response.len());

    // Step 3: feed response to CDM.
    {
        let mut h = cdm.lock().unwrap();
        h.update_session(&session_id, &license_response).context("CDM update_session")?;
    }

    tracing::info!("widevine: keys loaded for session {}", session_id);
    Ok(session_id)
}
