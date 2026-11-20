#![windows_subsystem = "windows"]
//! fm — a Finder-style file manager for Windows.
//!
//! `fm [folder]` opens a window. `fm --render <out.png> [options]` paints one frame offscreen
//! (visual tests; see `render.rs`).

mod app;
mod droptarget;
mod fsys;
mod geom;
mod gfx;
mod icons;
mod layout;
mod paint;
mod prefs;
mod quicklook;
mod render;
mod search;
mod sel;
mod shell;
mod sidebar;
mod tags;
mod theme;
mod thumbs;
mod views;
mod win;

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--render") {
        std::process::exit(render::cli(&args[1..]));
    }
    let start = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| fsys::known_folder(&windows::Win32::UI::Shell::FOLDERID_Profile))
        .map(app::Loc::Folder)
        .unwrap_or(app::Loc::Computer);
    std::process::exit(win::run(start));
}
