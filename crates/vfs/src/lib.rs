//! The file system the engine sees.
//!
//! On desktop this is `std::fs`, re-exported unchanged, plus [`PathExt`] whose
//! methods forward to the inherent `Path` queries.
//!
//! In the browser there is no file system. The page mounts the player's own
//! games folder (picked in the page, or served by the local dev server) and
//! answers byte-range reads synchronously from a worker; everything the engine
//! writes (saves, caches, logs) lives in memory for the life of the tab. The
//! module mirrors the subset of `std::fs` the engine uses, so call sites only
//! swap `std::fs` for `vfs`.

#[cfg(not(target_arch = "wasm32"))]
pub use std::fs::*;

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::*;

use std::io;
use std::path::{Path, PathBuf};

/// `Path` queries that go through this file system. On desktop each forwards
/// to the inherent method of the same name without the `vfs_` prefix.
pub trait PathExt {
    fn vfs_exists(&self) -> bool;
    fn vfs_is_dir(&self) -> bool;
    fn vfs_is_file(&self) -> bool;
    fn vfs_metadata(&self) -> io::Result<Metadata>;
    fn vfs_read_dir(&self) -> io::Result<ReadDir>;
    fn vfs_canonicalize(&self) -> io::Result<PathBuf>;
}

impl PathExt for Path {
    fn vfs_exists(&self) -> bool {
        metadata(self).is_ok()
    }

    fn vfs_is_dir(&self) -> bool {
        metadata(self).is_ok_and(|meta| meta.is_dir())
    }

    fn vfs_is_file(&self) -> bool {
        metadata(self).is_ok_and(|meta| meta.is_file())
    }

    fn vfs_metadata(&self) -> io::Result<Metadata> {
        metadata(self)
    }

    fn vfs_read_dir(&self) -> io::Result<ReadDir> {
        read_dir(self)
    }

    fn vfs_canonicalize(&self) -> io::Result<PathBuf> {
        canonicalize(self)
    }
}

/// Wall-clock now as a `std` time, which file timestamps compare against.
/// `SystemTime::now()` panics in the browser; the page clock does not.
pub fn system_now() -> std::time::SystemTime {
    #[cfg(target_arch = "wasm32")]
    {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(js_sys::Date::now() / 1000.0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::time::SystemTime::now()
}

/// Distinguishes temporary file names between processes. The browser tab is
/// the only process writing its memory tree.
pub fn process_id() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        1
    }
    #[cfg(not(target_arch = "wasm32"))]
    std::process::id()
}

/// Says the next read will consume about `bytes` from where it starts, so the
/// browser can fetch them in one round trip instead of growing its read-ahead
/// step by step (each step is a page round trip, a network one for a LAN
/// joiner). A no-op on desktop.
/// Ask the browser to bring `len` bytes of `path` from `offset` into the
/// block cache in the background; `true` once they are there. A read of a
/// warm range answers from memory instead of stalling the page on disk or
/// the network. Native files are always warm.
pub fn warm(path: &Path, offset: u64, len: u64) -> bool {
    #[cfg(target_arch = "wasm32")]
    return web::warm(path, offset, len);
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (path, offset, len);
        true
    }
}

/// Runs `f` with this thread's reads kept out of the map's download list
/// (the manifest a player posts so the next one prefetches what a load
/// reads): for background reads that warm the cache, which a new player
/// should not have to wait for before the map starts.
pub fn untracked_reads<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(target_arch = "wasm32")]
    {
        let before = web::set_untracked(true);
        let out = f();
        web::set_untracked(before);
        out
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        f()
    }
}

pub fn read_hint(bytes: u64) {
    #[cfg(target_arch = "wasm32")]
    web::set_read_hint(bytes);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = bytes;
}
