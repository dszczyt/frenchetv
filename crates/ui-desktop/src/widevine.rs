/// Widevine CDM installer.
///
/// Strategies (tried in order):
///   1. Copy `libwidevinecdm.so` from a locally-installed Chrome/Chromium.
///   2. Download via Google's Omaha update server (XML POST → ZIP extract).
///   3. Download a Chrome OS recovery image, parse its GPT, and extract the CDM
///      from ROOT-A (partition 3, squashfs) using pure-Rust backhand.
///
/// The CDM is stored at `~/.local/share/frenchetv/widevine/libwidevinecdm.so`.
/// mpv picks it up via `--cdm-store=<dir>` when compiled with `--enable-cdm`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use anyhow::{bail, Context, Result};

const WIDEVINE_COMPONENT_ID: &str = "oimompecagnajdejgnnjijobebaeigek";

/// Chrome OS recovery manifest.
const RECOVERY_MANIFEST_URL: &str =
    "https://dl.google.com/dl/edgedl/chromeos/recovery/recovery.json";

/// Board name substrings that identify ARM boards in the recovery manifest.
/// Sorted roughly by image size (smallest first) so the filter picks the
/// smallest matching entry.
const ARM_BOARDS: &[&str] = &[
    "veyron", "nyan", "daisy", "snow", "kevin", "trogdor",
    "kukui", "jacuzzi", "corsola", "strongbad",
];

/// Board name substrings for small x86_64 boards (Haswell era).
const X86_BOARDS: &[&str] = &[
    "link", "peppy", "falco", "wolf", "clapper", "squawks",
];

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

fn platform_dir() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "linux_arm64",
        _         => "linux_x64",
    }
}

fn arch_tag() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _         => "x64",
    }
}

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
        .header("User-Agent", "GoogleUpdate/1.3.36.372;winhttp;cup-ecdsa")
        .body(body)
        .send().await.context("Omaha POST failed")?
        .text().await.context("reading Omaha response")?;

    tracing::debug!("widevine omaha response: {}", xml);

    let codebase = xml
        .split("codebase=\"").nth(1)
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
    tracing::info!("widevine: Omaha → downloading ZIP from {}", url);

    let bytes = client
        .get(&url)
        .send().await.context("CDM ZIP download")?
        .error_for_status().context("CDM ZIP HTTP error")?
        .bytes().await.context("reading CDM ZIP bytes")?;

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

    tracing::info!("widevine: Omaha installed {} bytes → {}", cdm.len(), cdm_path().display());
    Ok(())
}

// ── Strategy 3: Chrome OS recovery image (Kodi approach) ─────────────────────
//
// Recovery flow:
//   1. Fetch recovery.json manifest
//   2. Pick smallest ARM (or x86_64) board image
//   3. Download recovery zip  (~200 MB for VEYRON)
//   4. Extract .bin disk image (~2 GB)
//   5. Verify SHA-1
//   6. Parse GPT → partition 3 = ROOT-A (squashfs)
//   7. unsquashfs -offset <ROOT-A offset> recovery.bin <cdm path>
//   8. Install CDM; clean up temp files

struct RecoveryEntry {
    url:      String,
    sha1:     String,    // SHA-1 of the raw .bin file
    zip_size: u64,
    name:     String,
}

async fn fetch_recovery_manifest(client: &reqwest::Client) -> Result<Vec<RecoveryEntry>> {
    tracing::info!("widevine: fetching Chrome OS recovery manifest");
    let json: Vec<serde_json::Value> = client
        .get(RECOVERY_MANIFEST_URL)
        .send().await.context("GET recovery manifest")?
        .json().await.context("parse recovery manifest")?;

    let entries: Vec<RecoveryEntry> = json.iter().filter_map(|e| {
        Some(RecoveryEntry {
            url:      e.get("url")?.as_str()?.to_string(),
            sha1:     e.get("sha1")?.as_str()?.to_string(),
            zip_size: e.get("zipfilesize")?.as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(u64::MAX),
            name:     e.get("name")?.as_str()?.to_string(),
        })
    }).collect();

    tracing::info!("widevine: recovery manifest: {} entries", entries.len());
    Ok(entries)
}

fn select_recovery_entry(entries: &[RecoveryEntry]) -> Option<&RecoveryEntry> {
    let boards: &[&str] = match std::env::consts::ARCH {
        "aarch64" | "arm" => ARM_BOARDS,
        _                 => X86_BOARDS,
    };

    let entry = entries.iter()
        .filter(|e| {
            let n = e.name.to_lowercase();
            boards.iter().any(|b| n.contains(b))
        })
        .min_by_key(|e| e.zip_size)?;

    tracing::info!(
        "widevine: recovery entry '{}' ({} MB zip)",
        entry.name,
        entry.zip_size / 1_000_000,
    );
    Some(entry)
}

/// Download `url` to `dest`, retrying with exponential backoff.
async fn download_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    label: &str,
) -> Result<()> {
    let mut last_err = anyhow::anyhow!("no attempts");
    for attempt in 1u32..=3 {
        tracing::info!("widevine: downloading {} (attempt {})", label, attempt);
        match try_stream_to_file(client, url, dest).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                if attempt < 3 {
                    let wait = std::time::Duration::from_secs(2u64.pow(attempt));
                    tracing::warn!("widevine: retry in {:?} — {}", wait, last_err);
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
    Err(last_err)
}

async fn try_stream_to_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut resp = client.get(url).send().await.context("GET")?
        .error_for_status().context("HTTP error")?;

    let mut f = tokio::fs::File::create(dest).await.context("create file")?;
    while let Some(chunk) = resp.chunk().await.context("streaming")? {
        f.write_all(&chunk).await.context("write chunk")?;
    }
    f.flush().await.context("flush")?;
    Ok(())
}

/// Verify the SHA-1 of `path` against `expected_hex` (from the manifest).
/// Streams the file in 64 KiB chunks — safe for multi-GB images.
fn verify_sha1(path: &Path, expected_hex: &str) -> Result<()> {
    use sha1::{Digest, Sha1};

    tracing::info!("widevine: verifying SHA-1 of {} …", path.display());
    let mut f = std::fs::File::open(path).context("open for SHA-1")?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf).context("read for SHA-1")?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    let actual = format!("{:x}", hasher.finalize());

    if actual.eq_ignore_ascii_case(expected_hex) {
        tracing::info!("widevine: SHA-1 OK");
        Ok(())
    } else {
        bail!("SHA-1 mismatch: expected {} got {}", expected_hex, actual)
    }
}

/// Extract the single `.bin` file from a recovery zip archive.
fn extract_bin_from_zip(zip_path: &Path, bin_path: &Path) -> Result<()> {
    let file    = std::fs::File::open(zip_path).context("open recovery zip")?;
    let mut arc = zip::ZipArchive::new(file).context("parse recovery zip")?;

    for i in 0..arc.len() {
        let mut entry = arc.by_index(i).context("zip entry")?;
        if entry.name().ends_with(".bin") {
            tracing::info!("widevine: extracting {} from zip…", entry.name());
            let mut out = std::fs::File::create(bin_path).context("create .bin")?;
            std::io::copy(&mut entry, &mut out).context("extract .bin")?;
            return Ok(());
        }
    }
    bail!("no .bin file found in recovery zip")
}

/// Parse GPT partition table, return (start_byte_offset, size_bytes)
/// for `part_num` (1-based, as documented in `fdisk`).
fn gpt_partition_range(bin_path: &Path, part_num: usize) -> Result<(u64, u64)> {
    let mut f = std::fs::File::open(bin_path).context("open .bin")?;

    // GPT header at LBA 1 = byte offset 512
    let mut hdr = [0u8; 512];
    f.seek(SeekFrom::Start(512)).context("seek to GPT header")?;
    f.read_exact(&mut hdr).context("read GPT header")?;

    if &hdr[0..8] != b"EFI PART" {
        bail!("not a GPT disk image (bad signature)");
    }

    let entries_lba = u64::from_le_bytes(hdr[72..80].try_into()?);
    let num_entries = u32::from_le_bytes(hdr[80..84].try_into()?) as usize;
    let entry_size  = u32::from_le_bytes(hdr[84..88].try_into()?) as usize;

    let idx = part_num.checked_sub(1).context("part_num must be ≥ 1")?;
    if idx >= num_entries {
        bail!("partition {} not found ({} entries)", part_num, num_entries);
    }

    let entry_off = entries_lba * 512 + (idx * entry_size) as u64;
    let mut entry = vec![0u8; entry_size];
    f.seek(SeekFrom::Start(entry_off)).context("seek to partition entry")?;
    f.read_exact(&mut entry).context("read partition entry")?;

    let start_lba = u64::from_le_bytes(entry[32..40].try_into()?);
    let end_lba   = u64::from_le_bytes(entry[40..48].try_into()?);

    if start_lba == 0 {
        bail!("partition {} is empty", part_num);
    }

    let offset = start_lba * 512;
    let size   = (end_lba - start_lba + 1) * 512;
    tracing::debug!("widevine: partition {} at offset {} size {} MB", part_num, offset, size / 1_000_000);
    Ok((offset, size))
}

/// Extract `libwidevinecdm.so` from a squashfs filesystem embedded at
/// `sq_offset` bytes inside `bin_path` (a raw GPT disk image).
///
/// Uses the `backhand` crate — no external `unsquashfs` binary required.
fn extract_cdm_from_squashfs(bin_path: &Path, sq_offset: u64) -> Result<Vec<u8>> {
    use backhand::{FilesystemReader, InnerNode};

    let file   = std::fs::File::open(bin_path).context("open .bin for squashfs")?;
    let reader = std::io::BufReader::new(file);

    let fs = FilesystemReader::from_reader_with_offset(reader, sq_offset)
        .map_err(|e| anyhow::anyhow!("squashfs open failed: {e}"))?;

    for node in fs.files() {
        if !node.fullpath.to_string_lossy().ends_with("libwidevinecdm.so") {
            continue;
        }
        if let InnerNode::File(file_reader) = &node.inner {
            let mut reader = fs.file(file_reader).reader();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut buf)
                .context("reading libwidevinecdm.so from squashfs")?;
            tracing::info!(
                "widevine: extracted {} ({} bytes) from squashfs",
                node.fullpath.display(), buf.len(),
            );
            return Ok(buf);
        }
    }
    bail!("libwidevinecdm.so not found in ROOT-A squashfs")
}

/// Full Chrome OS recovery download + extraction flow.
async fn download_from_recovery(client: &reqwest::Client) -> Result<()> {
    let entries = fetch_recovery_manifest(client).await?;
    let entry   = select_recovery_entry(&entries)
        .ok_or_else(|| anyhow::anyhow!(
            "no suitable Chrome OS recovery image found for arch={}",
            std::env::consts::ARCH
        ))?;

    // Temp workspace — cleaned up regardless of success/failure.
    let ts  = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis()).unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("frenchetv_wv_{}", ts));
    std::fs::create_dir_all(&tmp).context("create temp dir")?;

    let result = recovery_install_inner(client, entry, &tmp).await;

    // Always clean up temp dir.
    if let Err(e) = std::fs::remove_dir_all(&tmp) {
        tracing::warn!("widevine: failed to clean up temp dir {}: {}", tmp.display(), e);
    }

    result
}

async fn recovery_install_inner(
    client: &reqwest::Client,
    entry:  &RecoveryEntry,
    tmp:    &Path,
) -> Result<()> {
    // Step 1 — download recovery zip
    let zip_path = tmp.join("recovery.zip");
    download_to_file(client, &entry.url, &zip_path, "Chrome OS recovery zip").await?;

    // Step 2 — extract .bin from zip
    let bin_path = tmp.join("recovery.bin");
    tracing::info!("widevine: extracting .bin (this may take a while)…");
    tokio::task::spawn_blocking({
        let zip_path = zip_path.clone();
        let bin_path = bin_path.clone();
        move || extract_bin_from_zip(&zip_path, &bin_path)
    }).await.context("spawn_blocking extract_bin")??;

    // Remove zip now to free disk space
    let _ = std::fs::remove_file(&zip_path);

    // Step 3 — verify SHA-1
    tokio::task::spawn_blocking({
        let bin_path = bin_path.clone();
        let sha1     = entry.sha1.clone();
        move || verify_sha1(&bin_path, &sha1)
    }).await.context("spawn_blocking sha1")??;

    // Step 4 — parse GPT, find partition 3 (ROOT-A = squashfs)
    let (sq_offset, _sq_size) = tokio::task::spawn_blocking({
        let bin_path = bin_path.clone();
        move || gpt_partition_range(&bin_path, 3)
    }).await.context("spawn_blocking gpt")??;

    // Step 5 — extract CDM from ROOT-A squashfs (pure Rust, no external tools)
    let cdm_bytes = tokio::task::spawn_blocking({
        let bin_path = bin_path.clone();
        move || extract_cdm_from_squashfs(&bin_path, sq_offset)
    }).await.context("spawn_blocking squashfs")??;

    // Step 6 — install CDM
    let dest_dir = dir();
    std::fs::create_dir_all(&dest_dir).context("create widevine dir")?;
    std::fs::write(cdm_path(), &cdm_bytes).context("write libwidevinecdm.so")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cdm_path(), std::fs::Permissions::from_mode(0o755))
            .context("set CDM permissions")?;
    }

    tracing::info!("widevine: CDM installed from Chrome OS recovery → {}", cdm_path().display());
    Ok(())
}

// ── Public entry-point ────────────────────────────────────────────────────────

/// Install the Widevine CDM.  No-op if already present ([`is_installed`]).
pub async fn install() -> Result<()> {
    // Fast path: copy from an already-installed Chrome/Chromium.
    if try_copy_from_system().is_some() {
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))  // large downloads
        .build()
        .context("building reqwest client")?;

    // Strategy 2: Omaha XML (~10 MB download, works on x86_64).
    tracing::info!("widevine: no system CDM; trying Omaha");
    match download_from_omaha(&client).await {
        Ok(()) => return Ok(()),
        Err(e) => tracing::warn!("widevine: Omaha failed ({}); trying Chrome OS recovery", e),
    }

    // Strategy 3: Chrome OS recovery image (reliable for all architectures;
    // ~200 MB zip + ~2 GB .bin temporary disk space required).
    tracing::info!("widevine: downloading Chrome OS recovery image (needs ~2.2 GB temp space)");
    download_from_recovery(&client).await
}
