//! Valve VPK archives (version 2): one `_dir.vpk` directory, entries stored
//! in numbered chunk files beside it.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub archive: u16,
    pub offset: u32,
    pub length: u32,
    /// Bytes stored in the directory itself, ahead of the archive part.
    pub preload: (usize, u16),
}

pub struct Vpk {
    dir_path: PathBuf,
    dir: Vec<u8>,
    entries: HashMap<String, Entry>,
}

fn cstr(data: &[u8], at: usize) -> Result<(String, usize), String> {
    let end = data[at..]
        .iter()
        .position(|&b| b == 0)
        .ok_or("vpk: unterminated string")?;
    Ok((
        String::from_utf8_lossy(&data[at..at + end]).into_owned(),
        at + end + 1,
    ))
}

impl Vpk {
    pub fn open(dir_path: &Path) -> Result<Self, String> {
        let dir = std::fs::read(dir_path).map_err(|e| format!("{}: {e}", dir_path.display()))?;
        if dir.len() < 12 || u32::from_le_bytes(dir[0..4].try_into().unwrap()) != 0x55aa_1234 {
            return Err("vpk: not a VPK directory".into());
        }
        let version = u32::from_le_bytes(dir[4..8].try_into().unwrap());
        let mut pos = match version {
            1 => 12,
            2 => 28,
            v => return Err(format!("vpk: unsupported version {v}")),
        };
        let mut entries = HashMap::new();
        loop {
            let (ext, next) = cstr(&dir, pos)?;
            pos = next;
            if ext.is_empty() {
                break;
            }
            loop {
                let (path, next) = cstr(&dir, pos)?;
                pos = next;
                if path.is_empty() {
                    break;
                }
                loop {
                    let (name, next) = cstr(&dir, pos)?;
                    pos = next;
                    if name.is_empty() {
                        break;
                    }
                    let b = &dir[pos..pos + 18];
                    let preload_len = u16::from_le_bytes([b[4], b[5]]);
                    let archive = u16::from_le_bytes([b[6], b[7]]);
                    let offset = u32::from_le_bytes(b[8..12].try_into().unwrap());
                    let length = u32::from_le_bytes(b[12..16].try_into().unwrap());
                    pos += 18;
                    let preload_at = pos;
                    pos += usize::from(preload_len);
                    let full = if path == " " {
                        format!("{name}.{ext}")
                    } else {
                        format!("{path}/{name}.{ext}")
                    };
                    entries.insert(
                        full.to_ascii_lowercase(),
                        Entry {
                            archive,
                            offset,
                            length,
                            preload: (preload_at, preload_len),
                        },
                    );
                }
            }
        }
        Ok(Self {
            dir_path: dir_path.to_path_buf(),
            dir,
            entries,
        })
    }

    /// Every entry's path, lowercase.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(&name.to_ascii_lowercase())
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>, String> {
        let entry = self
            .entries
            .get(&name.to_ascii_lowercase())
            .ok_or_else(|| format!("vpk: no entry {name}"))?;
        let mut out = Vec::with_capacity(entry.length as usize + usize::from(entry.preload.1));
        out.extend_from_slice(&self.dir[entry.preload.0..entry.preload.0 + usize::from(entry.preload.1)]);
        if entry.length == 0 {
            return Ok(out);
        }
        let path = if entry.archive == 0x7fff {
            self.dir_path.clone()
        } else {
            let stem = self
                .dir_path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix("_dir.vpk"))
                .ok_or("vpk: directory file is not *_dir.vpk")?;
            self.dir_path
                .with_file_name(format!("{stem}_{:03}.vpk", entry.archive))
        };
        let mut file = File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        file.seek(SeekFrom::Start(u64::from(entry.offset)))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut chunk = vec![0u8; entry.length as usize];
        file.read_exact(&mut chunk)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        out.extend_from_slice(&chunk);
        Ok(out)
    }
}
