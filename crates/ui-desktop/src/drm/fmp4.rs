/// Minimal fragmented MP4 / CENC parser.
///
/// Implements just enough of ISO 14496-12 to:
///   1. Extract `default_KID` and `default_IV_size` from an init segment's `tenc` box.
///   2. Rewrite an init segment's `encv`/`enca` sample entry to its plain codec form.
///   3. Per-media-segment: read `senc`, `trun`, and `mdat` to produce a list of
///      `SampleInfo` records (encrypted bytes + encryption parameters).
///   4. Rebuild a media segment with decrypted `mdat` (sans `senc`).
use anyhow::{bail, Context, Result};

// ─── Box reader ───────────────────────────────────────────────────────────────

/// A box (fourcc + payload slice).
#[derive(Clone, Copy)]
pub struct BoxRef<'a> {
    pub fourcc: [u8; 4],
    pub payload: &'a [u8],
    /// Byte offset of this box's first byte within the original buffer.
    pub offset: usize,
    /// Total on-wire size including header.
    pub total_size: usize,
}

/// Walk top-level boxes in `data`.
pub fn boxes(data: &[u8]) -> BoxIter<'_> {
    BoxIter { data, pos: 0 }
}

pub struct BoxIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = BoxRef<'a>;
    fn next(&mut self) -> Option<BoxRef<'a>> {
        if self.pos + 8 > self.data.len() {
            return None;
        }
        let start = self.pos;
        let size32 = u32::from_be_bytes(self.data[start..start + 4].try_into().ok()?) as usize;
        let fourcc: [u8; 4] = self.data[start + 4..start + 8].try_into().ok()?;
        let (header_size, total_size) = if size32 == 1 {
            if start + 16 > self.data.len() {
                return None;
            }
            let s64 = u64::from_be_bytes(self.data[start + 8..start + 16].try_into().ok()?);
            (16, s64 as usize)
        } else if size32 == 0 {
            (8, self.data.len() - start)
        } else {
            (8, size32)
        };
        if total_size < header_size || start + total_size > self.data.len() {
            return None;
        }
        let payload = &self.data[start + header_size..start + total_size];
        self.pos = start + total_size;
        Some(BoxRef {
            fourcc,
            payload,
            offset: start,
            total_size,
        })
    }
}

/// Find the first child box with `fourcc` inside `container_payload`.
pub fn find_box<'a>(container: &'a [u8], fourcc: &[u8; 4]) -> Option<BoxRef<'a>> {
    boxes(container).find(|b| &b.fourcc == fourcc)
}

/// Recursively search for a box fourcc within known container boxes (DFS).
/// Only descends into container fourcc types to avoid treating leaf payloads
/// (avcC, tkhd, mvhd, …) as box streams.  Used for diagnostics only.
pub fn find_box_in_init<'a>(data: &'a [u8], fourcc: &[u8; 4]) -> Option<BoxRef<'a>> {
    const CONTAINERS: &[[u8; 4]] = &[
        *b"moov", *b"trak", *b"mdia", *b"minf", *b"stbl", *b"stsd", *b"sinf", *b"schi", *b"encv",
        *b"enca", *b"avc1", *b"mp4a",
    ];
    for b in boxes(data) {
        if &b.fourcc == fourcc {
            return Some(b);
        }
        if CONTAINERS.contains(&b.fourcc) {
            if let Some(found) = find_box_in_init(b.payload, fourcc) {
                return Some(found);
            }
        }
    }
    None
}

/// Recursively find a box by path (e.g. `["moov","trak","mdia"]`).
#[allow(dead_code)]
pub fn find_box_path<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Option<BoxRef<'a>> {
    if path.is_empty() {
        return None;
    }
    let b = find_box(data, path[0])?;
    if path.len() == 1 {
        Some(b)
    } else {
        find_box_path(b.payload, &path[1..])
    }
}

// ─── Init-segment parsing ─────────────────────────────────────────────────────

/// Info extracted from a CENC init segment.
#[derive(Debug, Clone)]
pub struct InitInfo {
    pub default_kid: [u8; 16],
    pub default_iv_size: u8,
    /// 1 = CENC (AES-128-CTR), 2 = CBCS (AES-128-CBC).  Sourced from `sinf/schm`
    /// (Strategy 1) or the MPD `ContentProtection value` attribute (Strategy 2).
    pub encryption_scheme: u32,
}

/// Parse a DASH init segment and return CENC parameters.
///
/// Strategy (tried in order):
/// 1. `moov/trak/.../stsd/encv|enca/sinf/schi/tenc` — classic CENC init.
///    Encryption scheme is read from `sinf/schm` (`cenc`→1, `cbcs`→2).
/// 2. `moov/pssh` (Widevine) — CMAF-style init where `tenc` is absent and the
///    KID lives in a `pssh` box at the `moov` level.  IV size defaults to 8.
///    `scheme_hint` is used as the encryption scheme (caller should supply it
///    from the MPD `ContentProtection value` attribute before it is stripped).
///
/// Returns `Ok(None)` if the segment has no encryption info at all (clear track).
pub fn parse_init_segment(data: &[u8], scheme_hint: u32) -> Result<Option<InitInfo>> {
    // Path: moov → trak → mdia → minf → stbl → stsd → (encv|enca) → sinf → schi → tenc
    let moov = match find_box(data, b"moov") {
        Some(b) => b,
        None => return Ok(None),
    };
    // Strategy 1: classic tenc path.
    for trak in boxes(moov.payload).filter(|b| &b.fourcc == b"trak") {
        if let Some(info) = parse_trak_tenc(trak.payload)? {
            return Ok(Some(info));
        }
    }
    // Strategy 2: KID from Widevine PSSH at moov level (CMAF-style, no encv/enca).
    for pssh in boxes(moov.payload).filter(|b| &b.fourcc == b"pssh") {
        if let Some(kid) = extract_kid_from_widevine_pssh(pssh.payload) {
            return Ok(Some(InitInfo {
                default_kid: kid,
                default_iv_size: 8,
                encryption_scheme: scheme_hint,
            }));
        }
    }
    Ok(None)
}

/// Extract the complete Widevine PSSH box bytes from a DASH init segment.
///
/// Returns the full box (size + "pssh" + payload) suitable for passing directly
/// to `CdmHandle::create_session`.  The box is extracted verbatim from the CDN
/// response so it retains any provider/content_id fields the license server needs.
pub fn extract_widevine_pssh(data: &[u8]) -> Option<Vec<u8>> {
    let moov = find_box(data, b"moov")?;
    for pssh in boxes(moov.payload).filter(|b| &b.fourcc == b"pssh") {
        // pssh.payload = version(1)+flags(3)+SystemID(16)+...
        if pssh.payload.len() >= 20 && pssh.payload[4..20] == WV_SYSTEM_ID {
            let start = pssh.offset;
            let end = pssh.offset + pssh.total_size;
            if end <= moov.payload.len() {
                return Some(moov.payload[start..end].to_vec());
            }
        }
    }
    None
}

/// Widevine system ID bytes.
const WV_SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];

/// Extract the first KID from a Widevine PSSH box payload (the bytes after the
/// box size+fourcc header).
///
/// Supports both PSSH v0 (KID in WidevineCencHeader proto, field 2) and
/// PSSH v1 (KID list in the box header).
fn extract_kid_from_widevine_pssh(payload: &[u8]) -> Option<[u8; 16]> {
    // Full-box header: version(1) + flags(3) = 4 bytes, then SystemID(16).
    if payload.len() < 20 {
        return None;
    }
    let version = payload[0];
    if payload[4..20] != WV_SYSTEM_ID {
        return None;
    }

    if version == 1 {
        // v1: KID count (4 bytes) then KIDs.
        if payload.len() < 24 {
            return None;
        }
        let kid_count = u32::from_be_bytes(payload[20..24].try_into().ok()?) as usize;
        if kid_count > 0 && payload.len() >= 40 {
            return payload[24..40].try_into().ok();
        }
    } else {
        // v0: data_size(4) + WidevineCencHeader protobuf.
        if payload.len() < 24 {
            return None;
        }
        let data_size = u32::from_be_bytes(payload[20..24].try_into().ok()?) as usize;
        if payload.len() < 24 + data_size {
            return None;
        }
        let proto = &payload[24..24 + data_size];
        // Minimal protobuf scan for field 2 (key_id), wire type 2 (LEN).
        let mut pos = 0;
        while pos < proto.len() {
            let tag = proto[pos];
            pos += 1;
            let wire = tag & 0x07;
            let field = tag >> 3;
            match wire {
                0 => {
                    // varint — skip
                    while pos < proto.len() && proto[pos] & 0x80 != 0 {
                        pos += 1;
                    }
                    if pos < proto.len() {
                        pos += 1;
                    }
                }
                2 => {
                    // LEN
                    if pos >= proto.len() {
                        break;
                    }
                    let len = proto[pos] as usize;
                    pos += 1;
                    if field == 2 && len == 16 && pos + 16 <= proto.len() {
                        return proto[pos..pos + 16].try_into().ok();
                    }
                    pos += len;
                }
                _ => break,
            }
        }
    }
    None
}

fn parse_trak_tenc(trak: &[u8]) -> Result<Option<InitInfo>> {
    let mdia = match find_box(trak, b"mdia") {
        Some(b) => b,
        None => return Ok(None),
    };
    let minf = match find_box(mdia.payload, b"minf") {
        Some(b) => b,
        None => return Ok(None),
    };
    let stbl = match find_box(minf.payload, b"stbl") {
        Some(b) => b,
        None => return Ok(None),
    };
    let stsd = match find_box(stbl.payload, b"stsd") {
        Some(b) => b,
        None => return Ok(None),
    };
    // stsd version(1)+flags(3)+entry_count(4) then N entries
    if stsd.payload.len() < 8 {
        return Ok(None);
    }
    let entry_data = &stsd.payload[8..];
    // Try encv then enca
    for enc_type in [b"encv", b"enca"] {
        if let Some(enc) = find_box(entry_data, enc_type) {
            // VisualSampleEntry / AudioSampleEntry has a 6-byte reserved + 2-byte data-ref-index
            // = 8 bytes before nested boxes.  VisualSampleEntry adds 70 bytes of video params.
            // We just scan children of enc.payload for sinf.
            if let Some(sinf) = find_box(enc.payload, b"sinf") {
                // Detect encryption scheme from sinf/schm.
                // SchemeTypeBox layout: FullBox header (version 1B + flags 3B) + scheme_type 4CC.
                let encryption_scheme = find_box(sinf.payload, b"schm")
                    .filter(|b| b.payload.len() >= 8)
                    .map(|b| {
                        if &b.payload[4..8] == b"cbcs" {
                            2u32
                        } else {
                            1u32
                        }
                    })
                    .unwrap_or(1u32);
                if let Some(schi) = find_box(sinf.payload, b"schi") {
                    if let Some(tenc) = find_box(schi.payload, b"tenc") {
                        return parse_tenc(tenc.payload, encryption_scheme).map(Some);
                    }
                }
            }
        }
    }
    Ok(None)
}

fn parse_tenc(payload: &[u8], encryption_scheme: u32) -> Result<InitInfo> {
    // version(1) + flags(3) + reserved(1) + crypt/skip(1) + default_isEncrypted(1) +
    // default_IV_size(1) + default_KID(16)
    if payload.len() < 20 {
        bail!("tenc box too short ({} bytes)", payload.len());
    }
    let default_iv_size = payload[7];
    let mut default_kid = [0u8; 16];
    default_kid.copy_from_slice(&payload[8..24]);
    Ok(InitInfo {
        default_kid,
        default_iv_size,
        encryption_scheme,
    })
}

/// Rewrite an init segment: replace `encv`/`enca` sample entries with their
/// plain codec equivalents (e.g., `avc1`/`mp4a`) by stripping the `sinf` box
/// and relabelling the box type.  Also removes `pssh` boxes from `moov`.
///
/// This is required so that ffmpeg/mpv can read codec parameters (pixel format,
/// SPS/PPS) from the unencrypted H.264/AAC bitstream after the proxy has
/// pre-decrypted all media segments.
///
/// Returns a rebuilt copy.  Falls back to a raw clone if `moov` is not found.
pub fn strip_encryption_from_init(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for b in boxes(data) {
        if &b.fourcc == b"moov" {
            let inner = strip_enc_moov(b.payload);
            push_iso_box(&mut out, b"moov", &inner);
        } else {
            out.extend_from_slice(&data[b.offset..b.offset + b.total_size]);
        }
    }
    out
}

/// Rebuild `moov` payload: strip `pssh`, recurse into each `trak`.
fn strip_enc_moov(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    for b in boxes(payload) {
        match &b.fourcc {
            b"pssh" => {} // remove all pssh boxes
            b"trak" => {
                // Path: trak → mdia → minf → stbl → stsd
                let inner = strip_enc_path(b.payload, &[b"mdia", b"minf", b"stbl", b"stsd"]);
                push_iso_box(&mut out, b"trak", &inner);
            }
            _ => out.extend_from_slice(&payload[b.offset..b.offset + b.total_size]),
        }
    }
    out
}

/// Walk a chain of container boxes, passing unrelated boxes through verbatim,
/// and recursing into the named containers.  The last name in `chain` is `stsd`.
fn strip_enc_path(payload: &[u8], chain: &[&[u8; 4]]) -> Vec<u8> {
    if chain.is_empty() {
        return payload.to_vec();
    }
    let target = chain[0];
    let rest = &chain[1..];
    let mut out = Vec::with_capacity(payload.len());
    for b in boxes(payload) {
        if &b.fourcc == target {
            let inner = if rest.is_empty() {
                strip_enc_stsd(b.payload) // reached stsd
            } else {
                strip_enc_path(b.payload, rest) // keep descending
            };
            push_iso_box(&mut out, target, &inner);
        } else {
            out.extend_from_slice(&payload[b.offset..b.offset + b.total_size]);
        }
    }
    out
}

/// Rebuild `stsd` payload: convert `encv`/`enca` entries to plain codec form.
fn strip_enc_stsd(payload: &[u8]) -> Vec<u8> {
    // stsd: version(1)+flags(3)+entry_count(4) = 8-byte prefix before entries.
    if payload.len() < 8 {
        return payload.to_vec();
    }
    let mut out = payload[..8].to_vec();
    let entries = &payload[8..];
    for entry in boxes(entries) {
        let raw = &entries[entry.offset..entry.offset + entry.total_size];
        if &entry.fourcc == b"encv" || &entry.fourcc == b"enca" {
            let is_visual = &entry.fourcc == b"encv";
            match unprotect_sample_entry(entry.payload, is_visual) {
                Some(plain) => out.extend_from_slice(&plain),
                None => out.extend_from_slice(raw), // fallback: keep as-is
            }
        } else {
            out.extend_from_slice(raw);
        }
    }
    out
}

/// Convert an `encv` or `enca` payload to its unprotected plain-codec form.
///
/// * Reads the original codec type from `sinf/frma`.
/// * Rebuilds the box with the original fourcc and without the `sinf` child.
/// * Returns full box bytes (8-byte header + fixed fields + child boxes) or `None`.
///
/// Fixed-field sizes (after the 8-byte box header, before child boxes):
///   `encv` VisualSampleEntry  — 78 bytes
///   `enca` AudioSampleEntry   — 28 bytes
fn unprotect_sample_entry(payload: &[u8], is_visual: bool) -> Option<Vec<u8>> {
    let fixed = if is_visual { 78usize } else { 28usize };
    if payload.len() < fixed {
        return None;
    }

    let sinf = find_box(payload, b"sinf")?;
    let frma = find_box(sinf.payload, b"frma")?;
    if frma.payload.len() < 4 {
        return None;
    }
    let orig_type: [u8; 4] = frma.payload[..4].try_into().ok()?;

    // Collect child boxes, skipping sinf.
    let mut children = Vec::new();
    for child in boxes(&payload[fixed..]) {
        if &child.fourcc != b"sinf" {
            let raw = &payload[fixed..][child.offset..child.offset + child.total_size];
            children.extend_from_slice(raw);
        }
    }

    let total = 8 + fixed + children.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&orig_type);
    out.extend_from_slice(&payload[..fixed]);
    out.extend_from_slice(&children);
    Some(out)
}

fn push_iso_box(out: &mut Vec<u8>, fourcc: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(payload);
}

// ─── Media-segment parsing ────────────────────────────────────────────────────

/// Encryption parameters for a single sample.
#[derive(Debug, Clone)]
pub struct SampleEncInfo {
    pub iv: Vec<u8>,
    pub subsamples: Vec<(u32, u32)>, // (clear_bytes, cipher_bytes)
}

/// A sample's position within `mdat` and its encryption info.
#[derive(Debug, Clone)]
pub struct SampleInfo {
    pub mdat_offset: usize, // byte offset from start of mdat payload
    pub size: usize,
    pub enc: Option<SampleEncInfo>, // None = unencrypted
    pub decode_time: u64,
}

/// Parsed media segment.
pub struct ParsedSegment<'a> {
    pub samples: Vec<SampleInfo>,
    pub mdat_payload: &'a [u8],
    /// Byte offset of the mdat box header within the original buffer.
    pub mdat_box_offset: usize,
    pub mdat_box_size: usize,
}

/// Parse a DASH media segment.
///
/// `iv_size` comes from the init segment's `tenc` box.
pub fn parse_media_segment<'a>(data: &'a [u8], iv_size: u8) -> Result<ParsedSegment<'a>> {
    let moof = find_box(data, b"moof").context("no moof box in media segment")?;
    let traf = find_box(moof.payload, b"traf").context("no traf in moof")?;

    let _tfhd = find_box(traf.payload, b"tfhd").context("no tfhd in traf")?;
    let trun = find_box(traf.payload, b"trun").context("no trun in traf")?;
    let senc = find_box(traf.payload, b"senc"); // may be absent for unencrypted tracks

    let base_decode_time = parse_tfdt(traf.payload);
    let (_data_offset_delta, sample_sizes) = parse_trun(trun.payload)?;
    let enc_infos = if let Some(senc_b) = senc {
        parse_senc(senc_b.payload, iv_size, sample_sizes.len())?
    } else {
        vec![]
    };

    // mdat box
    let mdat_b = find_box(data, b"mdat").context("no mdat in media segment")?;
    let mdat_box_offset = mdat_b.offset;
    let mdat_box_size = mdat_b.total_size;
    let mdat_payload = mdat_b.payload;

    // Build sample list
    let mut samples = Vec::with_capacity(sample_sizes.len());
    let mut offset_in_mdat: usize = 0;
    for (i, &sz) in sample_sizes.iter().enumerate() {
        let enc = if i < enc_infos.len() {
            Some(enc_infos[i].clone())
        } else {
            None
        };
        samples.push(SampleInfo {
            mdat_offset: offset_in_mdat,
            size: sz,
            enc,
            decode_time: base_decode_time + i as u64, // approximate; good enough for CDM
        });
        offset_in_mdat += sz;
    }

    Ok(ParsedSegment {
        samples,
        mdat_payload,
        mdat_box_offset,
        mdat_box_size,
    })
}

fn parse_tfdt(traf_payload: &[u8]) -> u64 {
    let b = match find_box(traf_payload, b"tfdt") {
        Some(b) => b,
        None => return 0,
    };
    let p = b.payload;
    if p.is_empty() {
        return 0;
    }
    let version = p[0];
    if version == 1 && p.len() >= 12 {
        u64::from_be_bytes(p[4..12].try_into().unwrap_or([0u8; 8]))
    } else if p.len() >= 8 {
        u32::from_be_bytes(p[4..8].try_into().unwrap_or([0u8; 4])) as u64
    } else {
        0
    }
}

fn parse_trun(payload: &[u8]) -> Result<(i32, Vec<usize>)> {
    if payload.len() < 8 {
        bail!("trun too short");
    }
    let _version = payload[0];
    let flags = u32::from_be_bytes([0, payload[1], payload[2], payload[3]]);
    let sample_count = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as usize;

    let mut cursor = 8usize;
    let data_offset = if flags & 0x000001 != 0 {
        if cursor + 4 > payload.len() {
            bail!("trun data_offset truncated");
        }
        let v = i32::from_be_bytes(payload[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;
        v
    } else {
        0
    };

    if flags & 0x000004 != 0 {
        cursor += 4;
    } // first_sample_flags

    let has_duration = flags & 0x000100 != 0;
    let has_size = flags & 0x000200 != 0;
    let has_sflags = flags & 0x000400 != 0;
    let has_cts = flags & 0x000800 != 0;

    let per_sample_size =
        (has_duration as usize + has_size as usize + has_sflags as usize + has_cts as usize) * 4;

    let mut sizes = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        if cursor + per_sample_size > payload.len() {
            break;
        }
        let mut c2 = cursor;
        if has_duration {
            c2 += 4;
        }
        let sz = if has_size {
            u32::from_be_bytes(payload[c2..c2 + 4].try_into().unwrap()) as usize
        } else {
            0
        };
        sizes.push(sz);
        cursor += per_sample_size;
    }

    Ok((data_offset, sizes))
}

fn parse_senc(payload: &[u8], iv_size: u8, sample_count: usize) -> Result<Vec<SampleEncInfo>> {
    if payload.len() < 8 {
        bail!("senc too short");
    }
    let flags = u32::from_be_bytes([0, payload[1], payload[2], payload[3]]);
    let cnt = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as usize;
    let has_subsamples = flags & 0x000002 != 0;

    let mut cursor = 8usize;
    let mut infos = Vec::with_capacity(cnt.min(sample_count));

    for _ in 0..cnt.min(sample_count) {
        let iv_sz = iv_size as usize;
        if cursor + iv_sz > payload.len() {
            break;
        }
        let iv = payload[cursor..cursor + iv_sz].to_vec();
        cursor += iv_sz;

        let subsamples = if has_subsamples {
            if cursor + 2 > payload.len() {
                break;
            }
            let n = u16::from_be_bytes(payload[cursor..cursor + 2].try_into().unwrap()) as usize;
            cursor += 2;
            let mut subs = Vec::with_capacity(n);
            for _ in 0..n {
                if cursor + 6 > payload.len() {
                    break;
                }
                let clear =
                    u16::from_be_bytes(payload[cursor..cursor + 2].try_into().unwrap()) as u32;
                let cipher =
                    u32::from_be_bytes(payload[cursor + 2..cursor + 6].try_into().unwrap());
                subs.push((clear, cipher));
                cursor += 6;
            }
            subs
        } else {
            vec![]
        };

        infos.push(SampleEncInfo { iv, subsamples });
    }

    Ok(infos)
}

/// Rebuild a media segment with decrypted sample data.
///
/// Replaces the mdat payload and removes the `senc` box from `traf`.
/// All other boxes (moof, trun, etc.) are preserved.
pub fn rebuild_segment(
    original: &[u8],
    decrypted_samples: &[Vec<u8>],
    parsed: &ParsedSegment,
) -> Vec<u8> {
    // Build new mdat payload.
    let mut new_mdat_payload: Vec<u8> = Vec::new();
    for s in decrypted_samples {
        new_mdat_payload.extend_from_slice(s);
    }

    // Build new mdat box.
    let new_mdat_size = 8 + new_mdat_payload.len();
    let mut new_mdat_box = Vec::with_capacity(new_mdat_size);
    new_mdat_box.extend_from_slice(&(new_mdat_size as u32).to_be_bytes());
    new_mdat_box.extend_from_slice(b"mdat");
    new_mdat_box.extend_from_slice(&new_mdat_payload);

    // Remove senc from the moof region.
    let moof_end = parsed.mdat_box_offset; // mdat immediately follows moof
    let mut new_buf = remove_senc_from_moof(&original[..moof_end]);

    // Append new mdat.
    new_buf.extend_from_slice(&new_mdat_box);

    // Append anything after the original mdat (rare, but possible).
    let after_mdat = parsed.mdat_box_offset + parsed.mdat_box_size;
    if after_mdat < original.len() {
        new_buf.extend_from_slice(&original[after_mdat..]);
    }

    new_buf
}

/// Remove the `senc` box from a moof region.
fn remove_senc_from_moof(moof_region: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(moof_region.len());
    for b in boxes(moof_region) {
        if &b.fourcc == b"moof" {
            // Recurse: rebuild moof without senc.
            let inner = remove_senc_from_traf(b.payload);
            let new_size = (8 + inner.len()) as u32;
            out.extend_from_slice(&new_size.to_be_bytes());
            out.extend_from_slice(b"moof");
            out.extend_from_slice(&inner);
        } else {
            // Copy box verbatim.
            out.extend_from_slice(&moof_region[b.offset..b.offset + b.total_size]);
        }
    }
    if out.is_empty() {
        out.extend_from_slice(moof_region);
    }
    out
}

fn remove_senc_from_traf(moof_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(moof_payload.len());
    for b in boxes(moof_payload) {
        if &b.fourcc == b"traf" {
            let (inner, removed_bytes) = remove_enc_boxes_from_traf(b.payload);
            let new_size = (8 + inner.len()) as u32;
            out.extend_from_slice(&new_size.to_be_bytes());
            out.extend_from_slice(b"traf");
            // Fix trun.data_offset: we removed `removed_bytes` from moof payload,
            // so the byte offset from moof start to mdat data decreases by that amount.
            out.extend_from_slice(&fix_traf_trun_offset(&inner, -(removed_bytes as i32)));
        } else {
            out.extend_from_slice(&moof_payload[b.offset..b.offset + b.total_size]);
        }
    }
    if out.is_empty() {
        out.extend_from_slice(moof_payload);
    }
    out
}

/// Remove encryption-related boxes from a `traf` payload.
/// Returns `(new_traf_payload, total_bytes_removed)`.
///
/// Strips `senc`, `saiz`, and `saio` — all three carry per-sample encryption
/// metadata that is no longer valid after the proxy has pre-decrypted the samples.
fn remove_enc_boxes_from_traf(traf_payload: &[u8]) -> (Vec<u8>, usize) {
    const DROP: &[[u8; 4]] = &[*b"senc", *b"saiz", *b"saio"];
    let mut out = Vec::with_capacity(traf_payload.len());
    let mut removed = 0usize;
    for b in boxes(traf_payload) {
        if DROP.contains(&b.fourcc) {
            removed += b.total_size;
        } else {
            out.extend_from_slice(&traf_payload[b.offset..b.offset + b.total_size]);
        }
    }
    if out.is_empty() {
        out.extend_from_slice(traf_payload);
        removed = 0;
    }
    (out, removed)
}

/// Walk a rebuilt `traf` payload and adjust each `trun.data_offset` by `delta`.
///
/// `trun.data_offset` is relative to the start of the enclosing `moof` box.
/// When we shorten `moof` by removing enc boxes from `traf`, `data_offset`
/// must decrease by the same amount so ffmpeg finds the sample data correctly.
fn fix_traf_trun_offset(traf_payload: &[u8], delta: i32) -> Vec<u8> {
    if delta == 0 {
        return traf_payload.to_vec();
    }
    let mut out = Vec::with_capacity(traf_payload.len());
    for b in boxes(traf_payload) {
        if &b.fourcc == b"trun" {
            let fixed = adjust_trun_data_offset(b.payload, delta);
            let sz = (8 + fixed.len()) as u32;
            out.extend_from_slice(&sz.to_be_bytes());
            out.extend_from_slice(b"trun");
            out.extend_from_slice(&fixed);
        } else {
            out.extend_from_slice(&traf_payload[b.offset..b.offset + b.total_size]);
        }
    }
    out
}

/// Add `delta` to `trun.data_offset` if the data-offset flag is set.
fn adjust_trun_data_offset(payload: &[u8], delta: i32) -> Vec<u8> {
    if payload.len() < 12 {
        return payload.to_vec();
    }
    let flags = u32::from_be_bytes([0, payload[1], payload[2], payload[3]]);
    if flags & 0x000001 == 0 {
        return payload.to_vec();
    } // no data-offset field
    let old = i32::from_be_bytes(payload[8..12].try_into().unwrap());
    let mut out = payload.to_vec();
    out[8..12].copy_from_slice(&(old + delta).to_be_bytes());
    out
}
