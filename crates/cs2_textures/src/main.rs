//! `cs2_textures [--vmf <map.vmf>] [--radar <cs map>] --cs2 <Counter-Strike Global Offensive/game/csgo> --out <dir> --name <zone>`
//!
//! For every material a Source map uses, finds the same-named compiled
//! material in Counter-Strike 2's archives, reads its parameters, takes its
//! colour texture (tint baked in) and writes it as an IW4 IWI into
//! `<out>/<zone>_textures.iwd`, with a `<out>/<zone>.textures.tsv` manifest
//! (`material  image  width  height  source  flags`) the engine's Source
//! map lane reads to dress the faces. A blend material's second layer is
//! written as `<material>#2`, the name the map compiler gives the
//! displacement triangles painted over to that layer.
//!
//! The map's static props (`prop_static` models) are read from CS2's
//! `.vmdl_c` files and written to `<out>/<zone>.props.bin` (positions,
//! texture coordinates, triangles and draw calls per model); their
//! materials join the texture pack.
//!
//! A material CS2 lacks (it rebuilt and renamed several maps' surfaces)
//! is drawn with the nearest CS2 material by name (`nearest.rs`); the
//! report lists each stand-in.
//!
//! `--radar de_mirage` writes the map's minimap on its own: Counter-
//! Strike's overhead picture as `images/compass_map_<zone>.iwi` in
//! `<out>/<zone>_radar.iwd`, and `<out>/<zone>.radar.txt` with the world
//! corners it covers (`source_map::Radar`).

mod dxt;
mod iwi;
mod kv3;
mod lz4;
mod nearest;
mod resource;
mod vmdl;
mod vpk;
mod vtex;

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::PathBuf;

use kv3::Value;

struct Args {
    vmf: Option<PathBuf>,
    radar: Option<String>,
    cs2: PathBuf,
    out: PathBuf,
    name: String,
    max_size: u32,
}

fn parse_args() -> Result<Args, String> {
    let mut vmf = None;
    let mut radar = None;
    let mut cs2 = None;
    let mut out = None;
    let mut name = None;
    let mut max_size = 1024;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--vmf" => vmf = Some(PathBuf::from(value()?)),
            "--radar" => radar = Some(value()?),
            "--cs2" => cs2 = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--name" => name = Some(value()?),
            "--max" => max_size = value()?.parse().map_err(|e| format!("--max: {e}"))?,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if vmf.is_none() && radar.is_none() {
        return Err("--vmf or --radar is required".into());
    }
    Ok(Args {
        vmf,
        radar,
        cs2: cs2.ok_or("--cs2 is required")?,
        out: out.ok_or("--out is required")?,
        name: name.ok_or("--name is required")?,
        max_size,
    })
}

/// World textures are `cs2_*` and read whole by the browser (the map's look
/// hangs on them); prop textures are `cs2m_*`, which it caps like the game's
/// own, so a crate or a palm costs a few hundred pixels, not a megapixel.
fn image_prefix(material: &str) -> &'static str {
    if material.starts_with("models/") { "cs2m_" } else { "cs2_" }
}

fn fnv1a(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.split('.').next().unwrap_or(file)
}

/// What a compiled material says about itself: its shader and parameters.
#[derive(Default)]
struct MaterialInfo {
    shader: String,
    textures: BTreeMap<String, String>,
    vectors: BTreeMap<String, [f64; 4]>,
    ints: BTreeMap<String, i64>,
}

fn material_info(bytes: &[u8]) -> Result<MaterialInfo, String> {
    let blocks = resource::blocks(bytes)?;
    let data = blocks
        .iter()
        .find(|b| &b.kind == b"DATA")
        .ok_or("vmat: no DATA block")?;
    let root = kv3::parse(&bytes[data.offset..data.offset + data.size])?;
    let mut info = MaterialInfo {
        shader: root
            .get("m_shaderName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase(),
        ..MaterialInfo::default()
    };
    let entries = |list: &str| -> Vec<&Value> {
        root.get(list)
            .and_then(Value::as_array)
            .map(|items| items.iter().collect())
            .unwrap_or_default()
    };
    let name_of = |entry: &Value| entry.get("m_name").and_then(Value::as_str).map(str::to_owned);
    for entry in entries("m_textureParams") {
        if let (Some(name), Some(path)) = (name_of(entry), entry.get("m_pValue").and_then(Value::as_str)) {
            info.textures.insert(name, path.to_owned());
        }
    }
    for entry in entries("m_vectorParams") {
        if let (Some(name), Some(items)) = (name_of(entry), entry.get("m_value").and_then(Value::as_array)) {
            let mut v = [1.0f64; 4];
            for (slot, item) in v.iter_mut().zip(items) {
                *slot = item.as_f64().unwrap_or(1.0);
            }
            info.vectors.insert(name, v);
        }
    }
    for entry in entries("m_intParams") {
        if let (Some(name), Some(value)) = (name_of(entry), entry.get("m_nValue").and_then(Value::as_i64)) {
            info.ints.insert(name, value);
        }
    }
    Ok(info)
}

const LAYER1_COLOR: &[&str] = &["g_tColor", "g_tLayer1Color", "g_tColorA", "g_tColor1", "g_tGlassTintColor"];
const LAYER2_COLOR: &[&str] = &["g_tLayer2Color", "g_tColorB", "g_tColor2"];
const LAYER1_TINT: &[&str] = &["g_vColorTint", "g_vLayer1Tint"];
const LAYER2_TINT: &[&str] = &["g_vColorTint", "g_vLayer2Tint"];

fn layer_texture<'a>(info: &'a MaterialInfo, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| info.textures.get(*name).map(String::as_str))
}

/// The colour multiplier of a layer: every tint parameter present, in
/// sRGB space, which is where a multiply on the stored texels matches.
fn layer_tint(info: &MaterialInfo, names: &[&str]) -> [f32; 3] {
    let mut tint = [1.0f32; 3];
    for name in names {
        if let Some(v) = info.vectors.get(*name) {
            for (slot, value) in tint.iter_mut().zip(v) {
                *slot *= (*value as f32).clamp(0.0, 4.0);
            }
        }
    }
    tint
}

fn flags_of(info: &MaterialInfo) -> String {
    let on = |name: &str| info.ints.get(name).copied().unwrap_or(0) != 0;
    let mut flags = Vec::new();
    if on("F_ALPHA_TEST") {
        flags.push("alphatest");
    }
    if on("F_TRANSLUCENT") || info.shader.contains("glass") {
        flags.push("translucent");
    }
    if on("F_RENDER_BACKFACES") {
        flags.push("twosided");
    }
    if info.shader.contains("blend") || info.textures.contains_key("g_tLayer2Color") {
        flags.push("blend");
    }
    flags.join(",")
}

/// The colour texture among a material's references, for materials whose
/// parameters could not be read: the one that shares the most name tokens
/// with the material, among those that look like a colour map.
fn color_reference<'a>(material: &str, refs: &'a [String]) -> Option<&'a str> {
    let not_color = [
        "normal", "ao", "rough", "roughness", "mask", "spec", "specular", "blend", "selfillum",
        "height", "gloss", "emissive", "trans", "warp", "tglassdust", "metalness", "detail",
    ];
    let material_tokens: Vec<&str> = stem(material).split('_').filter(|t| !t.is_empty()).collect();
    let mut best: Option<(usize, usize, usize, &str)> = None;
    for (order, reference) in refs.iter().enumerate() {
        let lower = reference.to_ascii_lowercase();
        if !lower.ends_with(".vtex") || lower.contains("materials/default/") || lower.contains("/detail/") {
            continue;
        }
        let ref_stem = stem(&lower);
        let tokens: Vec<&str> = ref_stem.split('_').filter(|t| !t.is_empty()).collect();
        if tokens.iter().any(|t| not_color.contains(t)) {
            continue;
        }
        let kind = if tokens.contains(&"color") {
            2
        } else if tokens.contains(&"diffuse") || tokens.contains(&"albedo") {
            1
        } else {
            0
        };
        let shared = tokens.iter().filter(|t| material_tokens.contains(t)).count();
        let key = (kind, shared, usize::MAX - order, reference.as_str());
        if best.is_none_or(|b| (key.0, key.1, key.2) > (b.0, b.1, b.2)) {
            best = Some(key);
        }
    }
    best.map(|b| b.3)
}

fn mip_count(width: u32, height: u32) -> usize {
    (32 - width.max(height).leading_zeros()) as usize
}

struct Converted {
    width: u32,
    height: u32,
    format: iwi::IwiFormat,
    levels: Vec<Vec<u8>>,
    note: String,
}

fn apply_tint(rgba: &mut dxt::Rgba, tint: [f32; 3]) {
    for texel in rgba.data.chunks_exact_mut(4) {
        for (channel, factor) in texel.iter_mut().zip(tint) {
            *channel = (f32::from(*channel) * factor).round().clamp(0.0, 255.0) as u8;
        }
    }
}

fn convert(texture: &vtex::Texture, max_size: u32, tint: [f32; 3]) -> Result<Converted, String> {
    let mut drop = 0usize;
    while (texture.width.max(texture.height) >> drop) > max_size && drop + 1 < texture.levels.len() {
        drop += 1;
    }
    let width = (texture.width >> drop).max(1);
    let height = (texture.height >> drop).max(1);
    let source: Vec<&Vec<u8>> = texture.levels[drop..].iter().collect();
    let want = mip_count(width, height);
    let tinted = tint.iter().any(|t| (t - 1.0).abs() > 0.004);
    let decode = |level: usize, data: &[u8]| -> Result<dxt::Rgba, String> {
        let w = (width >> level).max(1);
        let h = (height >> level).max(1);
        Ok(match texture.format {
            vtex::Format::Dxt1 => dxt::decode_bc1(w, h, data),
            vtex::Format::Dxt5 => dxt::decode_bc3(w, h, data),
            vtex::Format::Bc7 => dxt::decode_bc7(w, h, data),
            vtex::Format::Rgba8 => dxt::Rgba {
                width: w,
                height: h,
                data: data.to_vec(),
            },
            vtex::Format::Bgra8 => dxt::Rgba {
                width: w,
                height: h,
                data: data
                    .chunks_exact(4)
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
                    .collect(),
            },
            vtex::Format::I8 => dxt::Rgba {
                width: w,
                height: h,
                data: data.iter().flat_map(|&v| [v, v, v, 255]).collect(),
            },
            other => return Err(format!("texture format {other:?} is not a colour map this tool converts")),
        })
    };
    let (format, mut levels, note) = match texture.format {
        vtex::Format::Dxt1 if !tinted => (
            iwi::IwiFormat::Dxt1,
            source.iter().map(|l| (*l).clone()).collect::<Vec<_>>(),
            "dxt1 copied".to_owned(),
        ),
        vtex::Format::Dxt5 if !tinted => (
            iwi::IwiFormat::Dxt5,
            source.iter().map(|l| (*l).clone()).collect::<Vec<_>>(),
            "dxt5 copied".to_owned(),
        ),
        _ => {
            let top = decode(0, source[0])?;
            let format = if dxt::has_transparency(&top) {
                iwi::IwiFormat::Dxt5
            } else {
                iwi::IwiFormat::Dxt1
            };
            let mut levels = Vec::with_capacity(source.len());
            for (level, data) in source.iter().enumerate() {
                let mut rgba = decode(level, data)?;
                if tinted {
                    apply_tint(&mut rgba, tint);
                }
                levels.push(match format {
                    iwi::IwiFormat::Dxt1 => dxt::encode_dxt1(&rgba),
                    iwi::IwiFormat::Dxt5 => dxt::encode_dxt5(&rgba),
                });
            }
            let how = if tinted {
                format!(
                    "tinted {:.2} {:.2} {:.2}, re-encoded as {format:?}",
                    tint[0], tint[1], tint[2]
                )
            } else {
                format!("re-encoded as {format:?}")
            };
            (format, levels, format!("{:?} {how}", texture.format))
        }
    };
    // IW4 wants the whole chain down to 1×1; finish it from the smallest level given.
    if levels.len() < want {
        let last = levels.len() - 1;
        let mut rgba = match format {
            iwi::IwiFormat::Dxt1 => dxt::decode_bc1((width >> last).max(1), (height >> last).max(1), &levels[last]),
            iwi::IwiFormat::Dxt5 => dxt::decode_bc3((width >> last).max(1), (height >> last).max(1), &levels[last]),
        };
        while levels.len() < want {
            rgba = dxt::halve(&rgba);
            levels.push(match format {
                iwi::IwiFormat::Dxt1 => dxt::encode_dxt1(&rgba),
                iwi::IwiFormat::Dxt5 => dxt::encode_dxt5(&rgba),
            });
        }
    }
    Ok(Converted {
        width,
        height,
        format,
        levels,
        note,
    })
}

/// The pack being written: the IWD and its manifest.
struct Pack {
    zip: zip::ZipWriter<std::io::BufWriter<std::fs::File>>,
    stored: zip::write::SimpleFileOptions,
    manifest: String,
    written: usize,
    total_bytes: usize,
    max_size: u32,
}

impl Pack {
    /// A 4×4 texture of one colour as the image of `material`.
    fn add_flat(&mut self, material: &str, rgba: [u8; 4]) -> Result<String, String> {
        let mut rgba = dxt::Rgba {
            width: 4,
            height: 4,
            data: rgba.repeat(16),
        };
        let mut levels = vec![dxt::encode_dxt1(&rgba)];
        while levels.len() < 3 {
            rgba = dxt::halve(&rgba);
            levels.push(dxt::encode_dxt1(&rgba));
        }
        let image = format!(
            "{}{:08x}",
            image_prefix(material),
            fnv1a(material) as u32 ^ (fnv1a(material) >> 32) as u32
        );
        let file = iwi::write(4, 4, iwi::IwiFormat::Dxt1, &levels);
        self.total_bytes += file.len();
        self.zip
            .start_file(format!("images/{image}.iwi"), self.stored)
            .map_err(|e| format!("zip: {e}"))?;
        self.zip.write_all(&file).map_err(|e| format!("zip: {e}"))?;
        self.manifest
            .push_str(&format!("{material}\t{image}\t4\t4\tflat\t\n"));
        self.written += 1;
        Ok(format!("{image} 4x4 flat colour"))
    }

    /// Converts `reference` (a `.vtex` path) into the pack as the image of
    /// `material`, returning the row's description.
    fn add(
        &mut self,
        pak: &vpk::Vpk,
        material: &str,
        reference: &str,
        tint: [f32; 3],
        flags: &str,
    ) -> Result<String, String> {
        let vtex_name = format!("{reference}_c");
        let texture = pak
            .read(&vtex_name)
            .and_then(|bytes| vtex::parse(&bytes))
            .map_err(|error| format!("{reference}: {error}"))?;
        let converted = convert(&texture, self.max_size, tint).map_err(|error| format!("{reference}: {error}"))?;
        let image = format!(
            "{}{:08x}",
            image_prefix(material),
            fnv1a(material) as u32 ^ (fnv1a(material) >> 32) as u32
        );
        let file = iwi::write(converted.width, converted.height, converted.format, &converted.levels);
        self.total_bytes += file.len();
        self.zip
            .start_file(format!("images/{image}.iwi"), self.stored)
            .map_err(|e| format!("zip: {e}"))?;
        self.zip.write_all(&file).map_err(|e| format!("zip: {e}"))?;
        self.manifest.push_str(&format!(
            "{material}\t{image}\t{}\t{}\t{}\t{flags}\n",
            converted.width, converted.height, reference
        ));
        self.written += 1;
        Ok(format!(
            "{image} {}x{} ({} levels, {}) <- {reference}{}",
            converted.width,
            converted.height,
            converted.levels.len(),
            converted.note,
            if flags.is_empty() { String::new() } else { format!(" [{flags}]") }
        ))
    }
}

/// What the minimap shows off the map: Counter-Strike's radar is clear
/// there, and through a clear minimap the world reads as part of the map.
const RADAR_GROUND: [u8; 3] = [14, 17, 16];

/// The radar's largest level laid over `RADAR_GROUND`, opaque as MW2's own
/// minimaps are; the smaller levels are made again from it.
fn on_radar_ground(texture: &vtex::Texture) -> Option<vtex::Texture> {
    let (width, height) = (texture.width, texture.height);
    let level = texture.levels.first()?;
    let mut data = match texture.format {
        vtex::Format::Rgba8 => level.clone(),
        vtex::Format::Bgra8 => level.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect(),
        vtex::Format::Dxt1 => dxt::decode_bc1(width, height, level).data,
        vtex::Format::Dxt5 => dxt::decode_bc3(width, height, level).data,
        vtex::Format::Bc7 => dxt::decode_bc7(width, height, level).data,
        _ => return None,
    };
    for texel in data.chunks_exact_mut(4) {
        let alpha = f32::from(texel[3]) / 255.0;
        for (channel, ground) in texel.iter_mut().zip(RADAR_GROUND) {
            *channel = (f32::from(*channel) * alpha + f32::from(ground) * (1.0 - alpha)).round() as u8;
        }
        texel[3] = 255;
    }
    Some(vtex::Texture {
        width: texture.width,
        height: texture.height,
        format: vtex::Format::Rgba8,
        levels: vec![data],
    })
}

/// `--radar <map>`: Counter-Strike's overhead picture of `<map>` and the
/// overview that places it, as the zone's minimap.
fn write_radar(pak: &vpk::Vpk, map: &str, args: &Args) -> Result<(), String> {
    let overview_name = format!("resource/overviews/{map}.txt");
    let overview = pak.read(&overview_name)?;
    let image = format!("compass_map_{}", args.name);
    let radar = source_map::Radar::from_overview(&image, &String::from_utf8_lossy(&overview))
        .map_err(|error| format!("{overview_name}: {error}"))?;
    let picture = format!("panorama/images/overheadmaps/{map}_radar_psd.vtex_c");
    let texture = pak
        .read(&picture)
        .and_then(|bytes| vtex::parse(&bytes))
        .map_err(|error| format!("{picture}: {error}"))?;
    let texture = on_radar_ground(&texture).ok_or_else(|| format!("{picture}: {:?} is not a radar format this tool reads", texture.format))?;
    let converted = convert(&texture, 1024, [1.0; 3]).map_err(|error| format!("{picture}: {error}"))?;
    let file = iwi::write(converted.width, converted.height, converted.format, &converted.levels);
    let iwd_path = args.out.join(format!("{}_radar.iwd", args.name));
    let out = std::fs::File::create(&iwd_path).map_err(|e| format!("{}: {e}", iwd_path.display()))?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(out));
    zip.start_file(
        format!("images/{image}.iwi"),
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )
    .map_err(|e| format!("zip: {e}"))?;
    zip.write_all(&file).map_err(|e| format!("zip: {e}"))?;
    zip.finish().map_err(|e| format!("zip: {e}"))?;
    let text_path = args.out.join(format!("{}.radar.txt", args.name));
    std::fs::write(&text_path, radar.to_text()).map_err(|e| format!("{}: {e}", text_path.display()))?;
    println!(
        "radar: {image} {}x{} ({} levels, {}) over {:?}..{:?} -> {} and {}",
        converted.width,
        converted.height,
        converted.levels.len(),
        converted.note,
        radar.upper_left,
        radar.lower_right,
        iwd_path.display(),
        text_path.display()
    );
    Ok(())
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let pak = vpk::Vpk::open(&args.cs2.join("pak01_dir.vpk"))?;
    std::fs::create_dir_all(&args.out).map_err(|e| format!("{}: {e}", args.out.display()))?;
    if let Some(map) = &args.radar {
        write_radar(&pak, map, &args)?;
    }
    let Some(vmf) = &args.vmf else {
        return Ok(());
    };
    let text = std::fs::read_to_string(vmf).map_err(|e| format!("{}: {e}", vmf.display()))?;
    let map = source_map::compile(&text, 512.0)?;
    let iwd_path = args.out.join(format!("{}_textures.iwd", args.name));
    let tsv_path = args.out.join(format!("{}.textures.tsv", args.name));
    let file = std::fs::File::create(&iwd_path).map_err(|e| format!("{}: {e}", iwd_path.display()))?;
    let mut pack = Pack {
        zip: zip::ZipWriter::new(std::io::BufWriter::new(file)),
        stored: zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .large_file(true),
        manifest: String::from("# material\timage\twidth\theight\tsource\tflags\n"),
        written: 0,
        total_bytes: 0,
        max_size: args.max_size,
    };
    let mut skipped: Vec<String> = Vec::new();
    let mut unread: Vec<String> = Vec::new();
    let nearest = nearest::Nearest::new(pak.names(), args.radar.as_deref().unwrap_or(&args.name));
    let mut stand_ins: Vec<String> = Vec::new();
    // The map's props: every model a prop entity names, read once.
    let mut prop_names: Vec<String> = map
        .entities
        .iter()
        .filter(|e| e.classname == "prop_static" || e.classname == "prop_dynamic" || e.classname == "prop_dynamic_override")
        .filter_map(|e| e.get("model"))
        .map(|m| m.replace('\\', "/").to_ascii_lowercase())
        .collect();
    prop_names.sort();
    prop_names.dedup();
    let mut props: Vec<(String, vmdl::Model)> = Vec::new();
    let mut prop_materials: BTreeSet<String> = BTreeSet::new();
    let mut prop_failures: Vec<String> = Vec::new();
    let (mut prop_vertices, mut prop_triangles) = (0usize, 0usize);
    for name in &prop_names {
        let stem = name.strip_suffix(".mdl").unwrap_or(name);
        let compiled = format!("{stem}.vmdl_c");
        if !pak.contains(&compiled) {
            prop_failures.push(format!("{name}: no model in CS2"));
            continue;
        }
        let model = match pak.read(&compiled).and_then(|bytes| vmdl::parse(&bytes)) {
            Ok(model) => model,
            Err(error) => {
                prop_failures.push(format!("{name}: {error}"));
                continue;
            }
        };
        prop_vertices += model.positions.len();
        prop_triangles += model.indices.len() / 3;
        for draw in &model.draws {
            prop_materials.insert(pack_material_name(&draw.material));
        }
        props.push((name.clone(), model));
    }
    println!(
        "props: {} models read of {} named, {} vertices, {} triangles, {} materials",
        props.len(),
        prop_names.len(),
        prop_vertices,
        prop_triangles,
        prop_materials.len()
    );
    // The compiler names a blend's second layer `<material>#2`; both come
    // from the one compiled material.
    let uses: BTreeMap<&str, usize> = map
        .material_uses
        .iter()
        .map(|(name, count)| (name.as_str(), *count))
        .collect();
    let mut bases: BTreeSet<&str> = uses
        .keys()
        .map(|name| name.strip_suffix("#2").unwrap_or(name))
        .filter(|name| !name.starts_with("tools/"))
        .collect();
    bases.extend(prop_materials.iter().map(String::as_str));
    for material in bases {
        let faces = uses.get(material).copied().unwrap_or(0) + uses.get(format!("{material}#2").as_str()).copied().unwrap_or(0);
        // The material itself, then its nearest stand-ins in turn.
        let mut tries: Vec<(String, Option<&str>)> = [
            format!("materials/{material}.vmat_c"),
            format!("materials/{material}_.vmat_c"),
        ]
        .into_iter()
        .filter(|name| pak.contains(name))
        .map(|name| (name, None))
        .collect();
        tries.extend(
            nearest
                .ranked(material, 8)
                .into_iter()
                .map(|path| (format!("materials/{path}.vmat_c"), Some(path))),
        );
        let mut failures: Vec<String> = Vec::new();
        let mut added = None;
        for (vmat, stand_in) in tries {
            match add_material(&mut pack, &pak, material, &vmat, stand_in, &mut unread) {
                Ok((info, note)) => {
                    added = Some((info, note, stand_in));
                    break;
                }
                Err(error) => failures.push(error),
            }
        }
        let Some((info, note, stand_in)) = added else {
            let why = failures.first().map_or("no material in CS2", String::as_str);
            skipped.push(format!("{material} ({faces} faces): {why}"));
            continue;
        };
        println!("{material}: {note}");
        if let Some(path) = stand_in {
            stand_ins.push(format!("{material} ({faces} faces) <- {path}"));
        }
        let flags = flags_of(&info);
        let second = format!("{material}#2");
        if uses.contains_key(second.as_str())
            && let Some(layer2) = layer_texture(&info, LAYER2_COLOR)
        {
            match pack.add(&pak, &second, layer2, layer_tint(&info, LAYER2_TINT), &flags) {
                Ok(note) => println!("{second}: {note}"),
                Err(error) => skipped.push(format!("{second}: {error}")),
            }
        }
    }
    // Hammer's `toolsblack` closes off what should read as pitch black (the
    // skirts under crates, the dark behind arches): a black texel does that
    // through the same colour-only material as everything else.
    if uses.contains_key("tools/toolsblack") {
        match pack.add_flat("tools/toolsblack", [0, 0, 0, 255]) {
            Ok(note) => println!("tools/toolsblack: {note}"),
            Err(error) => skipped.push(format!("tools/toolsblack: {error}")),
        }
    }
    pack.zip.finish().map_err(|e| format!("zip: {e}"))?;
    std::fs::write(&tsv_path, &pack.manifest).map_err(|e| format!("{}: {e}", tsv_path.display()))?;
    let props_path = args.out.join(format!("{}.props.bin", args.name));
    let props_bytes = write_props(&props);
    std::fs::write(&props_path, &props_bytes).map_err(|e| format!("{}: {e}", props_path.display()))?;
    println!("props: {} -> {} ({:.1} MB)", props.len(), props_path.display(), props_bytes.len() as f64 / 1e6);
    if !prop_failures.is_empty() {
        println!("\nprops not read {}:", prop_failures.len());
        for line in &prop_failures {
            println!("  {line}");
        }
    }
    println!(
        "\n{} textures, {:.1} MB -> {}\nmanifest {}",
        pack.written,
        pack.total_bytes as f64 / 1e6,
        iwd_path.display(),
        tsv_path.display()
    );
    if !stand_ins.is_empty() {
        println!("\nstood in by the nearest CS2 material {}:", stand_ins.len());
        for line in &stand_ins {
            println!("  {line}");
        }
    }
    if !unread.is_empty() {
        println!("\nparameters not read for {} (colour map guessed from references):", unread.len());
        for line in &unread {
            println!("  {line}");
        }
    }
    if !skipped.is_empty() {
        println!("\nskipped {}:", skipped.len());
        for line in &skipped {
            println!("  {line}");
        }
    }
    Ok(())
}

/// Adds `material` to the pack from the CS2 material `vmat` (`stand_in`
/// names it when it is not the material itself): its colour texture with
/// the tint baked in, and its flags. Returns what the material says and the
/// row's description.
fn add_material(
    pack: &mut Pack,
    pak: &vpk::Vpk,
    material: &str,
    vmat: &str,
    stand_in: Option<&str>,
    unread: &mut Vec<String>,
) -> Result<(MaterialInfo, String), String> {
    let bytes = pak.read(vmat)?;
    let info = match material_info(&bytes) {
        Ok(info) => info,
        Err(error) => {
            unread.push(format!("{material}: {error}"));
            MaterialInfo::default()
        }
    };
    let refs = resource::external_references(&bytes);
    let (layer1, how) = match layer_texture(&info, LAYER1_COLOR) {
        Some(path) => (path.to_owned(), "parameter"),
        None => match color_reference(stand_in.unwrap_or(material), &refs) {
            Some(path) => (path.to_owned(), "guessed"),
            None => {
                return Err(format!(
                    "no colour texture among {:?} / {refs:?}",
                    info.textures.values().collect::<Vec<_>>()
                ));
            }
        },
    };
    let note = pack.add(pak, material, &layer1, layer_tint(&info, LAYER1_TINT), &flags_of(&info))?;
    let note = format!("{note} ({how}, {})", info.shader);
    Ok((info, note))
}

/// The pack's name for a draw call's material: `materials/x/y.vmat` → `x/y`.
fn pack_material_name(path: &str) -> String {
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    let stem = lower.strip_prefix("materials/").unwrap_or(&lower);
    stem.strip_suffix(".vmat").unwrap_or(stem).to_owned()
}

/// `<zone>.props.bin`: every model the map's props use, as the engine
/// places them (crates/source_map/src/props.rs reads this).
fn write_props(props: &[(String, vmdl::Model)]) -> Vec<u8> {
    source_map::props::deflate_props(&write_props_raw(props))
}

fn write_props_raw(props: &[(String, vmdl::Model)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"IW4LPRP1");
    out.extend_from_slice(&(props.len() as u32).to_le_bytes());
    for (name, model) in props {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(model.positions.len() as u32).to_le_bytes());
        out.extend_from_slice(&(model.indices.len() as u32).to_le_bytes());
        out.extend_from_slice(&(model.draws.len() as u16).to_le_bytes());
        for (position, uv) in model.positions.iter().zip(&model.uvs) {
            for x in position.iter().chain(uv.iter()) {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        for index in &model.indices {
            out.extend_from_slice(&index.to_le_bytes());
        }
        for draw in &model.draws {
            out.extend_from_slice(&draw.first_index.to_le_bytes());
            out.extend_from_slice(&draw.index_count.to_le_bytes());
            let material = pack_material_name(&draw.material);
            out.extend_from_slice(&(material.len() as u16).to_le_bytes());
            out.extend_from_slice(material.as_bytes());
        }
    }
    out
}

fn main() {
    if let Err(error) = run() {
        eprintln!("cs2_textures: {error}");
        std::process::exit(1);
    }
}
