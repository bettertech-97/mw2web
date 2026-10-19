//! Source engine maps for the IW4 runtime.
//!
//! A Hammer `.vmf` (as `bspsrc` decompiles one) is brushes — convex sets of
//! planes — with a material and texture axes per face, displacement grids
//! over some faces, and key/value entities. [`compile`] turns one into flat
//! geometry (one surface per drawn face), outward-plane collision brushes
//! and the entity list; [`iw4_entity_string`] rewrites the entities as the
//! IW4 gametypes read them. Nothing here knows about zones or materials:
//! the caller maps each Source material name to whatever it draws with.

pub mod bake;

/// Inflates a zlib stream (the `.vmf.z` and `IW4LPRPZ` envelopes the tools
/// write beside a map: a Hammer file is text that shrinks fivefold, props
/// are floats that halve).
pub fn inflate(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut out = Vec::with_capacity(bytes.len() * 4);
    flate2::read::ZlibDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|error| format!("inflate: {error}"))?;
    Ok(out)
}

/// Deflates with zlib at the best level, for the tools.
pub fn deflate(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bytes).expect("vec write");
    encoder.finish().expect("vec finish")
}
pub mod brush;
pub mod compile;
pub mod disp;
pub mod entities;
pub mod keyvalues;
pub mod props;
pub mod radar;

pub use compile::{
    CONTENTS_CLIPSHOT, CONTENTS_DETAIL, CONTENTS_PLAYERCLIP, CONTENTS_SKY, CONTENTS_SOLID,
    CollisionBrush, CompiledMap, Entity, Stats, Sun, Surface, Vertex, compile, compile_with_sizes,
    surface_type_for,
};
pub use entities::iw4_entity_string;
pub use radar::Radar;

#[cfg(test)]
mod tests {
    /// `SOURCE_MAP_VMF=<file> cargo test -p source_map compile_real_map -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn compile_real_map() {
        let path = std::env::var("SOURCE_MAP_VMF").expect("SOURCE_MAP_VMF");
        let text = std::fs::read_to_string(&path).expect("read vmf");
        let started = std::time::Instant::now();
        let map = super::compile(&text, 512.0).expect("compile");
        eprintln!(
            "compiled in {:.0}ms: {:?}\nvertices={} indices={} surfaces={} brushes={} bounds={:?}..{:?} sky={} sun={:?}",
            started.elapsed().as_secs_f32() * 1000.0,
            map.stats,
            map.vertices.len(),
            map.indices.len(),
            map.surfaces.len(),
            map.brushes.len(),
            map.mins,
            map.maxs,
            map.sky_name,
            map.sun,
        );
        for (name, uses) in map.material_uses.iter().take(60) {
            eprintln!("{uses:6}  {name}  ({})", super::surface_type_for(name));
        }
        let entities = super::iw4_entity_string(&map);
        let spawns = entities.matches("\"mp_dm_spawn\"").count();
        eprintln!("entity string: {} bytes, {spawns} dm spawns", entities.len());
        eprintln!("{}", &entities[..entities.len().min(1200)]);
        assert!(spawns > 0);
    }
}

#[cfg(test)]
mod orientation_tests {
    /// `SOURCE_MAP_VMF=<file> cargo test -p source_map --release disp_orientation -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn disp_orientation() {
        let path = std::env::var("SOURCE_MAP_VMF").expect("SOURCE_MAP_VMF");
        let text = std::fs::read_to_string(&path).expect("read vmf");
        let map = super::compile(&text, 512.0).expect("compile");
        let mut agree = 0usize;
        let mut disagree = 0usize;
        let mut disp_faces = 0usize;
        let mut flat_area = 0.0f32;
        let mut tiny = 0usize;
        for face in &map.surfaces {
            let tris = &map.indices[face.first_index as usize..(face.first_index + face.index_count) as usize];
            for tri in tris.chunks_exact(3) {
                let a = map.vertices[tri[0] as usize].position;
                let b = map.vertices[tri[1] as usize].position;
                let c = map.vertices[tri[2] as usize].position;
                let n = super::brush::cross(super::brush::sub(b, a), super::brush::sub(c, a));
                if super::brush::dot(n, face.normal) >= 0.0 { agree += 1 } else { disagree += 1 }
                if super::brush::length(n) < 1e-3 { tiny += 1 }
                flat_area += super::brush::length(n) * 0.5;
            }
            if face.displacement { disp_faces += 1 }
        }
        eprintln!("triangles agreeing with face normal: {agree}, disagreeing: {disagree}, degenerate: {tiny}, displacement faces: {disp_faces}, area: {flat_area:.0}, fallbacks: {}", map.stats.displacement_fallbacks);
        for face in &map.surfaces {
            let inside = face.mins[0] < -1700.0 && face.maxs[0] > -2100.0 && face.mins[1] < -1700.0 && face.maxs[1] > -2100.0 && face.maxs[2] < -200.0 && face.mins[2] > -340.0;
            if inside && face.normal[2] > 0.5 {
                eprintln!("  face {} disp={} tris={} bounds={:?}..{:?}", face.material, face.displacement, face.index_count / 3, face.mins, face.maxs);
            }
        }
        let skipped_disp = map.stats.displacements;
        eprintln!("stats displacements: {skipped_disp}");
    }
}

#[cfg(test)]
mod seam_tests {
    use crate::brush::{Vec3, length, sub};

    /// Adjacent displacements share their edge vertices in Hammer ("sewn");
    /// if the grid convention here were wrong, those seams would open.
    /// `SOURCE_MAP_VMF=<file> cargo test -p source_map --release disp_seams -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn disp_seams() {
        let path = std::env::var("SOURCE_MAP_VMF").expect("SOURCE_MAP_VMF");
        let text = std::fs::read_to_string(&path).expect("read vmf");
        let roots = crate::keyvalues::parse(&text).unwrap();
        for variant in [
            crate::disp::DispVariant { transpose: false, flip_i: false, flip_j: false },
            crate::disp::DispVariant { transpose: true, flip_i: false, flip_j: false },
            crate::disp::DispVariant { transpose: false, flip_i: true, flip_j: false },
            crate::disp::DispVariant { transpose: false, flip_i: false, flip_j: true },
            crate::disp::DispVariant { transpose: true, flip_i: true, flip_j: false },
            crate::disp::DispVariant { transpose: true, flip_i: false, flip_j: true },
            crate::disp::DispVariant { transpose: false, flip_i: true, flip_j: true },
            crate::disp::DispVariant { transpose: true, flip_i: true, flip_j: true },
        ] {
            eprintln!("== variant {variant:?}");
            seams_for(&roots, variant);
        }
    }

    fn seams_for(roots: &[crate::keyvalues::Block], variant: crate::disp::DispVariant) {
        struct Patch {
            corners: [Vec3; 4],
            boundary: Vec<(Vec3, Vec3)>,
            all: Vec<Vec3>,
            id: u32,
        }
        let mut patches: Vec<Patch> = Vec::new();
        let mut not_quad = 0;
        for root in roots {
            let solids: Vec<&crate::keyvalues::Block> = root.children_named("solid").collect();
            for solid in solids {
                let Ok(brush) = crate::brush::parse_brush(solid) else { continue };
                for side in &brush.sides {
                    let Some(disp) = side.disp.as_ref() else { continue };
                    let Some(corners) = crate::disp::quad_corners(&side.winding) else { not_quad += 1; continue };
                    let Some(mesh) = crate::disp::build_disp_with(side, disp, variant) else { continue };
                    let n = mesh.size;
                    let mut boundary = Vec::new();
                    for i in 0..n {
                        for j in 0..n {
                            if i == 0 || j == 0 || i == n - 1 || j == n - 1 {
                                boundary.push((mesh.base[i * n + j], mesh.positions[i * n + j]));
                            }
                        }
                    }
                    patches.push(Patch { corners, boundary, all: mesh.positions.clone(), id: side.id });
                }
            }
        }
        eprintln!("patches={} not_quad={not_quad}", patches.len());
        let mut shared_edges = 0;
        let mut open = 0;
        let mut worst = 0.0f32;
        let mut examples = Vec::new();
        for a in 0..patches.len() {
            for b in (a + 1)..patches.len() {
                let pa = &patches[a];
                let pb = &patches[b];
                let shared: Vec<Vec3> = pa
                    .corners
                    .iter()
                    .filter(|ca| pb.corners.iter().any(|cb| length(sub(**ca, *cb)) < 1.0))
                    .copied()
                    .collect();
                if shared.len() < 2 {
                    continue;
                }
                shared_edges += 1;
                // Vertices of A on the shared edge (near the segment between the two shared corners).
                let (c0, c1) = (shared[0], shared[1]);
                let edge = sub(c1, c0);
                let edge_len = length(edge);
                let mut gap = 0.0f32;
                for (base, v) in &pa.boundary {
                    let t = crate::brush::dot(sub(*base, c0), edge) / (edge_len * edge_len);
                    if !(-0.01..=1.01).contains(&t) {
                        continue;
                    }
                    let foot = crate::brush::add(c0, crate::brush::scale(edge, t));
                    if length(sub(*base, foot)) > 0.5 {
                        continue;
                    }
                    let nearest = pb.all.iter().map(|w| length(sub(*v, *w))).fold(f32::MAX, f32::min);
                    gap = gap.max(nearest);
                }
                if gap > 2.0 {
                    open += 1;
                    if examples.len() < 8 {
                        examples.push((pa.id, pb.id, gap));
                    }
                }
                worst = worst.max(gap);
            }
        }
        eprintln!("shared edges={shared_edges} open seams(>2u)={open} worst gap={worst:.1}");
        for (a, b, gap) in examples {
            eprintln!("  sides {a} / {b}: gap {gap:.1}");
        }
    }
}
