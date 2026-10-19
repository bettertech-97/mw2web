//! `source_bake --vmf <map.vmf> --out <dir> --name <zone> [--texel 16] [--page 1024] [--threads N]`
//!
//! Bakes a Source map's lightmap (sun shadows and ambient occlusion) into
//! `<out>/<zone>.lightmap.bin`, which the engine reads beside the map.

use std::path::PathBuf;

fn main() {
    let mut vmf = None;
    let mut out = None;
    let mut name = None;
    let mut texel = 16.0f32;
    let mut page = 1024u32;
    let mut threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args.next().unwrap_or_default();
        match flag.as_str() {
            "--vmf" => vmf = Some(PathBuf::from(value)),
            "--out" => out = Some(PathBuf::from(value)),
            "--name" => name = Some(value),
            "--texel" => texel = value.parse().unwrap_or(16.0),
            "--page" => page = value.parse().unwrap_or(1024),
            "--threads" => threads = value.parse().unwrap_or(threads),
            other => {
                eprintln!("unknown flag {other}");
                std::process::exit(2);
            }
        }
    }
    let (Some(vmf), Some(out), Some(name)) = (vmf, out, name) else {
        eprintln!("usage: source_bake --vmf <map.vmf> --out <dir> --name <zone> [--texel 16] [--page 1024] [--threads N]");
        std::process::exit(2);
    };
    // The engine prefers `<name>.vmf.z` (zlib) when it finds one: a fifth of
    // the bytes to download for the same map.
    match std::fs::read(&vmf) {
        Ok(raw) => {
            let packed = source_map::deflate(&raw);
            let path = out.join(format!("{name}.vmf.z"));
            match std::fs::write(&path, &packed) {
                Ok(()) => println!("wrote {} ({} -> {} bytes)", path.display(), raw.len(), packed.len()),
                Err(error) => eprintln!("source_bake: {}: {error}", path.display()),
            }
        }
        Err(error) => eprintln!("source_bake: {}: {error}", vmf.display()),
    }
    let text = match std::fs::read_to_string(&vmf) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("{}: {error}", vmf.display());
            std::process::exit(1);
        }
    };
    let started = std::time::Instant::now();
    let map = match source_map::compile(&text, 512.0) {
        Ok(map) => map,
        Err(error) => {
            eprintln!("compile: {error}");
            std::process::exit(1);
        }
    };
    let mut map = map;
    let props_path = out.join(format!("{name}.props.bin"));
    match std::fs::read(&props_path).map_err(|e| e.to_string()).and_then(|bytes| source_map::props::PropSet::from_bytes(&bytes)) {
        Ok(set) => {
            let stats = source_map::props::append_props(&mut map, &set);
            println!(
                "props: {} of {} instances placed from {} models, {} triangles",
                stats.placed,
                stats.instances,
                set.models.len(),
                stats.triangles
            );
        }
        Err(error) => println!("props: none ({error})"),
    }
    println!(
        "compiled: {} faces, {} vertices, {} triangles in {:.1}s",
        map.surfaces.len(),
        map.vertices.len(),
        map.indices.len() / 3,
        started.elapsed().as_secs_f32()
    );
    let started = std::time::Instant::now();
    let (bake, stats) = source_map::bake::bake(&map, texel, page, threads);
    println!(
        "baked: {} faces, {} texels, {} pages of {page}x{page}, {} occluder triangles, {} rays in {:.1}s on {threads} threads",
        stats.faces,
        stats.texels,
        stats.pages,
        stats.triangles,
        stats.rays,
        started.elapsed().as_secs_f32()
    );
    let bytes = bake.to_bytes();
    let path = out.join(format!("{name}.lightmap.bin"));
    if let Err(error) = std::fs::create_dir_all(&out).and_then(|()| std::fs::write(&path, &bytes)) {
        eprintln!("{}: {error}", path.display());
        std::process::exit(1);
    }
    println!("{}: {:.1} MB", path.display(), bytes.len() as f64 / 1e6);
}
