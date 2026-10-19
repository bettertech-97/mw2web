//! The map's entities in IW4's `{ "key" "value" }` string, with Counter-
//! Strike's spawns and bomb sites renamed to what the IW4 gametypes look
//! for.

use std::fmt::Write as _;

use crate::compile::{CompiledMap, Entity};

fn push_block(out: &mut String, pairs: &[(&str, String)]) {
    out.push_str("{\n");
    for (key, value) in pairs {
        let _ = writeln!(out, "\"{key}\" \"{value}\"");
    }
    out.push_str("}\n");
}

fn vec_text(v: [f32; 3]) -> String {
    format!("{} {} {}", trim(v[0]), trim(v[1]), trim(v[2]))
}

fn trim(x: f32) -> String {
    if (x - x.round()).abs() < 1e-3 {
        format!("{}", x.round() as i64)
    } else {
        format!("{x:.3}")
    }
}

fn spawn_blocks(out: &mut String, entity: &Entity, classnames: &[&str]) {
    let Some(origin) = entity.origin() else {
        return;
    };
    let origin = vec_text(origin);
    let angles = entity.angles();
    // Spawn points look along their yaw; Source may carry a camera pitch.
    let angles = vec_text([0.0, angles[1], 0.0]);
    for classname in classnames {
        push_block(
            out,
            &[
                ("classname", (*classname).to_owned()),
                ("origin", origin.clone()),
                ("angles", angles.clone()),
            ],
        );
    }
}

/// The IW4 entity string for a compiled map.
pub fn iw4_entity_string(map: &CompiledMap) -> String {
    let mut out = String::new();
    let mut worldspawn: Vec<(&str, String)> = vec![("classname", "worldspawn".to_owned())];
    if let Some(world) = map.entities.iter().find(|e| e.classname == "worldspawn") {
        if let Some(sky) = world.get("skyname") {
            worldspawn.push(("skyname", sky.to_owned()));
        }
    }
    if let Some(sun) = map.sun {
        let pitch = sun.to_sun[2].asin().to_degrees();
        let yaw = sun.to_sun[1].atan2(sun.to_sun[0]).to_degrees();
        worldspawn.push(("sundirection", format!("{} {} 0", trim(-pitch), trim(yaw))));
        worldspawn.push(("suncolor", vec_text(sun.color)));
        worldspawn.push(("ambient", vec_text(sun.ambient)));
    }
    // North is +Y, up in Hammer's top view and on Counter-Strike's radar.
    worldspawn.push(("northyaw", "90".to_owned()));
    push_block(&mut out, &worldspawn);

    // The intermission camera: above the middle of the spawns, looking down
    // at the map's centre. Counter-Strike's camera points are authored for
    // its own menus and often sit outside the playable area.
    let spawn_origins: Vec<[f32; 3]> = map
        .entities
        .iter()
        .filter(|e| {
            matches!(
                e.classname.as_str(),
                "info_player_terrorist" | "info_player_counterterrorist" | "info_player_start"
            )
        })
        .filter_map(Entity::origin)
        .collect();
    let center = if spawn_origins.is_empty() {
        [
            (map.mins[0] + map.maxs[0]) * 0.5,
            (map.mins[1] + map.maxs[1]) * 0.5,
            (map.mins[2] + map.maxs[2]) * 0.5,
        ]
    } else {
        let n = spawn_origins.len() as f32;
        let sum = spawn_origins
            .iter()
            .fold([0.0f32; 3], |acc, o| [acc[0] + o[0], acc[1] + o[1], acc[2] + o[2]]);
        [sum[0] / n, sum[1] / n, sum[2] / n]
    };
    let eye = [center[0] - 600.0, center[1] - 600.0, center[2] + 700.0];
    let to_center = [center[0] - eye[0], center[1] - eye[1], center[2] - eye[2]];
    let flat = (to_center[0] * to_center[0] + to_center[1] * to_center[1]).sqrt();
    let pitch = (-to_center[2]).atan2(flat).to_degrees();
    let yaw = to_center[1].atan2(to_center[0]).to_degrees();
    push_block(
        &mut out,
        &[
            ("classname", "mp_global_intermission".to_owned()),
            ("origin", vec_text(eye)),
            ("angles", vec_text([pitch, yaw, 0.0])),
        ],
    );

    let mut t_spawns = 0usize;
    let mut ct_spawns = 0usize;
    for entity in &map.entities {
        match entity.classname.as_str() {
            "info_player_terrorist" => {
                t_spawns += 1;
                spawn_blocks(
                    &mut out,
                    entity,
                    &[
                        "mp_dm_spawn",
                        "mp_tdm_spawn",
                        "mp_tdm_spawn_axis_start",
                        "mp_sd_spawn_attacker",
                        "mp_dom_spawn",
                        "mp_dom_spawn_axis_start",
                        "mp_ctf_spawn_axis",
                        "mp_sab_spawn_axis_start",
                    ],
                );
            }
            "info_player_counterterrorist" => {
                ct_spawns += 1;
                spawn_blocks(
                    &mut out,
                    entity,
                    &[
                        "mp_dm_spawn",
                        "mp_tdm_spawn",
                        "mp_tdm_spawn_allies_start",
                        "mp_sd_spawn_defender",
                        "mp_dom_spawn",
                        "mp_dom_spawn_allies_start",
                        "mp_ctf_spawn_allies",
                        "mp_sab_spawn_allies_start",
                    ],
                );
            }
            "info_player_start" | "info_player_deathmatch" => {
                spawn_blocks(&mut out, entity, &["mp_dm_spawn"]);
            }
            _ => {}
        }
    }
    let _ = (t_spawns, ct_spawns);

    // Search and Destroy. The bomb starts at the attackers' (terrorists')
    // spawn; the carry object's trigger follows it when dropped.
    let t_origins: Vec<[f32; 3]> = map
        .entities
        .iter()
        .filter(|e| e.classname == "info_player_terrorist")
        .filter_map(Entity::origin)
        .collect();
    let bomb_origin = if t_origins.is_empty() {
        center
    } else {
        let n = t_origins.len() as f32;
        let sum = t_origins
            .iter()
            .fold([0.0f32; 3], |acc, p| [acc[0] + p[0], acc[1] + p[1], acc[2] + p[2]]);
        [sum[0] / n, sum[1] / n, sum[2] / n]
    };
    push_block(
        &mut out,
        &[
            ("classname", "trigger_radius".to_owned()),
            ("targetname", "sd_bomb_pickup_trig".to_owned()),
            ("origin", vec_text(bomb_origin)),
            ("radius", "64".to_owned()),
            ("height", "96".to_owned()),
        ],
    );
    push_block(
        &mut out,
        &[
            ("classname", "script_model".to_owned()),
            ("targetname", "sd_bomb".to_owned()),
            ("model", "prop_suitcase_bomb".to_owned()),
            ("origin", vec_text(bomb_origin)),
        ],
    );
    // Each bomb site: the plant volume (inline model `*N`), its marker,
    // and the defuse trigger the gametype moves onto the planted bomb.
    let mut sites = 0usize;
    for (index, trigger) in map.triggers.iter().enumerate() {
        if trigger.classname != "func_bomb_target" {
            continue;
        }
        let name = trigger.targetname.to_ascii_lowercase();
        let label = if name.starts_with('b') || (sites == 1 && !name.starts_with('a')) {
            "_b"
        } else {
            "_a"
        };
        sites += 1;
        let center = [
            (trigger.mins[0] + trigger.maxs[0]) * 0.5,
            (trigger.mins[1] + trigger.maxs[1]) * 0.5,
            trigger.mins[2],
        ];
        // As the MW2 maps have it: use-touch triggers at the site (the
        // gametype puts its icons at the trigger's origin), the plant volume
        // as inline model `*(2n+1)` and the defuse box as `*(2n+2)`, both
        // relative to that origin so the defuse trigger can follow the bomb.
        push_block(
            &mut out,
            &[
                ("classname", "trigger_use_touch".to_owned()),
                ("targetname", "bombzone".to_owned()),
                ("script_gameobjectname", "bombzone".to_owned()),
                ("script_bombmode_original", "1".to_owned()),
                ("script_label", label.to_owned()),
                ("target", format!("bombzone{label}_visual")),
                ("model", format!("*{}", 2 * index + 1)),
                ("origin", vec_text(center)),
            ],
        );
        push_block(
            &mut out,
            &[
                ("classname", "script_model".to_owned()),
                ("targetname", format!("bombzone{label}_visual")),
                ("target", format!("bombzone{label}_defuse")),
                ("script_gameobjectname", "bombzone".to_owned()),
                ("model", "com_bomb_objective".to_owned()),
                ("origin", vec_text(center)),
            ],
        );
        push_block(
            &mut out,
            &[
                ("classname", "trigger_use_touch".to_owned()),
                ("targetname", format!("bombzone{label}_defuse")),
                ("script_gameobjectname", "bombzone".to_owned()),
                ("model", format!("*{}", 2 * index + 2)),
                ("origin", vec_text(center)),
            ],
        );
    }

    push_block(
        &mut out,
        &[
            ("classname", "script_model".to_owned()),
            ("targetname", "care_package".to_owned()),
            ("target", "care_package_collision".to_owned()),
        ],
    );
    push_block(
        &mut out,
        &[
            ("classname", "script_brushmodel".to_owned()),
            ("targetname", "care_package_collision".to_owned()),
        ],
    );
    out
}
