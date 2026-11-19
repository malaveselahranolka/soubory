//! fm-dialogkit: adds "+ Nový ⌄" (Explorer's own New menu) to every Windows Open/Save dialog.
//!
//! Modes: no arguments = background helper with a tray icon; `--inject` = one hook round-trip
//! (used by the 64-bit helper to reach 32-bit dialogs); `--act` / `--dump` = diagnostics.
#![windows_subsystem = "windows"]

mod app;
mod detect;
mod inject;
mod newmenu;
mod pill;
mod tray;
mod util;

fn attach_console() {
    use windows::Win32::System::Console::*;
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn dump() -> i32 {
    use windows::{Win32::{Foundation::*, UI::WindowsAndMessaging::*}, core::BOOL};
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        unsafe { (*(l.0 as *mut Vec<HWND>)).push(h) };
        BOOL(1)
    }
    let mut tops: Vec<HWND> = vec![];
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut tops as *mut _ as isize));
    }
    for h in tops {
        if let Some(kind) = detect::classify(h) {
            let t = detect::target(h);
            println!("{}\t{kind:?}\tpid={}\t{}\t{}-bit\televated={}\t{}", h.0 as usize, t.pid, t.exe, if t.is32 { 32 } else { 64 }, t.above_us, util::text_of(h));
        }
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("--inject") => inject::cli(&args[2..]),
        Some("--act") => {
            attach_console();
            app::act_cli(&args[2..])
        }
        Some("--dump") => {
            attach_console();
            dump()
        }
        _ => app::run(),
    };
    std::process::exit(code);
}
