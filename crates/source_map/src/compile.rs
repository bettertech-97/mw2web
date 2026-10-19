//! One Hammer map file into flat geometry, collision brushes and its entities.

use std::collections::{BTreeMap, HashMap};

use crate::brush::{Brush, Vec3, cross, dot, normalize, parse_brush, scale, sub};
use crate::disp::build_disp;
use crate::keyvalues::{Block, parse};

/// IW4 brush contents.
pub const CONTENTS_SOLID: u32 = 0x1;
pub const CONTENTS_SKY: u32 = 0x800;
pub const CONTENTS_CLIPSHOT: u32 = 0x2000;
pub const CONTENTS_PLAYERCLIP: u32 = 0x1_0000;
pub const CONTENTS_DETAIL: u32 = 0x800_0000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vertex {
    pub position: Vec3,
    pub normal: Vec3,
    /// Tangent along the texture's U axis and the sign of its bitangent.
    pub tangent: [f32; 4],
    pub uv: [f32; 2],
    /// Displacement blend weight (0 = first texture, 1 = second).
    pub alpha: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Surface {
    /// Source material name, lower case, forward slashes.
    pub material: String,
    pub first_vertex: u32,
    pub vertex_count: u32,
    pub first_index: u32,
    pub index_count: u32,
    pub mins: Vec3,
    pub maxs: Vec3,
    pub normal: Vec3,
    pub displacement: bool,
    /// Sky brush face: drawn only when the engine has a sky to put there.
    pub sky: bool,
    /// A placed prop's draw call (see `props.rs`): not planar, lit per
    /// vertex rather than through a lightmap chart.
    pub prop: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CollisionBrush {
    /// Outward planes `[nx, ny, nz, dist]`; the first six are the axial
    /// bounding box, in the order +x -x +y -y +z -z.
    pub planes: Vec<[f32; 4]>,
    pub contents: u32,
    /// The surface type the brush sounds like when stepped on or shot, as
    /// `weapon_iw4::SURFACE_TYPE_NAMES` spells it.
    pub surface_type: &'static str,
    pub mins: Vec3,
    pub maxs: Vec3,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entity {
    pub classname: String,
    pub pairs: Vec<(String, String)>,
    pub solids: usize,
}

impl Entity {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    pub fn origin(&self) -> Option<Vec3> {
        let v = crate::brush::floats(self.get("origin")?).ok()?;
        (v.len() == 3).then(|| [v[0], v[1], v[2]])
    }

    pub fn angles(&self) -> Vec3 {
        self.get("angles")
            .and_then(|a| crate::brush::floats(a).ok())
            .filter(|v| v.len() == 3)
            .map_or([0.0; 3], |v| [v[0], v[1], v[2]])
    }
}

/// The map's sun, from `light_environment`.
/// A brush entity kept only as a volume: a bomb site, say, which the IW4
/// gametypes want as a trigger.
#[derive(Clone, Debug, PartialEq)]
pub struct TriggerVolume {
    pub classname: String,
    pub targetname: String,
    pub mins: Vec3,
    pub maxs: Vec3,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sun {
    /// Unit vector toward the sun.
    pub to_sun: Vec3,
    /// Linear sun colour, brightness folded in.
    pub color: Vec3,
    /// Linear sky ambient colour.
    pub ambient: Vec3,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub solids: usize,
    pub sides: usize,
    pub drawn_sides: usize,
    pub displacements: usize,
    /// Displacement sides whose grid could not be built; drawn flat.
    pub displacement_fallbacks: usize,
    pub skipped_solids: usize,
    pub clip_brushes: usize,
    pub entity_solids: usize,
    pub bad_solids: usize,
    pub entities: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CompiledMap {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub surfaces: Vec<Surface>,
    pub brushes: Vec<CollisionBrush>,
    /// Volumes of trigger-like brush entities (`func_bomb_target`), in
    /// the order the clip map numbers their inline models from 1.
    pub triggers: Vec<TriggerVolume>,
    pub entities: Vec<Entity>,
    pub mins: Vec3,
    pub maxs: Vec3,
    pub sky_name: String,
    pub sun: Option<Sun>,
    pub stats: Stats,
    /// Source material name -> faces using it, most used first.
    pub material_uses: Vec<(String, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideKind {
    Draw,
    Decal,
    NoDraw,
    Sky,
    Clip,
    ShotClip,
    /// hint, skip, trigger, areaportal, fog volumes: no brush at all.
    NonSolid,
}

fn side_kind(material: &str) -> SideKind {
    if let Some(tool) = material.strip_prefix("tools/tools") {
        return match tool {
            "nodraw" | "invisible" | "invisibleladder" | "invisibledisplacement" | "origin" => {
                SideKind::NoDraw
            }
            // Solid black in Hammer, closing off what should not be seen into.
            "black" => SideKind::Draw,
            // NPC and physics clips stop no player in Counter-Strike: they
            // fall through to non-solid below.
            "clip" | "playerclip" => SideKind::Clip,
            "blockbullets" => SideKind::ShotClip,
            "skybox" | "skybox2d" => SideKind::Sky,
            _ => SideKind::NonSolid,
        };
    }
    // Decal materials on brush faces are often the only face a wall has
    // (the brush under them was culled), so they are drawn like any other.
    SideKind::Draw
}

/// What a brush of this entity class contributes to the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SolidRole {
    World,
    /// Drawn, not solid.
    Illusion,
    /// Solid, not drawn (`rendermode 10`).
    Hidden,
    Skip,
}

/// The role of an entity's brushes, as Counter-Strike starts the round
/// with them: `func_brush` keeps its `Solidity` (0 with the entity, 1
/// never, 2 always) and starts gone when `StartDisabled`; `rendermode 10`
/// draws nothing. `func_clip_vphysics` (physics props only) and
/// `func_nav_blocker` (bots' paths only) never stop a player.
fn solid_role(entity: &Block, classname: &str) -> SolidRole {
    let key = |name: &str| entity.get(name).map(str::trim);
    let hidden = key("rendermode") == Some("10");
    let solid = match classname {
        "func_brush" => {
            if key("StartDisabled") == Some("1") {
                return SolidRole::Skip;
            }
            key("Solidity") != Some("1")
        }
        "worldspawn" | "func_detail" | "func_breakable" | "func_breakable_surf" | "func_wall"
        | "func_wall_toggle" | "func_door" | "func_door_rotating" | "func_movelinear"
        | "func_physbox" | "func_physbox_multiplayer" | "func_lod" | "func_rotating"
        | "func_tracktrain" | "func_conveyor" | "func_reflective_glass" | "func_monitor"
        | "prop_static" => true,
        "func_illusionary" | "func_dustmotes" | "func_dustcloud" => false,
        _ => return SolidRole::Skip,
    };
    match (solid, hidden) {
        (true, false) => SolidRole::World,
        (true, true) => SolidRole::Hidden,
        (false, false) => SolidRole::Illusion,
        (false, true) => SolidRole::Skip,
    }
}

/// The surface type a Source material name suggests, in IW4's vocabulary.
pub fn surface_type_for(material: &str) -> &'static str {
    let name = material.rsplit('/').next().unwrap_or(material);
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w) || material.contains(w));
    if has(&["glass", "window"]) {
        "glass"
    } else if has(&["metal", "steel", "iron", "ibeam", "rail", "grate", "chainlink"]) {
        "metal"
    } else if has(&["wood", "plank", "plywood", "beam", "crate", "door"]) {
        "wood"
    } else if has(&["carpet", "rug", "cloth", "fabric", "tarp"]) {
        "carpet"
    } else if has(&["brick"]) {
        "brick"
    } else if has(&["tile", "ceramic", "marble"]) {
        "ceramic"
    } else if has(&["plaster", "stucco", "wall"]) {
        "plaster"
    } else if has(&["sand", "dust"]) {
        "sand"
    } else if has(&["dirt", "ground", "mud", "nature"]) {
        "dirt"
    } else if has(&["gravel", "rubble"]) {
        "gravel"
    } else if has(&["rock", "stone", "cobble"]) {
        "rock"
    } else if has(&["asphalt", "road", "blacktop"]) {
        "asphalt"
    } else if has(&["grass", "foliage", "leaf"]) {
        "grass"
    } else {
        "concrete"
    }
}

struct Builder {
    out: CompiledMap,
    texture_size: f32,
    texture_sizes: HashMap<String, (f32, f32)>,
    uses: BTreeMap<String, usize>,
}

impl Builder {
    fn add_brush(&mut self, brush: &Brush, role: SolidRole) {
        let kinds: Vec<SideKind> = brush
            .sides
            .iter()
            .map(|side| side_kind(&side.material))
            .collect();
        self.out.stats.sides += kinds.len();
        let meshes: Vec<Option<crate::disp::DispMesh>> = brush
            .sides
            .iter()
            .map(|side| side.disp.as_ref().and_then(|disp| build_disp(side, disp)))
            .collect();
        if role == SolidRole::Skip
            || kinds.iter().all(|k| *k == SideKind::NonSolid)
            || kinds.iter().any(|k| *k == SideKind::NonSolid)
                && !kinds.iter().any(|k| matches!(k, SideKind::Draw | SideKind::NoDraw | SideKind::Sky))
        {
            self.out.stats.skipped_solids += 1;
            return;
        }
        let clip = kinds.iter().any(|k| *k == SideKind::Clip);
        let shot_clip = kinds.iter().any(|k| *k == SideKind::ShotClip);
        let sky = kinds.iter().any(|k| *k == SideKind::Sky);
        let contents = if clip {
            CONTENTS_PLAYERCLIP
        } else if shot_clip {
            CONTENTS_CLIPSHOT
        } else if sky {
            CONTENTS_SOLID | CONTENTS_SKY
        } else if matches!(role, SolidRole::World | SolidRole::Hidden) {
            CONTENTS_SOLID
        } else {
            0
        };
        let drawn_material = brush
            .sides
            .iter()
            .zip(&kinds)
            .find(|(_, k)| **k == SideKind::Draw)
            .map(|(s, _)| s.material.as_str())
            .unwrap_or("concrete");
        if contents != 0 {
            // A displaced top face is collided with as its own mesh; the
            // brush under it stops below the lowest point of that mesh so
            // it never pokes through where the ground dips.
            let mut top = brush.maxs[2];
            for (side, mesh) in brush.sides.iter().zip(&meshes) {
                if let Some(mesh) = mesh
                    && side.plane[2] > 0.7
                {
                    let lowest = mesh
                        .positions
                        .iter()
                        .map(|p| p[2])
                        .fold(f32::INFINITY, f32::min);
                    top = top.min(lowest - 1.0);
                }
            }
            // Ground displaced below the whole brush (a thin base brush, the
            // mesh dipping under it) leaves nothing of the brush to keep:
            // clamped back up it stood over the visible ground, and players
            // walked on it in midair (Inferno's grass, 15 units up).
            let keep = top >= brush.mins[2] + 1.0;
            let top = top.max(brush.mins[2] + 1.0);
            let mut planes: Vec<[f32; 4]> = vec![
                [1.0, 0.0, 0.0, brush.maxs[0]],
                [-1.0, 0.0, 0.0, -brush.mins[0]],
                [0.0, 1.0, 0.0, brush.maxs[1]],
                [0.0, -1.0, 0.0, -brush.mins[1]],
                [0.0, 0.0, 1.0, top],
                [0.0, 0.0, -1.0, -brush.mins[2]],
            ];
            for side in &brush.sides {
                let n = [side.plane[0], side.plane[1], side.plane[2]];
                let axial = n.iter().any(|c| (c.abs() - 1.0).abs() < 1e-4);
                if !axial {
                    planes.push(side.plane);
                }
            }
            if keep {
                self.out.brushes.push(CollisionBrush {
                    planes,
                    contents,
                    surface_type: surface_type_for(drawn_material),
                    mins: brush.mins,
                    maxs: [brush.maxs[0], brush.maxs[1], top],
                });
            }
            if clip {
                self.out.stats.clip_brushes += 1;
            }
        }
        if clip || role == SolidRole::Hidden {
            return;
        }
        for ((side, kind), mesh) in brush.sides.iter().zip(&kinds).zip(&meshes) {
            if side.winding.len() < 3 {
                continue;
            }
            let is_sky = *kind == SideKind::Sky;
            if !(*kind == SideKind::Draw || is_sky) {
                continue;
            }
            self.out.stats.drawn_sides += 1;
            *self.uses.entry(side.material.clone()).or_insert(0) += 1;
            let normal = [side.plane[0], side.plane[1], side.plane[2]];
            let tangent = {
                let u = side.uaxis.dir;
                let t = normalize(sub(u, scale(normal, dot(u, normal))));
                let sign = if dot(cross(normal, t), side.vaxis.dir) < 0.0 {
                    -1.0
                } else {
                    1.0
                };
                [t[0], t[1], t[2], sign]
            };
            let (tex_w, tex_h) = self
                .texture_sizes
                .get(&side.material)
                .copied()
                .unwrap_or((self.texture_size, self.texture_size));
            let uv = |p: Vec3| {
                [
                    (dot(p, side.uaxis.dir) / side.uaxis.scale + side.uaxis.offset) / tex_w,
                    (dot(p, side.vaxis.dir) / side.vaxis.scale + side.vaxis.offset) / tex_h,
                ]
            };
            let first_vertex = self.out.vertices.len() as u32;
            let first_index = self.out.indices.len() as u32;
            let mut mins = [f32::INFINITY; 3];
            let mut maxs = [f32::NEG_INFINITY; 3];
            let mut displacement = false;
            // Triangles painted over to a blend material's second layer:
            // where their indices start, and how many.
            let mut second: Option<(u32, u32)> = None;
            if let Some(mesh) = mesh {
                displacement = true;
                self.out.stats.displacements += 1;
                for (i, position) in mesh.positions.iter().enumerate() {
                    self.out.vertices.push(Vertex {
                        position: *position,
                        normal: mesh.normals[i],
                        tangent,
                        uv: uv(*position),
                        alpha: mesh.alphas[i] / 255.0,
                    });
                }
                if mesh.alphas.iter().any(|a| *a > 127.5) {
                    let (mut first_layer, mut second_layer) = (Vec::new(), Vec::new());
                    for tri in mesh.indices.chunks_exact(3) {
                        let alpha = tri.iter().map(|i| mesh.alphas[*i as usize]).sum::<f32>() / 3.0;
                        let layer = if alpha > 127.5 { &mut second_layer } else { &mut first_layer };
                        layer.extend(tri.iter().map(|i| i + first_vertex));
                    }
                    self.out.indices.extend_from_slice(&first_layer);
                    let split = self.out.indices.len() as u32;
                    self.out.indices.extend_from_slice(&second_layer);
                    if !second_layer.is_empty() {
                        second = Some((split, second_layer.len() as u32));
                    }
                } else {
                    self.out
                        .indices
                        .extend(mesh.indices.iter().map(|i| i + first_vertex));
                }
            } else {
                if side.disp.is_some() {
                    self.out.stats.displacement_fallbacks += 1;
                }
                for p in &side.winding {
                    self.out.vertices.push(Vertex {
                        position: *p,
                        normal,
                        tangent,
                        uv: uv(*p),
                        alpha: 0.0,
                    });
                }
                for i in 1..side.winding.len() as u32 - 1 {
                    self.out
                        .indices
                        .extend_from_slice(&[first_vertex, first_vertex + i, first_vertex + i + 1]);
                }
            }
            for v in &self.out.vertices[first_vertex as usize..] {
                for axis in 0..3 {
                    mins[axis] = mins[axis].min(v.position[axis]);
                    maxs[axis] = maxs[axis].max(v.position[axis]);
                }
            }
            let vertex_count = self.out.vertices.len() as u32 - first_vertex;
            let end = self.out.indices.len() as u32;
            let first_count = second.map_or(end, |(split, _)| split) - first_index;
            if first_count > 0 {
                self.out.surfaces.push(Surface {
                    material: side.material.clone(),
                    first_vertex,
                    vertex_count,
                    first_index,
                    index_count: first_count,
                    mins,
                    maxs,
                    normal,
                    displacement,
                    sky: is_sky,
                    prop: false,
                });
            }
            if let Some((split, count)) = second {
                let material = format!("{}#2", side.material);
                *self.uses.entry(material.clone()).or_insert(0) += 1;
                self.out.surfaces.push(Surface {
                    material,
                    first_vertex,
                    vertex_count,
                    first_index: split,
                    index_count: count,
                    mins,
                    maxs,
                    normal,
                    displacement,
                    sky: is_sky,
                    prop: false,
                });
            }
        }
    }

    fn add_solids(&mut self, block: &Block, role: SolidRole, entity: bool) {
        for solid in block.children_named("solid") {
            self.out.stats.solids += 1;
            if entity {
                self.out.stats.entity_solids += 1;
            }
            match parse_brush(solid) {
                Ok(brush) => self.add_brush(&brush, role),
                Err(_) => self.out.stats.bad_solids += 1,
            }
        }
    }
}

fn parse_color(text: &str) -> Option<([f32; 3], f32)> {
    let v = crate::brush::floats(text).ok()?;
    match v.as_slice() {
        [r, g, b] => Some(([r / 255.0, g / 255.0, b / 255.0], 1.0)),
        [r, g, b, brightness, ..] => Some(([r / 255.0, g / 255.0, b / 255.0], *brightness)),
        _ => None,
    }
}

fn sun_from(entity: &Entity) -> Option<Sun> {
    let angles = entity.angles();
    let pitch = entity
        .get("pitch")
        .and_then(|p| p.parse::<f32>().ok())
        .unwrap_or(angles[0]);
    let yaw = angles[1];
    let (sp, cp) = pitch.to_radians().sin_cos();
    let (sy, cy) = yaw.to_radians().sin_cos();
    // Source pitches light downward with negative values: the light
    // travels along (cp cy, cp sy, sp); the sun sits the other way.
    let travel = [cp * cy, cp * sy, sp];
    let to_sun = normalize(scale(travel, -1.0));
    let (color, brightness) = entity.get("_light").and_then(parse_color)?;
    let (ambient, ambient_brightness) = entity
        .get("_ambient")
        .and_then(parse_color)
        .unwrap_or(([0.5, 0.6, 0.8], 200.0));
    // Counter-Strike's suns run 600-700; at /520 Inferno came out a third
    // brighter than CS2's own screenshots of it (mean 177 against ~130 of
    // 255, 5% of the screen clipped white). /750 brings it in line.
    let sun_scale = (brightness / 750.0).clamp(0.4, 1.5);
    let ambient_scale = (ambient_brightness / 400.0).clamp(0.2, 1.5);
    Some(Sun {
        to_sun,
        color: scale(color, sun_scale),
        ambient: scale(ambient, ambient_scale),
    })
}

/// Compiles a `.vmf` text. `texture_size` is the texel edge every Hammer
/// texture is assumed to have when `texture_sizes` does not name it.
pub fn compile(text: &str, texture_size: f32) -> Result<CompiledMap, String> {
    compile_with_sizes(text, texture_size, HashMap::new())
}

/// [`compile`] with the real size of each material's texture, so Hammer's
/// texel-space UVs come out exactly.
pub fn compile_with_sizes(
    text: &str,
    texture_size: f32,
    texture_sizes: HashMap<String, (f32, f32)>,
) -> Result<CompiledMap, String> {
    let roots = parse(text)?;
    let mut builder = Builder {
        out: CompiledMap::default(),
        texture_size,
        texture_sizes,
        uses: BTreeMap::new(),
    };
    for root in &roots {
        match root.name.as_str() {
            "world" => {
                builder.out.sky_name = root.get("skyname").unwrap_or("").to_owned();
                let mut entity = Entity {
                    classname: "worldspawn".to_owned(),
                    pairs: root.pairs.clone(),
                    solids: root.children_named("solid").count(),
                };
                entity.pairs.retain(|(k, _)| k != "id");
                builder.out.entities.push(entity);
                builder.add_solids(root, SolidRole::World, false);
                for group in root.children_named("group") {
                    builder.add_solids(group, SolidRole::World, false);
                }
            }
            "entity" => {
                let classname = root.get("classname").unwrap_or("").to_ascii_lowercase();
                let mut entity = Entity {
                    classname: classname.clone(),
                    pairs: root.pairs.clone(),
                    solids: root.children_named("solid").count(),
                };
                entity.pairs.retain(|(k, _)| k != "id");
                if classname == "light_environment" && builder.out.sun.is_none() {
                    builder.out.sun = sun_from(&entity);
                }
                if classname == "func_bomb_target" {
                    let mut mins = [f32::INFINITY; 3];
                    let mut maxs = [f32::NEG_INFINITY; 3];
                    for brush in root.children_named("solid").filter_map(|solid| parse_brush(solid).ok()) {
                        for axis in 0..3 {
                            mins[axis] = mins[axis].min(brush.mins[axis]);
                            maxs[axis] = maxs[axis].max(brush.maxs[axis]);
                        }
                    }
                    if mins[0] <= maxs[0] {
                        builder.out.triggers.push(TriggerVolume {
                            classname: classname.clone(),
                            targetname: root.get("targetname").unwrap_or("").to_owned(),
                            mins,
                            maxs,
                        });
                    }
                }
                builder.add_solids(root, solid_role(root, &classname), true);
                builder.out.entities.push(entity);
            }
            _ => {}
        }
    }
    builder.out.stats.entities = builder.out.entities.len();
    let mut mins = [f32::INFINITY; 3];
    let mut maxs = [f32::NEG_INFINITY; 3];
    for v in &builder.out.vertices {
        for axis in 0..3 {
            mins[axis] = mins[axis].min(v.position[axis]);
            maxs[axis] = maxs[axis].max(v.position[axis]);
        }
    }
    for b in &builder.out.brushes {
        for axis in 0..3 {
            mins[axis] = mins[axis].min(b.mins[axis]);
            maxs[axis] = maxs[axis].max(b.maxs[axis]);
        }
    }
    if builder.out.vertices.is_empty() {
        return Err("the map has no drawable faces".into());
    }
    builder.out.mins = mins;
    builder.out.maxs = maxs;
    let mut uses: Vec<(String, usize)> = builder.uses.into_iter().collect();
    uses.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    builder.out.material_uses = uses;
    Ok(builder.out)
}
