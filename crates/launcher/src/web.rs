//! Browser entry. The page mounts the player's own game folder at
//! [`GAMES_ROOT`] before the module starts; nothing from the games ships with
//! the page.
//!
//! Two ways in: the module's own start (`main`) when it is instantiated on
//! the page, or [`iw4l_worker_main`] when the page runs the engine in a Web
//! Worker and hands over its canvas (see `web/engine-worker.js`).

use std::path::PathBuf;

use wasm_bindgen::prelude::*;

/// Where the page's virtual file system mounts the player's games folder.
const GAMES_ROOT: &str = "/games";
/// Saves, caches and logs; in memory for the life of the tab.
const ARTIFACTS_ROOT: &str = "/iw4l-artifacts";

pub fn main() {
    // No window: a worker instantiated the module and will call
    // `iw4l_worker_main` with the canvas and the page's address.
    let Some(window) = web_sys::window() else {
        return;
    };
    let location = window.location();
    let search = location.search().unwrap_or_default();
    let protocol = location.protocol().unwrap_or_default();
    let host = location.host().unwrap_or_default();
    boot(&search, &protocol, &host);
}

/// The engine inside a Web Worker: `canvas` is the page's canvas transferred
/// as an `OffscreenCanvas`, `search`/`protocol`/`host` the page's address
/// (the worker has no `location` of the page's), the size in physical pixels.
#[wasm_bindgen]
pub fn iw4l_worker_main(
    canvas: JsValue,
    search: String,
    protocol: String,
    host: String,
    width: u32,
    height: u32,
    scale: f64,
) {
    #[cfg(target_feature = "atomics")]
    web_threads::web::init();
    bootstrap::web_worker::install(canvas, width, height, scale);
    boot(&search, &protocol, &host);
}

/// A thread's entry (`web/engine-thread.js`): runs the closure
/// `web_threads::web::spawn` boxed behind `ptr`.
#[cfg(target_feature = "atomics")]
#[wasm_bindgen]
pub fn iw4l_thread_entry(ptr: u32) {
    web_threads::web::run_entry(ptr);
}

/// After `iw4l_thread_entry`: the address of the word a resident thread's
/// script waits on, 0 when the thread is not resident
/// (`web_threads::web::spawn_resident`).
#[cfg(target_feature = "atomics")]
#[wasm_bindgen]
pub fn iw4l_thread_resident() -> u32 {
    web_threads::web::resident_word()
}

/// One step of a resident thread, each time its word changes.
#[cfg(target_feature = "atomics")]
#[wasm_bindgen]
pub fn iw4l_thread_step() {
    web_threads::web::resident_step();
}

/// Input the page forwards to the worker; see `bootstrap::web_worker::input`.
#[wasm_bindgen]
pub fn iw4l_worker_input(kind: u32, a: f64, b: f64, c: f64, s: &str) {
    bootstrap::web_worker::input(kind, a, b, c, s);
}

fn boot(search: &str, protocol: &str, host: &str) {
    console_error_panic_hook::set_once();
    let artifacts = PathBuf::from(ARTIFACTS_ROOT);
    let log = diag::init_log(&artifacts);
    diag::info!(Launch, "log: {}", log.display());
    let params = web_sys::UrlSearchParams::new_with_str(search).ok();
    if params.as_ref().and_then(|params| params.get("frames")).as_deref() == Some("every") {
        bootstrap::web_worker::set_even_pacing(false);
        diag::info!(Launch, "frames: every refresh (no even pacing)");
    }
    if params.as_ref().and_then(|params| params.get("pipeline")).as_deref() == Some("0") {
        bootstrap::web_worker::set_pipeline(false);
    }
    if params.as_ref().and_then(|params| params.get("frames")).as_deref() == Some("free") {
        bootstrap::web_worker::set_free_running(true);
        diag::info!(Launch, "frames: free-running");
    }
    #[cfg(target_feature = "atomics")]
    if let Some(threads) = params
        .as_ref()
        .and_then(|params| params.get("framepool"))
        .and_then(|value| value.parse::<usize>().ok())
    {
        web_threads::web::set_frame_workers(threads);
        diag::info!(Launch, "frame pool: {threads} worker threads asked for");
    }
    let args = query_args(params.as_ref());
    lan_launch(params.as_ref(), protocol, host);
    console::set_launch_args(args.clone());
    let (mode, acceptance, cheats) =
        bootstrap::parse_cli(args.into_iter()).unwrap_or_else(|e| diag::exit_launch_error(&e));
    asset_transport::set_games_root(PathBuf::from(GAMES_ROOT));
    let games = asset_transport::GamesRoot(PathBuf::from(GAMES_ROOT));
    bootstrap::launch(games, artifacts, mode, acceptance, cheats);
}

/// A page served by `iw4l-master lan` adds `lan` to its URL: the relay is
/// that server's WebSocket bridge, and `host=NAME` or `join=ROOM` (with an
/// optional `password`) stand in for `IW4L_MASTER_HOST_NAME` /
/// `IW4L_MASTER_JOIN` / `IW4L_MASTER_PASSWORD`.
fn lan_launch(params: Option<&web_sys::UrlSearchParams>, protocol: &str, host: &str) {
    let Some(params) = params.filter(|params| params.has("lan")) else {
        return;
    };
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    let url = format!("{scheme}://{host}/ws");
    if let Err(error) = updater::select_web("LAN", &url) {
        diag::warn!(Launch, "lan: {error}");
        return;
    }
    let mut vars = Vec::new();
    for (param, var) in [
        ("host", "IW4L_MASTER_HOST_NAME"),
        ("join", "IW4L_MASTER_JOIN"),
        ("password", "IW4L_MASTER_PASSWORD"),
        ("players", "IW4L_MASTER_MAX_PLAYERS"),
        ("mode", "IW4L_GAMETYPE"),
        ("name", "IW4L_PLAYER_NAME"),
        ("time", "IW4L_TIME_LIMIT_MINUTES"),
        ("next", "IW4L_MASTER_NEXT_MATCH_SECS"),
        ("rotation", "IW4L_MASTER_ROTATION"),
        ("textures", "IW4L_MAX_TEXTURE"),
    ] {
        if let Some(value) = params.get(param) {
            vars.push((var.to_owned(), value));
        }
    }
    // The decoder cannot read the launch table (asset_material sits below
    // net), and the browser has no environment for IW4L_MAX_TEXTURE: hand the
    // cap over directly, before the first image is decoded.
    if let Some(cap) = params.get("textures").and_then(|value| value.trim().parse::<u32>().ok()) {
        asset_material::material_images::set_web_max_texture(cap);
        diag::info!(Launch, "texture cap {cap} px");
    }
    diag::info!(Launch, "lan relay {url}");
    net::set_web_launch(vars);
}

/// `?map=mp_boneyard&cmds=spawn 0` is `iw4l --cmds 'spawn 0' map mp_boneyard`;
/// no `map` opens the menu.
fn query_args(params: Option<&web_sys::UrlSearchParams>) -> Vec<String> {
    let get = |key: &str| params.and_then(|params| params.get(key));
    let mut args = Vec::new();
    if let Some(cmds) = get("cmds") {
        args.extend(["--cmds".to_owned(), cmds]);
    }
    match get("map") {
        Some(map) => args.extend(["map".to_owned(), map]),
        None => args.push("menu".to_owned()),
    }
    args
}
