//! A Source map (Hammer `.vmf`) as a match world.
//!
//! No zone: the map file is compiled by `source_map` into faces, brushes
//! and entities; a donor MW2 map zone (`mp_invasion` unless
//! `IW4L_SOURCE_DONOR` names another) lends its materials, each Hammer
//! material name matched to the nearest one (`source_donors`); the faces
//! become one DPVS cell of world surfaces and the brushes a clip map
//! (`asset_world::world_source`). The gametypes see IW4 spawns written
//! from the Counter-Strike ones and a generated map script.

use std::collections::HashMap;
use std::path::Path;

use super::{LoadedWorld, lane};
use crate::lane_capability::PreparedCapability;
use crate::session_load::WorldDrawPolicy;
use asset_core::{AssetNamespace, MaterialKey, ZoneGame};
use asset_transport::{LoadProgress, StageId, find_zone_for_tree, open_zone_shared};

/// The smallest MW2 map zone (49 MB): a Source map needs a zone only for
/// what a match brings along, not for its world.
pub(crate) const DEFAULT_DONOR: &str = "mp_rust";
/// Texel edge assumed for every Hammer texture: their UVs are in texels of
/// a texture we never see, and MW2's are mostly 512.
const TEXTURE_SIZE: f32 = 512.0;

/// While the donor map zone is walked for a Source map, the world and
/// model materials it should keep textures for (everything else of its
/// world is never drawn here, so its images are not decoded).
static DONOR_KEEP: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

pub(crate) fn donor_keep() -> Option<std::collections::HashSet<String>> {
    DONOR_KEEP.lock().ok().and_then(|keep| keep.clone())
}

fn set_donor_keep(keep: Option<std::collections::HashSet<String>>) {
    if let Ok(mut slot) = DONOR_KEEP.lock() {
        *slot = keep;
    }
}

pub(crate) fn is_source_map(path: &Path) -> bool {
    asset_transport::is_source_map(path)
}

fn gap(reason: String, addr: &'static str) -> LoadedWorld {
    LoadedWorld::with_gap(
        WorldDrawPolicy::iw4(),
        PreparedCapability::PreparedWorld,
        reason,
        Some(addr),
    )
}

pub(crate) fn load_source_world(
    path: &Path,
    progress: &LoadProgress,
    shared_surfaces: asset_model::SharedXModelSurfaces,
    material_seed: asset_material::MaterialCatalog,
    common_film_visions: &super::FilmVisionCatalog,
) -> LoadedWorld {
    let mut report = Vec::new();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_else(|| "source.vmf".to_owned());
    let deflated = file_name.ends_with(".vmf.z");
    let zone_name = file_name
        .trim_end_matches(".z")
        .trim_end_matches(".vmf")
        .to_owned();

    let stage = progress.begin_scoped(StageId::MapAssets, "source", None);
    let text = match read_map_text(path, deflated) {
        Ok(text) => text,
        Err(error) => {
            stage.fail();
            return gap(
                format!("source map: read {}: {error}", path.display()),
                "assets::lane::source::load_source_world/read",
            );
        }
    };
    // A texture pack written by `cs2_textures` beside the map: the game's
    // own textures, one IWI per Hammer material, in `iw4l-maps/*.iwd`.
    let pack = read_texture_pack(&path.with_file_name(format!("{zone_name}.textures.tsv")));
    let sizes: HashMap<String, (f32, f32)> = pack
        .iter()
        .map(|row| (row.material.clone(), (row.width as f32, row.height as f32)))
        .collect();
    let mut compiled = match source_map::compile_with_sizes(&text, TEXTURE_SIZE, sizes) {
        Ok(map) => map,
        Err(error) => {
            stage.fail();
            return gap(
                format!("source map: compile {}: {error}", path.display()),
                "assets::lane::source::load_source_world/compile",
            );
        }
    };
    // The map's static props, as `cs2_textures` wrote them beside it: placed
    // as ordinary faces before anything counts materials or faces.
    let props_path = path.with_file_name(format!("{zone_name}.props.bin"));
    match vfs::read(&props_path).map_err(|e| e.to_string()).and_then(|bytes| source_map::props::PropSet::from_bytes(&bytes)) {
        Ok(set) => {
            let stats = source_map::props::append_props(&mut compiled, &set);
            report.push(format!(
                "source props: {} of {} instances placed from {} models ({} missing), {} vertices, {} triangles",
                stats.placed,
                stats.instances,
                set.models.len(),
                stats.missing_models,
                stats.vertices,
                stats.triangles
            ));
        }
        Err(error) => report.push(format!("source props: none ({error})")),
    }
    let text_len = text.len();
    drop(text);
    stage.done();
    report.push(format!(
        "source map `{zone_name}`: {} bytes compiled: solids={} sides={} drawn={} displacements={} skipped={} clip={} entity_solids={} bad={} entities={} vertices={} brushes={} bounds={:?}..{:?} sky=`{}` sun={}",
        text_len,
        compiled.stats.solids,
        compiled.stats.sides,
        compiled.stats.drawn_sides,
        compiled.stats.displacements,
        compiled.stats.skipped_solids,
        compiled.stats.clip_brushes,
        compiled.stats.entity_solids,
        compiled.stats.bad_solids,
        compiled.stats.entities,
        compiled.vertices.len(),
        compiled.brushes.len(),
        compiled.mins,
        compiled.maxs,
        compiled.sky_name,
        compiled.sun.is_some(),
    ));
    for (name, uses) in compiled.material_uses.iter().take(40) {
        report.push(format!(
            "source material: {uses:6} {name} -> {}",
            super::source_donors::candidates(name)
                .first()
                .copied()
                .unwrap_or("-")
        ));
    }

    // The donor zone lends its materials.
    let donor_name = std::env::var("IW4L_SOURCE_DONOR").unwrap_or_else(|_| DEFAULT_DONOR.to_owned());
    let donor = match find_zone_for_tree(path, &donor_name) {
        Ok(found) => found,
        Err(error) => {
            return gap(
                format!("source map: donor zone `{donor_name}`: {error}"),
                "assets::lane::source::load_source_world/donor",
            );
        }
    };
    // The donor is walked as the map it is: its materials dress the faces,
    // and everything else a map zone brings a match — weapon and body
    // models, effects, the impact table, sounds, the film look — stays.
    // Only its geometry, collision, entities and scripts are replaced.
    let image = match open_zone_shared(&donor.path) {
        Ok(image) => image,
        Err(error) => {
            return gap(
                format!("source map: open donor {}: {error}", donor.path.display()),
                "assets::lane::source::load_source_world/donor",
            );
        }
    };
    // The donor rows this map may draw with: every candidate for its
    // materials, the pack's templates, the sky and black.
    let mut keep: std::collections::HashSet<String> = compiled
        .material_uses
        .iter()
        .flat_map(|(name, _)| super::source_donors::candidates(name))
        .map(str::to_owned)
        .collect();
    for row in &pack {
        keep.extend(template_candidates(row).iter().map(|name| (*name).to_owned()));
    }
    keep.extend(["wc/sky_af_chase", "wc/sky_sp_roadkill", "wc/com_black"].map(str::to_owned));
    set_donor_keep(Some(keep));
    let mut loaded = lane(ZoneGame::Iw4).load_world(
        &donor.path,
        &image,
        progress,
        shared_surfaces,
        material_seed,
        common_film_visions,
    );
    set_donor_keep(None);
    drop(image);
    if !loaded.gaps.is_empty() {
        loaded.report.extend(report);
        loaded.report.push(format!(
            "source map: donor `{donor_name}` did not load: {}",
            loaded.gaps.iter().map(|g| g.reason.as_str()).collect::<Vec<_>>().join("; ")
        ));
        return loaded;
    }
    let mut donor_report = std::mem::take(&mut loaded.report);
    report.append(&mut donor_report);
    // `IW4L_SOURCE_DUMP_DONOR_ENTITIES=<file>` writes the donor map's entity
    // string: the reference for what the gametypes expect to find.
    if let Ok(dump) = std::env::var("IW4L_SOURCE_DUMP_DONOR_ENTITIES")
        && let Some(donor_entities) = loaded.scripts.entities()
    {
        let _ = std::fs::write(&dump, donor_entities);
    }
    report.push(format!(
        "source map donor `{donor_name}`: {} materials, {} images",
        loaded.materials.materials.len(),
        loaded.materials.images.len()
    ));

    // Every Hammer material name to a donor row, once.
    let mut resolved: HashMap<String, Option<usize>> = HashMap::new();
    let mut unmatched = Vec::new();
    for (name, _) in &compiled.material_uses {
        let mut found = None;
        for candidate in super::source_donors::candidates(name) {
            let key = MaterialKey {
                namespace: AssetNamespace::Iw4,
                name: candidate.to_owned(),
            };
            if let Some(index) = loaded.materials.material_index_by_key(&key) {
                found = Some(index.order());
                break;
            }
        }
        if found.is_none() {
            unmatched.push(name.clone());
        }
        resolved.insert(name.clone(), found);
    }
    if !unmatched.is_empty() {
        report.push(format!(
            "source map: {} materials without a donor row: {}",
            unmatched.len(),
            unmatched.join(", ")
        ));
    }
    if !pack.is_empty() {
        let added = adopt_texture_pack(&mut loaded.materials, &pack, &mut resolved);
        // A blend's second layer without its own row draws like the first.
        for (name, _) in &compiled.material_uses {
            if let Some(base) = name.strip_suffix("#2")
                && !pack.iter().any(|row| row.material == *name)
                && let Some(first) = resolved.get(base).copied()
            {
                resolved.insert(name.clone(), first);
            }
        }
        report.push(format!(
            "source textures: {added} of {} pack rows became materials (donor rows keep their lighting and surface type)",
            pack.len()
        ));
        // A face nothing matched is still solid: drawn as nothing it is an
        // invisible wall. It borrows the map's most used drawn surface of its
        // kind (plaster, metal, wood…), else the most used of all; glass
        // stays unseen, as a pane mostly is.
        let mut by_kind: HashMap<&'static str, (usize, usize)> = HashMap::new();
        let mut most_used: Option<(usize, usize)> = None;
        for (name, uses) in &compiled.material_uses {
            let Some(Some(index)) = resolved.get(name).copied() else {
                continue;
            };
            if !pack.iter().any(|row| row.material == *name) {
                continue;
            }
            let kind = source_map::surface_type_for(name);
            let best = by_kind.entry(kind).or_insert((*uses, index));
            if *uses > best.0 {
                *best = (*uses, index);
            }
            if most_used.is_none_or(|(top, _)| *uses > top) {
                most_used = Some((*uses, index));
            }
        }
        let mut borrowed = 0usize;
        for (name, _) in &compiled.material_uses {
            let kind = source_map::surface_type_for(name);
            if kind == "glass" || name.starts_with("tools/") || !matches!(resolved.get(name), Some(None)) {
                continue;
            }
            if let Some((_, index)) = by_kind.get(kind).copied().or(most_used) {
                resolved.insert(name.clone(), Some(index));
                borrowed += 1;
            }
        }
        report.push(format!("source textures: {borrowed} unmatched materials borrow a surface of their kind"));
    }
    // Only what this match draws gets its textures: the world materials
    // the map's faces resolve to, the weapon and body materials, and the
    // script models' props. The donor's other world and model materials
    // stay in the catalog (surfaces hold indices into it) but lose their
    // bindings, so their IWD reads and GPU uploads never happen. Effects,
    // HUD and everything else keep theirs.
    let mut keep: std::collections::HashSet<usize> = resolved.values().flatten().copied().collect();
    keep.extend(loaded.fpv_meshes.bound_material_indices());
    // Hands and weapons bind their materials at match install, bodies
    // too: keep everything they name.
    let model_keys: Vec<MaterialKey> = (0..)
        .map_while(|index| loaded.fpv_meshes.get_at(index))
        .flat_map(|entry| entry.material_keys.iter().flatten().cloned())
        .chain(
            loaded
                .bodies
                .names()
                .filter_map(|name| loaded.bodies.get(name))
                .flat_map(|entry| entry.material_keys.iter().flatten().cloned()),
        )
        .collect();
    for key in &model_keys {
        if let Some(index) = loaded.materials.material_index_by_key(key) {
            keep.insert(index.order());
        }
    }
    let prop_words = [
        "bomb", "suitcase", "plasticcase", "objective", "briefcase", "hands", "viewmodel", "weapon",
    ];
    let mut unbound = 0usize;
    for (index, material) in loaded.materials.materials.iter_mut().enumerate() {
        let name = material.name.as_str();
        let prop = name.starts_with("mc/") && prop_words.iter().any(|word| name.contains(word));
        if !keep.contains(&index)
            && (name.starts_with("wc/") || name.starts_with("mc/"))
            && !prop
            && !material.textures.is_empty()
        {
            material.textures.clear();
            unbound += 1;
        }
    }
    report.push(format!(
        "source textures: {unbound} donor world/model materials unbound (not drawn here); {} kept",
        keep.len()
    ));
    let image_stage = progress.begin_scoped(StageId::Images, "pack", None);
    report.extend(super::iw4::decode_map_material_images(
        &donor.path,
        &mut loaded.materials,
        image_stage,
        Vec::new(),
        Vec::new(),
    ));
    let materials = &loaded.materials;

    let stage = progress.begin_scoped(StageId::MapAssets, "geometry", None);
    let sun = compiled.sun.map(|sun| asset_world::SourceSun {
        to_sun: sun.to_sun,
        color: sun.color,
    });
    let material_of = |name: &str| resolved.get(name).copied().flatten();
    // A lightmap baked beside the map by `source_bake`: sun shadows and
    // occluded sky light per texel. Without one, a flat page.
    let bake_path = path.with_file_name(format!("{zone_name}.lightmap.bin"));
    let bake_clock = web_time::Instant::now();
    let bake = match vfs::read(&bake_path) {
        Ok(bytes) => match source_map::bake::Bake::from_bytes(&bytes) {
            // Charts are per face: a bake of another compile of the map
            // (an older one, before brushes changed role) would light the
            // wrong faces.
            Ok(bake) if bake.charts.len() != compiled.surfaces.len() => {
                report.push(format!(
                    "source lightmap: baked for {} faces, the map has {}; flat page",
                    bake.charts.len(),
                    compiled.surfaces.len()
                ));
                None
            }
            Ok(bake) => {
                report.push(format!(
                    "source lightmap: {} pages of {}x{} at {} units per texel, {} charts ({} bytes)",
                    bake.pages.len(),
                    bake.page_size,
                    bake.page_size,
                    bake.texel,
                    bake.charts.iter().filter(|c| c.is_some()).count(),
                    bytes.len()
                ));
                Some(bake)
            }
            Err(error) => {
                report.push(format!("source lightmap: {error}; flat page"));
                None
            }
        },
        Err(_) => {
            report.push("source lightmap: none baked (flat page)".to_owned());
            None
        }
    };
    let bake_ms = bake_clock.elapsed().as_millis();
    let built = asset_world::build_source_world_draw(
        &compiled,
        materials,
        &material_of,
        sun,
        true,
        bake.as_ref(),
    );
    let (mut draw, world_report) = match built {
        Ok(built) => {
            stage.done();
            built
        }
        Err(error) => {
            stage.fail();
            return gap(
                format!("source map: world: {error}"),
                "assets::lane::source::load_source_world/world",
            );
        }
    };
    report.push(format!(
        "source world: faces={} sky={} surfaces={} unmapped_faces={} batches={} aabb_nodes={} triangles={}",
        world_report.faces,
        world_report.sky_faces,
        world_report.surfaces,
        world_report.unmapped_faces,
        world_report.batches,
        world_report.aabb_nodes,
        draw.stats.triangles,
    ));
    report.push(format!(
        "source world timings: bake_read={bake_ms}ms {}",
        world_report
            .timings_ms
            .iter()
            .map(|(name, ms)| format!("{name}={ms}ms"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    if !world_report.unmapped_materials.is_empty() {
        report.push(format!(
            "source world unmapped: {}",
            world_report.unmapped_materials.join(", ")
        ));
    }

    let stage = progress.begin_scoped(StageId::MapAssets, "collision", None);
    let clip = asset_world::build_source_clip_collision(&compiled);
    stage.done();
    report.push(format!(
        "source clip: brushes={} nodes={} leaves={} leafbrushes={}",
        clip.brushes.len(),
        clip.nodes.len(),
        clip.leaves.len(),
        clip.leafbrushes.len()
    ));

    let mut entities = source_map::iw4_entity_string(&compiled);
    // The minimap `cs2_textures --radar` wrote beside the map: its picture
    // (in `iw4l-maps/<zone>_radar.iwd`) and the corners that place it.
    let radar_path = path.with_file_name(format!("{zone_name}.radar.txt"));
    let radar = match vfs::read_to_string(&radar_path) {
        Ok(text) => match source_map::Radar::parse(&text) {
            Ok(radar) => {
                report.push(format!(
                    "source radar: {} over {:?}..{:?}",
                    radar.image, radar.upper_left, radar.lower_right
                ));
                entities.push_str(&radar.corner_entities());
                Some(radar)
            }
            Err(error) => {
                report.push(format!("source radar: {error}"));
                None
            }
        },
        Err(_) => {
            report.push("source radar: none, so no minimap".to_owned());
            None
        }
    };
    let spawns = asset_world::mp_spawn_points_from_text(&entities);
    report.push(format!(
        "source spawns: {} ({} dm)",
        spawns.len(),
        spawns
            .iter()
            .filter(|spawn| spawn.classname == "mp_dm_spawn")
            .count()
    ));
    let mut declarations = asset_world::MapDeclarations::default();
    // Terrorists (axis here) attack: they plant in Search and Destroy.
    declarations.set_teams("axis", "allies");
    if let Some(radar) = &radar {
        declarations.set_compass(&radar.image);
    }
    let mut scripts = crate::ScriptSources::default();
    let module = format!("maps/mp/{zone_name}");
    let mut map_main = declarations.map_script(&entities);
    if let Some(end) = map_main.rfind('}') {
        map_main.insert_str(end, "\tthread iw4l_maps\\destructibles::main();\n");
    }
    scripts.insert_source(
        "iw4l_maps/destructibles",
        crate::map_scripts::DESTRUCTIBLES.to_owned(),
    );
    scripts.insert_source(&module, map_main);
    let intermission_view = asset_world::parse_intermission_view(&entities);
    let script_placements = asset_world::parse_script_model_placements(&entities);
    scripts.set_entities(entities);

    let min = draw.stats.min;
    let max = draw.stats.max;
    let world_bounds = draw.stats.bounds;
    let donor_world = &mut loaded.world;
    // Every surface samples reflection probe 0; the donor's probes (and
    // their decoded cubemaps, kept below) stand in until the map bakes its
    // own.
    draw.reflection_probes = donor_world
        .draw
        .as_ref()
        .map(|donor| donor.reflection_probes.clone())
        .unwrap_or_default();
    donor_world.source_namespace = Some(AssetNamespace::Iw4);
    donor_world.draw = Some(draw);
    donor_world.static_model_meshes = Vec::new();
    donor_world.static_model_instances = Vec::new();
    // The map's own script models (the Search and Destroy bomb and the
    // bomb sites' markers), drawn with the donor's captured models.
    let mut script_model_instances = Vec::new();
    let script_gaps = super::helpers::script_model_scene_instances(
        &script_placements,
        &mut donor_world.map_xmodel_scene_assets,
        &HashMap::new(),
        &mut script_model_instances,
    );
    report.push(format!(
        "source script models: {} ({} not captured by the donor)",
        script_model_instances.len(),
        script_gaps
    ));
    donor_world.script_model_instances = script_model_instances;
    donor_world.script_brush_models = Vec::new();
    donor_world.flag_descriptors = Vec::new();
    donor_world.script_structs = Vec::new();
    donor_world.dyn_ents = Default::default();
    donor_world.smodel_lighting_samples = Vec::new();
    // The donor's light grid stays: first-person weapons and bodies are lit
    // from the model lighting atlas built on it, and a map without one
    // never finishes spawning.
    donor_world.intermission_view = intermission_view;
    donor_world.min = min;
    donor_world.max = max;
    donor_world.world_bounds = world_bounds;
    donor_world.policy = WorldDrawPolicy::iw4();
    loaded.scripts = scripts;
    loaded.collision = Some(clip);
    loaded.spawns = spawns;
    loaded.facts = crate::MapFacts {
        north_yaw: Some(90.0),
        minimap_corners: radar.as_ref().map(|radar| asset_world::MinimapCorners {
            a: radar.upper_left,
            b: radar.lower_right,
        }),
        compass: radar
            .map(|radar| asset_world::MapCompassDeclaration {
                material: Some(radar.image.clone()),
                image: Some(radar.image),
                max_range: None,
                script: Some(module.clone()),
            })
            .unwrap_or_default(),
        script_sound: std::mem::take(&mut loaded.facts.script_sound),
        ..Default::default()
    };
    loaded.report = report;
    loaded
}

/// One row of a `<zone>.textures.tsv` manifest.
struct PackRow {
    material: String,
    image: String,
    width: u32,
    height: u32,
    /// `alphatest`, `translucent`, `twosided`, `blend`: what the game's
    /// material says about how its texture is drawn.
    flags: Vec<String>,
}

impl PackRow {
    fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }
}

/// The donor materials a pack row is drawn like, by what its flags ask
/// for: a colour-only lit material for most (no MW2 normal or specular map
/// showing through the game's own texture), an alpha-tested one for
/// cut-outs, a blended one for glass.
fn template_candidates(row: &PackRow) -> &'static [&'static str] {
    if row.has("alphatest") {
        &["wc/me_fence_chainlink", "wc/me_fence_chainlink_dblface", "wc/ch_factory_floorgrate"]
    } else if row.has("translucent") {
        &["wc/ad_sign128x256", "wc/gfx_dirt"]
    } else {
        &["wc/me_wire_black", "wc/window_newspaper", "wc/ch_factory_rustybeam3"]
    }
}

/// The Hammer file's text, inflated when the games folder holds the
/// `.vmf.z` the tools write for shipping.
fn read_map_text(path: &Path, deflated: bool) -> Result<String, String> {
    if !deflated {
        return vfs::read_to_string(path).map_err(|error| error.to_string());
    }
    let bytes = vfs::read(path).map_err(|error| error.to_string())?;
    let raw = source_map::inflate(&bytes)?;
    String::from_utf8(raw).map_err(|error| format!("vmf.z: {error}"))
}

fn read_texture_pack(path: &Path) -> Vec<PackRow> {
    let Ok(text) = vfs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            let mut cells = line.split('\t');
            let material = cells.next()?.trim().to_ascii_lowercase();
            let image = cells.next()?.trim().to_owned();
            let width = cells.next()?.trim().parse().ok()?;
            let height = cells.next()?.trim().parse().ok()?;
            let _source = cells.next();
            let flags = cells
                .next()
                .unwrap_or("")
                .split(',')
                .map(|flag| flag.trim().to_ascii_lowercase())
                .filter(|flag| !flag.is_empty())
                .collect();
            (!material.is_empty() && !image.is_empty()).then_some(PackRow {
                material,
                image,
                width,
                height,
                flags,
            })
        })
        .collect()
}

/// Each pack row becomes a copy of a template material (a colour-only lit
/// one unless the row asks for alpha test or blending), with the pack's
/// image as its colour map: the technique is the donor's, the texture is
/// the game's own. Without a template the family donor it stood in for
/// is copied instead.
fn adopt_texture_pack(
    materials: &mut asset_material::MaterialCatalog,
    pack: &[PackRow],
    resolved: &mut HashMap<String, Option<usize>>,
) -> usize {
    let mut added = 0;
    for row in pack {
        let template = template_candidates(row).iter().find_map(|name| {
            let key = MaterialKey {
                namespace: AssetNamespace::Iw4,
                name: (*name).to_owned(),
            };
            materials.material_index_by_key(&key).map(|index| index.order())
        });
        let Some(donor) = template.or_else(|| resolved.get(&row.material).copied().flatten()) else {
            continue;
        };
        let Some(mut material) = materials.materials.get(donor).cloned() else {
            continue;
        };
        let Some(binding) = material
            .textures
            .iter_mut()
            .find(|binding| binding.semantic == asset_material::TS_COLOR_MAP)
        else {
            continue;
        };
        let Some(mut image) = binding.image.and_then(|index| materials.images.get(index).cloned())
        else {
            continue;
        };
        image.name = asset_core::AssetRef::decode(&row.image);
        image.width = row.width.min(u32::from(u16::MAX)) as u16;
        image.height = row.height.min(u32::from(u16::MAX)) as u16;
        image.level_count = (32 - row.width.max(row.height).max(1).leading_zeros()) as u8;
        image.payload = std::sync::Arc::new(Vec::new());
        image.decoded = None;
        image.decoded_variant = None;
        image.decoded_by = None;
        image.pending_decode = None;
        image.common_owned = false;
        let image_index = materials.images.len();
        materials.images.push(image);
        binding.image = Some(image_index);
        material.name = asset_core::AssetRef::decode(&format!("cs2/{}", row.material));
        let index = materials.materials.len();
        materials.materials.push(material);
        resolved.insert(row.material.clone(), Some(index));
        added += 1;
    }
    added
}
