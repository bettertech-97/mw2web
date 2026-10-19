//! Bakes a zone's sounds and strings into `iw4l-maps/` so clients read what
//! they play instead of the whole zone:
//!
//!   sound_pack [--games <mw2 folder>] [--zone localized_common_mp] [--lang english]
//!
//! writes `<zone>.sounds` (the sound catalog with FLAC audio, read per sound
//! on first play) and `<zone>.localize` (the zone's strings). The sound pack
//! is read back afterwards and compared with the walk.

use std::path::PathBuf;

fn main() {
    let mut games = PathBuf::from(std::env::var("IW4L_GAMES").unwrap_or_else(|_| {
        "C:/Program Files (x86)/Steam/steamapps/common/Call of Duty Modern Warfare 2".to_owned()
    }));
    let mut zone = "localized_common_mp".to_owned();
    let mut lang = "english".to_owned();
    let mut priority: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--games" => games = PathBuf::from(args.next().expect("--games <folder>")),
            "--zone" => zone = args.next().expect("--zone <name>"),
            "--lang" => lang = args.next().expect("--lang <name>"),
            // One loaded sound name per line, stored first in this order
            // (write it with IW4L_DUMP_REQUIRED_CLIPS=<file> on a native run).
            "--priority" => {
                let file = args.next().expect("--priority <file>");
                priority = std::fs::read_to_string(&file)
                    .expect("priority file")
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            other => {
                eprintln!("unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let zone_path = games.join("zone").join(&lang).join(format!("{zone}.ff"));
    let maps = games.join("iw4l-maps");
    std::fs::create_dir_all(&maps).expect("iw4l-maps folder");
    let sounds_out = maps.join(format!("{zone}.{}", asset_audio::SOUND_PACK_EXT));
    let strings_out = maps.join(format!("{zone}.localize"));

    let started = std::time::Instant::now();
    let catalog = match asset_audio::load_sound_catalog(&zone_path) {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("{}: {error}", zone_path.display());
            std::process::exit(1);
        }
    };
    let pcm: usize = catalog
        .loaded_sounds()
        .iter()
        .map(|sound| sound.encoded_bytes().len())
        .sum();
    println!(
        "walked {} in {:.1}s: {} aliases, {} loaded sounds, {:.1} MiB of PCM",
        zone_path.display(),
        started.elapsed().as_secs_f64(),
        catalog.sounds().len(),
        catalog.loaded_sounds().len(),
        pcm as f64 / 1048576.0
    );

    let report = match asset_audio::write_sound_pack(&catalog, &sounds_out, &priority) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{}: {error}", sounds_out.display());
            std::process::exit(1);
        }
    };
    println!(
        "wrote {}: {} sounds ({} prioritized of {} named), {:.1} MiB of audio from {:.1} MiB of PCM, index {:.1} MiB, {:.1}s",
        sounds_out.display(),
        report.sounds,
        report.prioritized,
        priority.len(),
        report.audio_bytes as f64 / 1048576.0,
        report.pcm_bytes as f64 / 1048576.0,
        report.index_bytes as f64 / 1048576.0,
        report.elapsed_ms / 1000.0
    );
    for skipped in &report.skipped {
        println!("  skipped {skipped}");
    }
    println!(
        "FLAC kept for {} sounds; {} streams the game's decoder refused were stored as PCM",
        report.flac_kept,
        report.flac_refused.len()
    );
    for refused in report.flac_refused.iter().take(12) {
        println!("  flac refused: {refused}");
    }

    match asset_audio::read_sound_pack(&sounds_out) {
        Ok(back) => {
            let bank_backed = back
                .loaded_sounds()
                .iter()
                .filter(|sound| sound.sab_media.is_some())
                .count();
            println!(
                "read back: {} aliases, {} loaded sounds ({} bank-backed), {} rawfiles",
                back.sounds().len(),
                back.loaded_sounds().len(),
                bank_backed,
                back.rawfiles_len()
            );
            if back.sounds().len() != catalog.sounds().len() {
                eprintln!("alias count differs from the walk");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("read back failed: {error}");
            std::process::exit(1);
        }
    }

    match asset_game::write_localize_pack(&zone_path, &strings_out) {
        Ok(strings) => println!("wrote {} ({strings} strings)", strings_out.display()),
        Err(error) => {
            eprintln!("{}: {error}", strings_out.display());
            std::process::exit(1);
        }
    }
}
