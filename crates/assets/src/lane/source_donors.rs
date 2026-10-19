//! Which MW2 world material stands in for a Source one. The map's own
//! textures never ship, so every Hammer material name is matched to the
//! closest-looking material a donor MW2 map zone carries: an explicit row
//! for the names that cover most of a map, a keyword family for the rest.

const PLASTER: &[&str] = &[
    "wc/me_plaster_02",
    "wc/me_plaster_01",
    "wc/me_plaster_brown",
    "wc/plaster_whitewall_01",
    "wc/plaster_cracked_paint",
    "wc/ch_plaster04",
    "wc/plaster_cracked_paint03",
];
const PLASTER_BLUE: &[&str] = &["wc/plaster_rough_blue", "wc/plaster_rough_blue_shiny"];
const BRICK: &[&str] = &[
    "wc/me_brickwall_01",
    "wc/ct_wall_brick_old01",
    "wc/ct_wall_brick_brn01",
    "wc/brick_modern_rtan01",
    "wc/ct_wall_brick_big01",
];
const STONE_WALL: &[&str] = &[
    "wc/ch_stonewall_04",
    "wc/stone_interiorwall_1",
    "wc/rock_limestone_wall02",
];
const STONE_FLOOR: &[&str] = &["wc/me_stonefloor01", "wc/me_stone_road01", "wc/me_sidewalk_hex"];
const STONE_TRIM: &[&str] = &["wc/ch_trim_stone01", "wc/me_sidewalk_curb"];
const TILE: &[&str] = &[
    "wc/me_floor_04",
    "wc/ch_tile_floor02",
    "wc/ch_tile_floor03",
    "wc/kitchen_tile",
    "wc/ch_tile_floor01",
    "wc/ch_tile_floor04",
];
const WOOD: &[&str] = &[
    "wc/me_woodplanks01",
    "wc/me_woodboard_02",
    "wc/me_pinewood",
    "wc/ch_woodwall01",
    "wc/ch_woodpaint_white01",
];
const PLYWOOD: &[&str] = &["wc/ch_plywood02", "wc/me_woodboard_02"];
const WOOD_BEAM: &[&str] = &["wc/me_woodbeam_01", "wc/ch_trim_woodlong01"];
const CONCRETE: &[&str] = &[
    "wc/concrete_dirty_cracked",
    "wc/me_floor_concrete",
    "wc/ch_concrete_03",
    "wc/me_concrete_groovy",
    "wc/concrete_sidewalk_dirty",
];
const ASPHALT: &[&str] = &["wc/me_asphalt_01", "wc/asphalt_road"];
const METAL: &[&str] = &[
    "wc/me_metal_grey_01",
    "wc/me_metal_rust_02",
    "wc/ch_metal_rusty_02",
    "wc/me_metal_rust_03",
];
const CORRUGATED: &[&str] = &["wc/ch_factory_corrugatedsiding2", "wc/metal_aluminum_siding_02"];
const METAL_TRIM: &[&str] = &["wc/me_trim_metal_01", "wc/me_trim_metalplate_1"];
const METAL_DOOR: &[&str] = &["wc/door_metal_01", "wc/me_door_rollup1"];
const WOOD_DOOR: &[&str] = &["wc/me_door_04", "wc/me_door_06", "wc/me_door_02"];
const WINDOW: &[&str] = &["wc/me_window_01", "wc/me_window_02", "wc/me_window_07"];
const ROOF: &[&str] = &["wc/ch_roof_tiles_4"];
const DIRT: &[&str] = &["wc/ch_ground_dirt03", "wc/dirt_cracked_01"];
const RUBBLE: &[&str] = &["wc/ch_rubble01"];
const ROCK: &[&str] = &["wc/rock_brown2", "wc/rock_brown3"];
const GRASS: &[&str] = &["wc/grass_crabgrass"];
const CLOTH: &[&str] = &["wc/com_blue_tarp", "wc/ap_office_carpet"];
const MARBLE: &[&str] = &["wc/marble_floor_tiles_02"];
const CINDERBLOCK: &[&str] = &["wc/me_cinderblock_wall2"];
const CEILING: &[&str] = &["wc/ch_ceiling01", "wc/me_office_ceiling_tiles"];
/// Whichever donor zone is walked, its sky: Rust's afternoon haze first
/// (the default donor), then Invasion's.
const SKY: &[&str] = &["wc/sky_af_chase", "wc/sky_sp_roadkill"];
const BLACK: &[&str] = &["wc/com_black"];

/// Exact rows: the Source materials that cover most of the maps we care
/// about, each to the one MW2 material that looks nearest.
const EXACT: &[(&str, &str)] = &[
    ("de_dust/sitebwall13a", "wc/me_plaster_02"),
    ("de_dust/sitebwall08a", "wc/me_plaster_01"),
    ("de_dust/sitebwall07a", "wc/me_plaster_02"),
    ("de_dust/residwall04a", "wc/me_plaster_02"),
    ("de_dust/marketwall02", "wc/me_plaster_01"),
    ("de_dust/marketwall05a", "wc/me_plaster_brown"),
    ("de_dust/dusandwlltrim3", "wc/ch_trim_stone01"),
    ("de_dust/dutile1", "wc/me_floor_04"),
    ("de_dust/tilefloor01", "wc/ch_tile_floor02"),
    ("de_dust/tilefloor02", "wc/kitchen_tile"),
    ("de_dust/stonestep01", "wc/me_stonefloor01"),
    ("de_dust/stonestep02", "wc/me_stonefloor01"),
    ("de_dust/stonestep03", "wc/me_stonefloor01"),
    ("de_dust/stonestep04", "wc/me_stonefloor01"),
    ("de_dust/door04", "wc/me_door_04"),
    ("de_dust/door011", "wc/me_door_06"),
    ("de_mirage/brick/de_mirage_brick_ver1_diffuse", "wc/me_brickwall_01"),
    ("de_mirage/brick/de_mirage_brick_ver1_blend", "wc/me_brickwall_01"),
    ("de_mirage/brick/de_mirage_brick_ver2_blend_update", "wc/ct_wall_brick_brn01"),
    ("de_mirage/brick/de_mirage_brick_ver2_blend2_diffuse", "wc/ct_wall_brick_brn01"),
    ("de_mirage/brick/de_mirage_brick_ver1pl_blend_diffuse", "wc/ct_wall_brick_old01"),
    ("de_mirage/hr_mirage/mirage_plaster_blend_3", "wc/me_plaster_01"),
    ("de_mirage/hr_mirage/mirage_plaster_2", "wc/ch_plaster04"),
    ("de_mirage/hr_mirage/mirage_plaster_1", "wc/plaster_whitewall_01"),
    ("de_mirage/base/de_mirage_mid_ver1_diffuse", "wc/me_plaster_brown"),
    ("de_mirage/base/de_mirage_top_ver1_blend", "wc/plaster_cracked_paint"),
    ("de_mirage/base/de_mirage_top_ver1_diffuse", "wc/plaster_cracked_paint03"),
    ("de_mirage/base/de_mirage_base_ver1_blend", "wc/stone_interiorwall_1"),
    ("de_mirage/base/de_mirage_base_trim_ver1_diffuse", "wc/ch_trim_stone01"),
    ("de_mirage/wood/de_mirage_wood_ver3_diffuse", "wc/me_woodboard_02"),
    ("de_mirage/plaster/de_mirage_plaster_blue1_blend", "wc/plaster_rough_blue"),
    ("de_mirage/plaster/de_mirage_plaster_blue1_diffuse", "wc/plaster_rough_blue"),
    ("de_mirage/plaster/de_mirage_plaster_salmon1_blend", "wc/me_plaster_brown"),
    ("de_mirage/plaster/de_mirage_plaster_salmon1_diffuse", "wc/me_plaster_brown"),
    ("de_mirage/plaster_worn/de_mirage_plaster_brick4_diffuse", "wc/ct_wall_brick_old01"),
    ("de_mirage/ground/de_mirage_ground_tileh_blend_diffuse", "wc/me_stone_road01"),
    ("de_mirage/ground/de_mirage_ground_tileh_blend2_diffuse", "wc/me_stone_road01"),
    ("de_mirage/ground/de_mirage_ground_tilec_blend_diffuse", "wc/me_sidewalk_hex"),
    ("de_mirage/ground/de_mirage_tilef_diffuse", "wc/ch_tile_floor03"),
    ("de_mirage/tile/de_mirage_tile_ver4_diffuse", "wc/me_floor_04"),
    ("de_mirage/tile/de_mirage_tile_ver4_blend", "wc/me_floor_04"),
    ("de_mirage/marble/marble_01", "wc/marble_floor_tiles_02"),
    ("de_mirage/doors/door_a", "wc/me_door_02"),
    ("cs_italy/hpe_plaster_tan_wall", "wc/plaster_whitewall_01"),
    ("cs_italy/hpe_plaster_yellow_wall", "wc/me_plaster_02"),
    ("cs_italy/hpe_plaster_tint_tan", "wc/me_plaster_01"),
    ("cs_italy/hpe_plaster_trim_light", "wc/ct_plaster_trim01"),
    ("cs_italy/plasterwall04", "wc/me_plaster_02"),
    ("cs_italy/tileroof01", "wc/ch_roof_tiles_4"),
    ("cs_havana/door01", "wc/me_door_04"),
    ("wood/woodfloor005a", "wc/me_woodplanks01"),
    ("wood/milbeams003", "wc/me_woodbeam_01"),
    ("wood/woodbeam001a", "wc/me_woodbeam_01"),
    ("wood/plywood02", "wc/ch_plywood02"),
    ("wood/wood_dock_pylon_01", "wc/me_pinewood"),
    ("wood/milsteps001", "wc/me_woodplanks01"),
    ("wood/woodsteps001", "wc/me_woodplanks01"),
    ("wood/wood_int_02", "wc/ch_woodwall01"),
    ("wood/woodtrim_ext_01", "wc/ch_trim_woodlong01"),
    ("wood/wood_ext_05", "wc/me_woodboard_02"),
    ("wood/wooddoor026a", "wc/me_door_06"),
    ("wood/woodsiding_ext_05", "wc/ch_woodwall01"),
    ("brick/brick_ext_04", "wc/ct_wall_brick_old01"),
    ("brick/infwllg", "wc/brick_modern_rtan01"),
    ("brick/infwllg_overlay_a", "wc/brick_modern_rtan01"),
    ("concrete/concrete_ext_07", "wc/concrete_dirty_cracked"),
    ("concrete/blend_blacktopsand_01", "wc/me_asphalt_01"),
    ("concrete/concrete_floor_04", "wc/me_floor_concrete"),
    ("plaster/plaster_int_02", "wc/plaster_whitewall_01"),
    ("plaster/plaster_ext_19", "wc/me_plaster_01"),
    ("metal/metaltrack001a", "wc/me_metal_grey_01"),
    ("metal/metalhull003a", "wc/me_metal_rust_02"),
    ("metal/metalwall031a", "wc/me_metal_grey_01"),
    ("metal/metal_corrugated03a", "wc/ch_factory_corrugatedsiding2"),
    ("metal/metaldoor034a", "wc/door_metal_01"),
    ("metal/vertigo_ibeam", "wc/me_trim_metalplate_1"),
    ("de_nuke/nuke_metaltrims_01", "wc/me_trim_metal_01"),
    ("effects/metalrail011a", "wc/me_trim_metal_01"),
    ("tile/tile_mall_floor00", "wc/ch_tile_floor04"),
    ("tile/tilewall009b", "wc/dt_bathroom_tiles_blue"),
    ("tile/milflr002", "wc/ch_tile_floor01"),
    ("glass/glasswindow048a", "wc/me_window_01"),
    ("stone/counter01", "wc/ch_counterwall_1"),
    ("tools/toolsskybox", "wc/sky_af_chase"),
    ("tools/toolsblack", "wc/com_black"),
    ("tools/toolsskybox2d", "wc/sky_af_chase"),
];

fn family(source: &str) -> &'static [&'static str] {
    let name = source.rsplit('/').next().unwrap_or(source);
    let has = |words: &[&str]| words.iter().any(|w| source.contains(w));
    let _ = name;
    if has(&["skybox", "sky_"]) {
        SKY
    } else if has(&["toolsblack"]) {
        BLACK
    } else if has(&["glass", "window"]) {
        WINDOW
    } else if has(&["corrugat"]) {
        CORRUGATED
    } else if has(&["metaldoor", "door_metal", "garage", "roller", "shutter"]) {
        METAL_DOOR
    } else if has(&["door"]) {
        WOOD_DOOR
    } else if has(&["ibeam", "trim", "rail", "pipe"]) && has(&["metal", "steel", "iron", "ibeam", "rail", "pipe"]) {
        METAL_TRIM
    } else if has(&["metal", "steel", "iron", "grate", "chainlink", "tin"]) {
        METAL
    } else if has(&["beam", "rafter"]) {
        WOOD_BEAM
    } else if has(&["plywood"]) {
        PLYWOOD
    } else if has(&["wood", "plank", "crate", "lumber"]) {
        WOOD
    } else if has(&["roof"]) {
        ROOF
    } else if has(&["marble"]) {
        MARBLE
    } else if has(&["cinder"]) {
        CINDERBLOCK
    } else if has(&["ceiling"]) {
        CEILING
    } else if has(&["brick"]) {
        BRICK
    } else if has(&["tile", "ceramic", "mosaic"]) {
        TILE
    } else if has(&["blue"]) && has(&["plaster", "wall", "paint"]) {
        PLASTER_BLUE
    } else if has(&["plaster", "stucco", "adobe", "wall"]) {
        PLASTER
    } else if has(&["step", "stair", "curb", "trim", "ledge"]) {
        STONE_TRIM
    } else if has(&["cobble", "road", "pave", "street", "sidewalk", "ground"]) {
        STONE_FLOOR
    } else if has(&["stone", "rock", "cliff"]) {
        if has(&["floor"]) { STONE_FLOOR } else { STONE_WALL }
    } else if has(&["asphalt", "blacktop", "tarmac"]) {
        ASPHALT
    } else if has(&["gravel", "rubble", "debris"]) {
        RUBBLE
    } else if has(&["grass", "foliage"]) {
        GRASS
    } else if has(&["dirt", "mud", "sand", "dust", "nature", "soil"]) {
        DIRT
    } else if has(&["rock"]) {
        ROCK
    } else if has(&["cloth", "fabric", "carpet", "tarp", "canvas", "rug", "awning"]) {
        CLOTH
    } else if has(&["concrete", "cement"]) {
        CONCRETE
    } else if has(&["floor"]) {
        STONE_FLOOR
    } else {
        PLASTER
    }
}

fn stable_hash(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Donor material names to try for a Source material, best first.
pub(crate) fn candidates(source: &str) -> Vec<&'static str> {
    // A blend's second layer stands in for the same donor as its first.
    let source = source.strip_suffix("#2").unwrap_or(source);
    let mut out = Vec::new();
    if let Some((_, donor)) = EXACT.iter().find(|(name, _)| *name == source) {
        out.push(*donor);
    }
    let list = family(source);
    if !list.is_empty() {
        let start = (stable_hash(source) % list.len() as u64) as usize;
        for i in 0..list.len() {
            out.push(list[(start + i) % list.len()]);
        }
    }
    out.push("wc/me_plaster_02");
    out
}
