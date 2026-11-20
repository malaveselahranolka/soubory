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
mod undo;
mod theme;
mod thumbs;
mod views;
mod win;

use std::path::PathBuf;

/// Where a window opens for a command-line argument: that folder, else the user's folder.
pub fn start_loc(arg: &str) -> app::Loc {
    Some(PathBuf::from(arg.trim().trim_matches('"')))
        .filter(|p| !arg.trim().is_empty() && p.is_dir())
        .or_else(|| fsys::known_folder(&windows::Win32::UI::Shell::FOLDERID_Profile))
        .map(app::Loc::Folder)
        .unwrap_or(app::Loc::Computer)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--render") {
        std::process::exit(render::cli(&args[1..]));
    }
    let folder = args.iter().find(|a| !a.starts_with("--")).cloned().unwrap_or_default();
    // Test runs (`--dev-*`, `--new-instance`) never join a running instance.
    let solo = args.iter().any(|a| a.starts_with("--dev-") || a == "--new-instance");
    if !solo && win::hand_over(Some(std::path::Path::new(&folder)).filter(|_| !folder.is_empty())) {
        return;
    }
    std::process::exit(win::run(start_loc(&folder)));
}
