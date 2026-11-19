//! Small Win32 helpers and the log file.
use std::{io::Write, path::PathBuf};
use windows::{
    Win32::{Foundation::*, UI::WindowsAndMessaging::*},
    core::*,
};

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub fn class_of(h: HWND) -> String {
    let mut b = [0u16; 256];
    let n = unsafe { GetClassNameW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

pub fn text_of(h: HWND) -> String {
    let mut b = [0u16; 1024];
    let n = unsafe { GetWindowTextW(h, &mut b) };
    String::from_utf16_lossy(&b[..n.max(0) as usize])
}

pub fn descendants(parent: HWND) -> Vec<HWND> {
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        unsafe { (*(l.0 as *mut Vec<HWND>)).push(h) };
        BOOL(1)
    }
    let mut v = Vec::new();
    unsafe {
        let _ = EnumChildWindows(Some(parent), Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("fm")
}

/// Appends one line to `%LOCALAPPDATA%\fm\dialogkit.log` (truncated when it passes 1 MB).
pub fn log(line: impl AsRef<str>) {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("dialogkit.log");
    if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{secs} {}", line.as_ref());
    }
}

pub fn rect_w(r: &RECT) -> i32 {
    r.right - r.left
}

pub fn rect_h(r: &RECT) -> i32 {
    r.bottom - r.top
}

/// True when the user picked dark mode for apps.
pub fn apps_use_dark_theme() -> bool {
    use windows::Win32::System::Registry::*;
    let mut value = 1u32;
    let mut size = 4u32;
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = wide("AppsUseLightTheme");
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r.is_ok() && value == 0
}

pub fn high_contrast() -> bool {
    use windows::Win32::UI::Accessibility::*;
    let mut hc = HIGHCONTRASTW { cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    unsafe {
        SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, Some(&mut hc as *mut _ as *mut _), Default::default()).is_ok()
            && (hc.dwFlags.0 & HCF_HIGHCONTRASTON.0) != 0
    }
}

/// Scale factor of the monitor the window is on. Used instead of the window's own DPI: a
/// DPI-unaware dialog reports 96 but is stretched on screen, and our button must match what is shown.
pub fn monitor_scale(h: HWND) -> f32 {
    use windows::Win32::{Graphics::Gdi::*, UI::HiDpi::*};
    unsafe {
        let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
        let (mut x, mut y) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_err() {
            x = 96;
        }
        (x as f32 / 96.0).max(1.0)
    }
}
