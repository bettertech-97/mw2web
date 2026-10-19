//! Block-compressed texels: decoding through `bcdec`, and a plain DXT1/DXT5
//! encoder (endpoints from the block's extremes) for formats IW4 cannot take.

/// RGBA8 texels, row-major.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

fn decode_blocks(
    width: u32,
    height: u32,
    blocks: &[u8],
    block_bytes: usize,
    decode: impl Fn(&[u8], &mut [u8], usize),
) -> Rgba {
    let w = width.max(1) as usize;
    let h = height.max(1) as usize;
    let bw = w.div_ceil(4);
    let bh = h.div_ceil(4);
    let pitch = bw * 4 * 4;
    let mut padded = vec![0u8; pitch * bh * 4];
    for by in 0..bh {
        for bx in 0..bw {
            let at = (by * bw + bx) * block_bytes;
            if at + block_bytes > blocks.len() {
                continue;
            }
            let dst = (by * 4 * pitch) + bx * 16;
            decode(&blocks[at..at + block_bytes], &mut padded[dst..], pitch);
        }
    }
    let mut data = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        data.extend_from_slice(&padded[y * pitch..y * pitch + w * 4]);
    }
    Rgba {
        width: w as u32,
        height: h as u32,
        data,
    }
}

pub fn decode_bc1(width: u32, height: u32, blocks: &[u8]) -> Rgba {
    decode_blocks(width, height, blocks, 8, bcdec_rs::bc1)
}

pub fn decode_bc3(width: u32, height: u32, blocks: &[u8]) -> Rgba {
    decode_blocks(width, height, blocks, 16, bcdec_rs::bc3)
}

pub fn decode_bc7(width: u32, height: u32, blocks: &[u8]) -> Rgba {
    decode_blocks(width, height, blocks, 16, bcdec_rs::bc7)
}

pub fn halve(src: &Rgba) -> Rgba {
    let w = (src.width / 2).max(1);
    let h = (src.height / 2).max(1);
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0u32; 4];
            let mut n = 0u32;
            for dy in 0..2u32 {
                for dx in 0..2u32 {
                    let sx = (x * 2 + dx).min(src.width - 1);
                    let sy = (y * 2 + dy).min(src.height - 1);
                    let at = ((sy * src.width + sx) * 4) as usize;
                    for c in 0..4 {
                        sum[c] += u32::from(src.data[at + c]);
                    }
                    n += 1;
                }
            }
            for c in sum {
                data.push((c / n) as u8);
            }
        }
    }
    Rgba {
        width: w,
        height: h,
        data,
    }
}

pub fn has_transparency(rgba: &Rgba) -> bool {
    rgba.data.chunks_exact(4).any(|p| p[3] < 250)
}

fn to_565(c: [u8; 3]) -> u16 {
    (u16::from(c[0] >> 3) << 11) | (u16::from(c[1] >> 2) << 5) | u16::from(c[2] >> 3)
}

fn from_565(c: u16) -> [i32; 3] {
    let r = ((c >> 11) & 31) as i32;
    let g = ((c >> 5) & 63) as i32;
    let b = (c & 31) as i32;
    [(r * 255 + 15) / 31, (g * 255 + 31) / 63, (b * 255 + 15) / 31]
}

fn block_pixels(rgba: &Rgba, bx: usize, by: usize) -> [[u8; 4]; 16] {
    let mut out = [[0u8; 4]; 16];
    for (i, px) in out.iter_mut().enumerate() {
        let x = (bx * 4 + i % 4).min(rgba.width as usize - 1);
        let y = (by * 4 + i / 4).min(rgba.height as usize - 1);
        let at = (y * rgba.width as usize + x) * 4;
        px.copy_from_slice(&rgba.data[at..at + 4]);
    }
    out
}

/// One DXT1 colour block: endpoints are the block's brightest and darkest
/// texels, every texel takes the nearest of the four palette entries.
fn encode_color_block(px: &[[u8; 4]; 16], out: &mut [u8]) {
    let luma = |p: &[u8; 4]| u32::from(p[0]) * 299 + u32::from(p[1]) * 587 + u32::from(p[2]) * 114;
    let (mut hi, mut lo) = (px[0], px[0]);
    for p in px {
        if luma(p) > luma(&hi) {
            hi = *p;
        }
        if luma(p) < luma(&lo) {
            lo = *p;
        }
    }
    let mut c0 = to_565([hi[0], hi[1], hi[2]]);
    let mut c1 = to_565([lo[0], lo[1], lo[2]]);
    if c0 < c1 {
        std::mem::swap(&mut c0, &mut c1);
    }
    let e0 = from_565(c0);
    let e1 = from_565(c1);
    let palette: [[i32; 3]; 4] = if c0 == c1 {
        [e0, e1, e0, e1]
    } else {
        [
            e0,
            e1,
            [
                (2 * e0[0] + e1[0]) / 3,
                (2 * e0[1] + e1[1]) / 3,
                (2 * e0[2] + e1[2]) / 3,
            ],
            [
                (e0[0] + 2 * e1[0]) / 3,
                (e0[1] + 2 * e1[1]) / 3,
                (e0[2] + 2 * e1[2]) / 3,
            ],
        ]
    };
    let mut indices = 0u32;
    for (i, p) in px.iter().enumerate() {
        let mut best = 0usize;
        let mut best_d = i32::MAX;
        for (k, c) in palette.iter().enumerate() {
            let d = (i32::from(p[0]) - c[0]).pow(2)
                + (i32::from(p[1]) - c[1]).pow(2)
                + (i32::from(p[2]) - c[2]).pow(2);
            if d < best_d {
                best_d = d;
                best = k;
            }
        }
        indices |= (best as u32) << (i * 2);
    }
    out[0..2].copy_from_slice(&c0.to_le_bytes());
    out[2..4].copy_from_slice(&c1.to_le_bytes());
    out[4..8].copy_from_slice(&indices.to_le_bytes());
}

fn encode_alpha_block(px: &[[u8; 4]; 16], out: &mut [u8]) {
    let a0 = px.iter().map(|p| p[3]).max().unwrap_or(255);
    let a1 = px.iter().map(|p| p[3]).min().unwrap_or(0);
    let (a0, a1) = if a0 == a1 { (a0, a0) } else { (a0, a1) };
    let palette: [i32; 8] = if a0 > a1 {
        let (a0, a1) = (i32::from(a0), i32::from(a1));
        [
            a0,
            a1,
            (6 * a0 + a1) / 7,
            (5 * a0 + 2 * a1) / 7,
            (4 * a0 + 3 * a1) / 7,
            (3 * a0 + 4 * a1) / 7,
            (2 * a0 + 5 * a1) / 7,
            (a0 + 6 * a1) / 7,
        ]
    } else {
        let a = i32::from(a0);
        [a, a, a, a, a, a, a, a]
    };
    let mut bits = 0u64;
    for (i, p) in px.iter().enumerate() {
        let a = i32::from(p[3]);
        let mut best = 0usize;
        let mut best_d = i32::MAX;
        for (k, c) in palette.iter().enumerate() {
            let d = (a - c).abs();
            if d < best_d {
                best_d = d;
                best = k;
            }
        }
        bits |= (best as u64) << (i * 3);
    }
    out[0] = a0;
    out[1] = a1;
    out[2..8].copy_from_slice(&bits.to_le_bytes()[..6]);
}

pub fn encode_dxt1(rgba: &Rgba) -> Vec<u8> {
    let bw = (rgba.width as usize).div_ceil(4);
    let bh = (rgba.height as usize).div_ceil(4);
    let mut out = vec![0u8; bw * bh * 8];
    for by in 0..bh {
        for bx in 0..bw {
            let px = block_pixels(rgba, bx, by);
            let at = (by * bw + bx) * 8;
            encode_color_block(&px, &mut out[at..at + 8]);
        }
    }
    out
}

pub fn encode_dxt5(rgba: &Rgba) -> Vec<u8> {
    let bw = (rgba.width as usize).div_ceil(4);
    let bh = (rgba.height as usize).div_ceil(4);
    let mut out = vec![0u8; bw * bh * 16];
    for by in 0..bh {
        for bx in 0..bw {
            let px = block_pixels(rgba, bx, by);
            let at = (by * bw + bx) * 16;
            encode_alpha_block(&px, &mut out[at..at + 8]);
            encode_color_block(&px, &mut out[at + 8..at + 16]);
        }
    }
    out
}
