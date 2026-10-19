use super::decode::{DecodedMips, MipStorage, decode_iwi_mips};
use std::sync::atomic::{AtomicU64, Ordering};

const MIP_CACHE_FORMAT: u32 = 2;
const MIP_MAGIC: &[u8; 8] = b"IWL1MIPS";
static MIP_HIT: AtomicU64 = AtomicU64::new(0);
static MIP_MISS: AtomicU64 = AtomicU64::new(0);
static MIP_IO_NS: AtomicU64 = AtomicU64::new(0);

pub fn mip_cache_cost() -> (u64, u64, f64) {
    (
        MIP_HIT.load(Ordering::Relaxed),
        MIP_MISS.load(Ordering::Relaxed),
        MIP_IO_NS.load(Ordering::Relaxed) as f64 / 1.0e6,
    )
}

fn encode_cache(mips: &DecodedMips) -> Vec<u8> {
    let mut out = Vec::with_capacity(28 + 4 * mips.level_sizes().len() + mips.payload().len());
    out.extend_from_slice(MIP_MAGIC);
    out.extend_from_slice(&MIP_CACHE_FORMAT.to_le_bytes());
    out.extend_from_slice(&mips.layout().width.to_le_bytes());
    out.extend_from_slice(&mips.layout().height.to_le_bytes());
    out.push(match mips.layout().storage {
        MipStorage::R32Float => 6,
        MipStorage::Rgba8 => 0,
        MipStorage::Bc1 => 1,
        MipStorage::Bc2 => 2,
        MipStorage::Bc3 => 3,
        MipStorage::Bc5 => 5,
    });
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&(mips.level_sizes().len() as u32).to_le_bytes());
    let mut at = 0usize;
    for &size in mips.level_sizes() {
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&mips.payload()[at..at + size as usize]);
        at += size as usize;
    }
    out
}

fn decode_cache(bytes: &[u8]) -> Option<DecodedMips> {
    if bytes.len() < 28 || &bytes[..8] != MIP_MAGIC {
        return None;
    }
    let word = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    if word(8)? != MIP_CACHE_FORMAT {
        return None;
    }
    let width = word(12)?;
    let height = word(16)?;
    let storage = match bytes[20] {
        6 => MipStorage::R32Float,
        0 => MipStorage::Rgba8,
        1 => MipStorage::Bc1,
        2 => MipStorage::Bc2,
        3 => MipStorage::Bc3,
        5 => MipStorage::Bc5,
        _ => return None,
    };
    let count = word(24)? as usize;
    if count == 0 || count > bytes.len().saturating_sub(28) / 4 {
        return None;
    }
    let mut at = 28usize;
    let mut packed = Vec::with_capacity(bytes.len().saturating_sub(28));
    let mut sizes = Vec::with_capacity(count);
    for _ in 0..count {
        let size = word(at)?;
        at = at.checked_add(4)?;
        let end = at.checked_add(size as usize)?;
        packed.extend_from_slice(bytes.get(at..end)?);
        at = end;
        sizes.push(size);
    }
    if at != bytes.len() {
        return None;
    }
    DecodedMips::from_packed(width, height, storage, packed, sizes)
}

fn read_cached_mips(key: &str, io_at: web_time::Instant) -> Option<DecodedMips> {
    let hit = crate::cache_get("mips", key)?;
    MIP_IO_NS.fetch_add(io_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let mips = decode_cache(&hit)?;
    MIP_HIT.fetch_add(1, Ordering::Relaxed);
    Some(mips)
}

fn mip_cache_key(crc: u32, size: u64, entry: &str) -> String {
    format!(
        "{MIP_CACHE_FORMAT:08x}-{crc:08x}-{size:x}-{:016x}",
        crate::fnv1a64(entry.as_bytes())
    )
}

pub(super) fn load_or_decode_mips(
    candidate: &asset_transport::IwdFile,
) -> Result<DecodedMips, String> {
    // Glyph atlases reach here as ordinary material images; capped to 128 px
    // the HUD loses letters, so fonts are read at their authored size. A
    // Source map's own textures follow the page's cap with a floor instead
    // of the game's 64 px: world textures (`cs2_*`) keep 512 px, which at
    // 720p reads as the 1024 the pack holds and costs a quarter of the bytes
    // to fetch, decode and upload; prop textures (`cs2m_*`) keep 256 px.
    let entry = candidate.entry().to_ascii_lowercase();
    if entry.contains("font") {
        return super::decode::with_uncapped_textures(|| load_or_decode_mips_capped(candidate));
    }
    if entry.contains("/cs2m_") {
        return super::decode::with_texture_cap_floor(256, || load_or_decode_mips_capped(candidate));
    }
    if entry.contains("/cs2_") {
        return super::decode::with_texture_cap_floor(512, || load_or_decode_mips_capped(candidate));
    }
    load_or_decode_mips_capped(candidate)
}

fn load_or_decode_mips_capped(
    candidate: &asset_transport::IwdFile,
) -> Result<DecodedMips, String> {
    let key = mip_cache_key(candidate.crc32(), candidate.size(), candidate.entry());
    let io_at = web_time::Instant::now();
    if let Some(mips) = read_cached_mips(&key, io_at) {
        return Ok(mips);
    }

    let _flight = crate::cache_flight("mips", &key);
    if let Some(mips) = read_cached_mips(&key, web_time::Instant::now()) {
        return Ok(mips);
    }
    let bytes = read_capped(candidate)?;
    let mips = decode_iwi_mips(&bytes)?;
    let blob = encode_cache(&mips);
    let store_at = web_time::Instant::now();
    if let Err(error) = crate::cache_put("mips", &key, &blob) {
        diag::warn!(Zone, "mip cache store {key}: {error}");
    }
    MIP_IO_NS.fetch_add(store_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
    MIP_MISS.fetch_add(1, Ordering::Relaxed);
    Ok(mips)
}

/// The browser build keeps no level above `WEB_MAX_TEXTURE`, so it reads only
/// the prefix that holds the levels it keeps: the header's picmip table names
/// the exact size, and the decoder reads such a prefix as the smaller texture
/// it is. Over a tunnel a 2048² texture then costs a sixteenth of its bytes.
/// Desktop reads the whole file.
fn read_capped(candidate: &asset_transport::IwdFile) -> Result<Vec<u8>, String> {
    #[cfg(target_arch = "wasm32")]
    {
        let head = candidate.read_header(asset_iw4::IWI_V8_HEADER_LEN)?;
        if let Some(prefix) =
            super::decode::picmip_prefix(&head, super::decode::web_max_texture())
            && (prefix as u64) < candidate.size()
        {
            return candidate.read_header(prefix);
        }
    }
    candidate.read()
}
