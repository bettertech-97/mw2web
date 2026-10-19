//! Source 2 textures (`.vtex_c`, data version 1): a header in the DATA
//! block, mip levels after the block table from the smallest up, each
//! optionally LZ4-compressed.

use crate::lz4;
use crate::resource;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Dxt1,
    Dxt5,
    I8,
    Rgba8,
    Bgra8,
    Bc7,
    Bc5,
    Bc4,
    Other(u8),
}

impl Format {
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Dxt1,
            2 => Self::Dxt5,
            3 => Self::I8,
            4 => Self::Rgba8,
            20 => Self::Bc7,
            21 => Self::Bc5,
            27 => Self::Bc4,
            28 => Self::Bgra8,
            other => Self::Other(other),
        }
    }

    pub fn block_bytes(self) -> Option<usize> {
        match self {
            Self::Dxt1 | Self::Bc4 => Some(8),
            Self::Dxt5 | Self::Bc7 | Self::Bc5 => Some(16),
            _ => None,
        }
    }

    pub fn level_bytes(self, width: u32, height: u32) -> usize {
        let (w, h) = (width.max(1) as usize, height.max(1) as usize);
        match self {
            Self::I8 => w * h,
            Self::Rgba8 | Self::Bgra8 => w * h * 4,
            Self::Other(_) => w * h * 4,
            compressed => w.div_ceil(4) * h.div_ceil(4) * compressed.block_bytes().unwrap_or(16),
        }
    }
}

pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// Mip level 0 first (the largest), then halving.
    pub levels: Vec<Vec<u8>>,
}

pub fn parse(data: &[u8]) -> Result<Texture, String> {
    let blocks = resource::blocks(data)?;
    let block = blocks
        .iter()
        .find(|b| &b.kind == b"DATA")
        .ok_or("vtex: no DATA block")?;
    let d = &data[block.offset..block.offset + block.size];
    if d.len() < 40 {
        return Err("vtex: DATA block too short".into());
    }
    let u16_at = |at: usize| u16::from_le_bytes([d[at], d[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes(d[at..at + 4].try_into().unwrap());
    let version = u16_at(0);
    if version != 1 {
        return Err(format!("vtex: data version {version}"));
    }
    let flags = u16_at(2);
    let width = u32::from(u16_at(20));
    let height = u32::from(u16_at(22));
    let depth = u16_at(24);
    let format = Format::from_code(d[26]);
    let mip_count = usize::from(d[27]);
    if depth != 1 || flags & 0x10 != 0 || flags & 0x20 != 0 || flags & 0x40 != 0 {
        return Err(format!("vtex: not a plain 2D texture (depth {depth}, flags {flags:#x})"));
    }
    if mip_count == 0 {
        return Err("vtex: no mip levels".into());
    }
    let extra_offset = u32_at(32) as usize;
    let extra_count = u32_at(36) as usize;
    let mut compressed_sizes: Option<Vec<u32>> = None;
    let mut at = 32 + extra_offset;
    for _ in 0..extra_count {
        if at + 12 > d.len() {
            break;
        }
        let kind = u32_at(at);
        let offset = at + 4 + u32_at(at + 4) as usize;
        if kind == 4 && offset + 12 <= d.len() {
            let compressed = u32_at(offset) == 1;
            let mips = u32_at(offset + 8) as usize;
            if compressed && offset + 12 + mips * 4 <= d.len() {
                compressed_sizes = Some(
                    (0..mips)
                        .map(|i| u32_at(offset + 12 + i * 4))
                        .collect(),
                );
            }
        }
        at += 12;
    }
    // Image data follows the last block; levels run from the smallest up.
    let data_start = blocks.iter().map(|b| b.offset + b.size).max().unwrap_or(0);
    let mut pos = data_start;
    let mut levels: Vec<Vec<u8>> = vec![Vec::new(); mip_count];
    for level in (0..mip_count).rev() {
        let w = (width >> level).max(1);
        let h = (height >> level).max(1);
        let raw = format.level_bytes(w, h);
        let stored = compressed_sizes
            .as_ref()
            .and_then(|sizes| sizes.get(level).copied())
            .map_or(raw, |s| s as usize);
        let bytes = data
            .get(pos..pos + stored)
            .ok_or_else(|| format!("vtex: level {level} truncated"))?;
        levels[level] = if stored == raw {
            bytes.to_vec()
        } else {
            lz4::decompress(bytes, raw)?
        };
        pos += stored;
    }
    Ok(Texture {
        width,
        height,
        format,
        levels,
    })
}
