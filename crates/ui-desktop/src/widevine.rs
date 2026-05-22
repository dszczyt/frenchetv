/// Widevine CDM installer.
///
/// Strategy (in order):
///   1. Copy `libwidevinecdm.so` from a locally-installed Chrome/Chromium.
///   2. Download via Google's Omaha update server (XML POST → ZIP extract).
///
/// The CDM is stored at `~/.local/share/frenchetv/widevine/libwidevinecdm.so`.
/// mpv picks it up via `--cdm-store=<dir>` when compiled with `--enable-cdm`.

use std::path::PathBuf;
use anyhow::{bail, Context, Result};

const WIDEVINE_COMPONENT_ID: &str = "oimompecagnajdejgnnjijobebaeigek";

// ── Paths ─────────────────────────────────────────────────────────────────────

/// `~/.local/share/frenchetv/widevine`
pub fn dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()))
        .join("frenchetv")
        .join("widevine")
}

/// Full path to the local CDM copy.
pub fn cdm_path() -> PathBuf {
    dir().join("libwidevinecdm.so")
}

/// `true` when the CDM is already on disk.
pub fn is_installed() -> bool {
    cdm_path().exists()
}

// ── Architecture ──────────────────────────────────────────────────────────────

/// Platform subdirectory as Google names it inside the CRX/ZIP.
fn platform_dir() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "linux_arm64",
        _         => "linux_x64",
    }
}

/// Arch tag used in Omaha requests and ZIP package names.
fn arch_tag() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _         => "x64",
    }
}

/// nacl_arch used in Omaha requests.
fn nacl_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _         => "x86-64",
    }
}

// ── Strategy 1: copy from installed Chrome/Chromium ──────────────────────────

fn system_cdm_candidates() -> Vec<PathBuf> {
    let plat = platform_dir();
    [
        format!("/opt/google/chrome/WidevineCdm/_platform_specific/{plat}/libwidevinecdm.so"),
        format!("/opt/google/chrome-beta/WidevineCdm/_platform_specific/{plat}/libwidevinecdm.so"),
        format!("/opt/google/chrome-unstable/WidevineCdm/_platform_specific/{plat}/libwidevinecdm.so"),
        format!("/usr/lib/chromium/WidevineCdm/_platform_specific/{plat}/libwidevinecdm.so"),
        format!("/usr/lib/chromium-browser/WidevineCdm/_platform_specific/{plat}/libwidevinecdm.so"),
        format!("/snap/chromium/current/usr/lib/chromium-browser/WidevineCdm/_platform_specific/{plat}/libwidevinecdm.so"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

fn try_copy_from_system() -> Option<()> {
    for src in system_cdm_candidates() {
        if src.exists() {
            let dest_dir = dir();
            std::fs::create_dir_all(&dest_dir).ok()?;
            std::fs::copy(&src, cdm_path()).ok()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(cdm_path(), std::fs::Permissions::from_mode(0o755));
            }
            tracing::info!("widevine: copied CDM from {}", src.display());
            return Some(());
        }
    }
    None
}

// ── Strategy 2: Omaha XML download ───────────────────────────────────────────

/// POST to Google's Omaha update server; parse codebase + package name.
async fn omaha_fetch_url(client: &reqwest::Client) -> Result<String> {
    let arch = arch_tag();
    let body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<request protocol="3.0">
  <os platform="linux" arch="{arch}"/>
  <app appid="{id}" version="0.0.0.0">
    <updatecheck/>
  </app>
</request>"#,
        arch = arch,
        id   = WIDEVINE_COMPONENT_ID,
    );

    let xml = client
        .post("https://clients2.google.com/service/update2")
        .header("Content-Type", "application/xml")
        .header(
            "User-Agent",
            "GoogleUpdate/1.3.36.372;winhttp;cup-ecdsa",
        )
        .body(body)
        .send()
        .await
        .context("Omaha POST failed")?
        .text()
        .await
        .context("reading Omaha response")?;

    tracing::debug!("widevine omaha response: {}", xml);

    // Extract codebase="..." and name="..." without an XML crate.
    let codebase = xml
        .split("codebase=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .ok_or_else(|| anyhow::anyhow!("codebase not found in Omaha response"))?
        .to_string();

    let package_name = xml
        .split("name=\"")
        .find_map(|s| {
            let n = s.split('"').next()?;
            if n.ends_with(".zip") { Some(n.to_string()) } else { None }
        })
        .ok_or_else(|| anyhow::anyhow!("package name (.zip) not found in Omaha response"))?;

    Ok(format!("{}{}", codebase, package_name))
}

/// Find `libwidevinecdm.so` anywhere inside a ZIP archive.
fn extract_cdm_from_zip(bytes: &[u8]) -> Result<Vec<u8>> {
    let cursor  = std::io::Cursor::new(bytes);
    let mut arc = zip::ZipArchive::new(cursor).context("parsing ZIP")?;

    for i in 0..arc.len() {
        let mut entry = arc.by_index(i)?;
        if entry.name().ends_with("libwidevinecdm.so") {
            let mut buf = Vec::new();
            std::io::copy(&mut entry, &mut buf).context("extracting CDM")?;
            return Ok(buf);
        }
    }
    bail!("libwidevinecdm.so not found inside ZIP")
}

async fn download_from_omaha(client: &reqwest::Client) -> Result<()> {
    let url = omaha_fetch_url(client).await?;
    tracing::info!("widevine: downloading ZIP from {}", url);

    let bytes = client
        .get(&url)
        .send()
        .await
        .context("CDM ZIP download")?
        .error_for_status()
        .context("CDM ZIP HTTP error")?
        .bytes()
        .await
        .context("reading CDM ZIP bytes")?;

    tracing::info!("widevine: downloaded {} bytes", bytes.len());
    if bytes.is_empty() {
        bail!("CDM ZIP download returned 0 bytes");
    }

    let cdm = extract_cdm_from_zip(&bytes)?;

    let dest_dir = dir();
    std::fs::create_dir_all(&dest_dir).context("creating widevine dir")?;
    std::fs::write(cdm_path(), &cdm).context("writing libwidevinecdm.so")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cdm_path(), std::fs::Permissions::from_mode(0o755))
            .context("setting CDM permissions")?;
    }

    tracing::info!("widevine: installed {} bytes → {}", cdm.len(), cdm_path().display());
    Ok(())
}

// ── Public entry-point ────────────────────────────────────────────────────────

/// Install the Widevine CDM.  No-op if already present ([`is_installed`]).
pub async fn install() -> Result<()> {
    // Fast path: copy from an already-installed Chrome/Chromium.
    if try_copy_from_system().is_some() {
        return Ok(());
    }
    tracing::info!("widevine: no system CDM found, downloading via Omaha");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("building reqwest client")?;

    download_from_omaha(&client).await
}
