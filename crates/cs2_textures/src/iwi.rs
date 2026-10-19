//! IW4 texture files (IWI version 8): a 32-byte header, then every mip
//! level from the smallest up. The picmip table in the header names where
//! each of the four largest levels ends, so a reader that caps texture size
//! can stop after a prefix of the file.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IwiFormat {
    Dxt1,
    Dxt5,
}

impl IwiFormat {
    fn code(self) -> u8 {
        match self {
            Self::Dxt1 => 11,
            Self::Dxt5 => 13,
        }
    }
}

/// `levels` run from the largest (level 0) down to 1×1, each already in
/// `format`'s block layout.
pub fn write(width: u32, height: u32, format: IwiFormat, levels: &[Vec<u8>]) -> Vec<u8> {
    let header_len = 32usize;
    let total: usize = header_len + levels.iter().map(Vec::len).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"IWi");
    out.push(8);
    out.push(0); // flags: mipmapped, not a cubemap
    out.push(0);
    out.push(0); // usage: a regular texture
    out.push(0);
    out.push(format.code());
    out.push(0);
    out.extend_from_slice(&(width as u16).to_le_bytes());
    out.extend_from_slice(&(height as u16).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    // The file ends with level 0; the table holds the end of level 0 and
    // the ends of levels 1, 2 and 3 (where the next larger level starts).
    let mut ends = [total as u32; 4];
    let mut cursor = total;
    for (k, level) in levels.iter().enumerate().take(4) {
        ends[k] = cursor as u32;
        cursor -= level.len();
    }
    for end in ends {
        out.extend_from_slice(&end.to_le_bytes());
    }
    for level in levels.iter().rev() {
        out.extend_from_slice(level);
    }
    out
}
