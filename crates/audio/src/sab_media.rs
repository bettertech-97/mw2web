use vfs::File;
use std::io::{Read, Seek, SeekFrom};

use asset_audio::{SabCodec, SabMediaSource};

use crate::decode_budget::{DecodeReservation, DecodeSamples};
use crate::media::{PcmBuffer, PcmError};
use crate::pcm::DecodeError;

/// Match-set preparation reads hundreds of clips from one pack, each a
/// synchronous round trip to the browser's VFS worker. The pack is baked with
/// those clips first, so while the set is being prepared the pack is read in
/// 1 MiB windows: a few dozen reads instead of ~600, and one contiguous
/// download on a cold client. Off again once the match is live, when single
/// clips play on demand and a window would over-fetch.
const WINDOW: u64 = 1 << 20;
const MAX_WINDOWS: usize = 64;

struct WindowCache {
    bulk: bool,
    windows: Vec<((std::path::PathBuf, u64), std::sync::Arc<Vec<u8>>)>,
}

static WINDOWS: std::sync::Mutex<WindowCache> = std::sync::Mutex::new(WindowCache {
    bulk: false,
    windows: Vec::new(),
});

pub(crate) fn set_bulk_reads(on: bool) {
    let on = on && std::env::var("IW4L_SOUND_WINDOWS").map_or(true, |value| value != "0");
    let mut cache = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    let was = cache.bulk;
    cache.bulk = on;
    if !on {
        cache.windows.clear();
    }
    drop(cache);
    if was && !on {
        warm_sound_pack();
    }
}

/// The IW4L sound pack the match set was read from; its other sounds play
/// on first use, so the rest of the file is read once in the background
/// after the set is ready (on the web that downloads it into the block
/// cache, so a sound heard for the first time mid-match decodes locally).
static PACK_PATH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
static PACK_WARMED: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

fn note_pack(bank: &std::path::Path) {
    if bank.extension().is_some_and(|ext| ext == asset_audio::SOUND_PACK_EXT) {
        let mut path = PACK_PATH.lock().unwrap_or_else(|e| e.into_inner());
        if path.as_deref() != Some(bank) {
            *path = Some(bank.to_path_buf());
        }
    }
}

fn warm_sound_pack() {
    let Some(path) = PACK_PATH.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return;
    };
    {
        let mut warmed = PACK_WARMED.lock().unwrap_or_else(|e| e.into_inner());
        if warmed.contains(&path) {
            return;
        }
        warmed.push(path.clone());
    }
    let spawned = web_threads::spawn_named("sound-pack-warm", move || {
        let started = web_time::Instant::now();
        let Ok(mut file) = File::open(&path) else {
            return;
        };
        let mut buffer = vec![0u8; WINDOW as usize];
        let mut total = 0u64;
        // Kept out of the map's download list: a new player should not wait
        // for the whole pack before the map starts, only for the match set.
        vfs::untracked_reads(|| loop {
            match file.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n as u64,
            }
        });
        diag::info!(
            Audio,
            "audio: sound pack `{}` read in the background ({:.1} MiB, {:.1}s)",
            path.display(),
            total as f64 / 1048576.0,
            started.elapsed().as_secs_f64()
        );
    });
    if let Err(error) = spawned {
        diag::warn!(Audio, "audio: sound pack warm-up not started ({error})");
    }
}

fn window_bytes(
    bank: &std::path::Path,
    index: u64,
) -> Result<std::sync::Arc<Vec<u8>>, DecodeError> {
    {
        let cache = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, bytes)) = cache
            .windows
            .iter()
            .find(|((path, window), _)| *window == index && path == bank)
        {
            return Ok(std::sync::Arc::clone(bytes));
        }
    }
    let mut file = File::open(bank).map_err(|_| DecodeError::Read)?;
    let len = file.metadata().map_err(|_| DecodeError::Read)?.len();
    let start = index * WINDOW;
    if start >= len {
        return Err(DecodeError::Read);
    }
    let take = (len - start).min(WINDOW) as usize;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(take)
        .map_err(|_| DecodeError::Pcm(PcmError::MemoryLimit))?;
    bytes.resize(take, 0);
    file.seek(SeekFrom::Start(start))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|_| DecodeError::Read)?;
    let bytes = std::sync::Arc::new(bytes);
    let mut cache = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
    if cache.bulk {
        if cache.windows.len() >= MAX_WINDOWS {
            cache.windows.remove(0);
        }
        cache
            .windows
            .push(((bank.to_path_buf(), index), std::sync::Arc::clone(&bytes)));
    }
    Ok(bytes)
}

/// `size` bytes of `bank` from `offset`: through the window cache while a
/// match set is being prepared, straight from the file otherwise.
fn read_range(bank: &std::path::Path, offset: u64, size: usize) -> Result<Vec<u8>, DecodeError> {
    note_pack(bank);
    let bulk = WINDOWS
        .lock()
        .map(|cache| cache.bulk)
        .unwrap_or(false);
    let end = offset
        .checked_add(size as u64)
        .ok_or(DecodeError::Read)?;
    if bulk && size as u64 <= WINDOW && size > 0 {
        let mut out = Vec::new();
        out.try_reserve_exact(size)
            .map_err(|_| DecodeError::Pcm(PcmError::MemoryLimit))?;
        for index in offset / WINDOW..=(end - 1) / WINDOW {
            let window = window_bytes(bank, index)?;
            let base = index * WINDOW;
            let from = offset.max(base) - base;
            let to = end.min(base + window.len() as u64) - base;
            let (from, to) = (from as usize, to as usize);
            if to < from || to > window.len() {
                return Err(DecodeError::Read);
            }
            out.extend_from_slice(&window[from..to]);
        }
        if out.len() != size {
            return Err(DecodeError::Read);
        }
        return Ok(out);
    }
    let mut file = File::open(bank).map_err(|_| DecodeError::Read)?;
    if end > file.metadata().map_err(|_| DecodeError::Read)?.len() {
        return Err(DecodeError::Read);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| DecodeError::Pcm(PcmError::MemoryLimit))?;
    bytes.resize(size, 0);
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|_| DecodeError::Read)?;
    Ok(bytes)
}

pub(crate) fn prepare(source: &SabMediaSource) -> Result<PcmBuffer, DecodeError> {
    let codec = source.codec();
    if let SabCodec::Unsupported(_) = codec {
        return Err(DecodeError::UnsupportedCodec(codec));
    }
    let channels = u16::from(source.entry.channels);
    let rate = source
        .entry
        .frame_rate()
        .ok_or(DecodeError::Pcm(PcmError::InvalidRate))?;
    let frames = source.entry.frame_count as usize;
    let mut samples =
        DecodeSamples::for_frames(frames, channels, rate).map_err(DecodeError::Pcm)?;
    let count = frames
        .checked_mul(usize::from(channels))
        .ok_or(DecodeError::Pcm(PcmError::MemoryLimit))?;
    let size = source.entry.size as usize;
    if codec == SabCodec::PcmS16 && count.checked_mul(size_of::<i16>()) != Some(size) {
        return Err(DecodeError::MetadataMismatch);
    }
    let _input = DecodeReservation::reserve(size).map_err(DecodeError::Pcm)?;
    let bytes = read_range(&source.bank, u64::from(source.entry.offset), size)?;
    match codec {
        SabCodec::PcmS16 => {
            let mut block = [0i16; 4096];
            for chunk in bytes.chunks(block.len() * size_of::<i16>()) {
                for (sample, bytes) in block.iter_mut().zip(chunk.as_chunks::<2>().0) {
                    *sample = i16::from_le_bytes(*bytes);
                }
                samples
                    .extend(&block[..chunk.len() / 2])
                    .map_err(DecodeError::Pcm)?;
            }
            samples.into_pcm(channels, rate).map_err(DecodeError::Pcm)
        }
        SabCodec::Flac => decode_flac(bytes, samples, channels, rate, frames),
        SabCodec::Unsupported(_) => Err(DecodeError::UnsupportedCodec(codec)),
    }
}

fn decode_flac(
    bytes: Vec<u8>,
    mut samples: DecodeSamples,
    channels: u16,
    rate: u32,
    frames: usize,
) -> Result<PcmBuffer, DecodeError> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_FLAC, DecoderOptions};
    use symphonia::core::errors::Error;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let options = MediaSourceStreamOptions::default();
    let scratch = bytes
        .len()
        .checked_add(options.buffer_len)
        .ok_or(DecodeError::Pcm(PcmError::MemoryLimit))?;
    let _reader = DecodeReservation::reserve(scratch).map_err(DecodeError::Pcm)?;
    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), options);
    let mut format = symphonia::default::get_probe()
        .format(
            Hint::new().with_extension("flac"),
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|_| DecodeError::Decode)?
        .format;
    let track = format.default_track().ok_or(DecodeError::Decode)?;
    let track_id = track.id;
    let params = &track.codec_params;
    if params.codec != CODEC_TYPE_FLAC
        || params.sample_rate != Some(rate)
        || params.channels.map(|channels| channels.count()) != Some(usize::from(channels))
        || params
            .n_frames
            .is_some_and(|native| native != frames as u64)
    {
        return Err(DecodeError::MetadataMismatch);
    }
    // FLAC STREAMINFO's maximum block size is a u16. Symphonia's FLAC decoder
    // allocates one planar i32 buffer for that block when constructed.
    const MAX_BLOCK_FRAMES: usize = u16::MAX as usize;
    let _decoder =
        DecodeReservation::reserve(MAX_BLOCK_FRAMES * usize::from(channels) * size_of::<i32>())
            .map_err(DecodeError::Pcm)?;
    let mut decoder = symphonia::default::get_codecs()
        .make(
            params,
            &DecoderOptions {
                verify: params.verification_check.is_some(),
            },
        )
        .map_err(|_| DecodeError::Decode)?;
    let mut decoded_frames = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(_) => return Err(DecodeError::Decode),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = decoder.decode(&packet).map_err(|_| DecodeError::Decode)?;
        if decoded.spec().rate != rate || decoded.spec().channels.count() != usize::from(channels) {
            return Err(DecodeError::MetadataMismatch);
        }
        decoded_frames = decoded_frames
            .checked_add(decoded.frames())
            .filter(|&count| count <= frames)
            .ok_or(DecodeError::MetadataMismatch)?;
        if decoded.capacity() > MAX_BLOCK_FRAMES {
            return Err(DecodeError::MetadataMismatch);
        }
        let scratch = decoded
            .capacity()
            .checked_mul(usize::from(channels))
            .and_then(|count| count.checked_mul(size_of::<i16>()))
            .ok_or(DecodeError::Pcm(PcmError::MemoryLimit))?;
        let _interleaved = DecodeReservation::reserve(scratch).map_err(DecodeError::Pcm)?;
        let mut interleaved = SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec());
        interleaved.copy_interleaved_ref(decoded);
        samples
            .extend(interleaved.samples())
            .map_err(DecodeError::Pcm)?;
    }
    if decoded_frames != frames {
        return Err(DecodeError::MetadataMismatch);
    }
    if decoder.finalize().verify_ok == Some(false) {
        return Err(DecodeError::Decode);
    }
    samples.into_pcm(channels, rate).map_err(DecodeError::Pcm)
}
