//! A Source map as the IW4 renderer and clip map read one.
//!
//! The compiled map (`source_map`) is faces with a material name and convex
//! brushes. Here the faces become IW4 world surfaces — one per material per
//! spatial cell, indices absolute so visible neighbours merge into one draw
//! — under a single DPVS cell with an AABB tree, lit by the map's sun and no
//! lightmap; the brushes become a clip map under a k-d tree of axial splits.

use std::collections::BTreeMap;

use bevy::asset::RenderAssetUsages;
use bevy::image::ImageSampler;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use dpvs_iw4::{AABB_NODE_STRIDE, AabbNodeView, Bounds, SurfRange};

use crate::clip_collision::{ClipBrush, ClipCmodel, ClipCollision, ClipMapMaterial};
use crate::world_draw::{
    CameraRangeKind, CameraSurfRange, CameraSurfRanges, DpvsWorldData, SurfaceDrawFields,
    WorldDraw, WorldLightmap, WorldPrimaryLight, WorldVertexPayload, make_material_batches,
};
use crate::world_mesh::{WorldMeshError, WorldMeshStats};
use crate::{SurfaceCastsSunShadow, world_capture_from_casters};
use asset_material::MaterialCatalog;
use clipmap_iw4::{ClipLeaf, ClipNode};
use source_map::CompiledMap;

/// Faces of one material within a cube this wide share a surface.
const SURFACE_CELL: f32 = 1024.0;
/// Triangles a surface may hold (`tri_count` is 16 bits).
const SURFACE_TRI_LIMIT: usize = 60_000;
const AABB_LEAF_SURFACES: usize = 12;
const AABB_MAX_DEPTH: usize = 24;
const CLIP_LEAF_BRUSHES: usize = 6;
const CLIP_MAX_DEPTH: usize = 22;

#[derive(Clone, Debug, Default)]
pub struct SourceWorldReport {
    pub faces: usize,
    pub sky_faces: usize,
    pub surfaces: usize,
    pub unmapped_faces: usize,
    pub unmapped_materials: Vec<String>,
    pub batches: usize,
    pub aabb_nodes: usize,
    /// Wall time of each step of the build, for the load report.
    pub timings_ms: Vec<(&'static str, u32)>,
}

pub struct SourceSun {
    pub to_sun: [f32; 3],
    pub color: [f32; 3],
}

fn pack_color(rgba: [f32; 4]) -> u32 {
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    u32::from_le_bytes([byte(rgba[2]), byte(rgba[1]), byte(rgba[0]), byte(rgba[3])])
}

fn pack_iw4_vertex(
    position: [f32; 3],
    normal: [f32; 3],
    tangent: [f32; 4],
    color: [f32; 4],
    uv: [f32; 2],
    lm: [f32; 2],
    packed_normal: u32,
    packed_tangent: u32,
) -> [u8; asset_iw4::size::GFX_WORLD_VERTEX] {
    let mut out = [0u8; asset_iw4::size::GFX_WORLD_VERTEX];
    let mut put = |at: usize, bytes: [u8; 4]| out[at..at + 4].copy_from_slice(&bytes);
    put(0, position[0].to_le_bytes());
    put(4, position[1].to_le_bytes());
    put(8, position[2].to_le_bytes());
    put(12, tangent[3].to_le_bytes());
    put(16, pack_color(color).to_le_bytes());
    put(20, uv[0].to_le_bytes());
    put(24, uv[1].to_le_bytes());
    put(28, lm[0].to_le_bytes());
    put(32, lm[1].to_le_bytes());
    put(36, packed_normal.to_le_bytes());
    put(40, packed_tangent.to_le_bytes());
    let _ = normal;
    out
}

/// Every lit world material samples the lightmap, so a world without one
/// is refused at draw time. Until the map is baked, one flat page lights
/// everything evenly: an even ambient, a straight-up direction, full sun.
fn flat_lightmap() -> WorldLightmap {
    const SIZE: u32 = 4;
    let texels = (SIZE * SIZE) as usize;
    let ambient: Vec<u8> = (0..texels).flat_map(|_| [78u8, 78, 78, 128]).collect();
    let directional: Vec<u8> = (0..texels).flat_map(|_| [78u8, 78, 78, 128]).collect();
    let mut secondary = ambient.clone();
    secondary.extend_from_slice(&directional);
    let usage = RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD;
    let page = |data: Vec<u8>, height: u32, format: TextureFormat| {
        let mut image = Image::new(
            Extent3d {
                width: SIZE,
                height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            data,
            format,
            usage,
        );
        image.sampler = ImageSampler::linear();
        image
    };
    let mask = page(vec![255u8; texels], SIZE, TextureFormat::R8Unorm);
    WorldLightmap {
        primary_image: Some(mask.clone()),
        secondary_image: Some(page(secondary, SIZE * 2, TextureFormat::Rgba8Unorm)),
        secondary_b_image: None,
        ambient_image: page(ambient, SIZE, TextureFormat::Rgba8Unorm),
        directional_image: page(directional, SIZE, TextureFormat::Rgba8Unorm),
        sun_mask_image: mask,
        ambient_source_name: "source map flat lightmap".to_owned(),
        sun_mask_source_name: "source map flat sun mask".to_owned(),
        ambient_size: UVec2::new(SIZE, SIZE),
        sun_mask_size: UVec2::new(SIZE, SIZE),
    }
}

/// How bright open sky lights a surface, on the flat page's scale (112
/// left CS2's near-white plaster clipping in full sun; see `sun_from`).
const BAKED_AMBIENT: f32 = 88.0;
/// The share of that an enclosed spot keeps: bounce light that no ray
/// measured.
const BAKED_AMBIENT_FLOOR: f32 = 0.45;

/// One baked page as a lightmap of the engine: the sun mask is the bake's
/// visibility, ambient and directional the occluded sky light.
fn baked_lightmap(page: &source_map::bake::Page, size: u32) -> WorldLightmap {
    let texels = (size * size) as usize;
    let ambient: Vec<u8> = page
        .ambient
        .iter()
        .take(texels)
        .flat_map(|&a| {
            let open = f32::from(a) / 255.0;
            let level = (BAKED_AMBIENT * (BAKED_AMBIENT_FLOOR + (1.0 - BAKED_AMBIENT_FLOOR) * open))
                .round()
                .clamp(0.0, 255.0) as u8;
            [level, level, level, 128]
        })
        .collect();
    let mut secondary = ambient.clone();
    secondary.extend_from_slice(&ambient);
    let usage = RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD;
    let image = |data: Vec<u8>, height: u32, format: TextureFormat| {
        let mut image = Image::new(
            Extent3d {
                width: size,
                height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            data,
            format,
            usage,
        );
        image.sampler = ImageSampler::linear();
        image
    };
    let mut sun = page.sun.clone();
    sun.resize(texels, 255);
    let mask = image(sun, size, TextureFormat::R8Unorm);
    WorldLightmap {
        primary_image: Some(mask.clone()),
        secondary_image: Some(image(secondary, size * 2, TextureFormat::Rgba8Unorm)),
        secondary_b_image: None,
        ambient_image: image(ambient.clone(), size, TextureFormat::Rgba8Unorm),
        directional_image: image(ambient, size, TextureFormat::Rgba8Unorm),
        sun_mask_image: mask,
        ambient_source_name: "source map baked lightmap".to_owned(),
        sun_mask_source_name: "source map baked sun mask".to_owned(),
        ambient_size: UVec2::new(size, size),
        sun_mask_size: UVec2::new(size, size),
    }
}

/// Builds the drawable world. `material_of` maps a Source material name to
/// a row of `materials`; faces it cannot map are dropped and counted.
pub fn build_source_world_draw(
    map: &CompiledMap,
    materials: &MaterialCatalog,
    material_of: &dyn Fn(&str) -> Option<usize>,
    sun: Option<SourceSun>,
    draw_sky: bool,
    bake: Option<&source_map::bake::Bake>,
) -> Result<(WorldDraw, SourceWorldReport), WorldMeshError> {
    let mut report = SourceWorldReport::default();
    let mut clock = web_time::Instant::now();
    let mut lap = |report: &mut SourceWorldReport, name: &'static str| {
        report.timings_ms.push((name, clock.elapsed().as_millis() as u32));
        clock = web_time::Instant::now();
    };
    let vertex_count = map.vertices.len();
    // Lightmap coordinates: a baked chart per face, or the middle of the
    // flat page.
    let mut vertex_lightmap_uv = vec![[0.5f32, 0.5f32]; vertex_count];
    let mut face_page = vec![0u16; map.surfaces.len()];
    // Props read a reserved texel (sunlit or shadowed) and carry their sky
    // occlusion as vertex colour.
    let mut vertex_shade = vec![1.0f32; vertex_count];
    if let Some(bake) = bake {
        let size = bake.page_size as f32;
        for (face_index, face) in map.surfaces.iter().enumerate() {
            if !face.prop {
                continue;
            }
            let texel = if bake.face_sun.get(face_index).copied() == Some(2) {
                source_map::bake::PROP_TEXEL_LIT
            } else {
                source_map::bake::PROP_TEXEL_SHADOW
            };
            let uv = [(texel[0] as f32 + 0.5) / size, (texel[1] as f32 + 0.5) / size];
            let first = face.first_vertex as usize;
            for index in first..(first + face.vertex_count as usize).min(vertex_count) {
                vertex_lightmap_uv[index] = uv;
                let open = f32::from(bake.vertex_open.get(index).copied().unwrap_or(255)) / 255.0;
                vertex_shade[index] = BAKED_AMBIENT_FLOOR + (1.0 - BAKED_AMBIENT_FLOOR) * open;
            }
        }
        for (face_index, chart) in bake.charts.iter().enumerate() {
            let (Some(chart), Some(face)) = (chart, map.surfaces.get(face_index)) else {
                continue;
            };
            face_page[face_index] = chart.page;
            let first = face.first_vertex as usize;
            for (offset, v) in map.vertices[first..first + face.vertex_count as usize]
                .iter()
                .enumerate()
            {
                vertex_lightmap_uv[first + offset] = chart.uv(v.position, bake.texel, bake.page_size);
            }
        }
    }
    lap(&mut report, "lightmap_uv");
    let mut packed_vertices = Vec::with_capacity(vertex_count);
    let mut positions = Vec::with_capacity(vertex_count);
    let mut normals = Vec::with_capacity(vertex_count);
    let mut tangents = Vec::with_capacity(vertex_count);
    let mut colors = Vec::with_capacity(vertex_count);
    let mut texture_uvs = Vec::with_capacity(vertex_count);
    let mut lightmap_uvs = Vec::with_capacity(vertex_count);
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let white = [1.0, 1.0, 1.0, 1.0];
    let black = [0.0, 0.0, 0.0, 1.0];
    let mut last_tangent = ([f32::NAN; 3], 0u32);
    // Hammer's `toolsblack` is solid black whatever is bound: the vertex
    // colour, which the world shaders multiply in, does that here.
    let mut vertex_black = vec![false; vertex_count];
    for face in &map.surfaces {
        if face.material == "tools/toolsblack" {
            let first = face.first_vertex as usize;
            for flag in vertex_black.iter_mut().skip(first).take(face.vertex_count as usize) {
                *flag = true;
            }
        }
    }
    for (vertex_index, v) in map.vertices.iter().enumerate() {
        let shade = vertex_shade[vertex_index];
        let color = if vertex_black[vertex_index] {
            black
        } else if shade < 1.0 {
            [shade, shade, shade, 1.0]
        } else {
            white
        };
        for axis in 0..3 {
            min[axis] = min[axis].min(v.position[axis]);
            max[axis] = max[axis].max(v.position[axis]);
        }
        // Tangents repeat along a face (and every prop carries the same one),
        // so the last answer is usually the next one.
        let n = asset_model::pack_unit_vec_fast(v.normal);
        let tangent = [v.tangent[0], v.tangent[1], v.tangent[2]];
        let t = if last_tangent.0 == tangent {
            last_tangent.1
        } else {
            let packed = asset_model::pack_unit_vec_fast(tangent);
            last_tangent = (tangent, packed);
            packed
        };
        let lm = vertex_lightmap_uv[vertex_index];
        packed_vertices.push(pack_iw4_vertex(
            v.position, v.normal, v.tangent, color, v.uv, lm, n, t,
        ));
        positions.push(v.position);
        normals.push(v.normal);
        tangents.push(v.tangent);
        colors.push(color);
        texture_uvs.push(v.uv);
        lightmap_uvs.push(lm);
    }

    lap(&mut report, "vertices");
    // Faces grouped into surfaces: one per material per spatial cell.
    let primary_light: u8 = if sun.is_some() { 1 } else { 0 };
    let mut groups: BTreeMap<(usize, u16, i32, i32, i32), Vec<usize>> = BTreeMap::new();
    let mut unmapped: BTreeMap<String, usize> = BTreeMap::new();
    // Diagnostics: `IW4L_SOURCE_ONLY=disp|flat` draws one kind of face.
    let only = std::env::var("IW4L_SOURCE_ONLY").unwrap_or_default();
    for (face_index, face) in map.surfaces.iter().enumerate() {
        report.faces += 1;
        if (only == "disp" && !face.displacement) || (only == "flat" && face.displacement) {
            continue;
        }
        if face.sky {
            report.sky_faces += 1;
            if !draw_sky {
                continue;
            }
        }
        let Some(material) = material_of(&face.material) else {
            report.unmapped_faces += 1;
            *unmapped.entry(face.material.clone()).or_insert(0) += 1;
            continue;
        };
        let cell = |axis: usize| ((face.mins[axis] + face.maxs[axis]) * 0.5 / SURFACE_CELL).floor() as i32;
        groups
            .entry((material, face_page[face_index], cell(0), cell(1), cell(2)))
            .or_default()
            .push(face_index);
    }
    report.unmapped_materials = unmapped
        .into_iter()
        .map(|(name, faces)| format!("{name} ({faces})"))
        .collect();

    lap(&mut report, "grouping");
    let mut packed_indices: Vec<u32> = Vec::with_capacity(map.indices.len());
    let mut surface_index_ranges = Vec::new();
    let mut surface_materials = Vec::new();
    let mut surface_draw_fields = Vec::new();
    let mut surface_bounds = Vec::new();
    let mut surface_sky = Vec::new();
    let mut surface_prop = Vec::new();
    let mut surface_pages: Vec<u8> = Vec::new();
    for ((material, page, _, _, _), faces) in &groups {
        let mut pending = faces.iter().copied().peekable();
        while pending.peek().is_some() {
            let range_start = packed_indices.len() as u32;
            let mut mins = [f32::INFINITY; 3];
            let mut maxs = [f32::NEG_INFINITY; 3];
            let mut tris = 0usize;
            let mut sky = false;
            let mut prop = true;
            while let Some(&face_index) = pending.peek() {
                let face = &map.surfaces[face_index];
                let face_tris = face.index_count as usize / 3;
                if tris > 0 && tris + face_tris > SURFACE_TRI_LIMIT {
                    break;
                }
                pending.next();
                let first = face.first_index as usize;
                let end = first + face.index_count as usize;
                // IW4 winds its front faces the other way round from Hammer.
                for tri in map.indices[first..end].chunks_exact(3) {
                    packed_indices.extend_from_slice(&[tri[0], tri[2], tri[1]]);
                }
                tris += face_tris;
                sky |= face.sky;
                prop &= face.prop;
                for axis in 0..3 {
                    mins[axis] = mins[axis].min(face.mins[axis]);
                    maxs[axis] = maxs[axis].max(face.maxs[axis]);
                }
            }
            if tris == 0 {
                continue;
            }
            surface_index_ranges.push((range_start, (tris * 3) as u32));
            surface_materials.push(Some(*material));
            surface_draw_fields.push(SurfaceDrawFields {
                first_vertex: 0,
                tri_count: tris as u16,
                base_index: range_start,
                lightmap_index: (*page).min(255) as u8,
                reflection_probe_index: 0,
                primary_light_index: primary_light,
            });
            surface_bounds.push(Bounds::from_mins_maxs(mins, maxs));
            surface_sky.push(sky);
            surface_prop.push(prop);
            surface_pages.push((*page).min(255) as u8);
        }
    }
    lap(&mut report, "indices");
    let surface_count = surface_index_ranges.len();
    if surface_count == 0 {
        return Err(WorldMeshError::NoGeometry);
    }
    if surface_count > usize::from(u16::MAX) {
        return Err(WorldMeshError::NoGeometry);
    }
    report.surfaces = surface_count;

    // Brush faces cast the dynamic sun shadow (onto players); props keep
    // their shadows in the bake, which spares the shadow pass a million
    // triangles a frame.
    let mut casters = SurfaceCastsSunShadow::with_len(surface_count);
    for i in 0..surface_count {
        if !surface_sky[i] && !surface_prop[i] {
            casters.set(i);
        }
    }
    let surface_lightmapped = vec![true; surface_count];
    let surface_lightmap_indices = surface_pages;
    let surface_reflection_probes = vec![0u8; surface_count];
    let surface_primary_lights = vec![primary_light; surface_count];
    let surface_first_vertex = vec![0u32; surface_count];

    let (batches, surface_batch_ranges) = make_material_batches(
        &positions,
        &normals,
        &tangents,
        &colors,
        &texture_uvs,
        &lightmap_uvs,
        &packed_indices,
        &surface_index_ranges,
        &surface_materials,
        &surface_lightmapped,
        &surface_lightmap_indices,
        &surface_primary_lights,
        &surface_reflection_probes,
    );
    report.batches = batches.len();
    lap(&mut report, "batches");

    // One DPVS cell; its AABB tree orders the surfaces.
    let n = surface_count as u32;
    let mut dpvs = DpvsWorldData::new(CameraSurfRanges::new(
        CameraSurfRange {
            kind: CameraRangeKind::LitOpaque,
            begin: 0,
            end: n,
        },
        vec![
            CameraSurfRange {
                kind: CameraRangeKind::LitTrans,
                begin: n,
                end: n,
            },
            CameraSurfRange {
                kind: CameraRangeKind::Emissive,
                begin: n,
                end: n,
            },
        ],
    ));
    dpvs.cell_count = 1;
    dpvs.nodes = vec![1];
    dpvs.static_surface_count = surface_count;
    dpvs.static_surface_count_no_decal = surface_count;
    dpvs.lit_opaque_begin = 0;
    dpvs.lit_opaque_end = n;
    dpvs.emissive_surfs_begin = n;
    dpvs.emissive_surfs_end = n;
    dpvs.surface_bounds = surface_bounds.clone();
    let (nodes, sorted) = build_aabb_tree(&surface_bounds);
    report.aabb_nodes = nodes.len();
    dpvs.cell_roots = vec![SurfRange {
        start: 0,
        count: surface_count as u16,
    }];
    dpvs.aabb_trees = vec![nodes];
    dpvs.aabb_smodel_indices = vec![Vec::new()];
    dpvs.sorted_surf_index = sorted;
    dpvs.portals_per_cell = vec![Vec::new()];
    dpvs.cell_reflection_probes = vec![Vec::new()];
    let dpvs = dpvs.checked()?;
    lap(&mut report, "aabb");

    let mut primary_lights = Vec::new();
    let mut sun_primary_light_count = 0;
    if let Some(sun) = sun {
        let dark = WorldPrimaryLight {
            is_sun: false,
            light_type: 0,
            can_cast_shadow: false,
            exponent: 0,
            color: [0.0; 3],
            direction: [0.0, 0.0, 1.0],
            origin: [0.0; 3],
            radius: 0.0,
            cos_outer: 0.0,
            cos_inner: 0.0,
            cos_half_fov_expanded: 0.0,
            def_name: None,
            falloff_image_width: None,
            lmap_lookup_start: 0,
            attenuation_image: None,
            attenuation_sampler: 0,
            t5_attenuation: None,
            t5_falloff: None,
            t5_a_ab_b: None,
            t5_angle: None,
            t5_cookie0: None,
            t5_cookie1: None,
            t5_cookie2: None,
            t5_diffuse_color: None,
            t5_specular_color: None,
        };
        let sun_light = WorldPrimaryLight {
            is_sun: true,
            light_type: lighting_iw4::GFX_LIGHT_TYPE_DIR,
            can_cast_shadow: true,
            color: sun.color,
            direction: sun.to_sun,
            ..dark.clone()
        };
        primary_lights.push(dark);
        primary_lights.push(sun_light);
        sun_primary_light_count = 1;
    }

    let stats = WorldMeshStats {
        vertices: positions.len(),
        triangles: packed_indices.len() / 3,
        surfaces: surface_count,
        skipped_surfaces: report.unmapped_faces,
        sky_surfaces: report.sky_faces,
        sky_material: None,
        unrouted_surfaces: 0,
        undecided_state_bits_surfaces: 0,
        min,
        max,
        bounds: Some([min[0], min[1], min[2], max[0], max[1], max[2]]),
    };
    let _ = materials;
    let lightmap = match bake {
        Some(bake) if !bake.pages.is_empty() => bake
            .pages
            .iter()
            .map(|page| Some(baked_lightmap(page, bake.page_size)))
            .collect(),
        _ => vec![Some(flat_lightmap())],
    };
    lap(&mut report, "lightmap_pages");
    Ok((
        WorldDraw {
            batches,
            sky_model: None,
            lightmap: Ok(lightmap),
            stats,
            packed_vertices: WorldVertexPayload::Iw4(packed_vertices),
            vertex_layer: Vec::new(),
            surface_vertex_layer: Vec::new(),
            surface_first_vertex,
            surface_draw_fields,
            positions,
            normals,
            tangents,
            colors,
            texture_uvs,
            lightmap_uvs,
            packed_indices,
            surface_index_ranges,
            surface_batch_ranges,
            surface_lightmapped,
            surface_lightmap_indices,
            surface_reflection_probes,
            surface_primary_lights,
            sort_key_distortion: None,
            capture: world_capture_from_casters(casters),
            brush_models: Vec::new(),
            brush_model_bounds: Vec::new(),
            surface_materials,
            primary_lights,
            light_defs: Vec::new(),
            sun_primary_light_count,
            sun_stages: Vec::new(),
            light_region_hulls: None,
            shadow_geometry: Vec::new(),
            reflection_probes: Vec::new(),
            dpvs,
            outdoor_image_name: None,
            outdoor_image: None,
            outdoor_lookup: [0; 16],
            sun_effects: None,
            t5_sun_parse_exposure: None,
            t6_exposure: None,
            sky_dynamic_intensity: None,
            t5_sun_light: None,
            t5_tree_scatter_intensity: None,
            t5_tree_scatter_amount: None,
            t5_exposure_volume_count: 0,
        },
        report,
    ))
}

fn union_bounds(bounds: &[Bounds], items: &[u16]) -> Bounds {
    let mut mins = [f32::INFINITY; 3];
    let mut maxs = [f32::NEG_INFINITY; 3];
    for &i in items {
        let b = bounds[usize::from(i)];
        let (mid, half) = (b.mid(), b.half());
        for axis in 0..3 {
            mins[axis] = mins[axis].min(mid[axis] - half[axis]);
            maxs[axis] = maxs[axis].max(mid[axis] + half[axis]);
        }
    }
    if items.is_empty() {
        return Bounds::from_mins_maxs([0.0; 3], [0.0; 3]);
    }
    Bounds::from_mins_maxs(mins, maxs)
}

/// An AABB tree over the surfaces: children follow their parent in the
/// node array, and every node's surfaces are one run of the sorted index.
fn build_aabb_tree(bounds: &[Bounds]) -> (Vec<AabbNodeView>, Vec<u16>) {
    let mut nodes: Vec<AabbNodeView> = Vec::new();
    let mut sorted: Vec<u16> = Vec::with_capacity(bounds.len());
    let items: Vec<u16> = (0..bounds.len() as u16).collect();
    nodes.push(placeholder());
    build_aabb_node(0, items, 0, bounds, &mut nodes, &mut sorted);
    (nodes, sorted)
}

fn placeholder() -> AabbNodeView {
    AabbNodeView {
        bounds: Bounds::from_mins_maxs([0.0; 3], [0.0; 3]),
        child_count: 0,
        children_offset: 0,
        start_surf: 0,
        surface_count: 0,
        start_surf_no_decal: 0,
        surface_count_no_decal: 0,
        smodel_index_start: 0,
        smodel_index_count: 0,
    }
}

fn build_aabb_node(
    index: usize,
    mut items: Vec<u16>,
    depth: usize,
    bounds: &[Bounds],
    nodes: &mut Vec<AabbNodeView>,
    sorted: &mut Vec<u16>,
) {
    let node_bounds = union_bounds(bounds, &items);
    let start = sorted.len() as u16;
    if items.len() <= AABB_LEAF_SURFACES || depth >= AABB_MAX_DEPTH {
        sorted.extend_from_slice(&items);
        nodes[index] = AabbNodeView {
            bounds: node_bounds,
            child_count: 0,
            children_offset: 0,
            start_surf: start,
            surface_count: items.len() as u16,
            start_surf_no_decal: start,
            surface_count_no_decal: items.len() as u16,
            smodel_index_start: 0,
            smodel_index_count: 0,
        };
        return;
    }
    let half = node_bounds.half();
    let axis = (0..3)
        .max_by(|&a, &b| half[a].partial_cmp(&half[b]).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(0);
    items.sort_by(|&a, &b| {
        bounds[usize::from(a)].mid()[axis]
            .partial_cmp(&bounds[usize::from(b)].mid()[axis])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let right = items.split_off(items.len() / 2);
    let left = items;
    let first_child = nodes.len();
    nodes.push(placeholder());
    nodes.push(placeholder());
    build_aabb_node(first_child, left, depth + 1, bounds, nodes, sorted);
    build_aabb_node(first_child + 1, right, depth + 1, bounds, nodes, sorted);
    let count = sorted.len() as u16 - start;
    nodes[index] = AabbNodeView {
        bounds: node_bounds,
        child_count: 2,
        children_offset: ((first_child - index) * AABB_NODE_STRIDE) as i32,
        start_surf: start,
        surface_count: count,
        start_surf_no_decal: start,
        surface_count_no_decal: count,
        smodel_index_start: 0,
        smodel_index_count: 0,
    };
}

/// The surface type index (`weapon_iw4::SURFACE_TYPE_NAMES`) a brush sounds like.
fn surface_type_index(name: &str) -> u32 {
    weapon_iw4::SURFACE_TYPE_NAMES
        .iter()
        .position(|n| *n == name)
        .unwrap_or(0) as u32
}

/// The clip map: every brush as outward planes, under a k-d tree.
pub fn build_source_clip_collision(map: &CompiledMap) -> ClipCollision {
    let mut out = ClipCollision::default();
    let mut material_rows: BTreeMap<u32, usize> = BTreeMap::new();
    for brush in &map.brushes {
        let surface_type = surface_type_index(brush.surface_type);
        let surface_flags = surface_type << 20;
        material_rows.entry(surface_type).or_insert_with(|| {
            out.materials.push(ClipMapMaterial {
                name: brush.surface_type.to_owned(),
                surface_flags,
                content_flags: brush.contents,
            });
            out.materials.len() - 1
        });
        out.brushes.push(ClipBrush {
            planes: brush.planes.clone(),
            contents: brush.contents,
            plane_surface_flags: vec![surface_flags; brush.planes.len()],
            glass_encoded: 0,
        });
    }
    build_displacement_mesh(map, &mut out, &mut material_rows);
    let items: Vec<u16> = (0..map.brushes.len().min(usize::from(u16::MAX)) as u16).collect();
    build_clip_node(
        None,
        items,
        0,
        map,
        &mut out.nodes,
        &mut out.leaves,
        &mut out.leafbrushes,
    );
    let mins = map.mins;
    let maxs = map.maxs;
    let half = [
        (maxs[0] - mins[0]) * 0.5,
        (maxs[1] - mins[1]) * 0.5,
        (maxs[2] - mins[2]) * 0.5,
    ];
    out.cmodels.push(ClipCmodel {
        mins,
        maxs,
        radius: (half[0] * half[0] + half[1] * half[1] + half[2] * half[2]).sqrt(),
        first_brush: 0,
        num_brushes: 0,
    });
    // Inline models 1.. are the trigger volumes (bomb sites) as plain boxes
    // relative to the site's centre, which the entity string gives as the
    // trigger's origin: `*(2n+1)` is site n's plant volume, `*(2n+2)` the
    // box of its defuse trigger, which the gametype moves onto the bomb.
    for trigger in &map.triggers {
        let center = [
            (trigger.mins[0] + trigger.maxs[0]) * 0.5,
            (trigger.mins[1] + trigger.maxs[1]) * 0.5,
            trigger.mins[2],
        ];
        let mins = [
            trigger.mins[0] - center[0],
            trigger.mins[1] - center[1],
            trigger.mins[2] - center[2] - 16.0,
        ];
        let maxs = [
            trigger.maxs[0] - center[0],
            trigger.maxs[1] - center[1],
            (trigger.maxs[2] - center[2]).max(72.0),
        ];
        let box_model = |mins: [f32; 3], maxs: [f32; 3]| {
            let half = [
                (maxs[0] - mins[0]) * 0.5,
                (maxs[1] - mins[1]) * 0.5,
                (maxs[2] - mins[2]) * 0.5,
            ];
            ClipCmodel {
                mins,
                maxs,
                radius: (half[0] * half[0] + half[1] * half[1] + half[2] * half[2]).sqrt(),
                first_brush: 0,
                num_brushes: 0,
            }
        };
        out.cmodels.push(box_model(mins, maxs));
        out.cmodels.push(box_model([-48.0, -48.0, -24.0], [48.0, 48.0, 80.0]));
    }
    out
}

/// One partition of the displacement collision mesh while its tree is built.
struct MeshPart {
    partition: usize,
    material: u16,
    mins: [f32; 3],
    maxs: [f32; 3],
}

/// Displacement faces as collision triangles: IW4's terrain tables, which
/// the tracer walks beside the brushes. Each displacement becomes
/// partitions of up to 255 triangles under one AABB tree, so a player
/// stands on the sand itself rather than on the slab under it.
fn build_displacement_mesh(
    map: &CompiledMap,
    out: &mut ClipCollision,
    material_rows: &mut BTreeMap<u32, usize>,
) {
    use clipmap_iw4::{ClipAabbNode, ClipMeshTables, ClipPartition, VERTS_PER_SEGMENT};
    let mut mesh = ClipMeshTables::default();
    let mut tri_material_index: Vec<u16> = Vec::new();
    let mut parts: Vec<MeshPart> = Vec::new();
    let mut edge_bits: Vec<u8> = Vec::new();
    let mut set_edge_bit = |bits: &mut Vec<u8>, bit: usize| {
        if bits.len() <= bit >> 3 {
            bits.resize((bit >> 3) + 1, 0);
        }
        bits[bit >> 3] |= 1 << (bit & 7);
    };
    for face in map.surfaces.iter().filter(|face| face.displacement) {
        let surface_type = surface_type_index(source_map::compile::surface_type_for(&face.material));
        let surface_flags = surface_type << 20;
        let row = *material_rows.entry(surface_type).or_insert_with(|| {
            out.materials.push(ClipMapMaterial {
                name: source_map::compile::surface_type_for(&face.material).to_owned(),
                surface_flags,
                content_flags: source_map::CONTENTS_SOLID,
            });
            out.materials.len() - 1
        }) as u16;
        let first_vertex = face.first_vertex as usize;
        let vertex_count = face.vertex_count as usize;
        // Triangle indices are u16 offsets from a 1024-vertex segment base;
        // a face whose vertices would not fit starts a fresh segment.
        let mut vert_base = mesh.verts.len();
        let mut segment = vert_base / VERTS_PER_SEGMENT;
        if vert_base - segment * VERTS_PER_SEGMENT + vertex_count > usize::from(u16::MAX) {
            segment += 1;
            mesh.verts.resize(segment * VERTS_PER_SEGMENT, [0.0; 3]);
            vert_base = mesh.verts.len();
        }
        if segment > usize::from(u8::MAX) {
            break;
        }
        let rel_base = vert_base - segment * VERTS_PER_SEGMENT;
        mesh.verts.extend(
            map.vertices[first_vertex..first_vertex + vertex_count]
                .iter()
                .map(|v| v.position),
        );
        let first_index = face.first_index as usize;
        let indices = &map.indices[first_index..first_index + face.index_count as usize];
        for chunk in indices.chunks(3 * 255) {
            let first_tri = mesh.tri_indices.len() / 3;
            let mut mins = [f32::INFINITY; 3];
            let mut maxs = [f32::NEG_INFINITY; 3];
            for tri in chunk.chunks_exact(3) {
                let p = [
                    map.vertices[tri[0] as usize].position,
                    map.vertices[tri[1] as usize].position,
                    map.vertices[tri[2] as usize].position,
                ];
                let e1 = [p[1][0] - p[0][0], p[1][1] - p[0][1], p[1][2] - p[0][2]];
                let e2 = [p[2][0] - p[0][0], p[2][1] - p[0][1], p[2][2] - p[0][2]];
                let n = [
                    e1[1] * e2[2] - e1[2] * e2[1],
                    e1[2] * e2[0] - e1[0] * e2[2],
                    e1[0] * e2[1] - e1[1] * e2[0],
                ];
                let vn = map.vertices[tri[0] as usize].normal;
                // The tracer is one-sided and takes the face from the winding,
                // IW4's (D3D's) clockwise way: its normal is
                // (v0 - v2) x (v0 - v1), the opposite of `n` above. So `n` must
                // point away from where the displacement faces. Wound the other
                // way, every displacement faced into the ground: a player fell
                // through the sand onto the brush under it (950 points of
                // Mirage's floor, `source_holes`).
                let ordered = if n[0] * vn[0] + n[1] * vn[1] + n[2] * vn[2] > 0.0 {
                    [tri[0], tri[2], tri[1]]
                } else {
                    [tri[0], tri[1], tri[2]]
                };
                for index in ordered {
                    let position = map.vertices[index as usize].position;
                    for axis in 0..3 {
                        mins[axis] = mins[axis].min(position[axis]);
                        maxs[axis] = maxs[axis].max(position[axis]);
                    }
                    mesh.tri_indices
                        .push((index as usize - first_vertex + rel_base) as u16);
                }
                let tri_index = mesh.tri_surface_flags.len();
                for k in 0..3 {
                    set_edge_bit(&mut edge_bits, tri_index * 3 + k);
                }
                mesh.tri_surface_flags.push(surface_flags);
                mesh.tri_content_flags.push(source_map::CONTENTS_SOLID);
                tri_material_index.push(row);
            }
            let partition = mesh.partitions.len();
            mesh.partitions.push(ClipPartition {
                tri_count: (chunk.len() / 3) as u8,
                first_tri: first_tri as i32,
                first_vert_segment: segment as u8,
                border_count: 0,
                first_border: 0,
            });
            parts.push(MeshPart {
                partition,
                material: row,
                mins,
                maxs,
            });
        }
    }
    if parts.is_empty() {
        return;
    }
    mesh.tri_edge_is_walkable = edge_bits;
    let mut nodes: Vec<ClipAabbNode> = vec![ClipAabbNode::default()];
    build_mesh_aabb(0, &mut parts, &mut nodes);
    mesh.aabb_trees = nodes;
    mesh.aabb_roots = vec![0];
    out.mesh = std::sync::Arc::new(mesh);
    out.tri_material_index = tri_material_index;
}

/// Fills `nodes[slot]` with the tree over `parts`; children sit side by
/// side, as the tracer expects.
fn build_mesh_aabb(slot: usize, parts: &mut [MeshPart], nodes: &mut Vec<clipmap_iw4::ClipAabbNode>) {
    let mut mins = [f32::INFINITY; 3];
    let mut maxs = [f32::NEG_INFINITY; 3];
    for part in parts.iter() {
        for axis in 0..3 {
            mins[axis] = mins[axis].min(part.mins[axis]);
            maxs[axis] = maxs[axis].max(part.maxs[axis]);
        }
    }
    let origin = [
        (mins[0] + maxs[0]) * 0.5,
        (mins[1] + maxs[1]) * 0.5,
        (mins[2] + maxs[2]) * 0.5,
    ];
    let half_size = [
        (maxs[0] - mins[0]) * 0.5 + 0.5,
        (maxs[1] - mins[1]) * 0.5 + 0.5,
        (maxs[2] - mins[2]) * 0.5 + 0.5,
    ];
    if parts.len() == 1 {
        nodes[slot] = clipmap_iw4::ClipAabbNode {
            origin,
            half_size,
            material_index: parts[0].material,
            child_count: 0,
            u: parts[0].partition as i32,
        };
        return;
    }
    let axis = (0..3)
        .max_by(|&a, &b| (maxs[a] - mins[a]).total_cmp(&(maxs[b] - mins[b])))
        .unwrap_or(0);
    parts.sort_by(|a, b| {
        (a.mins[axis] + a.maxs[axis]).total_cmp(&(b.mins[axis] + b.maxs[axis]))
    });
    let (left, right) = parts.split_at_mut(parts.len() / 2);
    let first_child = nodes.len();
    nodes.push(clipmap_iw4::ClipAabbNode::default());
    nodes.push(clipmap_iw4::ClipAabbNode::default());
    build_mesh_aabb(first_child, left, nodes);
    build_mesh_aabb(first_child + 1, right, nodes);
    nodes[slot] = clipmap_iw4::ClipAabbNode {
        origin,
        half_size,
        material_index: 0,
        child_count: 2,
        u: first_child as i32,
    };
}

/// Returns the child reference for the parent: a node index, or `!leaf`.
fn build_clip_node(
    slot: Option<(usize, usize)>,
    items: Vec<u16>,
    depth: usize,
    map: &CompiledMap,
    nodes: &mut Vec<ClipNode>,
    leaves: &mut Vec<ClipLeaf>,
    leafbrushes: &mut Vec<u16>,
) -> i32 {
    let brush_bounds = |i: u16| {
        let b = &map.brushes[usize::from(i)];
        (b.mins, b.maxs)
    };
    let mut split = None;
    if items.len() > CLIP_LEAF_BRUSHES && depth < CLIP_MAX_DEPTH {
        let mut mins = [f32::INFINITY; 3];
        let mut maxs = [f32::NEG_INFINITY; 3];
        for &i in &items {
            let (bmin, bmax) = brush_bounds(i);
            for axis in 0..3 {
                mins[axis] = mins[axis].min(bmin[axis]);
                maxs[axis] = maxs[axis].max(bmax[axis]);
            }
        }
        let mut best: Option<(usize, f32, Vec<u16>, Vec<u16>)> = None;
        let mut axes: Vec<usize> = (0..3).collect();
        axes.sort_by(|&a, &b| {
            (maxs[b] - mins[b])
                .partial_cmp(&(maxs[a] - mins[a]))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for axis in axes {
            let mut mids: Vec<f32> = items
                .iter()
                .map(|&i| {
                    let (bmin, bmax) = brush_bounds(i);
                    (bmin[axis] + bmax[axis]) * 0.5
                })
                .collect();
            mids.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let value = mids[mids.len() / 2];
            let mut front = Vec::new();
            let mut back = Vec::new();
            for &i in &items {
                let (bmin, bmax) = brush_bounds(i);
                if bmax[axis] > value {
                    front.push(i);
                }
                if bmin[axis] < value {
                    back.push(i);
                }
            }
            let progress = front.len() < items.len() && back.len() < items.len();
            if progress && best.as_ref().is_none_or(|b| front.len() + back.len() < b.2.len() + b.3.len()) {
                best = Some((axis, value, front, back));
            }
        }
        split = best;
    }
    match split {
        Some((axis, value, front, back)) => {
            let index = nodes.len();
            let mut normal = [0.0f32; 3];
            normal[axis] = 1.0;
            nodes.push(ClipNode {
                plane: [normal[0], normal[1], normal[2], value],
                children: [0, 0],
            });
            let front_child = build_clip_node(Some((index, 0)), front, depth + 1, map, nodes, leaves, leafbrushes);
            let back_child = build_clip_node(Some((index, 1)), back, depth + 1, map, nodes, leaves, leafbrushes);
            nodes[index].children = [front_child, back_child];
            let _ = slot;
            index as i32
        }
        None => {
            let first = leafbrushes.len() as u32;
            leafbrushes.extend_from_slice(&items);
            leaves.push(ClipLeaf {
                first_brush: first,
                num_brushes: items.len() as u16,
                first_coll_aabb_index: 0,
                coll_aabb_count: 0,
            });
            !((leaves.len() - 1) as i32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aabb_tree_spans_are_runs_of_the_sorted_index() {
        let bounds: Vec<Bounds> = (0..100)
            .map(|i| {
                let x = (i % 10) as f32 * 100.0;
                let y = (i / 10) as f32 * 100.0;
                Bounds::from_mins_maxs([x, y, 0.0], [x + 50.0, y + 50.0, 10.0])
            })
            .collect();
        let (nodes, sorted) = build_aabb_tree(&bounds);
        assert_eq!(sorted.len(), 100);
        let mut seen = sorted.clone();
        seen.sort_unstable();
        assert_eq!(seen, (0..100u16).collect::<Vec<_>>());
        for (index, node) in nodes.iter().enumerate() {
            let end = usize::from(node.start_surf) + usize::from(node.surface_count);
            assert!(end <= sorted.len());
            if node.child_count != 0 {
                let child0 = index + node.children_offset as usize / AABB_NODE_STRIDE;
                let mut covered = 0;
                for c in 0..usize::from(node.child_count) {
                    let child = &nodes[child0 + c];
                    assert_eq!(usize::from(child.start_surf), usize::from(node.start_surf) + covered);
                    covered += usize::from(child.surface_count);
                }
                assert_eq!(covered, usize::from(node.surface_count));
            }
        }
    }
}


#[cfg(test)]
mod displacement_collision_tests {
    use super::*;
    use crate::clip_collision::MASK_PLAYER_SOLID;

    /// A box dropped onto a flat displacement must stop on it, from above.
    #[test]
    fn displacement_floor_holds_a_falling_box() {
        let up = [0.0f32, 0.0, 1.0];
        let vertex = |x: f32, y: f32| source_map::Vertex {
            position: [x, y, 0.0],
            normal: up,
            tangent: [1.0, 0.0, 0.0, 1.0],
            uv: [0.0, 0.0],
            alpha: 0.0,
        };
        let mut map = CompiledMap::default();
        map.vertices = vec![vertex(-64.0, -64.0), vertex(64.0, -64.0), vertex(64.0, 64.0), vertex(-64.0, 64.0)];
        // Counter-clockwise from above, as the compiler emits them.
        map.indices = vec![0, 1, 2, 0, 2, 3];
        map.surfaces = vec![source_map::Surface {
            material: "nature/sand".to_owned(),
            first_vertex: 0,
            vertex_count: 4,
            first_index: 0,
            index_count: 6,
            mins: [-64.0, -64.0, 0.0],
            maxs: [64.0, 64.0, 0.0],
            normal: up,
            displacement: true,
            sky: false,
            prop: false,
        }];
        map.mins = [-64.0, -64.0, -64.0];
        map.maxs = [64.0, 64.0, 64.0];
        let clip = build_source_clip_collision(&map);
        let hit = clip.sweep_box([10.0, 5.0, 40.0], [10.0, 5.0, -40.0], [-15.0, -15.0, 0.0], [15.0, 15.0, 70.0], MASK_PLAYER_SOLID);
        assert!(hit.fraction < 1.0, "fell through the displacement: {hit:?}");
        assert!(hit.endpos[2].abs() < 1.0, "stopped at {:?}", hit.endpos);
        assert!(hit.normal[2] > 0.9, "normal {:?}", hit.normal);
    }
}
