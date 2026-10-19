//! `lobbyclasses`: the class editor on its own, for the web lobby's
//! "Create a Class" button. The page boots the engine to its menu with this
//! command; the editor opens once the menu is up, and when the player backs
//! out of it the page is told (`iw4lClassesDone`) and returns to its lobby.
//! The classes save as they are edited (`ui::classes::store`), so the next
//! match reads them.

use bevy::prelude::*;
use frame::{AppScreen, UiMenuRequest};

use crate::{CommandSpec, ConsoleCommand, ConsoleRegistry};

const EDITOR: &str = "class_setup";
/// How long to keep asking for the editor before giving up and telling the
/// page anyway (a menu catalog without it would otherwise strand the player).
const OPEN_TIMEOUT_SECS: f32 = 120.0;
/// Between open requests while the menu is not up yet.
const RETRY_SECS: f32 = 1.0;

#[derive(Default)]
pub(crate) enum LobbyClasses {
    #[default]
    Off,
    /// Asked for; waiting for the front-end menu to come up.
    Opening { waited: f32, since_ask: f32 },
    /// The editor is open over `base` menus (the front end's own).
    Open { base: usize },
    Done,
}

pub(crate) fn register(registry: &mut ConsoleRegistry) {
    registry.register(
        CommandSpec::new("lobbyclasses")
            .usage("lobbyclasses — open the class editor; closing it returns the web page to its lobby"),
    );
}

pub(crate) fn run(
    mut events: MessageReader<ConsoleCommand>,
    mut state: Local<LobbyClasses>,
    screen: Res<AppScreen>,
    menus: Option<Res<hud::ScriptMenus>>,
    catalog: Option<Res<asset_game::MenuCatalog>>,
    time: Res<Time<Real>>,
    mut requests: MessageWriter<UiMenuRequest>,
) {
    if events.read().any(|command| command.name == "lobbyclasses") {
        diag::info!(Ui, "lobby classes: opening the class editor once the menu is up");
        *state = LobbyClasses::Opening {
            waited: 0.0,
            since_ask: RETRY_SECS,
        };
    }
    let open_names = menus
        .as_deref()
        .map(hud::ScriptMenus::open_names)
        .unwrap_or_default();
    let editor_index = open_names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(EDITOR));
    match &mut *state {
        LobbyClasses::Off | LobbyClasses::Done => {}
        LobbyClasses::Opening { waited, since_ask } => {
            if let Some(base) = editor_index {
                diag::info!(Ui, "lobby classes: editor open over [{}]", open_names[..base].join(","));
                *state = LobbyClasses::Open { base };
                return;
            }
            let delta = time.delta_secs();
            *waited += delta;
            *since_ask += delta;
            if *waited > OPEN_TIMEOUT_SECS {
                diag::warn!(Ui, "lobby classes: the class editor never opened; back to the lobby");
                *state = LobbyClasses::Done;
                tell_page_done();
                return;
            }
            // The front end resets its menus when it first comes up, so ask
            // only once its own menu is on screen, and again until it shows.
            let menu_up = *screen == AppScreen::MainMenu && catalog.is_some() && !open_names.is_empty();
            if menu_up && *since_ask >= RETRY_SECS {
                *since_ask = 0.0;
                requests.write(UiMenuRequest::Open(EDITOR.into()));
            }
        }
        LobbyClasses::Open { base } => {
            // Closed: the editor (and anything it opened) is gone from the
            // stack, leaving the front end's own menus.
            if editor_index.is_none() || open_names.len() <= *base {
                diag::info!(Ui, "lobby classes: editor closed; back to the lobby");
                *state = LobbyClasses::Done;
                tell_page_done();
            }
        }
    }
}

/// The page's hook (`web/index.html`, relayed by `web/engine-worker.js` when
/// the engine runs in a worker). Natively the log line above is the signal.
fn tell_page_done() {
    #[cfg(target_arch = "wasm32")]
    web_threads::on_engine(|| {
        use wasm_bindgen::JsCast;
        let hook = js_sys::Reflect::get(&js_sys::global(), &"iw4lClassesDone".into());
        if let Ok(hook) = hook
            && let Some(hook) = hook.dyn_ref::<js_sys::Function>()
        {
            let _ = hook.call0(&wasm_bindgen::JsValue::NULL);
        }
    });
}
