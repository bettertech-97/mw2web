//! `IW4L_LIST_ZONE=<zone.ff> cargo test -p assets --profile play list_zone_materials -- --ignored --nocapture`
//! prints every material a zone carries with its technique set, colour map
//! and surface type: the donor census a Source-map material table is written
//! from.

#[test]
#[ignore]
fn list_zone_materials() {
    let zone = std::env::var("IW4L_LIST_ZONE").expect("IW4L_LIST_ZONE names the zone file");
    let path = std::path::PathBuf::from(zone);
    let image = asset_transport::open_zone_shared(&path).expect("open zone");
    let progress = asset_transport::LoadProgress::new(1);
    let population = super::lane(asset_core::ZoneGame::Iw4).load_material_population(
        &path,
        &image,
        &progress,
        asset_material::MaterialCatalog::default(),
    );
    let catalog = population.materials;
    for line in &population.report {
        eprintln!("# {line}");
    }
    for material in &catalog.materials {
        let color = material
            .textures
            .iter()
            .find(|binding| binding.semantic == asset_material::TS_COLOR_MAP)
            .and_then(|binding| binding.image)
            .and_then(|index| catalog.images.get(index));
        println!(
            "{}\t{}\t{}\t{}x{}\tsurf={:?}\tgame={:#x}\tsort={}",
            material.name.as_str(),
            material.technique_set.as_str(),
            color.map_or("-", |image| image.name.as_str()),
            color.map_or(0, |image| image.width),
            color.map_or(0, |image| image.height),
            material.surface_type_bits,
            material.info_game_flags,
            material.sort_key,
        );
    }
}

/// `IW4L_LIST_ZONE=<common_mp.ff> IW4L_DUMP_SCRIPTS=common_scripts/_dynamic_world,maps/mp/_load cargo test -p assets --profile play dump_common_scripts -- --ignored --nocapture`
#[test]
#[ignore]
fn dump_common_scripts() {
    let zone = std::env::var("IW4L_LIST_ZONE").expect("IW4L_LIST_ZONE names common_mp.ff");
    let modules = std::env::var("IW4L_DUMP_SCRIPTS").expect("IW4L_DUMP_SCRIPTS names modules");
    let path = std::path::PathBuf::from(zone);
    let image = asset_transport::open_zone_shared(&path).expect("open zone");
    let progress = asset_transport::LoadProgress::new(1);
    let census = super::lane(asset_core::ZoneGame::Iw4).load_common_mp(
        &path,
        &image,
        &progress,
        false,
        asset_material::MaterialCatalog::default(),
    );
    for module in modules.split(',') {
        match census.scripts.read(module.trim()) {
            Ok(bytes) => {
                println!("===== {module} ({} bytes)", bytes.len());
                println!("{}", String::from_utf8_lossy(&bytes));
            }
            Err(error) => println!("===== {module}: {error}"),
        }
    }
}
