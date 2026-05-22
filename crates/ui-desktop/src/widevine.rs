/// Widevine CDM downloader.
///
/// Downloads libwidevinecdm.so from Google's component update server and stores
/// it under `~/.local/share/frenchetv/widevine/`.  mpv 0.40+ can load it via
/// `--cdm-store=<dir>` when compiled with `--enable-cdm`.

use std::path::PathBuf;
use anyhow::{bail, Context, Result};

/// Widevine component ID on Google's update server (architecture-independent key).
const WIDEVINE_COMPONENT_ID: &str = "oimompecagnajdejgnnjijobebaeigek";

// ── Paths ─────────────────────────────────────────────────────────────────────

/// `~/.local/share/frenchetv/widevine`
pub fn dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()))
        .join("frenchetv")
        .join("widevine")
}

/// Full path to `libwidevinecdm.so` inside the local store.
pub fn cdm_path() -> PathBuf {
    dir().join("libwidevinecdm.so")
}

/// Returns true when the CDM is already present on disk.
pub fn is_installed() -> bool {
    cdm_path().exists()
}

// ── Architecture ──────────────────────────────────────────────────────────────

fn arch_tag() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64"  => "x64",
        "aarch64" => "arm64",
        _         => "x64",
    }
}

fn nacl_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64"  => "x86-64",
        "aarch64" => "arm64",
        _         => "x86-64",
    }
}

// ── Version query ─────────────────────────────────────────────────────────────

/// Ask Google's Omaha/CRX update endpoint for the current Widevine version.
/// Returns something like `"4.10.3050.0"`.
// ── CRX download & extraction ─────────────────────────────────────────────────

/// Download and unpack `libwidevinecdm.so` from Google's servers.
///
/// The package is a CRX3 file (Chrome Extension) whose payload is a plain ZIP.
/// CRX3 header layout:
///   bytes  0– 3  magic  "Cr24"
///   bytes  4– 7  version 3  (LE uint32)
///   bytes  8–11  header_size  (LE uint32)
///   bytes 12..12+header_size  protobuf header (skipped)
///   rest         ZIP data containing libwidevinecdm.so
fn extract_cdm_from_crx(crx: &[u8]) -> Result<Vec<u8>> {
    if crx.len() < 12 || &crx[0..4] != b"Cr24" {
        bail!("not a CRX3 file (wrong magic)");
    }
    let version = u32::from_le_bytes(crx[4..8].try_into().unwrap());
    if version != 3 {
        bail!("unsupported CRX version {version}; expected 3");
    }
    let header_size = u32::from_le_bytes(crx[8..12].try_into().unwrap()) as usize;
    let zip_start   = 12 + header_size;
    if zip_start >= crx.len() {
        bail!("CRX header truncated");
    }

    let cursor = std::io::Cursor::new(&crx[zip_start..]);
    let mut archive = zip::ZipArchive::new(cursor)
        .context("parsing inner ZIP")?;

    let mut cdm_bytes = Vec::new();
    let mut found = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.name().ends_with("libwidevinecdm.so") {
            std::io::copy(&mut entry, &mut cdm_bytes)
                .context("extracting libwidevinecdm.so")?;
            found = true;
            break;
        }
    }
    if !found {
        bail!("libwidevinecdm.so not found inside CRX/ZIP");
    }
    Ok(cdm_bytes)
}

// ── Public install entry-point ────────────────────────────────────────────────

/// Download the Widevine CDM and store it in the frenchetv local data dir.
///
/// No-op if already installed; callers can check [`is_installed`] first.
pub async fn install() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("building reqwest client")?;

    // Build CRX redirect URL — Google resolves the current version and
    // architecture server-side.  reqwest follows the redirect automatically.
    let arch  = arch_tag();
    let nacl  = nacl_arch();
    let prod  = "110.0.5481.100";
    let url = format!(
        "https://clients2.google.com/service/update2/crx\
         ?response=redirect\
         &acceptformat=crx3\
         &prodversion={prod}\
         &arch={arch}\
         &nacl_arch={nacl}\
         &os=linux\
         &x=id%3D{id}%26v%3D0.0.0.0%26uc",
        prod = prod,
        arch = arch,
        nacl = nacl,
        id   = WIDEVINE_COMPONENT_ID,
    );
    tracing::info!("widevine: downloading from {}", url);

    // Download (follows redirects automatically).
    let crx_bytes = client
        .get(&url)
        .header("User-Agent", format!(
            "Mozilla/5.0 (X11; Linux {raw}) AppleWebKit/537.36 Chrome/{prod}",
            raw  = std::env::consts::ARCH,
            prod = prod,
        ))
        .send()
        .await
        .context("downloading Widevine CRX")?
        .error_for_status()
        .context("Widevine download HTTP error")?
        .bytes()
        .await
        .context("reading Widevine CRX bytes")?;

    tracing::info!("widevine: downloaded {} bytes", crx_bytes.len());

    // Extract.
    let cdm = extract_cdm_from_crx(&crx_bytes)
        .context("extracting Widevine CDM from CRX")?;

    // Write to disk.
    let dest_dir = dir();
    std::fs::create_dir_all(&dest_dir)
        .context("creating widevine dir")?;
    let dest = cdm_path();
    std::fs::write(&dest, &cdm)
        .context("writing libwidevinecdm.so")?;

    // Mark executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .context("setting CDM permissions")?;
    }

    tracing::info!("widevine: installed {} bytes → {}", cdm.len(), dest.display());
    Ok(())
}
