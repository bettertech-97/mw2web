//! Source 2 models (`.vmdl_c`) as CS2 compiles its static props: the mesh
//! lives in the model as a `MDAT` block (KV3: scene objects and draw calls)
//! and a `MBUF` block (vertex and index buffers, meshoptimizer-compressed),
//! named by the `CTRL` block's `embedded_meshes`.

use crate::kv3::{self, Value};
use crate::resource;

pub struct Draw {
    /// The material's `.vmat` path as the draw call names it.
    pub material: String,
    pub first_index: u32,
    pub index_count: u32,
}

pub struct Model {
    pub positions: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    /// Triangle list over `positions`.
    pub indices: Vec<u32>,
    pub draws: Vec<Draw>,
}

struct Attribute {
    name: String,
    format: u32,
    offset: usize,
}

struct Buffer {
    count: usize,
    stride: usize,
    attributes: Vec<Attribute>,
    data: Vec<u8>,
}

fn u32_at(d: &[u8], at: usize) -> Result<u32, String> {
    d.get(at..at + 4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| "vmdl: buffer table truncated".to_owned())
}

/// Reads one buffer table (vertex or index) of a `MBUF`/`VBIB` block;
/// `table` is where the table's offset field sits.
fn read_buffers(d: &[u8], table: usize, with_attributes: bool) -> Result<Vec<Buffer>, String> {
    let offset = u32_at(d, table)? as usize;
    let count = u32_at(d, table + 4)? as usize;
    let mut at = table + offset;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let element_count = u32_at(d, at)? as usize;
        let stride = u32_at(d, at + 4)? as usize;
        let attribute_offset = u32_at(d, at + 8)? as usize;
        let attribute_count = u32_at(d, at + 12)? as usize;
        let data_offset = u32_at(d, at + 16)? as usize;
        let data_size = u32_at(d, at + 20)? as usize;
        let mut attributes = Vec::new();
        if with_attributes {
            let mut a = at + 8 + attribute_offset;
            for _ in 0..attribute_count {
                let name_bytes = d.get(a..a + 32).ok_or("vmdl: attribute truncated")?;
                let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(32);
                let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
                let index = u32_at(d, a + 32)?;
                let format = u32_at(d, a + 36)?;
                let offset = u32_at(d, a + 40)? as usize;
                attributes.push(Attribute {
                    name: format!("{name}{index}"),
                    format,
                    offset,
                });
                a += 56;
            }
        }
        let start = at + 16 + data_offset;
        let raw = d
            .get(start..start + data_size)
            .ok_or("vmdl: buffer data truncated")?;
        let data = if data_size < element_count * stride {
            decode_meshopt(raw, element_count, stride, with_attributes)?
        } else {
            raw.to_vec()
        };
        out.push(Buffer {
            count: element_count,
            stride,
            attributes,
            data,
        });
        at += 24;
    }
    Ok(out)
}

fn decode_meshopt(raw: &[u8], count: usize, stride: usize, vertices: bool) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; count * stride];
    let status = unsafe {
        if vertices {
            meshopt::ffi::meshopt_decodeVertexBuffer(
                out.as_mut_ptr().cast(),
                count,
                stride,
                raw.as_ptr(),
                raw.len(),
            )
        } else {
            meshopt::ffi::meshopt_decodeIndexBuffer(
                out.as_mut_ptr().cast(),
                count,
                stride,
                raw.as_ptr(),
                raw.len(),
            )
        }
    };
    if status != 0 {
        return Err(format!(
            "vmdl: meshopt {} decode failed ({status})",
            if vertices { "vertex" } else { "index" }
        ));
    }
    Ok(out)
}

fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = ((bits >> 10) & 0x1f) as i32;
    let mantissa = (bits & 0x3ff) as f32;
    match exponent {
        0 => sign * mantissa * 2f32.powi(-24),
        31 => {
            if mantissa == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        e => sign * (1.0 + mantissa / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Reads an attribute's two or three components as floats, by DXGI format.
fn read_floats(bytes: &[u8], format: u32, out: &mut [f32]) -> Result<(), String> {
    match format {
        // R32G32B32_FLOAT, R32G32_FLOAT
        6 | 16 => {
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
            }
        }
        // R16G16_FLOAT (34) and R16G16B16A16_FLOAT (10)
        34 | 10 => {
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = half(u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]));
            }
        }
        // R16G16_SNORM (37): texture coordinates scaled to the usual range.
        37 | 13 => {
            for (i, slot) in out.iter_mut().enumerate() {
                let v = i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
                *slot = f32::from(v) / 32767.0;
            }
        }
        // R16G16_UNORM (35)
        35 | 11 => {
            for (i, slot) in out.iter_mut().enumerate() {
                let v = u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
                *slot = f32::from(v) / 65535.0;
            }
        }
        other => return Err(format!("vmdl: vertex format {other} is not read")),
    }
    Ok(())
}

/// Parses a model: its first LOD mesh's positions, texture coordinates,
/// triangle indices and draw calls.
pub fn parse(bytes: &[u8]) -> Result<Model, String> {
    let blocks = resource::blocks(bytes)?;
    let block_value = |index: usize| -> Result<Value, String> {
        let block = blocks.get(index).ok_or("vmdl: block index out of range")?;
        kv3::parse(&bytes[block.offset..block.offset + block.size])
    };
    let ctrl_index = blocks
        .iter()
        .position(|b| &b.kind == b"CTRL")
        .ok_or("vmdl: no CTRL block")?;
    let ctrl = block_value(ctrl_index)?;
    let meshes = ctrl
        .get("embedded_meshes")
        .and_then(Value::as_array)
        .ok_or("vmdl: no embedded meshes")?;
    // LOD group masks say which meshes belong to the first LOD.
    let lod_masks: Vec<u64> = blocks
        .iter()
        .position(|b| &b.kind == b"DATA")
        .and_then(|index| block_value(index).ok())
        .and_then(|data| {
            data.get("m_refLODGroupMasks")
                .and_then(Value::as_array)
                .map(|masks| masks.iter().filter_map(|m| m.as_i64()).map(|m| m as u64).collect())
        })
        .unwrap_or_default();
    let mut model = Model {
        positions: Vec::new(),
        uvs: Vec::new(),
        indices: Vec::new(),
        draws: Vec::new(),
    };
    for mesh in meshes {
        let mesh_index = mesh.get("mesh_index").and_then(Value::as_i64).unwrap_or(0) as usize;
        if lod_masks.get(mesh_index).is_some_and(|mask| mask & 1 == 0) {
            continue;
        }
        let data_block = mesh.get("data_block").and_then(Value::as_i64).ok_or("vmdl: mesh without data block")? as usize;
        let vbib_block = mesh.get("vbib_block").and_then(Value::as_i64).ok_or("vmdl: mesh without buffer block")? as usize;
        let mdat = block_value(data_block)?;
        let vbib = blocks.get(vbib_block).ok_or("vmdl: buffer block index out of range")?;
        let d = &bytes[vbib.offset..vbib.offset + vbib.size];
        let vertex_buffers = read_buffers(d, 0, true)?;
        let index_buffers = read_buffers(d, 8, false)?;
        // Every vertex buffer's vertices, appended; draw calls index into
        // them with the buffer's base added.
        let mut vertex_base = Vec::with_capacity(vertex_buffers.len());
        for buffer in &vertex_buffers {
            vertex_base.push(model.positions.len() as u32);
            let position = buffer
                .attributes
                .iter()
                .find(|a| a.name == "POSITION0")
                .ok_or("vmdl: vertex buffer without positions")?;
            let texcoord = buffer.attributes.iter().find(|a| a.name == "TEXCOORD0");
            for v in 0..buffer.count {
                let at = v * buffer.stride;
                let mut p = [0.0f32; 3];
                read_floats(&buffer.data[at + position.offset..], position.format, &mut p)?;
                let mut uv = [0.0f32; 2];
                if let Some(texcoord) = texcoord {
                    read_floats(&buffer.data[at + texcoord.offset..], texcoord.format, &mut uv)?;
                }
                model.positions.push(p);
                model.uvs.push(uv);
            }
        }
        let scene_objects = mdat
            .get("m_sceneObjects")
            .and_then(Value::as_array)
            .ok_or("vmdl: mesh without scene objects")?;
        for object in scene_objects {
            let Some(draws) = object.get("m_drawCalls").and_then(Value::as_array) else {
                continue;
            };
            for draw in draws {
                let primitive = draw.get("m_nPrimitiveType").and_then(Value::as_str).unwrap_or("");
                if !primitive.is_empty() && primitive != "RENDER_PRIM_TRIANGLES" {
                    continue;
                }
                let start = draw.get("m_nStartIndex").and_then(Value::as_i64).unwrap_or(0) as usize;
                let count = draw.get("m_nIndexCount").and_then(Value::as_i64).unwrap_or(0) as usize;
                let base_vertex = draw.get("m_nBaseVertex").and_then(Value::as_i64).unwrap_or(0) as u32;
                let material = draw.get("m_material").and_then(Value::as_str).unwrap_or("").to_owned();
                let vb = draw
                    .get("m_vertexBuffers")
                    .and_then(Value::as_array)
                    .and_then(|list| list.first())
                    .and_then(|b| b.get("m_hBuffer"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0) as usize;
                let ib = draw
                    .get("m_indexBuffer")
                    .and_then(|b| b.get("m_hBuffer"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0) as usize;
                let buffer = index_buffers.get(ib).ok_or("vmdl: draw names a missing index buffer")?;
                let base = vertex_base.get(vb).copied().ok_or("vmdl: draw names a missing vertex buffer")? + base_vertex;
                let first_index = model.indices.len() as u32;
                for i in start..start + count {
                    let index = match buffer.stride {
                        2 => u32::from(u16::from_le_bytes([buffer.data[i * 2], buffer.data[i * 2 + 1]])),
                        4 => u32::from_le_bytes(buffer.data[i * 4..i * 4 + 4].try_into().unwrap()),
                        other => return Err(format!("vmdl: index size {other}")),
                    };
                    model.indices.push(index + base);
                }
                model.draws.push(Draw {
                    material,
                    first_index,
                    index_count: count as u32,
                });
            }
        }
    }
    if model.draws.is_empty() {
        return Err("vmdl: no triangle draw calls".into());
    }
    Ok(model)
}
