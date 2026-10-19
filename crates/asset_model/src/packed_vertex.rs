pub fn unpack_unit_vec(packed: u32) -> [f32; 3] {
    dpvs_iw4::skin_unpack_unit_vec(packed)
}

pub fn unpack_unit_vec_t6(packed: u32) -> [f32; 3] {
    let axis = |shift: u32| {
        let bits = (packed >> shift) & 0x3FF;
        let raw = bits
            .wrapping_sub(2 * (bits & 0x200))
            .wrapping_add(0x4040_0000);
        (f32::from_bits(raw) - 3.0) * 8208.031
    };
    [axis(0), axis(10), axis(20)]
}

pub fn pack_unit_vec(v: [f32; 3]) -> u32 {
    let n = normalize_or_up(v);
    let mut out = 0u32;
    let mut best_dir = f32::MAX;
    let mut best_len = f32::MAX;
    for scale_byte in 0..=255u8 {
        let encode = 32_385.0 / (f32::from(scale_byte) + 192.0);
        let byte = |c: f32| (c * encode + 127.5) as i32 as i8 as u8;
        let bytes = [byte(n[0]), byte(n[1]), byte(n[2]), scale_byte];
        let candidate = u32::from_le_bytes(bytes);
        let decoded = unpack_unit_vec(candidate);
        let len =
            (decoded[0] * decoded[0] + decoded[1] * decoded[1] + decoded[2] * decoded[2]).sqrt();
        let len_error = (len - 1.0).abs();
        if len_error >= 0.001 {
            continue;
        }
        let d = normalize_or_up(decoded);
        let dir_error = (d[0] * n[0] + d[1] * n[1] + d[2] * n[2] - 1.0).abs();
        if best_dir > dir_error || (best_dir <= dir_error && best_len > len_error) {
            best_dir = dir_error;
            best_len = len_error;
            out = candidate;
            if len_error + dir_error == 0.0 {
                break;
            }
        }
    }
    out
}

/// [`pack_unit_vec`] without the full scan: the finest quantisation whose
/// bytes still fit is the scale byte where `max|c| * encode` reaches 127.5,
/// and coarser scales only lose precision, so a short window above it holds
/// the best candidate. Converted worlds pack millions of smooth normals,
/// which the 256-step scan made the slowest part of their load.
pub fn pack_unit_vec_fast(v: [f32; 3]) -> u32 {
    let n = normalize_or_up(v);
    let max = n[0].abs().max(n[1].abs()).max(n[2].abs());
    // c * encode + 127.5 stays within a byte while 32385 / (scale + 192) <= 127.5 / max.
    let first = ((max * 254.0) - 192.0).ceil().clamp(0.0, 255.0) as u32;
    let mut out = None;
    let mut best_dir = f32::MAX;
    let mut best_len = f32::MAX;
    for scale_byte in first..=(first + 24).min(255) {
        let encode = 32_385.0 / (scale_byte as f32 + 192.0);
        let byte = |c: f32| (c * encode + 127.5) as i32 as i8 as u8;
        let bytes = [byte(n[0]), byte(n[1]), byte(n[2]), scale_byte as u8];
        let candidate = u32::from_le_bytes(bytes);
        let decoded = unpack_unit_vec(candidate);
        let len =
            (decoded[0] * decoded[0] + decoded[1] * decoded[1] + decoded[2] * decoded[2]).sqrt();
        let len_error = (len - 1.0).abs();
        if len_error >= 0.001 {
            continue;
        }
        let d = normalize_or_up(decoded);
        let dir_error = (d[0] * n[0] + d[1] * n[1] + d[2] * n[2] - 1.0).abs();
        if best_dir > dir_error || (best_dir <= dir_error && best_len > len_error) {
            best_dir = dir_error;
            best_len = len_error;
            out = Some(candidate);
            // Within the quantisation floor of a byte per component: a coarser
            // scale cannot do meaningfully better.
            if dir_error < 1.0e-5 {
                break;
            }
        }
    }
    out.unwrap_or_else(|| pack_unit_vec(v))
}

pub fn repack_vertex_t6(mut vertex: [u8; 32]) -> [u8; 32] {
    let word = |v: &[u8; 32], o: usize| u32::from_le_bytes(v[o..o + 4].try_into().unwrap());
    let uv = word(&vertex, 20);
    vertex[20..24].copy_from_slice(&uv.rotate_left(16).to_le_bytes());
    for o in [24, 28] {
        let packed = pack_unit_vec(unpack_unit_vec_t6(word(&vertex, o)));
        vertex[o..o + 4].copy_from_slice(&packed.to_le_bytes());
    }
    vertex
}

pub fn normalize_or_up(v: [f32; 3]) -> [f32; 3] {
    let length = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if length == 0.0 {
        [0.0, 0.0, 1.0]
    } else {
        [v[0] / length, v[1] / length, v[2] / length]
    }
}

pub fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = (bits >> 10) & 0x1f;
    let mantissa = u32::from(bits & 0x03ff);
    let value = match exponent {
        0 if mantissa == 0 => sign,
        0 => {
            let mut mantissa = mantissa;
            let mut exponent = -14i32;
            while mantissa & 0x0400 == 0 {
                mantissa <<= 1;
                exponent -= 1;
            }
            sign | (((exponent + 127) as u32) << 23) | ((mantissa & 0x03ff) << 13)
        }
        0x1f => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((u32::from(exponent) + 112) << 23) | (mantissa << 13),
    };
    f32::from_bits(value)
}

pub fn unpack_packed_tex_coords(packed: u32) -> [f32; 2] {
    [
        half_to_f32((packed >> 16) as u16),
        half_to_f32(packed as u16),
    ]
}

pub fn unpack_color_u8(packed: u32) -> [u8; 4] {
    let [b, g, r, a] = packed.to_le_bytes();
    [r, g, b, a]
}

/// Widens stored vertex colours for a mesh colour attribute.
pub fn colors_f32(colors: &[[u8; 4]]) -> Vec<[f32; 4]> {
    colors
        .iter()
        .map(|c| c.map(|v| f32::from(v) / 255.0))
        .collect()
}

pub fn unpack_color(packed: u32) -> [f32; 4] {
    let [r, g, b, a] = unpack_color_u8(packed);
    [
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        f32::from(a) / 255.0,
    ]
}

#[cfg(test)]
mod pack_tests {
    use super::*;

    /// The window search decodes as close to the input as the full scan.
    #[test]
    fn fast_pack_matches_full_scan_quality() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0
        };
        let mut worst_fast = 0.0f32;
        let mut worst_full = 0.0f32;
        for _ in 0..20_000 {
            let v = normalize_or_up([next(), next(), next()]);
            let error = |packed: u32| {
                let d = normalize_or_up(unpack_unit_vec(packed));
                (d[0] * v[0] + d[1] * v[1] + d[2] * v[2] - 1.0).abs()
            };
            worst_fast = worst_fast.max(error(pack_unit_vec_fast(v)));
            worst_full = worst_full.max(error(pack_unit_vec(v)));
        }
        assert!(worst_fast < 1e-4, "fast packer direction error {worst_fast}");
        assert!(
            worst_fast <= worst_full * 1.5 + 1e-6,
            "fast {worst_fast} vs full {worst_full}"
        );
        for axis in [[1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, 0.0]] {
            assert_eq!(pack_unit_vec_fast(axis), pack_unit_vec(axis));
        }
    }
}
