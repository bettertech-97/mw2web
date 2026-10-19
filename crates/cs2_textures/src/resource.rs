//! Source 2 compiled resources (`*_c`): a block table, of which we read the
//! external reference list (RERL) and the DATA block.

pub struct Block {
    pub kind: [u8; 4],
    pub offset: usize,
    pub size: usize,
}

pub fn blocks(data: &[u8]) -> Result<Vec<Block>, String> {
    if data.len() < 16 {
        return Err("resource: too short".into());
    }
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    let block_offset = u32_at(8) as usize;
    let block_count = u32_at(12) as usize;
    let mut at = 8 + block_offset;
    let mut out = Vec::with_capacity(block_count);
    for _ in 0..block_count {
        if at + 12 > data.len() {
            return Err("resource: block table truncated".into());
        }
        let kind: [u8; 4] = data[at..at + 4].try_into().unwrap();
        let offset = at + 4 + u32_at(at + 4) as usize;
        let size = u32_at(at + 8) as usize;
        if offset + size > data.len() {
            return Err(format!(
                "resource: block {} spans past the file",
                String::from_utf8_lossy(&kind)
            ));
        }
        out.push(Block { kind, offset, size });
        at += 12;
    }
    Ok(out)
}


/// The resource names a compiled material (or anything) references.
pub fn external_references(data: &[u8]) -> Vec<String> {
    let Some(rerl) = blocks(data)
        .ok()
        .and_then(|bs| bs.into_iter().find(|b| &b.kind == b"RERL"))
    else {
        return Vec::new();
    };
    let base = rerl.offset;
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    let list = base + u32_at(base) as usize;
    let count = u32_at(base + 4) as usize;
    let mut names = Vec::with_capacity(count);
    for i in 0..count {
        let entry = list + i * 16;
        if entry + 16 > data.len() {
            break;
        }
        let name_at = entry + 8 + u32_at(entry + 8) as usize;
        let Some(end) = data[name_at..].iter().position(|&b| b == 0) else {
            break;
        };
        names.push(String::from_utf8_lossy(&data[name_at..name_at + end]).into_owned());
    }
    names
}
