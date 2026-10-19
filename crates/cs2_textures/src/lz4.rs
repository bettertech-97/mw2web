//! LZ4 block decompression (the raw block format, no frame).

pub fn decompress(src: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(expected);
    let mut i = 0usize;
    let n = src.len();
    while i < n {
        let token = src[i];
        i += 1;
        let mut literals = usize::from(token >> 4);
        if literals == 15 {
            loop {
                let b = *src.get(i).ok_or("lz4: truncated literal length")?;
                i += 1;
                literals += usize::from(b);
                if b != 255 {
                    break;
                }
            }
        }
        let end = i
            .checked_add(literals)
            .filter(|end| *end <= n)
            .ok_or("lz4: truncated literals")?;
        out.extend_from_slice(&src[i..end]);
        i = end;
        if i >= n {
            break;
        }
        let offset = usize::from(src[i]) | (usize::from(*src.get(i + 1).ok_or("lz4: truncated offset")?) << 8);
        i += 2;
        if offset == 0 || offset > out.len() {
            return Err(format!("lz4: bad match offset {offset} at {}", out.len()));
        }
        let mut length = usize::from(token & 15);
        if length == 15 {
            loop {
                let b = *src.get(i).ok_or("lz4: truncated match length")?;
                i += 1;
                length += usize::from(b);
                if b != 255 {
                    break;
                }
            }
        }
        length += 4;
        let start = out.len() - offset;
        for k in 0..length {
            let byte = out[start + k];
            out.push(byte);
        }
    }
    if out.len() != expected {
        return Err(format!("lz4: decompressed {} bytes, expected {expected}", out.len()));
    }
    Ok(out)
}
