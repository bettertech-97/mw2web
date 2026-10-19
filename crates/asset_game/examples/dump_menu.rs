//! Prints menus from the game's UI catalog as JSON, for writing variants:
//! `IW4L_GAMES=<mw2 dir> cargo run -p asset_game --example dump_menu -- class`.
use asset_transport::GamesRoot;

fn main() {
    let games = GamesRoot(std::env::var("IW4L_GAMES").expect("IW4L_GAMES").into());
    let (catalog, report) = asset_game::load_ui_menu_catalog(&games);
    for line in &report {
        eprintln!("{line}");
    }
    for name in std::env::args().skip(1) {
        match catalog.get(&name) {
            Some(def) => println!("{}", serde_json::to_string_pretty(def).expect("json")),
            None => eprintln!("no menu `{name}`"),
        }
    }
}
