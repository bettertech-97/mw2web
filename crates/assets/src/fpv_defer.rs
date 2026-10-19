//! Which first-person weapon textures a match decodes up front.
//!
//! The common zone binds a view model for every weapon in the game; decoding
//! all of them up front was the largest share of a match's texture work and
//! of a browser's download. Only the hands and the weapons a player can hold
//! at spawn (the stock classes, plus the account's own, when the page or the
//! class store names them) are decoded with the map; the rest are decoded
//! the first time a weapon is held (`render_anim`'s FPV job asks for them).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// The stock classes' weapons and the host's stand-ins.
const STOCK_WEAPONS: &[&str] = &[
    // the five stock classes
    "famas_mp", "spas12_mp", "tar21_mp", "scar_mp", "ump45_mp", "intervention_mp",
    "riotshield_mp", "usp_mp", "beretta_mp", "glock_mp",
    // the showcase custom classes and the host's stand-ins
    "m4_mp", "mp5k_mp", "m16_mp", "ak47_mp", "rpd_mp",
    "frag_grenade_mp", "flash_grenade_mp", "semtex_mp", "claymore_mp",
];

static PRELOAD: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Names the weapons (bare `x_mp` or `iw4:weapon/x_mp`) whose view models
/// decode with the map; the stock set stays in on top of them.
pub fn set_preload_weapons(names: impl IntoIterator<Item = String>) {
    let mut list = PRELOAD.lock().unwrap_or_else(|e| e.into_inner());
    for name in names {
        let bare = name.rsplit('/').next().unwrap_or(&name).to_owned();
        if !bare.is_empty() && !list.contains(&bare) {
            list.push(bare);
        }
    }
}

pub fn preload_weapons() -> Vec<String> {
    let list = PRELOAD.lock().unwrap_or_else(|e| e.into_inner());
    STOCK_WEAPONS
        .iter()
        .map(|name| (*name).to_owned())
        .chain(list.iter().cloned())
        .collect()
}

/// Whether deferral is on: `IW4L_DEFER_FPV=0` keeps every weapon decoded up
/// front (desktop; the browser has no environment and always defers).
pub fn deferral_enabled() -> bool {
    std::env::var("IW4L_DEFER_FPV").map_or(true, |value| value != "0")
}

pub struct FpvOnlyImages {
    pub materials: HashSet<usize>,
    pub images: HashSet<usize>,
    pub kept_meshes: usize,
    pub megapixels: f64,
}

/// The catalog images reachable only through first-person meshes that are
/// neither the hands nor a preload weapon's gun: what a match can leave for
/// later. An image a world model, a projectile or a kept mesh also binds
/// stays, whatever else binds it.
/// Materials that are weapons to the registry but props to the map (the
/// bomb case, the UAV, the care package): never deferred, whatever binds
/// them, since the map draws them through no weapon path.
const KEEP_WORDS: &[&str] = &[
    "bomb", "suitcase", "plasticcase", "objective", "briefcase", "uav", "flare", "oma", "lambert",
    "default", "sentry", "airdrop", "care", "laptop", "satchel", "package", "trophy",
];

pub fn fpv_only_images(
    catalog: &asset_material::MaterialDefinitions,
    fpv: &asset_model::FpvMeshCatalog,
    world_weapons: &asset_model::WorldWeaponCatalog,
    projectiles: &asset_model::ProjectileMeshCatalog,
    scene_models: &asset_world::MapXModelSceneCatalog,
    weapons: &asset_game::WeaponRegistry,
    preload: &[String],
    defer_world_models: bool,
) -> FpvOnlyImages {
    let mut keep_meshes = HashSet::new();
    if let Some(hands) = fpv.hands() {
        for order in 0..fpv.len() {
            if fpv
                .get_at(order)
                .is_some_and(|entry| Arc::ptr_eq(&entry.skel, &hands.skel))
            {
                keep_meshes.insert(order);
            }
        }
    }
    for name in preload {
        let id = weapons.index_of(name);
        if id == 0 {
            continue;
        }
        if let Some(order) = weapons
            .gun_xmodel_edge_of(id)
            .and_then(|edge| edge.bound_index())
        {
            keep_meshes.insert(order);
        }
    }
    let bound = |edges: &[asset_core::AssetEdge<asset_model::MaterialSpace>]| {
        edges
            .iter()
            .filter_map(|edge| edge.bound_index())
            .collect::<Vec<_>>()
    };
    let mut candidates = HashSet::new();
    let mut other = HashSet::new();
    for order in 0..fpv.len() {
        let Some(entry) = fpv.get_at(order) else {
            continue;
        };
        let target = if keep_meshes.contains(&order) {
            &mut other
        } else {
            &mut candidates
        };
        target.extend(bound(&entry.material_edges));
    }
    // World models (the gun as others see it) bind the same materials; with
    // `defer_world_models` only the preload weapons' world models keep them.
    let mut keep_world = HashSet::new();
    if defer_world_models {
        for name in preload {
            let id = weapons.index_of(name);
            if id == 0 {
                continue;
            }
            keep_world.extend(
                weapons
                    .world_model_edge_of(id)
                    .and_then(|edge| edge.bound_index()),
            );
            keep_world.extend(
                weapons
                    .attachment_world_model_edges_of(id)
                    .iter()
                    .filter_map(|edge| edge.bound_index()),
            );
        }
    }
    for i in 0..world_weapons.len() {
        let Some(entry) = world_weapons.get_at(i) else {
            continue;
        };
        let target = if defer_world_models && !keep_world.contains(&i) {
            &mut candidates
        } else {
            &mut other
        };
        target.extend(bound(&entry.material_edges));
    }
    for i in 0..projectiles.len() {
        if let Some(entry) = projectiles.get_at(i) {
            other.extend(bound(&entry.material_edges));
        }
    }
    other.extend(scene_models.bound_material_indices());
    let materials: HashSet<usize> = candidates
        .difference(&other)
        .copied()
        .filter(|&index| {
            catalog.materials.get(index).is_some_and(|material| {
                let name = material.name.as_str().to_ascii_lowercase();
                !KEEP_WORDS.iter().any(|word| name.contains(word))
            })
        })
        .collect();
    let mut images = HashSet::new();
    let mut shared = HashSet::new();
    for (index, material) in catalog.materials.iter().enumerate() {
        let target = if materials.contains(&index) {
            &mut images
        } else {
            &mut shared
        };
        target.extend(material.textures.iter().filter_map(|texture| texture.image));
    }
    let images: HashSet<usize> = images.difference(&shared).copied().collect();
    let megapixels = images
        .iter()
        .filter_map(|&i| catalog.images.get(i))
        .map(|image| f64::from(image.width) * f64::from(image.height) / 1.0e6)
        .sum();
    FpvOnlyImages {
        materials,
        images,
        kept_meshes: keep_meshes.len(),
        megapixels,
    }
}
