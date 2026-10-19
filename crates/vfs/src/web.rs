//! Browser file system: one tree of directories, mounted files and memory
//! files, keyed by lower-cased absolute path (the games were authored on a
//! case-insensitive file system and the engine spells paths both ways).
//!
//! Mounted files are the player's games folder, read on demand through
//! `globalThis.iw4lVfs` (see `web/vfs.js`): the page answers each read
//! synchronously from a worker, so nothing is copied up front. Memory files
//! are everything the engine writes.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = mountRoot)]
    fn js_mount_root() -> String;
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = entryCount)]
    fn js_entry_count() -> u32;
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = entryPath)]
    fn js_entry_path(index: u32) -> String;
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = entrySize)]
    fn js_entry_size(index: u32) -> f64;
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = entryModified)]
    fn js_entry_modified(index: u32) -> f64;
    /// Fills `dst` from byte `offset` of entry `index`; returns bytes read.
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = read)]
    fn js_read(index: u32, offset: f64, dst: &mut [u8]) -> u32;
    /// Brings a byte range of entry `index` into the block cache in the
    /// background; `true` once it is there. Older pages lack it.
    #[wasm_bindgen(catch, js_namespace = iw4lVfs, js_name = warm)]
    fn js_warm(index: u32, offset: f64, len: f64) -> Result<bool, JsValue>;

    // The persistent cache (origin-private storage, see web/vfs-worker.js).
    // Older pages lack it; `catch` turns the missing function into "no cache".
    #[wasm_bindgen(catch, js_namespace = iw4lVfs, js_name = cacheCount)]
    fn js_cache_count() -> Result<u32, JsValue>;
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = cachePath)]
    fn js_cache_path(index: u32) -> String;
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = cacheSize)]
    fn js_cache_size(index: u32) -> f64;
    #[wasm_bindgen(js_namespace = iw4lVfs, js_name = cacheRead)]
    fn js_cache_read(path: &str, offset: f64, dst: &mut [u8]) -> u32;
    #[wasm_bindgen(catch, js_namespace = iw4lVfs, js_name = cacheWrite)]
    fn js_cache_write(path: &str, src: &[u8]) -> Result<bool, JsValue>;
}

/// Engine-written files under here outlive the tab: they are saved to the
/// browser's private storage and listed again on the next load.
const PERSISTENT_ROOT: &str = "/iw4l-artifacts/cache";
/// The player's profile (account, classes) lives here; the page keeps these
/// too, and on a server with accounts, there as well. Their cache-relative
/// names start with `profile/`.
const PROFILE_ROOT: &str = "/iw4l-artifacts/profile";
const PROFILE_FILES: [&str; 4] = ["account.dat", "classes.txt", "camos.txt", "settings.cfg"];
/// Only the small derived caches are kept; decoded image and audio caches are
/// as large as their sources.
const PERSISTENT_KINDS: [&str; 3] = ["wgsl", "nav", "localize"];
/// Persisted cache files are mounted read-only like game files, with this bit
/// in their index; the low bits index [`persisted`].
const PERSISTED: u32 = 1 << 31;
/// Set on a read's entry word when the reading thread asked for its reads
/// to stay out of the map's download list (vfs-worker.js `UNTRACKED`).
const UNTRACKED: u32 = 1 << 30;

fn persisted() -> &'static Mutex<Vec<String>> {
    static PATHS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    PATHS.get_or_init(|| Mutex::new(Vec::new()))
}

/// The cache-relative path ("wgsl/ab/abcd") when `display` is a file of a
/// persistent kind.
fn persistent_rel(display: &Path) -> Option<String> {
    if let Ok(rel) = display.strip_prefix(PROFILE_ROOT) {
        let name = rel.to_string_lossy().replace('\\', "/");
        return PROFILE_FILES
            .contains(&name.as_str())
            .then(|| format!("profile/{name}"));
    }
    let rel = display.strip_prefix(PERSISTENT_ROOT).ok()?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    let kind = rel.split('/').next()?;
    PERSISTENT_KINDS.contains(&kind).then_some(rel)
}

/// Where a cache-relative name is mounted.
fn persistent_display(rel: &str) -> PathBuf {
    match rel.strip_prefix("profile/") {
        Some(name) => normalize(&Path::new(PROFILE_ROOT).join(name)),
        None => normalize(&Path::new(PERSISTENT_ROOT).join(rel)),
    }
}

/// Small reads (zip headers, chunk tables) are batched into one page round
/// trip: [`FIRST_READ_AHEAD`] after a seek, doubling up to [`READ_AHEAD`] while
/// the reader keeps going forward.
const FIRST_READ_AHEAD: usize = 16 * 1024;
const READ_AHEAD: usize = 1024 * 1024;
/// Largest single fetch a [`crate::read_hint`] can ask for.
const MAX_HINTED_READ: usize = 16 * 1024 * 1024;

thread_local! {
    // Per thread: with several load threads reading at once, one thread's
    // hint must not size another's read.
    static READ_HINT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static READS_UNTRACKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn set_untracked(on: bool) -> bool {
    READS_UNTRACKED.with(|flag| flag.replace(on))
}

pub(crate) fn set_read_hint(bytes: u64) {
    READ_HINT.with(|hint| hint.set(bytes));
}

/// Bytes of every mounted file kept after the first read; see [`head_of`].
const HEAD: usize = 64 * 1024;

#[derive(Clone)]
enum Node {
    Dir { children: BTreeMap<String, String> },
    Mounted { index: u32, size: u64 },
    Memory { data: Arc<Mutex<Vec<u8>>> },
}

#[derive(Clone)]
struct Entry {
    display: PathBuf,
    node: Node,
    modified: SystemTime,
}

struct Tree {
    entries: HashMap<String, Entry>,
}

fn now() -> SystemTime {
    crate::system_now()
}

pub(crate) fn warm(path: &Path, offset: u64, len: u64) -> bool {
    let index = match tree().get(&normalize(path)) {
        Some(Entry {
            node: Node::Mounted { index, .. },
            ..
        }) => *index,
        // Memory files and persisted caches are already in the page.
        Some(_) => return true,
        None => return true,
    };
    js_warm(index, offset as f64, len as f64).unwrap_or(true)
}

fn tree() -> MutexGuard<'static, Tree> {
    static TREE: OnceLock<Mutex<Tree>> = OnceLock::new();
    TREE.get_or_init(|| Mutex::new(Tree::mounted()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Absolute, `.`/`..`-free display path. Relative paths resolve against `/`,
/// the browser's only working directory.
fn normalize(path: &Path) -> PathBuf {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = PathBuf::from("/");
    for component in Path::new(&text).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}

fn key_of(display: &Path) -> String {
    display.to_string_lossy().to_lowercase()
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: no such file or directory", path.display()),
    )
}

fn read_only(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("{}: the games folder is read-only", path.display()),
    )
}

impl Tree {
    fn mounted() -> Self {
        let mut tree = Tree {
            entries: HashMap::new(),
        };
        tree.entries.insert(
            "/".into(),
            Entry {
                display: PathBuf::from("/"),
                node: Node::Dir {
                    children: BTreeMap::new(),
                },
                modified: UNIX_EPOCH,
            },
        );
        let root = normalize(Path::new(&js_mount_root()));
        tree.ensure_dirs(&root);
        for index in 0..js_entry_count() {
            let display = normalize(&root.join(js_entry_path(index)));
            if let Some(parent) = display.parent() {
                tree.ensure_dirs(parent);
            }
            let modified = UNIX_EPOCH + Duration::from_millis(js_entry_modified(index) as u64);
            tree.insert(
                display,
                Node::Mounted {
                    index,
                    size: js_entry_size(index) as u64,
                },
                modified,
            );
        }
        let cached = js_cache_count().unwrap_or(0);
        let mut paths = persisted().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for i in 0..cached {
            let rel = js_cache_path(i);
            let display = persistent_display(&rel);
            if persistent_rel(&display).is_none() {
                continue;
            }
            if let Some(parent) = display.parent() {
                tree.ensure_dirs(parent);
            }
            let index = PERSISTED | paths.len() as u32;
            paths.push(rel);
            tree.insert(
                display,
                Node::Mounted {
                    index,
                    size: js_cache_size(i) as u64,
                },
                UNIX_EPOCH,
            );
        }
        tree
    }

    fn get(&self, display: &Path) -> Option<&Entry> {
        self.entries.get(&key_of(display))
    }

    fn insert(&mut self, display: PathBuf, node: Node, modified: SystemTime) {
        if let Some(parent) = display.parent()
            && let Some(name) = display.file_name()
            && let Some(Entry {
                node: Node::Dir { children },
                ..
            }) = self.entries.get_mut(&key_of(parent))
        {
            let name = name.to_string_lossy().into_owned();
            children.insert(name.to_lowercase(), name);
        }
        self.entries.insert(
            key_of(&display),
            Entry {
                display,
                node,
                modified,
            },
        );
    }

    fn remove(&mut self, display: &Path) -> Option<Entry> {
        let removed = self.entries.remove(&key_of(display))?;
        if let Some(parent) = display.parent()
            && let Some(name) = display.file_name()
            && let Some(Entry {
                node: Node::Dir { children },
                ..
            }) = self.entries.get_mut(&key_of(parent))
        {
            children.remove(&name.to_string_lossy().to_lowercase());
        }
        Some(removed)
    }

    fn ensure_dirs(&mut self, display: &Path) {
        let mut at = PathBuf::from("/");
        for component in display.components() {
            if let Component::Normal(part) = component {
                at.push(part);
                if self.get(&at).is_none() {
                    self.insert(
                        at.clone(),
                        Node::Dir {
                            children: BTreeMap::new(),
                        },
                        now(),
                    );
                }
            }
        }
    }

    fn parent_is_dir(&self, display: &Path) -> bool {
        display.parent().is_none_or(|parent| {
            matches!(
                self.get(parent),
                Some(Entry {
                    node: Node::Dir { .. },
                    ..
                })
            )
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType {
    dir: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.dir
    }

    pub fn is_file(&self) -> bool {
        !self.dir
    }

    pub fn is_symlink(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub struct Metadata {
    file_type: FileType,
    len: u64,
    modified: SystemTime,
}

impl Metadata {
    fn of(entry: &Entry) -> Self {
        let len = match &entry.node {
            Node::Dir { .. } => 0,
            Node::Mounted { size, .. } => *size,
            Node::Memory { data } => data.lock().map_or(0, |data| data.len() as u64),
        };
        Self {
            file_type: FileType {
                dir: matches!(entry.node, Node::Dir { .. }),
            },
            len,
            modified: entry.modified,
        }
    }

    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    pub fn is_dir(&self) -> bool {
        self.file_type.dir
    }

    pub fn is_file(&self) -> bool {
        !self.file_type.dir
    }

    pub fn is_symlink(&self) -> bool {
        false
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
}

pub fn metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    let display = normalize(path.as_ref());
    tree()
        .get(&display)
        .map(Metadata::of)
        .ok_or_else(|| not_found(path.as_ref()))
}

pub fn symlink_metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    metadata(path)
}

pub fn canonicalize<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
    let display = normalize(path.as_ref());
    tree()
        .get(&display)
        .map(|entry| entry.display.clone())
        .ok_or_else(|| not_found(path.as_ref()))
}

pub struct DirEntry {
    path: PathBuf,
    metadata: Metadata,
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> OsString {
        self.path.file_name().unwrap_or_default().to_owned()
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        Ok(self.metadata.clone())
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.metadata.file_type)
    }
}

pub struct ReadDir(std::vec::IntoIter<io::Result<DirEntry>>);

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

pub fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
    let display = normalize(path.as_ref());
    let tree = tree();
    let Some(Entry {
        node: Node::Dir { children },
        display: dir_display,
        ..
    }) = tree.get(&display)
    else {
        return Err(not_found(path.as_ref()));
    };
    // Children carry the caller's spelling of the directory, as std's do.
    let base = path.as_ref().to_path_buf();
    let mut entries = Vec::with_capacity(children.len());
    for name in children.values() {
        if let Some(child) = tree.get(&dir_display.join(name)) {
            entries.push(Ok(DirEntry {
                path: base.join(name),
                metadata: Metadata::of(child),
            }));
        }
    }
    Ok(ReadDir(entries.into_iter()))
}

pub fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let display = normalize(path.as_ref());
    let mut tree = tree();
    if let Some(entry) = tree.get(&display) {
        return if matches!(entry.node, Node::Dir { .. }) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: not a directory", path.as_ref().display()),
            ))
        };
    }
    tree.ensure_dirs(&display);
    Ok(())
}

pub fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let display = normalize(path.as_ref());
    let mut tree = tree();
    if tree.get(&display).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{}: exists", path.as_ref().display()),
        ));
    }
    if !tree.parent_is_dir(&display) {
        return Err(not_found(path.as_ref()));
    }
    tree.insert(
        display,
        Node::Dir {
            children: BTreeMap::new(),
        },
        now(),
    );
    Ok(())
}

fn remove_entry(path: &Path, want_dir: bool, recursive: bool) -> io::Result<()> {
    let display = normalize(path);
    let mut tree = tree();
    let Some(entry) = tree.get(&display).cloned() else {
        return Err(not_found(path));
    };
    match (&entry.node, want_dir) {
        (Node::Mounted { .. }, _) => return Err(read_only(path)),
        (Node::Dir { children }, true) => {
            if !children.is_empty() && !recursive {
                return Err(io::Error::new(
                    io::ErrorKind::DirectoryNotEmpty,
                    format!("{}: directory not empty", path.display()),
                ));
            }
            let prefix = format!("{}/", key_of(&display));
            if tree.entries.keys().any(|key| {
                key.starts_with(&prefix)
                    && matches!(tree.entries[key].node, Node::Mounted { .. })
            }) {
                return Err(read_only(path));
            }
            tree.entries.retain(|key, _| !key.starts_with(&prefix));
        }
        (Node::Dir { .. }, false) => {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{}: is a directory", path.display()),
            ));
        }
        (Node::Memory { .. }, true) => {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{}: not a directory", path.display()),
            ));
        }
        (Node::Memory { .. }, false) => {}
    }
    tree.remove(&display);
    Ok(())
}

pub fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    remove_entry(path.as_ref(), false, false)
}

pub fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    remove_entry(path.as_ref(), true, false)
}

pub fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    remove_entry(path.as_ref(), true, true)
}

pub fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    let source = normalize(from.as_ref());
    let target = normalize(to.as_ref());
    let mut tree = tree();
    let Some(entry) = tree.get(&source).cloned() else {
        return Err(not_found(from.as_ref()));
    };
    match entry.node {
        Node::Memory { .. } => {}
        Node::Mounted { .. } => return Err(read_only(from.as_ref())),
        Node::Dir { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "renaming directories is not supported in the browser",
            ));
        }
    }
    let replaces_persisted = matches!(
        tree.get(&target).map(|entry| &entry.node),
        Some(Node::Mounted { index, .. }) if index & PERSISTED != 0
    );
    if !replaces_persisted
        && matches!(
            tree.get(&target).map(|entry| &entry.node),
            Some(Node::Mounted { .. } | Node::Dir { .. })
        )
    {
        return Err(read_only(to.as_ref()));
    }
    if !tree.parent_is_dir(&target) {
        return Err(not_found(to.as_ref()));
    }
    // A finished cache file (written to a temporary name, then moved into
    // place) is saved for the next load. Failing to save only loses the cache.
    if let (Some(rel), Node::Memory { data }) = (persistent_rel(&target), &entry.node) {
        let bytes = data.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = js_cache_write(&rel, &bytes);
    }
    tree.remove(&source);
    tree.remove(&target);
    tree.insert(target, entry.node, entry.modified);
    Ok(())
}

/// Memory files share their bytes, as a hard link shares an inode.
pub fn hard_link<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> io::Result<()> {
    let source = normalize(original.as_ref());
    let target = normalize(link.as_ref());
    let mut tree = tree();
    let Some(entry) = tree.get(&source).cloned() else {
        return Err(not_found(original.as_ref()));
    };
    if matches!(entry.node, Node::Dir { .. }) {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            format!("{}: is a directory", original.as_ref().display()),
        ));
    }
    if tree.get(&target).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{}: exists", link.as_ref().display()),
        ));
    }
    if !tree.parent_is_dir(&target) {
        return Err(not_found(link.as_ref()));
    }
    // The first copy of a profile file is published by a link (see
    // write_atomic in console/local_account.rs); keep it like a rename.
    if let (Some(rel), Node::Memory { data }) = (persistent_rel(&target), &entry.node) {
        let bytes = data.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = js_cache_write(&rel, &bytes);
    }
    tree.insert(target, entry.node, entry.modified);
    Ok(())
}

pub fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<u64> {
    let bytes = read(from)?;
    let len = bytes.len() as u64;
    write(to, bytes)?;
    Ok(len)
}

pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let mut bytes = Vec::with_capacity(file.len as usize);
    file.read_to_end(&mut bytes)?;
    // A whole-file read of a big zone is seconds of main-thread time in the
    // browser; naming it next to the surrounding log lines says who asked.
    if bytes.len() >= 32 << 20 {
        console_log(&format!(
            "vfs: whole-file read of `{}` ({:.1} MiB)",
            path.display(),
            bytes.len() as f64 / (1024.0 * 1024.0)
        ));
    }
    Ok(bytes)
}

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(line: &str);
}

pub fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    File::create(path)?.write_all(contents.as_ref())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Permissions {
    readonly: bool,
}

impl Permissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

pub fn set_permissions<P: AsRef<Path>>(path: P, _perm: Permissions) -> io::Result<()> {
    metadata(path).map(|_| ())
}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    pub fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        let path = path.as_ref();
        let display = normalize(path);
        let writes = self.write || self.append;
        let mut tree = tree();
        let existing = tree.get(&display).cloned();
        if self.create_new && existing.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: exists", path.display()),
            ));
        }
        let entry = match existing {
            Some(entry) => entry,
            None if writes && (self.create || self.create_new) => {
                if !tree.parent_is_dir(&display) {
                    return Err(not_found(path));
                }
                let node = Node::Memory {
                    data: Arc::new(Mutex::new(Vec::new())),
                };
                tree.insert(display.clone(), node.clone(), now());
                Entry {
                    display: display.clone(),
                    node,
                    modified: now(),
                }
            }
            None => return Err(not_found(path)),
        };
        let handle = match entry.node {
            Node::Dir { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    format!("{}: is a directory", path.display()),
                ));
            }
            Node::Mounted { .. } if writes => return Err(read_only(path)),
            Node::Mounted { index, size } => Handle::Mounted {
                index,
                cache: Vec::new(),
                cache_start: 0,
                size,
            },
            Node::Memory { data } => {
                if self.truncate && writes {
                    data.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clear();
                }
                Handle::Memory(data)
            }
        };
        let len = match &handle {
            Handle::Mounted { size, .. } => *size,
            Handle::Memory(data) => data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len() as u64,
        };
        Ok(File {
            display,
            handle,
            pos: 0,
            ahead: FIRST_READ_AHEAD,
            len,
            append: self.append,
            writable: writes,
        })
    }
}

enum Handle {
    Mounted {
        index: u32,
        size: u64,
        cache: Vec<u8>,
        cache_start: u64,
    },
    Memory(Arc<Mutex<Vec<u8>>>),
}

pub struct File {
    display: PathBuf,
    handle: Handle,
    pos: u64,
    ahead: usize,
    len: u64,
    append: bool,
    writable: bool,
}

impl File {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new().read(true).open(path)
    }

    pub fn create<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }

    pub fn create_new<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        let len = match &self.handle {
            Handle::Mounted { size, .. } => *size,
            Handle::Memory(data) => data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len() as u64,
        };
        Ok(Metadata {
            file_type: FileType { dir: false },
            len,
            modified: metadata(&self.display).map_or(UNIX_EPOCH, |meta| meta.modified),
        })
    }

    pub fn set_len(&self, size: u64) -> io::Result<()> {
        match &self.handle {
            Handle::Mounted { .. } => Err(read_only(&self.display)),
            Handle::Memory(data) => {
                data.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .resize(size as usize, 0);
                Ok(())
            }
        }
    }

    /// One tab owns its memory tree; advisory locks always succeed.
    pub fn lock(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn lock_shared(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn try_lock(&self) -> Result<(), std::fs::TryLockError> {
        Ok(())
    }

    pub fn try_lock_shared(&self) -> Result<(), std::fs::TryLockError> {
        Ok(())
    }

    pub fn unlock(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }
}

/// The first [`HEAD`] bytes of a mounted file, read once per tab. The engine
/// sniffs zone headers on every lookup; on desktop the OS cache absorbs that.
fn head_of(index: u32, size: u64) -> Arc<Vec<u8>> {
    static HEADS: OnceLock<Mutex<HashMap<u32, Arc<Vec<u8>>>>> = OnceLock::new();
    let heads = HEADS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(head) = heads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&index)
    {
        return head.clone();
    }
    let mut head = vec![0; HEAD.min(size as usize)];
    let got = mounted_read(index, 0, &mut head);
    head.truncate(got);
    let head = Arc::new(head);
    heads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(index, head.clone());
    head
}

thread_local! {
    /// This thread's blocking reads since [`take_read_wait`]: milliseconds,
    /// calls, bytes.
    static READ_WAIT: std::cell::Cell<(f64, u32, u64)> = const { std::cell::Cell::new((0.0, 0, 0)) };
}

/// Time this thread spent blocked in file reads since the last call, with the
/// number of reads and bytes: the engine's frame clock names it in slow
/// frames, since a read that misses the page's caches waits on the network.
pub fn take_read_wait() -> (f64, u32, u64) {
    READ_WAIT.with(|wait| wait.replace((0.0, 0, 0)))
}

fn mounted_read(index: u32, offset: u64, dst: &mut [u8]) -> usize {
    let started = js_sys::Date::now();
    let got = mounted_read_blocking(index, offset, dst);
    let ms = js_sys::Date::now() - started;
    READ_WAIT.with(|wait| {
        let (total, n, bytes) = wait.get();
        wait.set((total + ms, n + 1, bytes + got as u64));
    });
    got
}

fn mounted_read_blocking(index: u32, offset: u64, dst: &mut [u8]) -> usize {
    if index & PERSISTED != 0 {
        let paths = persisted().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        return match paths.get((index & !PERSISTED) as usize) {
            Some(rel) => js_cache_read(rel, offset as f64, dst) as usize,
            None => 0,
        };
    }
    // The page's channel has several request slots (vfs.js SLOTS); the JS
    // read claims one per call, so threads (web_threads) read at once.
    let index = if READS_UNTRACKED.with(std::cell::Cell::get) {
        index | UNTRACKED
    } else {
        index
    };
    let mut done = 0;
    while done < dst.len() {
        let got = js_read(index, (offset + done as u64) as f64, &mut dst[done..]) as usize;
        if got == 0 {
            break;
        }
        done += got;
    }
    done
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.handle {
            Handle::Mounted {
                index,
                size,
                cache,
                cache_start,
            } => {
                if self.pos >= *size || buf.is_empty() {
                    return Ok(0);
                }
                let want = buf.len().min((*size - self.pos) as usize);
                if self.pos < HEAD as u64 {
                    let head = head_of(*index, *size);
                    if (self.pos as usize) < head.len() {
                        let at = self.pos as usize;
                        let n = want.min(head.len() - at);
                        buf[..n].copy_from_slice(&head[at..at + n]);
                        self.pos += n as u64;
                        return Ok(n);
                    }
                }
                let cached = self.pos >= *cache_start
                    && self.pos < *cache_start + cache.len() as u64;
                if !cached {
                    // Grow the read-ahead only while reads continue where the
                    // last window ended; a seek starts small again.
                    let sequential = self.pos == *cache_start + cache.len() as u64;
                    self.ahead = if sequential {
                        (self.ahead * 2).min(READ_AHEAD)
                    } else {
                        FIRST_READ_AHEAD
                    };
                    let hint = READ_HINT.with(|hint| hint.replace(0)) as usize;
                    let fill_at_least = hint.min(MAX_HINTED_READ);
                    if want >= self.ahead {
                        let got = mounted_read(*index, self.pos, &mut buf[..want]);
                        self.pos += got as u64;
                        cache.clear();
                        *cache_start = self.pos;
                        return Ok(got);
                    }
                    let fill = self
                        .ahead
                        .max(fill_at_least)
                        .min((*size - self.pos) as usize);
                    cache.resize(fill, 0);
                    let got = mounted_read(*index, self.pos, cache);
                    cache.truncate(got);
                    *cache_start = self.pos;
                    if got == 0 {
                        return Ok(0);
                    }
                }
                let at = (self.pos - *cache_start) as usize;
                let n = want.min(cache.len() - at);
                buf[..n].copy_from_slice(&cache[at..at + n]);
                self.pos += n as u64;
                Ok(n)
            }
            Handle::Memory(data) => {
                let data = data.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let at = (self.pos as usize).min(data.len());
                let n = buf.len().min(data.len() - at);
                buf[..n].copy_from_slice(&data[at..at + n]);
                self.pos += n as u64;
                Ok(n)
            }
        }
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{}: opened read-only", self.display.display()),
            ));
        }
        let Handle::Memory(data) = &self.handle else {
            return Err(read_only(&self.display));
        };
        let mut data = data.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.append {
            self.pos = data.len() as u64;
        }
        let at = self.pos as usize;
        if data.len() < at + buf.len() {
            data.resize(at + buf.len(), 0);
        }
        data[at..at + buf.len()].copy_from_slice(buf);
        self.pos += buf.len() as u64;
        self.len = data.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = match &self.handle {
            Handle::Mounted { size, .. } => *size,
            Handle::Memory(data) => data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len() as u64,
        };
        let next = match pos {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(delta) => len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        };
        let next = next.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start of file")
        })?;
        self.pos = next;
        Ok(next)
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File").field("path", &self.display).finish()
    }
}
