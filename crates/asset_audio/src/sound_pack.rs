//! A zone's sound catalog baked so the zone itself is never read.
//!
//! `localized_common_mp.ff` is one 120 MB zlib stream that a match opens for
//! nothing but its sounds: 4,864 aliases and 1,148 loaded sounds holding
//! 139 MiB of PCM. The pack (`iw4l-maps/<zone>.sounds`) is that catalog with
//! the audio FLAC-encoded and addressed by offset, so a client reads the
//! index up front and each sound's bytes the first time it plays, through the
//! same bank path the T6 sound banks use ([`SabMediaSource`]). On the web only
//! the sounds that play are ever downloaded.
//!
//! Layout: `IW4LSND1`, a little-endian u32 index length, the postcard-encoded
//! index, then the FLAC streams back to back. Entry offsets are relative to
//! the end of the index so the index can be sized before they are known.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sound_catalog::{
    CapturedAlias, CapturedSndCurve, CapturedSound, LoadedSoundEdge, LoadedSoundPcm, SoundCatalog,
    VoicePriority,
};
use crate::{
    AssetEdge, AssetEdgeReason, AssetNamespace, SAB_FRAME_RATES, SabEntry, SabMediaSource, ZoneGame,
    ZoneOwner,
};

pub const SOUND_PACK_MAGIC: &[u8; 8] = b"IW4LSND1";
pub const SOUND_PACK_EXT: &str = "sounds";
const HEADER_LEN: usize = 12;

/// Where the pack for a zone lives when one was baked:
/// `<games>/zone/<language>/<zone>.ff` → `<games>/iw4l-maps/<zone>.sounds`.
/// `None` when there is no such file.
pub fn sound_pack_for_zone(zone: &Path) -> Option<PathBuf> {
    let stem = zone.file_stem()?.to_str()?;
    let games = zone.parent()?.parent()?.parent()?;
    let pack = games
        .join("iw4l-maps")
        .join(format!("{stem}.{SOUND_PACK_EXT}"));
    vfs::metadata(&pack).is_ok().then_some(pack)
}

#[derive(Serialize, Deserialize)]
struct PackIndex {
    zone: String,
    sounds: Vec<PackSound>,
    loaded: Vec<PackLoaded>,
    curves: Vec<PackCurve>,
    rawfiles: Vec<(String, Vec<u8>)>,
    capture_gaps: u64,
    alias_flags_missing: u64,
    curve_capture_gaps: u64,
}

#[derive(Serialize, Deserialize)]
struct PackSound {
    name: String,
    aliases: Vec<PackAlias>,
}

#[derive(Serialize, Deserialize)]
struct PackCurve {
    key: String,
    name: String,
    knots: Vec<(f32, f32)>,
}

#[derive(Serialize, Deserialize)]
struct PackKnots {
    name: String,
    knots: Vec<(f32, f32)>,
}

#[derive(Serialize, Deserialize)]
struct PackVoice {
    thresholds: [u8; 2],
    values: [u8; 2],
    distance_max: f32,
}

#[derive(Serialize, Deserialize)]
struct PackLoaded {
    name: String,
    rate: u32,
    channels: u8,
    frames: u32,
    /// Offset of the stream from the end of the index.
    offset: u32,
    size: u32,
    /// A `SAB_FORMAT_*` codec.
    format: u8,
}

/// [`CapturedAlias`] field for field; the two `&'static str` pointer words
/// travel as codes and the loaded edge as its resolution state, which
/// `resolve_loaded_edges` rebuilds from the loaded name.
#[derive(Serialize, Deserialize)]
struct PackAlias {
    alias_name: String,
    subtitle: Option<String>,
    secondary: Option<String>,
    chain: Option<String>,
    mixer_group: Option<String>,
    loaded_name: Option<String>,
    loaded: u8,
    streamed: Option<(String, String)>,
    file_type: Option<u8>,
    file_exists: Option<u8>,
    file_name: Option<String>,
    file_u: Option<u8>,
    file_u_ptr: Option<String>,
    file_u_deref: Option<u8>,
    sequence: i32,
    vol_min: f32,
    vol_max: f32,
    vol_mod_index: Option<u32>,
    pitch_min: f32,
    pitch_max: f32,
    dist_min: f32,
    dist_max: f32,
    velocity_min: f32,
    flags: Option<u32>,
    looping: Option<bool>,
    slave_percentage: f32,
    probability: f32,
    lfe_percentage: f32,
    center_percentage: f32,
    start_delay: i32,
    volume_falloff: Option<PackKnots>,
    t5_distance_curves: Option<[u8; 2]>,
    near_falloff: Option<PackKnots>,
    voice_priority: Option<PackVoice>,
    envelop_min: f32,
    envelop_max: f32,
    envelop_percentage: f32,
    speaker_map: Option<String>,
    stereo_speaker_gains: Option<[[f32; 2]; 2]>,
    t6_speaker_pan: Option<[f32; 6]>,
    limit_count: Option<u8>,
    entity_limit_count: Option<u8>,
}

fn ptr_word_code(word: Option<&'static str>) -> Option<u8> {
    word.map(|word| match word {
        "null" => 0,
        "offset" => 1,
        "following" => 2,
        "insert" => 3,
        _ => 255,
    })
}

fn ptr_word(code: Option<u8>) -> Option<&'static str> {
    match code? {
        0 => Some("null"),
        1 => Some("offset"),
        2 => Some("following"),
        3 => Some("insert"),
        _ => None,
    }
}

fn edge_code(edge: LoadedSoundEdge) -> u8 {
    match edge {
        AssetEdge::Absent => 0,
        AssetEdge::Unresolved(AssetEdgeReason::TempFieldNotAliasable) => 1,
        // A bound edge comes back as a catalog miss and is bound again by
        // name once the pack's loaded sounds are registered.
        AssetEdge::Unresolved(AssetEdgeReason::CatalogMiss) | AssetEdge::Bound(_) => 2,
    }
}

fn edge_of(code: u8) -> LoadedSoundEdge {
    match code {
        0 => AssetEdge::Absent,
        1 => AssetEdge::Unresolved(AssetEdgeReason::TempFieldNotAliasable),
        _ => AssetEdge::Unresolved(AssetEdgeReason::CatalogMiss),
    }
}

fn knots_of(curve: &CapturedSndCurve) -> PackKnots {
    PackKnots {
        name: curve.name.clone(),
        knots: curve.knots.clone(),
    }
}

fn curve_of(knots: PackKnots) -> CapturedSndCurve {
    CapturedSndCurve {
        name: knots.name,
        knots: knots.knots,
    }
}

impl PackAlias {
    fn from_alias(alias: &CapturedAlias) -> Self {
        Self {
            alias_name: alias.alias_name.clone(),
            subtitle: alias.subtitle.clone(),
            secondary: alias.secondary.clone(),
            chain: alias.chain.clone(),
            mixer_group: alias.mixer_group.clone(),
            loaded_name: alias.loaded_name.clone(),
            loaded: edge_code(alias.loaded),
            streamed: alias.streamed.clone(),
            file_type: alias.file_type,
            file_exists: alias.file_exists,
            file_name: alias.file_name.clone(),
            file_u: ptr_word_code(alias.file_u),
            file_u_ptr: alias.file_u_ptr.clone(),
            file_u_deref: ptr_word_code(alias.file_u_deref),
            sequence: alias.sequence,
            vol_min: alias.vol_min,
            vol_max: alias.vol_max,
            vol_mod_index: alias.vol_mod_index,
            pitch_min: alias.pitch_min,
            pitch_max: alias.pitch_max,
            dist_min: alias.dist_min,
            dist_max: alias.dist_max,
            velocity_min: alias.velocity_min,
            flags: alias.flags,
            looping: alias.looping,
            slave_percentage: alias.slave_percentage,
            probability: alias.probability,
            lfe_percentage: alias.lfe_percentage,
            center_percentage: alias.center_percentage,
            start_delay: alias.start_delay,
            volume_falloff: alias.volume_falloff.as_ref().map(knots_of),
            t5_distance_curves: alias.t5_distance_curves,
            near_falloff: alias.near_falloff.as_ref().map(knots_of),
            voice_priority: alias.voice_priority.as_ref().map(|voice| PackVoice {
                thresholds: voice.thresholds,
                values: voice.values,
                distance_max: voice.distance_max,
            }),
            envelop_min: alias.envelop_min,
            envelop_max: alias.envelop_max,
            envelop_percentage: alias.envelop_percentage,
            speaker_map: alias.speaker_map.clone(),
            stereo_speaker_gains: alias.stereo_speaker_gains,
            t6_speaker_pan: alias.t6_speaker_pan,
            limit_count: alias.limit_count,
            entity_limit_count: alias.entity_limit_count,
        }
    }

    fn into_alias(self) -> CapturedAlias {
        CapturedAlias {
            alias_name: self.alias_name,
            subtitle: self.subtitle,
            secondary: self.secondary,
            chain: self.chain,
            mixer_group: self.mixer_group,
            loaded_name: self.loaded_name,
            loaded: edge_of(self.loaded),
            loaded_binding_origin: Default::default(),
            streamed: self.streamed,
            file_type: self.file_type,
            file_exists: self.file_exists,
            file_name: self.file_name,
            file_u: ptr_word(self.file_u),
            file_u_ptr: self.file_u_ptr,
            file_u_deref: ptr_word(self.file_u_deref),
            sequence: self.sequence,
            vol_min: self.vol_min,
            vol_max: self.vol_max,
            vol_mod_index: self.vol_mod_index,
            pitch_min: self.pitch_min,
            pitch_max: self.pitch_max,
            dist_min: self.dist_min,
            dist_max: self.dist_max,
            velocity_min: self.velocity_min,
            flags: self.flags,
            looping: self.looping,
            slave_percentage: self.slave_percentage,
            probability: self.probability,
            lfe_percentage: self.lfe_percentage,
            center_percentage: self.center_percentage,
            start_delay: self.start_delay,
            volume_falloff: self.volume_falloff.map(curve_of),
            t5_distance_curves: self.t5_distance_curves,
            near_falloff: self.near_falloff.map(curve_of),
            voice_priority: self.voice_priority.map(|voice| VoicePriority {
                thresholds: voice.thresholds,
                values: voice.values,
                distance_max: voice.distance_max,
            }),
            envelop_min: self.envelop_min,
            envelop_max: self.envelop_max,
            envelop_percentage: self.envelop_percentage,
            speaker_map: self.speaker_map,
            stereo_speaker_gains: self.stereo_speaker_gains,
            t6_speaker_pan: self.t6_speaker_pan,
            limit_count: self.limit_count,
            entity_limit_count: self.entity_limit_count,
        }
    }
}

/// Reads a pack into a catalog whose loaded sounds point back into the pack.
/// Only the header and the index are read here; audio follows on first play.
pub fn read_sound_pack(path: &Path) -> Result<SoundCatalog, String> {
    use std::io::Read;

    let display = path.display();
    let mut file = vfs::File::open(path).map_err(|error| format!("{display}: {error}"))?;
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|error| format!("{display}: header: {error}"))?;
    if &header[..8] != SOUND_PACK_MAGIC {
        return Err(format!("{display}: not a sound pack"));
    }
    let index_len = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
    let mut index_bytes = vec![0u8; index_len];
    file.read_exact(&mut index_bytes)
        .map_err(|error| format!("{display}: index: {error}"))?;
    let index: PackIndex = postcard::from_bytes(&index_bytes)
        .map_err(|error| format!("{display}: index does not parse: {error}"))?;
    let base = (HEADER_LEN + index_len) as u64;

    let mut catalog = SoundCatalog::default();
    catalog.set_capture_zone(ZoneOwner::intern(&index.zone));
    catalog.set_capture_game(ZoneGame::Iw4);
    let zone = catalog.capture_zone_for_ingest();
    for sound in index.sounds {
        catalog.ingest_sound(CapturedSound {
            name: sound.name,
            aliases: sound.aliases.into_iter().map(PackAlias::into_alias).collect(),
            game: ZoneGame::Iw4,
            zone,
        });
    }
    for (id, loaded) in index.loaded.into_iter().enumerate() {
        let rate_index = SAB_FRAME_RATES
            .iter()
            .position(|&rate| rate == loaded.rate)
            .ok_or_else(|| {
                format!(
                    "{display}: `{}` has the unsupported rate {}",
                    loaded.name, loaded.rate
                )
            })?;
        let offset = u32::try_from(base + u64::from(loaded.offset))
            .map_err(|_| format!("{display}: `{}` lies past 4 GiB", loaded.name))?;
        let entry = SabEntry {
            id: id as u32,
            size: loaded.size,
            offset,
            frame_count: loaded.frames,
            frame_rate_index: rate_index as u8,
            channels: loaded.channels,
            looping: false,
            format: loaded.format,
        };
        catalog.ingest_loaded(LoadedSoundPcm::from_pack(
            loaded.name,
            ZoneGame::Iw4,
            zone,
            SabMediaSource {
                bank: path.to_path_buf(),
                entry,
            },
        ));
    }
    for curve in index.curves {
        catalog.curves.insert(
            (AssetNamespace::Iw4, curve.key),
            CapturedSndCurve {
                name: curve.name,
                knots: curve.knots,
            },
        );
    }
    for (name, bytes) in index.rawfiles {
        catalog.rawfiles.insert((AssetNamespace::Iw4, name), bytes);
    }
    catalog.capture_gaps = index.capture_gaps as usize;
    catalog.alias_flags_missing = index.alias_flags_missing as usize;
    catalog.curve_capture_gaps = index.curve_capture_gaps as usize;
    catalog.resolve_curve_knots();
    catalog.resolve_ent_channels();
    catalog.publish();
    Ok(catalog)
}

/// What baking a pack produced.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct SoundPackReport {
    pub sounds: usize,
    pub aliases: usize,
    pub pcm_bytes: u64,
    pub audio_bytes: u64,
    pub index_bytes: usize,
    /// Sounds the priority list named, stored first.
    pub prioritized: usize,
    /// FLAC streams the game's decoder refused, kept as PCM instead, with
    /// the reason (name, frames, channels, rate, cause).
    pub flac_refused: Vec<String>,
    pub flac_kept: usize,
    /// Loaded sounds the pack could not carry, with the reason.
    pub skipped: Vec<String>,
    pub elapsed_ms: f64,
}

/// Decodes `bytes` with symphonia exactly as `audio::sab_media` does at
/// play time; the pack keeps FLAC only when this succeeds.
#[cfg(not(target_arch = "wasm32"))]
fn verify_flac(bytes: &[u8], channels: usize, rate: u32, frames: u32) -> Result<(), String> {
    use symphonia::core::codecs::{CODEC_TYPE_FLAC, DecoderOptions};
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(
        Box::new(std::io::Cursor::new(bytes.to_vec())),
        MediaSourceStreamOptions::default(),
    );
    let mut format = symphonia::default::get_probe()
        .format(
            Hint::new().with_extension("flac"),
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| format!("probe: {error}"))?
        .format;
    let track = format.default_track().ok_or("no track")?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    if params.codec != CODEC_TYPE_FLAC
        || params.sample_rate != Some(rate)
        || params.channels.map(|channels| channels.count()) != Some(channels)
        || params.n_frames.is_some_and(|native| native != u64::from(frames))
    {
        return Err(format!(
            "stream info rate={:?} channels={:?} frames={:?}",
            params.sample_rate,
            params.channels.map(|c| c.count()),
            params.n_frames
        ));
    }
    let mut decoder = symphonia::default::get_codecs()
        .make(
            &params,
            &DecoderOptions {
                verify: params.verification_check.is_some(),
            },
        )
        .map_err(|error| format!("decoder: {error}"))?;
    let mut decoded = 0u64;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(format!("packet: {error}")),
        };
        if packet.track_id() != track_id {
            continue;
        }
        decoded += decoder
            .decode(&packet)
            .map_err(|error| format!("decode: {error}"))?
            .frames() as u64;
    }
    if decoded != u64::from(frames) {
        return Err(format!("decoded {decoded} of {frames} frames"));
    }
    if decoder.finalize().verify_ok == Some(false) {
        return Err("md5".to_owned());
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
struct EncodedSound {
    name: String,
    rate: u32,
    channels: u8,
    frames: u32,
    format: u8,
    bytes: Vec<u8>,
    pcm_len: usize,
    /// Why the FLAC stream was not kept, when it was not.
    flac_refused: Option<String>,
}

/// flacenc writes STREAMINFO's minimum block size as the size of the short
/// final block, but the FLAC spec excludes the last block from it. A reader
/// that sees minimum != maximum treats the stream as variable-blocksize and
/// rejects every fixed-blocksize frame header (symphonia scans to the end of
/// the stream and reports "end of stream"), so every multi-block sound
/// failed to decode in the game. For a fixed-blocksize stream the minimum is
/// the maximum.
#[cfg(not(target_arch = "wasm32"))]
fn fix_min_block_size(bytes: &mut [u8]) {
    // "fLaC", then the STREAMINFO block header (4 bytes), then
    // min block size (u16 BE) and max block size (u16 BE).
    if bytes.len() < 12 || &bytes[..4] != b"fLaC" || bytes[4] & 0x7f != 0 {
        return;
    }
    let max = [bytes[10], bytes[11]];
    bytes[8] = max[0];
    bytes[9] = max[1];
}

/// One loaded sound as the pack stores it: FLAC when the encoder takes it,
/// plain 16-bit PCM otherwise. Both decode through the bank path.
#[cfg(not(target_arch = "wasm32"))]
fn encode_sound(sound: &LoadedSoundPcm) -> Result<EncodedSound, String> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    if sound.sab_media.is_some() {
        return Err("already bank-backed".to_owned());
    }
    if sound.format != crate::MSS_PCM {
        return Err(format!("format {} is not PCM", sound.format));
    }
    let channels = match sound.channels {
        1 | 2 => sound.channels as usize,
        n => return Err(format!("{n} channels")),
    };
    if !SAB_FRAME_RATES.contains(&sound.rate) {
        return Err(format!("rate {} is not in the bank table", sound.rate));
    }
    let bytes = sound.pcm.bytes();
    let mut samples: Vec<i32> = match sound.bits {
        16 => bytes
            .chunks_exact(2)
            .map(|pair| i32::from(i16::from_le_bytes([pair[0], pair[1]])))
            .collect(),
        8 => bytes
            .iter()
            .map(|&sample| (i32::from(sample) - 128) << 8)
            .collect(),
        bits => return Err(format!("{bits}-bit samples")),
    };
    samples.truncate(samples.len() / channels * channels);
    let frames = samples.len() / channels;
    if frames == 0 {
        return Err("no samples".to_owned());
    }
    let frames = u32::try_from(frames).map_err(|_| "too long".to_owned())?;
    let flac = (|| -> Result<Vec<u8>, String> {
        let config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(|(_, error)| format!("{error:?}"))?;
        let source =
            flacenc::source::MemSource::from_samples(&samples, channels, 16, sound.rate as usize);
        let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
            .map_err(|error| format!("{error:?}"))?;
        let mut sink = flacenc::bitsink::ByteSink::new();
        stream
            .write(&mut sink)
            .map_err(|error| format!("{error:?}"))?;
        let mut bytes = sink.as_slice().to_vec();
        fix_min_block_size(&mut bytes);
        Ok(bytes)
    })();
    let flac = flac.and_then(|bytes| {
        let checked = verify_flac(&bytes, channels, sound.rate, frames);
        if checked.is_err()
            && let Ok(path) = std::env::var("IW4L_SOUND_PACK_DUMP_REFUSED")
            && !std::path::Path::new(&path).exists()
        {
            let _ = std::fs::write(&path, &bytes);
        }
        checked.map(|()| bytes)
    });
    let pcm = || -> Vec<u8> {
        samples
            .iter()
            .flat_map(|&sample| (sample as i16).to_le_bytes())
            .collect()
    };
    let (format, bytes, flac_refused) = match flac {
        Ok(bytes) if bytes.len() < samples.len() * 2 => (crate::SAB_FORMAT_FLAC, bytes, None),
        Ok(_) => (crate::SAB_FORMAT_PCMS16, pcm(), None),
        Err(error) => (crate::SAB_FORMAT_PCMS16, pcm(), Some(error)),
    };
    Ok(EncodedSound {
        name: sound.name.clone(),
        rate: sound.rate,
        channels: channels as u8,
        frames,
        format,
        bytes,
        pcm_len: sound.pcm.bytes().len(),
        flac_refused,
    })
}

/// Bakes `catalog` (walked from its zone, audio and all) into `out`. Sounds
/// named in `priority` come first in that order: a match prepares its
/// required clips before AudioReady, and a pack that holds them back to back
/// is read in a few windows instead of one round trip per clip.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_sound_pack(
    catalog: &SoundCatalog,
    out: &Path,
    priority: &[String],
) -> Result<SoundPackReport, String> {
    use std::io::Write;

    let started = std::time::Instant::now();
    let rank: std::collections::HashMap<&str, usize> = priority
        .iter()
        .enumerate()
        .map(|(rank, name)| (name.as_str(), rank))
        .collect();
    let mut order: Vec<usize> = (0..catalog.loaded.len()).collect();
    order.sort_by_key(|&index| {
        (
            rank.get(catalog.loaded[index].name.as_str())
                .copied()
                .unwrap_or(usize::MAX),
            index,
        )
    });
    let sounds: Vec<&LoadedSoundPcm> = order
        .iter()
        .map(|&index| &catalog.loaded[index])
        .collect();
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 32);
    let mut encoded: Vec<Option<Result<EncodedSound, String>>> =
        (0..sounds.len()).map(|_| None).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut mine = Vec::new();
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(sound) = sounds.get(index) else {
                        break;
                    };
                    mine.push((index, encode_sound(sound)));
                }
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(mine);
            });
        }
    });
    for (index, result) in results.into_inner().unwrap_or_else(|e| e.into_inner()) {
        encoded[index] = Some(result);
    }

    let mut report = SoundPackReport {
        aliases: catalog.sounds.len(),
        prioritized: sounds
            .iter()
            .filter(|sound| rank.contains_key(sound.name.as_str()))
            .count(),
        ..Default::default()
    };
    let mut loaded = Vec::with_capacity(sounds.len());
    let mut audio: Vec<u8> = Vec::new();
    for (sound, result) in sounds.iter().zip(encoded) {
        match result {
            Some(Ok(sound)) => {
                if let Some(cause) = &sound.flac_refused {
                    report.flac_refused.push(format!(
                        "{} frames={} channels={} rate={}: {cause}",
                        sound.name, sound.frames, sound.channels, sound.rate
                    ));
                }
                if sound.format == crate::SAB_FORMAT_FLAC {
                    report.flac_kept += 1;
                }
                let offset = u32::try_from(audio.len())
                    .map_err(|_| "the pack's audio passes 4 GiB".to_owned())?;
                let size = u32::try_from(sound.bytes.len())
                    .map_err(|_| format!("`{}` passes 4 GiB", sound.name))?;
                loaded.push(PackLoaded {
                    name: sound.name,
                    rate: sound.rate,
                    channels: sound.channels,
                    frames: sound.frames,
                    offset,
                    size,
                    format: sound.format,
                });
                audio.extend_from_slice(&sound.bytes);
                report.sounds += 1;
                report.pcm_bytes += sound.pcm_len as u64;
                report.audio_bytes += u64::from(size);
            }
            Some(Err(error)) => report.skipped.push(format!("{}: {error}", sound.name)),
            None => report.skipped.push(format!("{}: not encoded", sound.name)),
        }
    }
    let index = PackIndex {
        zone: catalog.capture_zone_for_ingest().as_str().to_owned(),
        sounds: catalog
            .sounds
            .iter()
            .map(|sound| PackSound {
                name: sound.name.clone(),
                aliases: sound.aliases.iter().map(PackAlias::from_alias).collect(),
            })
            .collect(),
        loaded,
        curves: catalog
            .curves
            .iter()
            .filter(|((namespace, _), _)| *namespace == AssetNamespace::Iw4)
            .map(|((_, key), curve)| PackCurve {
                key: key.clone(),
                name: curve.name.clone(),
                knots: curve.knots.clone(),
            })
            .collect(),
        rawfiles: catalog
            .rawfiles
            .iter()
            .filter(|((namespace, _), _)| *namespace == AssetNamespace::Iw4)
            .map(|((_, name), bytes)| (name.clone(), bytes.clone()))
            .collect(),
        capture_gaps: catalog.capture_gaps as u64,
        alias_flags_missing: catalog.alias_flags_missing as u64,
        curve_capture_gaps: catalog.curve_capture_gaps as u64,
    };
    let index_bytes =
        postcard::to_allocvec(&index).map_err(|error| format!("index: {error}"))?;
    let index_len =
        u32::try_from(index_bytes.len()).map_err(|_| "index passes 4 GiB".to_owned())?;
    report.index_bytes = index_bytes.len();

    let display = out.display();
    let mut file = std::io::BufWriter::new(
        std::fs::File::create(out).map_err(|error| format!("{display}: {error}"))?,
    );
    file.write_all(SOUND_PACK_MAGIC)
        .and_then(|()| file.write_all(&index_len.to_le_bytes()))
        .and_then(|()| file.write_all(&index_bytes))
        .and_then(|()| file.write_all(&audio))
        .and_then(|()| file.flush())
        .map_err(|error| format!("{display}: {error}"))?;
    report.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    Ok(report)
}
