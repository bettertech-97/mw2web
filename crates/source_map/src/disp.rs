//! Displacements: a power-of-two grid over one quad face, each vertex pushed
//! along its own normal. A VMF `rowN` runs from the start corner along the
//! face's first edge (start → next corner); successive rows step across the
//! face toward the opposite edge. Checked against Mirage: neighbouring
//! patches meet at their sewn edges in this layout and in no other.

use crate::brush::{Side, Vec3, add, cross, dot, floats, length, lerp, normalize, scale, sub};
use crate::keyvalues::Block;

#[derive(Clone, Debug, PartialEq)]
pub struct DispInfo {
    pub power: u32,
    pub start: Vec3,
    pub elevation: f32,
    pub normals: Vec<Vec3>,
    pub distances: Vec<f32>,
    pub offsets: Vec<Vec3>,
    pub alphas: Vec<f32>,
}

impl DispInfo {
    pub fn size(&self) -> usize {
        (1usize << self.power) + 1
    }
}

fn rows(block: Option<&Block>, size: usize, width: usize) -> Result<Vec<f32>, String> {
    let mut out = vec![0.0; size * size * width];
    let Some(block) = block else {
        return Ok(out);
    };
    for row in 0..size {
        let Some(text) = block.get(&format!("row{row}")) else {
            continue;
        };
        let values = floats(text)?;
        let base = row * size * width;
        for (i, value) in values.into_iter().enumerate().take(size * width) {
            out[base + i] = value;
        }
    }
    Ok(out)
}

fn triples(values: Vec<f32>) -> Vec<Vec3> {
    values
        .chunks_exact(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect()
}

pub fn parse_disp(block: &Block) -> Result<DispInfo, String> {
    let power: u32 = block
        .get("power")
        .and_then(|p| p.parse().ok())
        .unwrap_or(3);
    if !(1..=4).contains(&power) {
        return Err(format!("displacement power {power}"));
    }
    let start = block
        .get("startposition")
        .map(|s| floats(&s.replace(['[', ']'], " ")))
        .transpose()?
        .filter(|v| v.len() == 3)
        .map(|v| [v[0], v[1], v[2]])
        .ok_or("displacement without a start position")?;
    let elevation = block
        .get("elevation")
        .and_then(|e| e.parse().ok())
        .unwrap_or(0.0);
    let size = (1usize << power) + 1;
    Ok(DispInfo {
        power,
        start,
        elevation,
        normals: triples(rows(block.child("normals"), size, 3)?),
        distances: rows(block.child("distances"), size, 1)?,
        offsets: triples(rows(block.child("offsets"), size, 3)?),
        alphas: rows(block.child("alphas"), size, 1)?,
    })
}

#[derive(Clone, Debug, Default)]
pub struct DispMesh {
    pub positions: Vec<Vec3>,
    /// Where each vertex sits on the flat face before displacement.
    pub base: Vec<Vec3>,
    pub normals: Vec<Vec3>,
    pub alphas: Vec<f32>,
    pub indices: Vec<u32>,
    pub size: usize,
}

/// The four corners of a face polygon, dropping colinear and duplicate
/// points; `None` when it is not a quad.
pub(crate) fn quad_corners(winding: &[Vec3]) -> Option<[Vec3; 4]> {
    let mut corners: Vec<Vec3> = Vec::new();
    let n = winding.len();
    for i in 0..n {
        let prev = winding[(i + n - 1) % n];
        let here = winding[i];
        let next = winding[(i + 1) % n];
        let a = normalize(sub(here, prev));
        let b = normalize(sub(next, here));
        if length(sub(here, prev)) < 0.1 {
            continue;
        }
        if length(cross(a, b)) < 1e-3 {
            continue;
        }
        corners.push(here);
    }
    (corners.len() == 4).then(|| [corners[0], corners[1], corners[2], corners[3]])
}

/// Which grid cell a VMF `rowN` value lands on, relative to the base
/// quad: the layout VBSP uses (see the module note) is the default; the
/// others exist so the seam test can tell them apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DispVariant {
    pub transpose: bool,
    pub flip_i: bool,
    pub flip_j: bool,
}

pub fn build_disp(side: &Side, disp: &DispInfo) -> Option<DispMesh> {
    build_disp_with(
        side,
        disp,
        DispVariant {
            transpose: true,
            flip_i: false,
            flip_j: false,
        },
    )
}

pub(crate) fn build_disp_with(side: &Side, disp: &DispInfo, variant: DispVariant) -> Option<DispMesh> {
    let corners = quad_corners(&side.winding)?;
    let start = (0..4)
        .min_by(|&a, &b| {
            length(sub(corners[a], disp.start))
                .partial_cmp(&length(sub(corners[b], disp.start)))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let p: [Vec3; 4] = std::array::from_fn(|i| corners[(start + i) % 4]);
    let size = disp.size();
    let face_normal = [side.plane[0], side.plane[1], side.plane[2]];
    let step = 1.0 / (size - 1) as f32;
    let mut positions = Vec::with_capacity(size * size);
    let mut base = Vec::with_capacity(size * size);
    let mut alphas = Vec::with_capacity(size * size);
    for i in 0..size {
        let row_start = lerp(p[0], p[1], i as f32 * step);
        let row_end = lerp(p[3], p[2], i as f32 * step);
        for j in 0..size {
            let (mut di, mut dj) = (i, j);
            if variant.flip_i {
                di = size - 1 - di;
            }
            if variant.flip_j {
                dj = size - 1 - dj;
            }
            let at = if variant.transpose {
                dj * size + di
            } else {
                di * size + dj
            };
            let mut v = lerp(row_start, row_end, j as f32 * step);
            base.push(v);
            let normal = disp.normals.get(at).copied().unwrap_or([0.0; 3]);
            let distance = disp.distances.get(at).copied().unwrap_or(0.0);
            v = add(v, scale(normal, distance));
            if let Some(offset) = disp.offsets.get(at) {
                v = add(v, *offset);
            }
            v = add(v, scale(face_normal, disp.elevation));
            positions.push(v);
            alphas.push(disp.alphas.get(at).copied().unwrap_or(0.0));
        }
    }
    let mut indices = Vec::with_capacity((size - 1) * (size - 1) * 6);
    for i in 0..size - 1 {
        for j in 0..size - 1 {
            let a = (i * size + j) as u32;
            let b = a + 1;
            let c = a + size as u32;
            let d = c + 1;
            let tris: [[u32; 3]; 2] = if (i + j) % 2 == 0 {
                [[a, b, d], [a, d, c]]
            } else {
                [[a, b, c], [b, d, c]]
            };
            for tri in tris {
                let n = cross(
                    sub(positions[tri[1] as usize], positions[tri[0] as usize]),
                    sub(positions[tri[2] as usize], positions[tri[0] as usize]),
                );
                if dot(n, face_normal) >= 0.0 {
                    indices.extend_from_slice(&tri);
                } else {
                    indices.extend_from_slice(&[tri[0], tri[2], tri[1]]);
                }
            }
        }
    }
    let mut normals = vec![[0.0f32; 3]; positions.len()];
    for tri in indices.chunks_exact(3) {
        let n = cross(
            sub(positions[tri[1] as usize], positions[tri[0] as usize]),
            sub(positions[tri[2] as usize], positions[tri[0] as usize]),
        );
        for &v in tri {
            normals[v as usize] = add(normals[v as usize], n);
        }
    }
    for n in &mut normals {
        *n = if length(*n) < 1e-9 {
            face_normal
        } else {
            normalize(*n)
        };
    }
    Some(DispMesh {
        positions,
        base,
        normals,
        alphas,
        indices,
        size,
    })
}
