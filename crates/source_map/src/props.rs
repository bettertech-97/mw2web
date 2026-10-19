//! Static props: the models a map's `prop_static` entities place, read from
//! `<zone>.props.bin` (written by `cs2_textures`) and appended to the
//! compiled map as ordinary faces, one per draw call per instance, so the
//! world builder, the bake and collision see them like any other geometry.

use std::collections::HashMap;

use crate::brush::{Vec3, cross, normalize, sub};
use crate::compile::{CompiledMap, Entity, Surface, Vertex};

const MAGIC: &[u8; 8] = b"IW4LPRP1";
/// The same file zlib-compressed after this magic: half the bytes to ship.
const MAGIC_DEFLATED: &[u8; 8] = b"IW4LPRPZ";

pub struct PropDraw {
    /// The pack material name (`models/x/y`).
    pub material: String,
    pub first_index: u32,
    pub index_count: u32,
}

pub struct PropModel {
    pub positions: Vec<Vec3>,
    pub uvs: Vec<[f32; 2]>,
    /// Smooth vertex normals in model space.
    pub normals: Vec<Vec3>,
    pub indices: Vec<u32>,
    pub draws: Vec<PropDraw>,
}

#[derive(Default)]
pub struct PropSet {
    /// By the model path the map names (`models/.../x.mdl`, lower case).
    pub models: HashMap<String, PropModel>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PropStats {
    pub instances: usize,
    pub placed: usize,
    pub missing_models: usize,
    pub triangles: usize,
    pub vertices: usize,
}

impl PropSet {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        if data.len() >= 8 && &data[..8] == MAGIC_DEFLATED {
            let inflated = crate::inflate(&data[8..])?;
            return Self::from_bytes(&inflated);
        }
        if data.len() < 12 || &data[..8] != MAGIC {
            return Err("props: not a props file".into());
        }
        let u32_at = |at: usize| -> Result<u32, String> {
            data.get(at..at + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .ok_or_else(|| "props: truncated".to_owned())
        };
        let u16_at = |at: usize| -> Result<u16, String> {
            data.get(at..at + 2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .ok_or_else(|| "props: truncated".to_owned())
        };
        let count = u32_at(8)? as usize;
        let mut at = 12;
        let mut models = HashMap::with_capacity(count);
        for _ in 0..count {
            let name_len = u16_at(at)? as usize;
            at += 2;
            let name = String::from_utf8_lossy(data.get(at..at + name_len).ok_or("props: truncated")?).into_owned();
            at += name_len;
            let vertex_count = u32_at(at)? as usize;
            let index_count = u32_at(at + 4)? as usize;
            let draw_count = u16_at(at + 8)? as usize;
            at += 10;
            let mut positions = Vec::with_capacity(vertex_count);
            let mut uvs = Vec::with_capacity(vertex_count);
            for _ in 0..vertex_count {
                let f = |k: usize| -> Result<f32, String> { u32_at(at + k * 4).map(f32::from_bits) };
                positions.push([f(0)?, f(1)?, f(2)?]);
                uvs.push([f(3)?, f(4)?]);
                at += 20;
            }
            let mut indices = Vec::with_capacity(index_count);
            for _ in 0..index_count {
                indices.push(u32_at(at)?);
                at += 4;
            }
            let mut draws = Vec::with_capacity(draw_count);
            for _ in 0..draw_count {
                let first_index = u32_at(at)?;
                let index_count = u32_at(at + 4)?;
                let material_len = u16_at(at + 8)? as usize;
                at += 10;
                let material = String::from_utf8_lossy(data.get(at..at + material_len).ok_or("props: truncated")?).into_owned();
                at += material_len;
                draws.push(PropDraw {
                    material,
                    first_index,
                    index_count,
                });
            }
            let normals = smooth_normals(&positions, &indices);
            models.insert(
                name,
                PropModel {
                    positions,
                    uvs,
                    normals,
                    indices,
                    draws,
                },
            );
        }
        Ok(Self { models })
    }
}

fn smooth_normals(positions: &[Vec3], indices: &[u32]) -> Vec<Vec3> {
    let mut normals = vec![[0.0f32; 3]; positions.len()];
    for tri in indices.chunks_exact(3) {
        let (a, b, c) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        if a >= positions.len() || b >= positions.len() || c >= positions.len() {
            continue;
        }
        let n = cross(sub(positions[b], positions[a]), sub(positions[c], positions[a]));
        for &i in &[a, b, c] {
            for axis in 0..3 {
                normals[i][axis] += n[axis];
            }
        }
    }
    for n in &mut normals {
        let length = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        *n = if length > 1e-12 {
            [n[0] / length, n[1] / length, n[2] / length]
        } else {
            [0.0, 0.0, 1.0]
        };
    }
    normals
}

/// Source's angle matrix: rows take a model-space vector to world space.
fn rotation(angles: [f32; 3]) -> [[f32; 3]; 3] {
    let (sp, cp) = angles[0].to_radians().sin_cos();
    let (sy, cy) = angles[1].to_radians().sin_cos();
    let (sr, cr) = angles[2].to_radians().sin_cos();
    [
        [cp * cy, sr * sp * cy - cr * sy, cr * sp * cy + sr * sy],
        [cp * sy, sr * sp * sy + cr * cy, cr * sp * sy - sr * cy],
        [-sp, sr * cp, cr * cp],
    ]
}

fn apply(m: &[[f32; 3]; 3], v: Vec3) -> Vec3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

fn prop_scale(entity: &Entity) -> f32 {
    entity
        .get("uniformscale")
        .or_else(|| entity.get("modelscale"))
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(1.0)
}

/// Wraps a props file in the deflated envelope.
pub fn deflate_props(raw: &[u8]) -> Vec<u8> {
    let mut out = MAGIC_DEFLATED.to_vec();
    out.extend(crate::deflate(raw));
    out
}

/// Appends every placed prop to the map. Faces come after the brush faces
/// in entity order, so the bake and the engine agree on face indices.
pub fn append_props(map: &mut CompiledMap, set: &PropSet) -> PropStats {
    let mut stats = PropStats::default();
    let mut uses: HashMap<String, usize> = map.material_uses.iter().cloned().collect();
    let entities: Vec<(String, Vec3, [f32; 3], f32)> = map
        .entities
        .iter()
        .filter(|e| matches!(e.classname.as_str(), "prop_static" | "prop_dynamic" | "prop_dynamic_override"))
        .filter_map(|e| {
            let model = e.get("model")?.replace('\\', "/").to_ascii_lowercase();
            let origin = e.origin()?;
            Some((model, origin, e.angles(), prop_scale(e)))
        })
        .collect();
    for (model_name, origin, angles, scale) in entities {
        stats.instances += 1;
        let Some(model) = set.models.get(&model_name) else {
            stats.missing_models += 1;
            continue;
        };
        let m = rotation(angles);
        let first_vertex = map.vertices.len() as u32;
        let mut mins = [f32::INFINITY; 3];
        let mut maxs = [f32::NEG_INFINITY; 3];
        for (i, p) in model.positions.iter().enumerate() {
            let scaled = [p[0] * scale, p[1] * scale, p[2] * scale];
            let rotated = apply(&m, scaled);
            let position = [rotated[0] + origin[0], rotated[1] + origin[1], rotated[2] + origin[2]];
            let normal = normalize(apply(&m, model.normals[i]));
            for axis in 0..3 {
                mins[axis] = mins[axis].min(position[axis]);
                maxs[axis] = maxs[axis].max(position[axis]);
            }
            map.vertices.push(Vertex {
                position,
                normal,
                tangent: [1.0, 0.0, 0.0, 1.0],
                uv: model.uvs[i],
                alpha: 0.0,
            });
        }
        for draw in &model.draws {
            let first_index = map.indices.len() as u32;
            let range = draw.first_index as usize..(draw.first_index + draw.index_count) as usize;
            let Some(indices) = model.indices.get(range) else {
                continue;
            };
            map.indices.extend(indices.iter().map(|i| i + first_vertex));
            map.surfaces.push(Surface {
                material: draw.material.clone(),
                first_vertex,
                vertex_count: model.positions.len() as u32,
                first_index,
                index_count: draw.index_count,
                mins,
                maxs,
                normal: [0.0, 0.0, 1.0],
                displacement: false,
                sky: false,
                prop: true,
            });
            *uses.entry(draw.material.clone()).or_insert(0) += 1;
            stats.triangles += draw.index_count as usize / 3;
        }
        stats.vertices += model.positions.len();
        stats.placed += 1;
        for axis in 0..3 {
            map.mins[axis] = map.mins[axis].min(mins[axis]);
            map.maxs[axis] = map.maxs[axis].max(maxs[axis]);
        }
    }
    let mut uses: Vec<(String, usize)> = uses.into_iter().collect();
    uses.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    map.material_uses = uses;
    stats
}
