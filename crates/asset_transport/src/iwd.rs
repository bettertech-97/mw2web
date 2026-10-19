use vfs::PathExt as _;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};

#[derive(Clone, Debug)]
pub struct IwdFile {
    archive: PathBuf,
    entry: String,
    crc32: u32,
    size: u64,
    reader: Arc<Mutex<OpenArchive>>,
}

impl IwdFile {
    pub fn entry(&self) -> &str {
        &self.entry
    }

    pub fn crc32(&self) -> u32 {
        self.crc32
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn read(&self) -> Result<Vec<u8>, String> {
        ENTRY_PAYLOAD_READS.fetch_add(1, Ordering::Relaxed);
        read_indexed_entry(self, None)
    }

    /// Inflate at most `limit` bytes of the entry.
    ///
    /// A header is a fixed prefix, and deflate stops where the reader stops:
    /// asking whether an image is a cubemap costs the first block of it, not
    /// the megabyte behind it.
    pub fn read_header(&self, limit: usize) -> Result<Vec<u8>, String> {
        ENTRY_HEADER_READS.fetch_add(1, Ordering::Relaxed);
        read_indexed_entry(self, Some(limit))
    }
}

#[derive(Debug, Default)]
pub struct IwdIndex {
    images: HashMap<String, Vec<IwdFile>>,
    archives: usize,
}

type IndexKey = (PathBuf, u64);

fn index_cache() -> &'static RwLock<HashMap<IndexKey, Arc<IwdIndex>>> {
    static CACHE: OnceLock<RwLock<HashMap<IndexKey, Arc<IwdIndex>>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

type IndexBuild = Arc<OnceLock<Result<Arc<IwdIndex>, String>>>;

fn index_builds() -> &'static Mutex<HashMap<IndexKey, IndexBuild>> {
    static BUILDS: OnceLock<Mutex<HashMap<IndexKey, IndexBuild>>> = OnceLock::new();
    BUILDS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl IwdIndex {
    pub fn open(directory: &Path) -> Result<Arc<Self>, String> {
        let key = (directory.to_path_buf(), installation_revision(directory));
        if let Some(hit) = index_cache()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Ok(Arc::clone(hit));
        }

        let slot = {
            let mut builds = index_builds()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(builds.entry(key.clone()).or_default())
        };

        let built = slot
            .get_or_init(|| Self::open_uncached(directory).map(Arc::new))
            .clone();
        {
            let mut builds = index_builds()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if builds
                .get(&key)
                .is_some_and(|live| Arc::ptr_eq(live, &slot))
            {
                builds.remove(&key);
            }
        }
        let built = built?;
        let mut cache = index_cache()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|existing, _| existing.0 != key.0 || existing == &key);
        Ok(Arc::clone(
            cache.entry(key).or_insert_with(|| Arc::clone(&built)),
        ))
    }

    pub fn is_cached(directory: &Path) -> bool {
        index_cache()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&(directory.to_path_buf(), installation_revision(directory)))
    }

    pub fn archive_count(&self) -> usize {
        self.archives
    }

    pub fn image_candidates(&self, name: &str) -> Option<&[IwdFile]> {
        self.images
            .get(&name.to_ascii_lowercase())
            .map(Vec::as_slice)
    }

    fn open_uncached(directory: &Path) -> Result<Self, String> {
        let mut archives = vfs::read_dir(directory)
            .map_err(|error| format!("cannot read IWD directory {}: {error}", directory.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_iwd_archive(path))
            .collect::<Vec<_>>();
        archives.sort();
        // Texture packs for maps that never had a zone (`iw4l-maps/`, beside
        // `main/`): their images are looked up like any other.
        if let Some(packs) = directory.parent().map(|parent| parent.join("iw4l-maps"))
            && let Ok(entries) = vfs::read_dir(&packs)
        {
            let mut extra = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| is_iwd_archive(path))
                .collect::<Vec<_>>();
            extra.sort();
            archives.extend(extra);
        }

        type IndexedArchive = (usize, Result<Vec<(String, IwdFile)>, String>);
        let next = std::sync::atomic::AtomicUsize::new(0);
        let done: Mutex<Vec<IndexedArchive>> = Mutex::new(Vec::with_capacity(archives.len()));
        // The browser has one thread; index in order.
        #[cfg(target_arch = "wasm32")]
        for (slot, path) in archives.iter().enumerate() {
            let indexed = index_image_entries(path);
            done.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((slot, indexed));
        }
        #[cfg(target_arch = "wasm32")]
        let _ = &next;
        #[cfg(not(target_arch = "wasm32"))]
        let lanes = archives.len().min(index_lane_width());
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::scope(|scope| {
            for _ in 0..lanes {
                scope.spawn(|| {
                    loop {
                        let slot = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = archives.get(slot) else {
                            return;
                        };
                        let indexed = index_image_entries(path);
                        done.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((slot, indexed));
                    }
                });
            }
        });
        let mut done = done
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        done.sort_by_key(|(slot, _)| *slot);
        let per_archive = done
            .into_iter()
            .map(|(_, indexed)| indexed)
            .collect::<Result<Vec<_>, String>>()?;

        let mut images: HashMap<String, Vec<IwdFile>> = HashMap::new();
        for entries in per_archive {
            for (name, file) in entries {
                images.entry(name).or_default().push(file);
            }
        }
        Ok(Self {
            images,
            archives: archives.len(),
        })
    }
}

/// macOS writes an AppleDouble `._<name>` beside every file it copies onto
/// exFAT, FAT or a network share. `._iw_00.iwd` is not a zip, and one of them
/// in `main/` would fail the whole index.
fn is_iwd_archive(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("iwd"))
        && !path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b"._"))
}

fn index_image_entries(path: &Path) -> Result<Vec<(String, IwdFile)>, String> {
    let file = vfs::File::open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|error| format!("cannot index {}: {error}", path.display()))?;
    let records = central_records(path, archive.central_directory_start())
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let reader = Arc::new(Mutex::new(archive));
    let mut entries = Vec::new();
    for record in records {
        let exact = record.name;
        let normalized = exact.replace('\\', "/");
        let Some(name) = normalized
            .strip_prefix("images/")
            .and_then(|name| name.strip_suffix(".iwi"))
        else {
            continue;
        };
        if name.is_empty() || name.contains('/') {
            continue;
        }
        entries.push((
            name.to_ascii_lowercase(),
            IwdFile {
                archive: path.to_path_buf(),
                entry: exact,
                crc32: record.crc32,
                size: record.size,
                reader: reader.clone(),
            },
        ));
    }
    Ok(entries)
}

struct CentralRecord {
    name: String,
    crc32: u32,
    size: u64,
}

/// Name, CRC-32 and uncompressed size of every central-directory record, from
/// one read of the directory. `ZipArchive::by_index` would read each entry's
/// local header instead: a seek per entry across the whole archive, which in
/// the browser is a round trip per entry. Names decode as the zip crate's do
/// for ASCII, which is all IWD archives contain.
fn central_records(path: &Path, start: u64) -> std::io::Result<Vec<CentralRecord>> {
    use std::io::{Read, Seek, SeekFrom};
    const SIGNATURE: u32 = 0x0201_4b50;
    const HEADER: usize = 46;
    let mut file = vfs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
    let u32_at = |at: usize| {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let mut records = Vec::new();
    let mut at = 0;
    while at + HEADER <= bytes.len() && u32_at(at) == SIGNATURE {
        let crc32 = u32_at(at + 16);
        let mut size = u64::from(u32_at(at + 24));
        let name_len = usize::from(u16_at(at + 28));
        let extra_len = usize::from(u16_at(at + 30));
        let comment_len = usize::from(u16_at(at + 32));
        let name_at = at + HEADER;
        let extra_at = name_at + name_len;
        let next = extra_at + extra_len + comment_len;
        if next > bytes.len() {
            break;
        }
        // ZIP64: the real size is the first field of the 0x0001 extra block.
        if size == u64::from(u32::MAX) {
            let mut field = extra_at;
            while field + 4 <= extra_at + extra_len {
                let id = u16_at(field);
                let len = usize::from(u16_at(field + 2));
                if id == 0x0001 && len >= 8 {
                    let mut wide = [0; 8];
                    wide.copy_from_slice(&bytes[field + 4..field + 12]);
                    size = u64::from_le_bytes(wide);
                    break;
                }
                field += 4 + len;
            }
        }
        records.push(CentralRecord {
            name: String::from_utf8_lossy(&bytes[name_at..extra_at]).into_owned(),
            crc32,
            size,
        });
        at = next;
    }
    Ok(records)
}

type OpenArchive = zip::ZipArchive<std::io::BufReader<vfs::File>>;

static PARKED: OnceLock<Mutex<HashMap<PathBuf, Vec<OpenArchive>>>> = OnceLock::new();

const PARKED_LIMIT: usize = 256;

static PARKED_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn index_lane_width() -> usize {
    static WIDTH: OnceLock<usize> = OnceLock::new();
    *WIDTH.get_or_init(|| {
        std::env::var("IW4L_IWD_INDEX_LANES")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|lanes| *lanes > 0)
            .unwrap_or(4)
    })
}

fn parked() -> &'static Mutex<HashMap<PathBuf, Vec<OpenArchive>>> {
    PARKED.get_or_init(|| Mutex::new(HashMap::new()))
}

struct ArchiveLease {
    path: PathBuf,
    archive: Option<OpenArchive>,
}

impl ArchiveLease {
    fn take(path: &Path) -> Result<Self, String> {
        let parked = parked()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(path)
            .and_then(Vec::pop);
        if let Some(archive) = parked {
            PARKED_N.fetch_sub(1, Ordering::Relaxed);
            return Ok(Self {
                path: path.to_path_buf(),
                archive: Some(archive),
            });
        }
        let opened_at = web_time::Instant::now();
        let disk = vfs::File::open(path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        let archive = zip::ZipArchive::new(std::io::BufReader::new(disk))
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        IWD_DIRECTORY_NS.fetch_add(opened_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
        IWD_DIRECTORY_OPENS.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            path: path.to_path_buf(),
            archive: Some(archive),
        })
    }
}

impl Drop for ArchiveLease {
    fn drop(&mut self) {
        let Some(archive) = self.archive.take() else {
            return;
        };

        if PARKED_N.fetch_add(1, Ordering::Relaxed) >= PARKED_LIMIT {
            PARKED_N.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        parked()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(self.path.clone())
            .or_default()
            .push(archive);
    }
}

static ENTRY_PAYLOAD_READS: AtomicU64 = AtomicU64::new(0);
static ENTRY_HEADER_READS: AtomicU64 = AtomicU64::new(0);

static IWD_DIRECTORY_NS: AtomicU64 = AtomicU64::new(0);
static IWD_DIRECTORY_OPENS: AtomicU64 = AtomicU64::new(0);
static IWD_INFLATE_NS: AtomicU64 = AtomicU64::new(0);

fn read_indexed_entry(file: &IwdFile, limit: Option<usize>) -> Result<Vec<u8>, String> {
    let mut archive = file
        .reader
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let inflate_at = web_time::Instant::now();
    // The local header and the deflated data follow each other: ask for both
    // at once (the data is no larger than the entry, plus header slack). A
    // bounded read wants only the deflated prefix behind its limit.
    let wanted = limit.map_or(file.size, |limit| file.size.min(limit as u64));
    vfs::read_hint(wanted + 1024);
    let mut entry = archive.by_name(&file.entry).map_err(|error| {
        format!(
            "cannot read {} in {}: {error}",
            file.entry,
            file.archive.display()
        )
    })?;
    if entry.crc32() != file.crc32 || entry.size() != file.size {
        return Err("archive changed during its publication lifetime".into());
    }
    let bytes = read_entry_bytes(&mut entry, limit)?;
    IWD_INFLATE_NS.fetch_add(inflate_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
    Ok(bytes)
}

fn read_pooled_entry(
    archive_path: &Path,
    entry_name: &str,
    limit: Option<usize>,
) -> Result<Vec<u8>, String> {
    let mut lease = ArchiveLease::take(archive_path)?;
    let archive = lease
        .archive
        .as_mut()
        .expect("lease holds its reader until drop");
    let inflate_at = web_time::Instant::now();
    let mut entry = archive.by_name(entry_name).map_err(|error| {
        format!("cannot open IWD entry {entry_name} in {archive_path:?}: {error}")
    })?;
    vfs::read_hint(entry.compressed_size());
    let bytes = read_entry_bytes(&mut entry, limit).map_err(|error| {
        format!("cannot read IWD entry {entry_name} in {archive_path:?}: {error}")
    })?;
    IWD_INFLATE_NS.fetch_add(inflate_at.elapsed().as_nanos() as u64, Ordering::Relaxed);
    Ok(bytes)
}

fn read_entry_bytes<R: Read>(
    entry: &mut zip::read::ZipFile<'_, R>,
    limit: Option<usize>,
) -> Result<Vec<u8>, String> {
    let size = entry.size();
    // Bound a prefix before converting the archive's u64 size to usize.
    let want = limit.map_or(size, |limit| size.min(limit as u64));
    let capacity = usize::try_from(want)
        .map_err(|_| format!("IWD entry size {want} does not fit in memory"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|error| format!("cannot allocate {want} bytes for IWD entry: {error}"))?;
    entry
        .by_ref()
        .take(want)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() != capacity {
        return Err(format!(
            "IWD entry length mismatch: expected {want} bytes, read {}",
            bytes.len()
        ));
    }
    if want == size {
        // Take stops at its limit without asking zip for EOF. Probe once to
        // validate the CRC and reject extra data without growing the buffer.
        let mut extra = [0u8; 1];
        loop {
            match entry.read(&mut extra) {
                Ok(0) => break,
                Ok(_) => {
                    return Err(format!(
                        "IWD entry length mismatch: expected {want} bytes, entry has additional data"
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    Ok(bytes)
}

pub fn cached_iwd_dirs() -> Vec<PathBuf> {
    let mut dirs = index_cache()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Entries inflated whole, and entries inflated only far enough to read a
/// header.
pub fn iwd_entry_reads() -> (u64, u64) {
    (
        ENTRY_PAYLOAD_READS.load(Ordering::Relaxed),
        ENTRY_HEADER_READS.load(Ordering::Relaxed),
    )
}

pub fn iwd_read_cost() -> (f64, u64, f64) {
    (
        IWD_DIRECTORY_NS.load(Ordering::Relaxed) as f64 / 1.0e6,
        IWD_DIRECTORY_OPENS.load(Ordering::Relaxed),
        IWD_INFLATE_NS.load(Ordering::Relaxed) as f64 / 1.0e6,
    )
}

pub fn game_main_for_zone(zone_ff: &Path) -> Result<PathBuf, String> {
    for ancestor in zone_ff.ancestors() {
        let main = ancestor.join("main");
        if main.vfs_is_dir() {
            return Ok(main);
        }
    }
    Err(format!(
        "no game tree with a `main/` directory above {}",
        zone_ff.display()
    ))
}

pub fn game_mains_by_root(games_root: &Path) -> Vec<Vec<PathBuf>> {
    crate::discover::search_roots(games_root)
        .into_iter()
        .map(|root| {
            let mut mains = Vec::new();
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                let main = dir.join("main");
                if main.vfs_is_dir() {
                    mains.push(main);
                }
                if let Ok(entries) = vfs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.vfs_is_dir() {
                            stack.push(path);
                        }
                    }
                }
            }
            mains.sort();
            mains.dedup();
            mains
        })
        .collect()
}

pub fn game_mains_under(games_root: &Path) -> Vec<PathBuf> {
    let mut mains: Vec<PathBuf> = Vec::new();
    for main in game_mains_by_root(games_root).into_iter().flatten() {
        if !mains.contains(&main) {
            mains.push(main);
        }
    }
    mains
}

pub fn read_iwd_named(games_root: &Path, want: &str) -> Option<Vec<u8>> {
    let want = want.replace('\\', "/");
    let mut stack: Vec<_> = crate::discover::search_roots(games_root)
        .into_iter()
        .rev()
        .collect();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = vfs::read_dir(&dir) else {
            continue;
        };
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if path.vfs_is_dir() {
                stack.push(path);
                continue;
            }
            if !is_iwd_archive(&path) {
                continue;
            }
            let Ok(file) = vfs::File::open(&path) else {
                continue;
            };
            let Ok(mut archive) = zip::ZipArchive::new(file) else {
                continue;
            };
            for index in 0..archive.len() {
                // Match on the central-directory name; open only the hit.
                if !archive
                    .name_for_index(index)
                    .is_some_and(|name| name.replace('\\', "/").eq_ignore_ascii_case(&want))
                {
                    continue;
                }
                let Ok(mut entry) = archive.by_index(index) else {
                    continue;
                };
                if let Ok(bytes) = read_entry_bytes(&mut entry, None) {
                    return Some(bytes);
                }
            }
        }
    }
    None
}

pub fn read_text(path: &Path) -> Result<String, String> {
    vfs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))
}

pub fn inflate_zlib(data: &[u8]) -> Result<Vec<u8>, String> {
    miniz_oxide::inflate::decompress_to_vec_zlib(data)
        .map_err(|error| format!("zlib inflate: {error:?}"))
}

#[derive(Debug, Default, Clone)]
pub struct IwdSoundIndex {
    dir: PathBuf,

    sounds: HashMap<String, (PathBuf, String)>,
}

fn sound_key(relative: &str) -> String {
    relative.replace('\\', "/").to_ascii_lowercase()
}

fn sound_rel_from_zip_name(name: &str, language: Option<&str>) -> Option<String> {
    let lower = name.replace('\\', "/").to_ascii_lowercase();
    let lower = language
        .and_then(|language| lower.strip_prefix(language)?.strip_prefix('/'))
        .unwrap_or(&lower);
    let rel = lower.strip_prefix("sound/")?;
    if rel.is_empty() {
        return None;
    }
    Some(rel.to_owned())
}

fn archive_language(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
    let (language, _) = stem.strip_prefix("localized_")?.rsplit_once('_')?;
    Some(language.to_owned())
}

impl IwdSoundIndex {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, String> {
        let directory = directory.as_ref();
        let mut archives = vfs::read_dir(directory)
            .map_err(|e| format!("cannot read {}: {e}", directory.display()))?
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| is_iwd_archive(p))
            .collect::<Vec<_>>();
        archives.sort();

        let mut sounds = HashMap::new();
        for path in archives {
            let file = vfs::File::open(&path)
                .map_err(|e| format!("cannot open {}: {e}", path.display()))?;

            let archive = zip::ZipArchive::new(std::io::BufReader::new(file))
                .map_err(|e| format!("zip {}: {e}", path.display()))?;
            let language = archive_language(&path);
            for name in archive.file_names() {
                let Some(rel) = sound_rel_from_zip_name(name, language.as_deref()) else {
                    continue;
                };
                sounds
                    .entry(rel)
                    .or_insert_with(|| (path.clone(), name.to_owned()));
            }
        }
        Ok(Self {
            dir: directory.to_path_buf(),
            sounds,
        })
    }

    pub fn indexed_dir(&self) -> &Path {
        &self.dir
    }

    pub fn sound_count(&self) -> usize {
        self.sounds.len()
    }

    pub fn read_sound(&self, relative: &str) -> Option<Result<Vec<u8>, String>> {
        let (archive, entry) = self.sounds.get(&sound_key(relative))?;
        Some(read_pooled_entry(archive, entry, None))
    }

    /// Where `relative` sits in its archive: the archive path and the byte
    /// range covering the entry's local header and compressed data, so the
    /// range can be brought into memory before `read_sound` touches it.
    pub fn sound_span(&self, relative: &str) -> Option<(PathBuf, u64, u64)> {
        let (archive, entry) = self.sounds.get(&sound_key(relative))?;
        let spans = central_directory_spans(archive)?;
        let (header_start, compressed) = *spans.get(entry.as_str())?;
        // The local header repeats the name and may carry its own extra
        // field; a little slack past the central directory's figures covers
        // both without a read.
        let len = 30 + entry.len() as u64 + 256 + compressed;
        Some((archive.clone(), header_start, len))
    }
}

/// Entry name → (local header offset, compressed size), read once per
/// archive from the central directory, which is a few hundred KiB at the
/// end of the file and already cached from opening the archive. ZIP64
/// archives are not handled; a `None` just means nothing is warmed.
fn central_directory_spans(archive: &Path) -> Option<Arc<HashMap<String, (u64, u64)>>> {
    static SPANS: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<HashMap<String, (u64, u64)>>>>>> =
        OnceLock::new();
    let cache = SPANS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(archive)
    {
        return known.clone();
    }
    let parsed = parse_central_directory(archive).map(Arc::new);
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(archive.to_path_buf(), parsed.clone());
    parsed
}

fn parse_central_directory(archive: &Path) -> Option<HashMap<String, (u64, u64)>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = vfs::File::open(archive).ok()?;
    let size = file.metadata().ok()?.len();
    // The end-of-central-directory record is in the last 64 KiB + 22 bytes.
    let tail_len = size.min(65_557);
    file.seek(SeekFrom::Start(size - tail_len)).ok()?;
    let mut tail = vec![0u8; tail_len as usize];
    file.read_exact(&mut tail).ok()?;
    let eocd = tail
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x05, 0x06])?;
    let u16_at = |b: &[u8], at: usize| u16::from_le_bytes([b[at], b[at + 1]]);
    let u32_at = |b: &[u8], at: usize| {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    };
    let rec = &tail[eocd..];
    if rec.len() < 22 {
        return None;
    }
    let entries = u16_at(rec, 10) as usize;
    let cd_size = u32_at(rec, 12) as u64;
    let cd_offset = u32_at(rec, 16) as u64;
    if entries == 0xffff || cd_size == 0xffff_ffff || cd_offset == 0xffff_ffff {
        return None;
    }
    file.seek(SeekFrom::Start(cd_offset)).ok()?;
    let mut cd = vec![0u8; usize::try_from(cd_size).ok()?];
    file.read_exact(&mut cd).ok()?;
    let mut spans = HashMap::with_capacity(entries);
    let mut at = 0usize;
    for _ in 0..entries {
        if at + 46 > cd.len() || cd[at..at + 4] != [0x50, 0x4b, 0x01, 0x02] {
            break;
        }
        let compressed = u32_at(&cd, at + 20) as u64;
        let name_len = u16_at(&cd, at + 28) as usize;
        let extra_len = u16_at(&cd, at + 30) as usize;
        let comment_len = u16_at(&cd, at + 32) as usize;
        let header = u32_at(&cd, at + 42) as u64;
        let name_end = at + 46 + name_len;
        if name_end > cd.len() {
            break;
        }
        if compressed != 0xffff_ffff && header != 0xffff_ffff {
            let name = String::from_utf8_lossy(&cd[at + 46..name_end]).into_owned();
            spans.insert(name, (header, compressed));
        }
        at = name_end + extra_len + comment_len;
    }
    Some(spans)
}

pub fn installation_revision(root: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    fn visit(
        path: &Path,
        seen: &mut std::collections::HashSet<PathBuf>,
        rows: &mut Vec<(PathBuf, u64)>,
    ) {
        let canonical = path.vfs_canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !seen.insert(canonical) {
            return;
        }
        let Ok(entries) = vfs::read_dir(path) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
        paths.sort();
        for path in paths {
            let Ok(meta) = vfs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                visit(&path, seen, rows);
            } else if meta.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| matches!(ext.to_str(), Some("ff" | "iwd" | "ipak" | "iwi")))
            {
                rows.push((path.clone(), file_revision(&path)));
            }
        }
    }
    let mut rows = Vec::new();
    visit(root, &mut std::collections::HashSet::new(), &mut rows);
    rows.sort();
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    rows.hash(&mut hash);
    hash.finish()
}

pub(crate) fn file_revision(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    if let Ok(meta) = vfs::metadata(path) {
        meta.len().hash(&mut hash);
        meta.modified().ok().hash(&mut hash);
        meta.created().ok().hash(&mut hash);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()).hash(&mut hash);
        }
    }
    hash.finish()
}
