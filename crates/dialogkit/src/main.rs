//! fm-dialogkit: adds "+ Nový ⌄" (Explorer's own New menu) to every Windows Open/Save dialog,
//! and while the Explorer redirect is on, hands File Explorer windows to fm (explorer.rs).
//!
//! Modes: no arguments = background helper with a tray icon; `--inject` = one hook round-trip
//! (used by the 64-bit helper to reach 32-bit dialogs); `--act` / `--dump` = diagnostics.
#![windows_subsystem = "windows"]

mod app;
mod detect;
mod explorer;
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
        _ => {
            let code = app::run();
            // The pill draws through Direct2D on the GPU; some drivers (Intel igd10um64xe.dll)
            // can spin forever in their process-detach code. Everything is cleaned up by now.
            unsafe {
                let _ = windows::Win32::System::Threading::TerminateProcess(windows::Win32::System::Threading::GetCurrentProcess(), code as u32);
            }
            code
        }
    };
    std::process::exit(code);
}
