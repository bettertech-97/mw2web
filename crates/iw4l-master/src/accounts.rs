//! Player accounts, keyed by the visitor's address.
//!
//! The engine already keeps a player's whole MW2 profile (identity, XP,
//! rank, stats, skill ratings) in one file it writes after every match, and
//! the class editor keeps another. The browser build has no disk for them,
//! so the page sends them here and asks for them back before the engine
//! starts. One account per IP address: simple, no passwords, and the
//! player only picks a name. Each account is a directory under the
//! accounts root named by a hash of the address, holding `meta.json` and
//! the profile files.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path as RoutePath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Files the engine keeps per profile, as the page names them.
pub const PROFILE_FILES: [&str; 3] = ["account.dat", "classes.txt", "camos.txt"];
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_NAME_CHARS: usize = 16;
const MIN_NAME_CHARS: usize = 2;

#[derive(Clone)]
pub struct Accounts {
    root: PathBuf,
    /// Lower-cased name to account directory, for uniqueness.
    names: Arc<Mutex<HashMap<String, String>>>,
}

impl Accounts {
    pub fn open(root: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        let mut names = HashMap::new();
        for entry in std::fs::read_dir(&root)?.flatten() {
            let dir = entry.file_name().to_string_lossy().into_owned();
            if let Some(meta) = read_meta(&entry.path())
                && let Some(name) = meta.get("name").and_then(|name| name.as_str())
                && !name.is_empty()
            {
                names.insert(name.to_lowercase(), dir);
            }
        }
        Ok(Self {
            root,
            names: Arc::new(Mutex::new(names)),
        })
    }

    fn dir_of(&self, ip: &str) -> PathBuf {
        let digest = ring::digest::digest(&ring::digest::SHA256, ip.as_bytes());
        let hex: String = digest.as_ref()[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.root.join(hex)
    }
}

/// The visitor's address: the last entry of `X-Forwarded-For`, which is the
/// one the reverse proxy in front of us appended (anything before it the
/// visitor could have sent themselves), else the peer.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    if let Some(ip) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
    {
        return ip.to_owned();
    }
    peer.ip().to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn read_meta(dir: &Path) -> Option<serde_json::Value> {
    let bytes = std::fs::read(dir.join("meta.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_meta(dir: &Path, meta: &serde_json::Value) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_atomic(&dir.join("meta.json"), meta.to_string().as_bytes())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// A name a player may pick: letters, digits, `_` and `-`, 2 to 16 of them.
fn valid_name(name: &str) -> bool {
    let count = name.chars().count();
    (MIN_NAME_CHARS..=MAX_NAME_CHARS).contains(&count)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, byte)| n | (u32::from(*byte) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `GET /lan/account`: the caller's name and profile files.
pub async fn get(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(&headers, peer);
    let dir = accounts.dir_of(&ip);
    let meta = read_meta(&dir).unwrap_or_else(|| serde_json::json!({}));
    let mut files = serde_json::Map::new();
    for name in PROFILE_FILES {
        if let Ok(bytes) = std::fs::read(dir.join(name)) {
            files.insert(name.to_owned(), serde_json::Value::String(base64(&bytes)));
        }
    }
    axum::Json(serde_json::json!({
        "name": meta.get("name").and_then(|name| name.as_str()).unwrap_or(""),
        "created": meta.get("created").and_then(|at| at.as_u64()),
        "updated": meta.get("updated").and_then(|at| at.as_u64()),
        "files": files,
    }))
    .into_response()
}

/// `POST /lan/account/name` with `{"name": "..."}`: the caller's name, unique
/// across accounts (case-insensitively).
pub async fn set_name(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(posted) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "expected JSON\n").into_response();
    };
    let Some(name) = posted.get("name").and_then(|name| name.as_str()) else {
        return (StatusCode::BAD_REQUEST, "expected {\"name\"}\n").into_response();
    };
    let name = name.trim();
    if !valid_name(name) {
        return (
            StatusCode::BAD_REQUEST,
            "a name is 2 to 16 letters, digits, _ or -\n",
        )
            .into_response();
    }
    let ip = client_ip(&headers, peer);
    let dir = accounts.dir_of(&ip);
    let dir_name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut names = accounts.names.lock().unwrap_or_else(PoisonError::into_inner);
    let key = name.to_lowercase();
    if names.get(&key).is_some_and(|owner| *owner != dir_name) {
        return (StatusCode::CONFLICT, "that name belongs to someone else\n").into_response();
    }
    let mut meta = read_meta(&dir).unwrap_or_else(|| serde_json::json!({}));
    if let Some(previous) = meta.get("name").and_then(|name| name.as_str()) {
        names.remove(&previous.to_lowercase());
    }
    let created = meta.get("created").and_then(|at| at.as_u64()).unwrap_or_else(now);
    meta = serde_json::json!({
        "ip": ip,
        "name": name,
        "created": created,
        "updated": now(),
    });
    if let Err(error) = write_meta(&dir, &meta) {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    names.insert(key, dir_name);
    StatusCode::NO_CONTENT.into_response()
}

/// `PUT /lan/account/file/{name}`: one of the profile files, as bytes.
pub async fn put_file(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    RoutePath(name): RoutePath<String>,
    body: Bytes,
) -> Response {
    if !PROFILE_FILES.contains(&name.as_str()) {
        return (StatusCode::NOT_FOUND, "not a profile file\n").into_response();
    }
    if body.len() > MAX_FILE_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let ip = client_ip(&headers, peer);
    let dir = accounts.dir_of(&ip);
    let mut meta = read_meta(&dir).unwrap_or_else(|| serde_json::json!({}));
    if meta.get("created").is_none() {
        meta = serde_json::json!({ "ip": ip, "name": "", "created": now(), "updated": now() });
    } else if let Some(object) = meta.as_object_mut() {
        object.insert("updated".to_owned(), serde_json::Value::from(now()));
    }
    if let Err(error) = write_meta(&dir, &meta).and_then(|()| write_atomic(&dir.join(&name), &body))
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /lan/account/file/{name}`: one profile file, raw.
pub async fn get_file(
    State(accounts): State<Accounts>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    RoutePath(name): RoutePath<String>,
) -> Response {
    if !PROFILE_FILES.contains(&name.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ip = client_ip(&headers, peer);
    match std::fs::read(accounts.dir_of(&ip).join(&name)) {
        Ok(bytes) => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
