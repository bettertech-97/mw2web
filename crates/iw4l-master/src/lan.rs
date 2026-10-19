//! `iw4l-master lan`: one process that lets browsers on the local network play
//! together. It runs the relay on loopback QUIC and, over HTTPS, serves the web
//! build, the host's own MW2 folder and a WebSocket bridge into the relay.
//!
//! Browsers cannot open QUIC connections to an arbitrary server, so each
//! WebSocket becomes one QUIC connection to the relay. A binary message's first
//! byte says which QUIC lane it belongs to; the rest is that lane's bytes.
//!
//! The games folder is served to the local network only (private addresses are
//! the only peers admitted). It is the host's own install: do not forward this
//! port to the internet.
//!
//! `--public` also opens a Cloudflare quick tunnel to the plain-HTTP listener.
//! Requests that arrive through it (they carry Cloudflare's headers) get the
//! page, the room list and the relay, never the log: by default a player on the
//! internet plays from their own MW2 install. `--public-games` additionally
//! serves the host's own game files over the tunnel, so internet players need
//! no MW2 install of their own (the host serves their copy; the log stays
//! local-only).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path as RoutePath, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum_server::tls_rustls::RustlsConfig;
use futures_util::{SinkExt, StreamExt};
use master_protocol::{ALPN, MAX_BOOTSTRAP_STREAM_BYTES, MAX_RELAY_UNI_STREAMS};
use quinn::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::{Result, ServiceState};

/// Lane tags, shared with `crates/net/src/transport/ws_conn.rs`.
const LANE_CONTROL: u8 = 0;
const LANE_DATAGRAM: u8 = 1;
const LANE_UNI: u8 = 2;

const SERVER_NAME: &str = "iw4l-lan";
const BRIDGE_QUEUE: usize = 1024;

pub struct LanSpec {
    pub https: SocketAddr,
    pub http: Option<SocketAddr>,
    pub web: PathBuf,
    pub games: PathBuf,
    /// Start a Cloudflare quick tunnel to the HTTP listener.
    pub public: bool,
    /// Serve the host's game files over the tunnel too, so internet players
    /// need no MW2 install of their own. Off by default: the host opts in.
    pub public_games: bool,
    /// Where pages fetch game file bytes instead of this server's `/games`:
    /// an object store or CDN holding the same files under the same relative
    /// paths (`main/iw_00.iwd`). The index still comes from `--games`.
    pub games_url: Option<String>,
    /// Keep the per-map download manifests here (`<map>.json`) so a restart
    /// does not send the next player back to a blocked tab; in memory only
    /// when unset.
    pub manifests: Option<PathBuf>,
    /// Keep player accounts (name and profile files, one per address) here;
    /// no accounts when unset.
    pub accounts: Option<PathBuf>,
    pub cloudflared: Option<PathBuf>,
}

/// Sent by Cloudflare (and any other reverse proxy) on every request it relays.
const PROXY_HEADERS: [&str; 4] = ["cf-connecting-ip", "cf-ray", "x-forwarded-for", "forwarded"];

fn proxied(headers: &axum::http::HeaderMap) -> bool {
    PROXY_HEADERS.iter().any(|name| headers.contains_key(*name))
}

/// The peer as the logs should name it: Cloudflare's view of the visitor when
/// the request came through the tunnel.
fn visitor(headers: &axum::http::HeaderMap, peer: SocketAddr) -> String {
    headers
        .get("cf-connecting-ip")
        .and_then(|value| value.to_str().ok())
        .map(|ip| format!("{ip} via cloudflare"))
        .unwrap_or_else(|| peer.to_string())
}

/// Paths that hand out the host's game files or write to its disk.
fn local_only_path(path: &str) -> bool {
    path == "/games" || path.starts_with("/games/") || path == "/games-index.json" || path == "/log"
}

#[derive(Clone)]
struct Lan {
    state: Arc<Mutex<ServiceState>>,
    relay: SocketAddr,
    client: quinn::ClientConfig,
    games: PathBuf,
    log: PathBuf,
    public_games: bool,
    games_url: Option<String>,
    /// Per map, the hosted-file blocks a player's engine read to load it:
    /// posted by whoever has played it, fetched by the next player so their
    /// page can download the map before the engine stalls on it.
    manifests: Arc<std::sync::Mutex<HashMap<String, Manifest>>>,
    manifest_dir: Option<PathBuf>,
    /// Translated shaders and bot navigation one player's engine produced,
    /// as the page packs them (gzip); handed to the next player's cold
    /// cache so their first load skips that work. The richest one wins.
    cache_seed: Arc<std::sync::Mutex<Option<CacheSeed>>>,
    /// Whether player accounts are kept (`/lan/account`).
    accounts: bool,
    /// The tunnel's address once cloudflared reports it, for the page's
    /// "join this match" link.
    public: Arc<std::sync::Mutex<Option<String>>>,
}

/// Block runs per file path, as the page posts them.
type Manifest = HashMap<String, Vec<(u64, u64)>>;

const MAX_MANIFEST_BYTES: usize = 1 << 20;

struct CacheSeed {
    records: u64,
    /// The pack as posted: gzip bytes the browser inflates on fetch.
    bytes: Vec<u8>,
}

const MAX_SEED_BYTES: usize = 64 << 20;
const SEED_FILE: &str = "cache-seed.gz";
const SEED_RECORDS_FILE: &str = "cache-seed.records";

pub async fn serve(spec: LanSpec) -> Result<()> {
    let lan_ip = local_ip();
    let (ca, certs, key) = mint_identity(lan_ip)?;
    let relay = SocketAddr::from(([127, 0, 0, 1], spec.https.port()));
    let state = Arc::new(Mutex::new(ServiceState::default()));

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca)?;
    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let mut client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(master_protocol::SESSION_IDLE).expect("idle fits VarInt"),
    ));
    transport.keep_alive_interval(Some(master_protocol::SESSION_KEEP_ALIVE));
    transport.max_concurrent_uni_streams(MAX_RELAY_UNI_STREAMS.into());
    client.transport_config(Arc::new(transport));

    let games = std::fs::canonicalize(&spec.games)
        .map_err(|e| format!("--games {}: {e}", spec.games.display()))?;
    let web = std::fs::canonicalize(&spec.web)
        .map_err(|e| format!("--web {}: {e}", spec.web.display()))?;
    if !web.join("pkg").join("iw4l_bg.wasm").is_file() {
        return Err(format!("{} has no pkg/iw4l_bg.wasm; run scripts/web/build.sh", web.display()).into());
    }
    let public = Arc::new(std::sync::Mutex::new(None));
    let lan = Lan {
        state: Arc::clone(&state),
        relay,
        client,
        games: games.clone(),
        log: web.join("dev.log"),
        public_games: spec.public_games,
        games_url: spec.games_url.clone(),
        manifests: Arc::new(std::sync::Mutex::new(load_manifests(spec.manifests.as_deref()))),
        manifest_dir: spec.manifests.clone(),
        cache_seed: Arc::new(std::sync::Mutex::new(load_cache_seed(spec.manifests.as_deref()))),
        accounts: spec.accounts.is_some(),
        public: Arc::clone(&public),
    };
    let gate_lan = lan.clone();
    let accounts = match &spec.accounts {
        Some(root) => Some(
            crate::accounts::Accounts::open(root.clone())
                .map_err(|e| format!("--accounts {}: {e}", root.display()))?,
        ),
        None => None,
    };
    let account_routes = match accounts {
        Some(accounts) => Router::new()
            .route("/lan/account", get(crate::accounts::get))
            .route("/lan/account/name", post(crate::accounts::set_name))
            .route(
                "/lan/account/file/{name}",
                get(crate::accounts::get_file).put(crate::accounts::put_file),
            )
            .with_state(accounts),
        None => Router::new(),
    };
    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/lan/rooms", get(rooms))
        .route("/lan/info", get(info))
        .route("/lan/manifest/{map}", get(manifest_get).post(manifest_put))
        .route(
            "/lan/cache-seed",
            get(cache_seed_get)
                .post(cache_seed_put)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_SEED_BYTES)),
        )
        .route("/games-index.json", get(games_index))
        .route("/log", post(log))
        .nest_service("/games", ServeDir::new(&games))
        .fallback_service(ServeDir::new(&web))
        .with_state(lan)
        .merge(account_routes)
        .layer(axum::middleware::from_fn_with_state(gate_lan, gate))
        .layer(isolation_header("cross-origin-opener-policy", "same-origin"))
        .layer(isolation_header("cross-origin-embedder-policy", "require-corp"))
        .layer(isolation_header("cross-origin-resource-policy", "same-origin"))
        // Revalidate rather than refetch: the engine alone is ~120 MB.
        .layer(isolation_header("cache-control", "no-cache"));

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs.clone(), key.clone_key())?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];

    let mut out = std::io::stderr();
    use std::io::Write;
    let shown = |path: &Path| path.display().to_string().trim_start_matches(r"\\?\").to_owned();
    writeln!(out, "iw4l lan: games folder {}", shown(&games))?;
    writeln!(out, "iw4l lan: web build    {}", shown(&web))?;
    if let Some(http) = spec.http {
        writeln!(out, "iw4l lan: this PC      http://localhost:{}/", http.port())?;
    }
    match lan_ip {
        Some(ip) => writeln!(out, "iw4l lan: other PCs    https://{ip}:{}/", spec.https.port())?,
        None => writeln!(out, "iw4l lan: other PCs    https://<this PC's address>:{}/", spec.https.port())?,
    }
    writeln!(
        out,
        "iw4l lan: the certificate is self-signed; the browser warns once per PC (Advanced > Proceed)"
    )?;

    let https = axum_server::bind_rustls(spec.https, RustlsConfig::from_config(Arc::new(tls)))
        .serve(app.clone().into_make_service_with_connect_info::<SocketAddr>());
    let http = async {
        match spec.http {
            Some(bind) => {
                let listener = tokio::net::TcpListener::bind(bind).await?;
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await?;
                Ok(())
            }
            None => std::future::pending::<Result<()>>().await,
        }
    };
    let tunnel = async {
        match (spec.public, spec.http) {
            (true, Some(http)) => {
                tunnel(spec.cloudflared.as_deref(), http, spec.public_games, public).await
            }
            (true, None) => Err("--public needs the HTTP listener (--http)".into()),
            (false, _) => std::future::pending::<Result<()>>().await,
        }
    };
    tokio::select! {
        result = crate::serve_with(relay, certs, key, state) => result,
        result = https => Ok(result?),
        result = http => result,
        result = tunnel => result,
    }
}

/// Runs `cloudflared tunnel --url http://<http listener>` and prints the
/// public address it is given. Ends (and so stops the server) if it exits.
async fn tunnel(
    cloudflared: Option<&Path>,
    http: SocketAddr,
    public_games: bool,
    public: Arc<std::sync::Mutex<Option<String>>>,
) -> Result<()> {
    use tokio::io::AsyncBufReadExt;
    let program = cloudflared
        .map(Path::to_path_buf)
        .or_else(find_cloudflared)
        .ok_or("--public needs cloudflared (https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/) or --cloudflared PATH")?;
    let origin = format!("http://127.0.0.1:{}", http.port());
    let mut child = tokio::process::Command::new(&program)
        .args(["tunnel", "--no-autoupdate", "--url", &origin])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    let mut lines = tokio::io::BufReader::new(child.stderr.take().ok_or("cloudflared stderr")?).lines();
    let mut announced = false;
    while let Some(line) = lines.next_line().await? {
        if !announced
            && let Some(url) = line
                .split_whitespace()
                .find(|word| word.starts_with("https://") && word.contains(".trycloudflare.com"))
        {
            announced = true;
            *public.lock().unwrap_or_else(PoisonError::into_inner) = Some(url.to_owned());
            writeln_err(format!("iw4l lan: internet     {url}/"))?;
            writeln_err(
                if public_games {
                    "iw4l lan: internet players need no MW2 install; your games are served over the tunnel"
                } else {
                    "iw4l lan: internet players pick their own MW2 folder; your games folder stays on your network"
                }
                .to_owned(),
            )?;
        }
        if line.contains(" ERR ") {
            writeln_err(format!("cloudflared: {line}"))?;
        }
    }
    let status = child.wait().await?;
    Err(format!("cloudflared exited ({status})").into())
}

fn find_cloudflared() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "cloudflared.exe" } else { "cloudflared" };
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(exe));
    let installed = [
        r"C:\Program Files (x86)\cloudflared\cloudflared.exe",
        r"C:\Program Files\cloudflared\cloudflared.exe",
    ]
    .into_iter()
    .map(PathBuf::from);
    on_path.chain(installed).find(|candidate| candidate.is_file())
}

fn isolation_header(
    name: &'static str,
    value: &'static str,
) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(
        header::HeaderName::from_static(name),
        HeaderValue::from_static(value),
    )
}

/// The games folder must stay on the local network. Requests through the
/// tunnel arrive from loopback, so the proxy's headers decide, not the peer.
async fn gate(
    State(lan): State<Lan>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if proxied(request.headers()) {
        // The log mirror writes to the host's disk, so it is never served over
        // the internet. Game files go out over the tunnel only when the host
        // opted in with --public-games.
        let path = request.uri().path();
        let blocked = if lan.public_games {
            path == "/log"
        } else {
            local_only_path(path)
        };
        if blocked {
            return (
                StatusCode::NOT_FOUND,
                "not served over the internet: use your own MW2 folder\n",
            )
                .into_response();
        }
        return next.run(request).await;
    }
    // The carrier-grade NAT block (100.64/10) is what Tailscale and similar
    // private overlays hand out to the owner's own devices.
    let local = match peer.ip() {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || (a == 100 && b & 0xc0 == 64)
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unicast_link_local()
                || ip.is_unique_local()
                || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback() || v4.is_private())
        }
    };
    if !local {
        return (StatusCode::FORBIDDEN, "local network only\n").into_response();
    }
    next.run(request).await
}

/// The address other PCs on the network reach this one at. Connecting a UDP
/// socket only picks a route; nothing is sent.
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// A throwaway CA and a certificate it signs, valid for this PC's names. The
/// relay and the HTTPS listener share the certificate; the bridge trusts the CA.
fn mint_identity(
    lan_ip: Option<IpAddr>,
) -> Result<(
    CertificateDer<'static>,
    Vec<CertificateDer<'static>>,
    PrivateKeyDer<'static>,
)> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, SanType};
    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "IW4L LAN (this session only)");
    let ca = ca_params.self_signed(&ca_key)?;

    let leaf_key = KeyPair::generate()?;
    let mut names = vec!["localhost".to_owned(), SERVER_NAME.to_owned()];
    if let Ok(host) = std::env::var("COMPUTERNAME") {
        names.push(host.to_ascii_lowercase());
    }
    let mut params = CertificateParams::new(names)?;
    params
        .subject_alt_names
        .push(SanType::IpAddress(IpAddr::from([127, 0, 0, 1])));
    if let Some(ip) = lan_ip {
        params.subject_alt_names.push(SanType::IpAddress(ip));
    }
    params
        .distinguished_name
        .push(DnType::CommonName, "IW4L LAN");
    let leaf = params.signed_by(&leaf_key, &ca, &ca_key)?;
    Ok((
        ca.der().clone(),
        vec![leaf.der().clone(), ca.der().clone()],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
    ))
}

async fn rooms(State(lan): State<Lan>) -> Response {
    let state = lan.state.lock().await;
    // Names in join order, the host first, for the page's roster.
    let mut rooms: Vec<_> = state
        .rooms
        .values()
        .map(|room| {
            let view = &room.view;
            let name_of = |member| view.member_names.get(member).cloned().unwrap_or_default();
            let names: Vec<String> = std::iter::once(&view.host)
                .chain(view.members.iter().filter(|member| **member != view.host))
                .map(name_of)
                .collect();
            (view.advert(), name_of(&view.host), names)
        })
        .collect();
    drop(state);
    rooms.sort_by_key(|(advert, _, _)| advert.id);
    let rooms: Vec<_> = rooms
        .into_iter()
        .map(|(advert, host, names)| {
            serde_json::json!({
                "id": advert.id.to_string(),
                "name": advert.name,
                "map": advert.map,
                "mode": advert.mode,
                "players": advert.players,
                "max_players": advert.max_players,
                "in_match": advert.in_match,
                "locked": advert.locked,
                "password": advert.password_protected,
                "host": host,
                "names": names,
            })
        })
        .collect();
    axum::Json(rooms).into_response()
}

/// What this visitor may use: the host's game files and the log mirror are
/// for the local network only.
async fn info(State(lan): State<Lan>, headers: axum::http::HeaderMap) -> Response {
    let local = !proxied(&headers);
    // Internet players can draw game files from the host when --public-games is
    // set; the log mirror stays local-only regardless.
    let files = local || lan.public_games;
    let public = lan.public.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let accounts = lan.accounts;
    axum::Json(serde_json::json!({
        "accounts": accounts,
        "files": files,
        "log": local,
        "public": public,
        "games": lan.games_url,
    }))
    .into_response()
}

/// Every `<map>.json` in the manifest folder, written by `manifest_put`.
fn load_manifests(dir: Option<&Path>) -> HashMap<String, Manifest> {
    let mut loaded = HashMap::new();
    let Some(dir) = dir else { return loaded };
    let Ok(entries) = std::fs::read_dir(dir) else { return loaded };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(key) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|_| path.extension().is_some_and(|ext| ext == "json"))
            .and_then(manifest_key)
        else {
            continue;
        };
        if let Some(manifest) = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()) {
            loaded.insert(key, manifest);
        }
    }
    if !loaded.is_empty() {
        eprintln!("iw4l lan: {} map manifest(s) from {}", loaded.len(), dir.display());
    }
    loaded
}

/// Written whole, via a temp file, so a crash mid-write leaves the old one.
fn store_manifest(dir: &Path, key: &str, manifest: &Manifest) {
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{key}.json.tmp"));
        std::fs::write(&tmp, serde_json::to_vec(manifest)?)?;
        std::fs::rename(tmp, dir.join(format!("{key}.json")))
    };
    if let Err(e) = write() {
        eprintln!("iw4l lan: manifest {key} not saved to {}: {e}", dir.display());
    }
}

/// The cache seed kept beside the manifests, if a player has posted one.
fn load_cache_seed(dir: Option<&Path>) -> Option<CacheSeed> {
    let dir = dir?;
    let bytes = std::fs::read(dir.join(SEED_FILE)).ok()?;
    let records = std::fs::read_to_string(dir.join(SEED_RECORDS_FILE))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    eprintln!(
        "iw4l lan: cache seed of {records} records ({:.1} MB) from {}",
        bytes.len() as f64 / 1e6,
        dir.display()
    );
    Some(CacheSeed { records, bytes })
}

fn store_cache_seed(dir: &Path, seed: &CacheSeed) {
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{SEED_FILE}.tmp"));
        std::fs::write(&tmp, &seed.bytes)?;
        std::fs::rename(tmp, dir.join(SEED_FILE))?;
        std::fs::write(dir.join(SEED_RECORDS_FILE), seed.records.to_string())
    };
    if let Err(e) = write() {
        eprintln!("iw4l lan: cache seed not saved to {}: {e}", dir.display());
    }
}

async fn cache_seed_get(State(lan): State<Lan>) -> Response {
    let seed = lan.cache_seed.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(seed) = seed.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_ENCODING, "gzip")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-records", seed.records)
        .body(axum::body::Body::from(seed.bytes.clone()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// A player's cache pack replaces the kept one when it holds at least as
/// many records: caches only grow, so the richest player's is the most
/// useful to hand out.
async fn cache_seed_put(
    State(lan): State<Lan>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> StatusCode {
    if body.len() > MAX_SEED_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    if body.len() < 2 || body[0] != 0x1f || body[1] != 0x8b {
        return StatusCode::BAD_REQUEST;
    }
    let records: u64 = headers
        .get("x-records")
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    if records == 0 {
        return StatusCode::BAD_REQUEST;
    }
    let mut kept = lan.cache_seed.lock().unwrap_or_else(PoisonError::into_inner);
    if kept.as_ref().is_some_and(|seed| seed.records > records) {
        return StatusCode::OK;
    }
    let seed = CacheSeed {
        records,
        bytes: body.to_vec(),
    };
    if let Some(dir) = &lan.manifest_dir {
        store_cache_seed(dir, &seed);
    }
    *kept = Some(seed);
    StatusCode::NO_CONTENT
}

fn manifest_key(map: &str) -> Option<String> {
    let plain = !map.is_empty()
        && map.len() <= 64
        && map.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    plain.then(|| map.to_ascii_lowercase())
}

async fn manifest_get(State(lan): State<Lan>, RoutePath(map): RoutePath<String>) -> Response {
    let Some(key) = manifest_key(&map) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let manifests = lan.manifests.lock().unwrap_or_else(PoisonError::into_inner);
    match manifests.get(&key) {
        Some(manifest) => axum::Json(manifest).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Players post what their engine read; the union is kept, so a player who
/// left early cannot shrink what the next one fetches.
async fn manifest_put(State(lan): State<Lan>, RoutePath(map): RoutePath<String>, body: Bytes) -> StatusCode {
    let Some(key) = manifest_key(&map) else {
        return StatusCode::BAD_REQUEST;
    };
    if body.len() > MAX_MANIFEST_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    let Ok(posted) = serde_json::from_slice::<Manifest>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let mut manifests = lan.manifests.lock().unwrap_or_else(PoisonError::into_inner);
    let kept = manifests.entry(key.clone()).or_default();
    for (path, runs) in posted {
        let merged = kept.entry(path).or_default();
        merged.extend(runs.into_iter().filter(|(first, last)| first <= last));
        merged.sort_unstable();
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(merged.len());
        for (first, last) in merged.drain(..) {
            match out.last_mut() {
                Some((_, end)) if first <= *end + 1 => *end = (*end).max(last),
                _ => out.push((first, last)),
            }
        }
        *merged = out;
    }
    if let Some(dir) = &lan.manifest_dir {
        store_manifest(dir, &key, kept);
    }
    StatusCode::NO_CONTENT
}

async fn games_index(State(lan): State<Lan>) -> Response {
    let root = lan.games.clone();
    let listed = tokio::task::spawn_blocking(move || {
        let mut entries = Vec::new();
        walk(&root, &root, &mut entries);
        entries
    })
    .await;
    match listed {
        Ok(listed) => axum::Json(listed).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<serde_json::Value>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            walk(root, &path, out);
        } else if meta.is_file() {
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_millis() as u64);
            out.push(serde_json::json!({
                "path": rel.to_string_lossy().replace('\\', "/"),
                "size": meta.len(),
                "modified": modified,
            }));
        }
    }
}

/// Pages opened with `?dev` mirror their console here, tagged with the PC.
async fn log(
    State(lan): State<Lan>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> StatusCode {
    let mut line = format!("[{}] ", peer.ip()).into_bytes();
    line.extend_from_slice(body.trim_ascii_end());
    line.push(b'\n');
    let path = lan.log.clone();
    let _ = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(&line))
    })
    .await;
    StatusCode::NO_CONTENT
}

async fn ws_upgrade(
    State(lan): State<Lan>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let peer = visitor(&headers, peer);
    upgrade
        .max_message_size(MAX_BOOTSTRAP_STREAM_BYTES + 64)
        .on_upgrade(move |socket| async move {
            let _ = writeln_err(format!("bridge {peer} open"));
            let reason = match bridge(socket, &lan, peer.clone()).await {
                Ok(()) => "closed".to_owned(),
                Err(error) => error.to_string(),
            };
            let _ = writeln_err(format!("bridge {peer} {reason}"));
        })
}

fn writeln_err(line: String) -> std::io::Result<()> {
    use std::io::Write;
    writeln!(std::io::stderr(), "{line}")
}

fn frame(lane: u8, bytes: &[u8]) -> Message {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.push(lane);
    out.extend_from_slice(bytes);
    Message::Binary(out.into())
}

/// One browser's WebSocket as one QUIC connection to the relay.
/// Per-bridge message counts, logged every 30 s while they move.
#[derive(Default)]
struct Traffic {
    /// Browser to relay, by lane.
    up: [AtomicU64; 3],
    /// Relay to browser, by lane.
    down: [AtomicU64; 3],
    /// Relay datagrams dropped because the browser's socket fell behind.
    down_dropped: AtomicU64,
}

impl Traffic {
    fn line(&self) -> String {
        let n = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        format!(
            "up ctrl={} dgram={} uni={} | down ctrl={} dgram={} uni={} dropped={}",
            n(&self.up[0]),
            n(&self.up[1]),
            n(&self.up[2]),
            n(&self.down[0]),
            n(&self.down[1]),
            n(&self.down[2]),
            n(&self.down_dropped),
        )
    }
}

/// Cloudflare closes a WebSocket after 100 s without traffic, and a page that
/// is loading a map can send nothing for longer. Pings keep it open.
const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(20);

async fn bridge(socket: WebSocket, lan: &Lan, peer: String) -> Result<()> {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(BRIDGE_QUEUE);
    let mut tasks = tokio::task::JoinSet::<Result<()>>::new();
    tasks.spawn(async move {
        while let Some(message) = out_rx.recv().await {
            ws_tx.send(message).await?;
        }
        Ok(())
    });
    tasks.spawn({
        let out_tx = out_tx.clone();
        async move {
            let mut beat = tokio::time::interval(KEEPALIVE);
            beat.tick().await;
            loop {
                beat.tick().await;
                if out_tx.send(Message::Ping(Bytes::new())).await.is_err() {
                    return Ok(());
                }
            }
        }
    });
    // The relay expects Hello soon after the handshake, and a page can stall
    // between opening the socket and writing it: connect on its first message.
    let first = loop {
        match ws_rx.next().await {
            None => return Ok(()),
            Some(message) => match message? {
                Message::Binary(data) => break data,
                Message::Close(_) => return Ok(()),
                _ => {}
            },
        }
    };
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
    endpoint.set_default_client_config(lan.client.clone());
    let connection = endpoint.connect(lan.relay, SERVER_NAME)?.await?;
    let mut first = Some(first);
    let traffic = Arc::new(Traffic::default());
    tasks.spawn({
        let traffic = Arc::clone(&traffic);
        let peer = peer.clone();
        async move {
            let mut last = String::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let line = traffic.line();
                if line != last {
                    let _ = writeln_err(format!("bridge {peer} traffic: {line}"));
                    last = line;
                }
            }
        }
    });
    tasks.spawn({
        let connection = connection.clone();
        let out_tx = out_tx.clone();
        let traffic = Arc::clone(&traffic);
        async move {
            loop {
                let bytes = connection.read_datagram().await?;
                // Datagrams may be lost; a slow browser loses them here.
                match out_tx.try_send(frame(LANE_DATAGRAM, &bytes)) {
                    Ok(()) => traffic.down[1].fetch_add(1, Ordering::Relaxed),
                    Err(_) => traffic.down_dropped.fetch_add(1, Ordering::Relaxed),
                };
            }
        }
    });
    tasks.spawn({
        let connection = connection.clone();
        let out_tx = out_tx.clone();
        let traffic = Arc::clone(&traffic);
        async move {
            loop {
                let mut recv = connection.accept_uni().await?;
                let out_tx = out_tx.clone();
                let traffic = Arc::clone(&traffic);
                tokio::spawn(async move {
                    if let Ok(bytes) = recv.read_to_end(MAX_BOOTSTRAP_STREAM_BYTES).await
                        && out_tx.send(frame(LANE_UNI, &bytes)).await.is_ok()
                    {
                        traffic.down[2].fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        }
    });

    let mut control: Option<quinn::SendStream> = None;
    let outcome: Result<()> = loop {
        tokio::select! {
            message = async {
                match first.take() {
                    Some(data) => Some(Ok(Message::Binary(data))),
                    None => ws_rx.next().await,
                }
            } => {
                let Some(message) = message else { break Ok(()) };
                let data = match message? {
                    Message::Binary(data) => data,
                    Message::Close(_) => break Ok(()),
                    _ => continue,
                };
                let Some((&lane, bytes)) = data.split_first() else { continue };
                if let Some(count) = traffic.up.get(usize::from(lane)) {
                    count.fetch_add(1, Ordering::Relaxed);
                }
                match lane {
                    LANE_CONTROL => {
                        if control.is_none() {
                            let (send, mut recv) = connection.open_bi().await?;
                            let out_tx = out_tx.clone();
                            let traffic = Arc::clone(&traffic);
                            tasks.spawn(async move {
                                let mut buf = vec![0_u8; 64 * 1024];
                                while let Some(n) = recv.read(&mut buf).await? {
                                    out_tx.send(frame(LANE_CONTROL, &buf[..n])).await?;
                                    traffic.down[0].fetch_add(1, Ordering::Relaxed);
                                }
                                Ok(())
                            });
                            control = Some(send);
                        }
                        control.as_mut().expect("control stream").write_all(bytes).await?;
                    }
                    LANE_DATAGRAM => {
                        connection.send_datagram(Bytes::copy_from_slice(bytes))?;
                    }
                    LANE_UNI => {
                        let mut send = connection.open_uni().await?;
                        let bytes = Bytes::copy_from_slice(bytes);
                        tokio::spawn(async move {
                            let _ = send.set_priority(-32);
                            if send.write_all(&bytes).await.is_ok() {
                                let _ = send.finish();
                            }
                        });
                    }
                    other => break Err(format!("unknown lane {other}").into()),
                }
            }
            done = tasks.join_next() => {
                match done {
                    Some(Ok(Err(error))) => break Err(error),
                    Some(Err(error)) => break Err(error.into()),
                    Some(Ok(Ok(()))) | None => break Ok(()),
                }
            }
        }
    };
    // Tell the page why, so its fault names the relay's reason.
    let reason = connection
        .close_reason()
        .map(|reason| reason.to_string())
        .or_else(|| outcome.as_ref().err().map(ToString::to_string))
        .unwrap_or_else(|| "closed".to_owned());
    connection.close(0_u8.into(), b"bridge closed");
    let mut reason = reason;
    reason.truncate(120);
    let _ = out_tx
        .send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: reason.into(),
        })))
        .await;
    drop(out_tx);
    // Let the writer flush the close frame.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    tasks.abort_all();
    outcome
}
