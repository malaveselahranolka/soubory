#![windows_subsystem = "windows"]
//! fm — a Finder-style file manager for Windows.
//!
//! `fm [place]` opens a window on a folder, file or shell place in Explorer's syntax (`target.rs`). `fm --render <out.png> [options]` paints one frame offscreen
//! (visual tests; see `render.rs`). `fm --set-default` / `fm --restore-explorer` make fm the
//! default for folders for the current user and undo it (`defaultfm.rs`; the installer runs them).

mod app;
mod batch;
mod brand;
mod defaultfm;
mod droptarget;
mod fsys;
mod geom;
mod gfx;
mod groups;
mod fileicons;
mod icons;
mod layout;
mod paint;
mod phosphor;
mod palette;
mod prefs;
mod quicklook;
mod render;
mod search;
mod sel;
mod shell;
mod shellns;
mod shellwin;
mod sidebar;
mod svgpath;
mod tags;
mod target;
mod uia;
mod undo;
mod theme;
mod thumbs;
mod views;
mod win;

fn main() {
    win::trace("main");
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--render") {
        std::process::exit(render::cli(&args[1..]));
    }
    // Before the hub: these never join or become the resident fm.
    match args.first().map(String::as_str) {
        Some("--set-default") => std::process::exit(defaultfm::cli(true)),
        Some("--restore-explorer") => std::process::exit(defaultfm::cli(false)),
        _ => {}
    }
    if args.iter().any(|a| a == "--quit") {
        win::ask_quit();
        return;
    }
    // Test runs (`--dev-*`, `--new-instance`) never join a running instance and end with their
    // last window; any other fm stays resident like Explorer (`--background`: no window, at logon).
    let solo = args.iter().any(|a| a.starts_with("--dev-") || a == "--new-instance");
    let background = args.iter().any(|a| a == "--background");
    let raw = target::raw_tail();
    let cwd = std::env::current_dir().unwrap_or_default();
    let resident = !solo
        && match win::hand_over(&cwd, &raw, background, win::is_elevated()) {
            win::Join::Owner => true,
            win::Join::HandedOver => return,
            win::Join::Unjoinable if background => return,
            win::Join::Unjoinable => false,
        };
    if resident {
        // Living on, it must not hold the launcher's folder (a USB stick, a repo) open: the
        // folder Explorer and sign-in launches start in. `cwd` keeps resolving this line.
        if let Some(sys) = std::env::var_os("SystemRoot") {
            let _ = std::env::set_current_dir(std::path::Path::new(&sys).join("System32"));
        }
        // Nor its environment (a terminal's, an installer's).
        win::refresh_env();
    }
    let code = win::run(&raw, &cwd, background, resident);
    // Everything is saved by now (prefs on close, the index in search::stop, the clipboard
    // flushed). Leave without running any DLL's exit code: the Intel D3D11 driver
    // (igd10um64xe.dll) can spin forever in its process-detach after ExitProcess has killed the
    // thread it waits for, leaving a windowless fm at 100% of a core that even Task Manager
    // cannot end.
    unsafe {
        let _ = windows::Win32::System::Threading::TerminateProcess(windows::Win32::System::Threading::GetCurrentProcess(), code as u32);
    }
    std::process::exit(code);
}
