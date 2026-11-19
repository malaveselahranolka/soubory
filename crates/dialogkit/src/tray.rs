//! Notification-area icon: pause/resume, quit, and balloon messages.
use crate::util::*;
use windows::{
    Win32::{
        Foundation::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

pub const WM_APP_TRAY: u32 = WM_APP + 2;
pub const CMD_PAUSE: u32 = 10;
pub const CMD_QUIT: u32 = 11;
const ID: u32 = 1;

fn icon() -> HICON {
    unsafe {
        // Resource 1 is the app icon (build.rs); fall back to the shell's folder icon.
        if let Ok(hinst) = GetModuleHandleW(None) {
            if let Ok(i) = LoadImageW(Some(hinst.into()), PCWSTR(1 as *const u16), IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR) {
                return HICON(i.0);
            }
        }
        let mut info = SHSTOCKICONINFO { cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32, ..Default::default() };
        if SHGetStockIconInfo(SIID_FOLDER, SHGSI_ICON | SHGSI_SMALLICON, &mut info).is_ok() {
            return info.hIcon;
        }
        LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
    }
}

fn base(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: ID, ..Default::default() }
}

/// NUL-terminated, truncated copy. Returned by value: NOTIFYICONDATAW is packed on x86.
fn fixed<const N: usize>(s: &str) -> [u16; N] {
    let mut out = [0u16; N];
    for (d, c) in out.iter_mut().take(N - 1).zip(s.encode_utf16()) {
        *d = c;
    }
    out
}

pub fn add(hwnd: HWND, paused: bool) {
    let mut nid = base(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    nid.hIcon = icon();
    nid.szTip = fixed(&tip(paused));
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    }
}

fn tip(paused: bool) -> String {
    if paused { "Nový v dialozích – pozastaveno".into() } else { "Nový v dialozích (Ctrl+N v okně Otevřít/Uložit)".into() }
}

pub fn set_paused(hwnd: HWND, paused: bool) {
    let mut nid = base(hwnd);
    nid.uFlags = NIF_TIP | NIF_SHOWTIP;
    nid.szTip = fixed(&tip(paused));
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

pub fn remove(hwnd: HWND) {
    let nid = base(hwnd);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

pub fn balloon(hwnd: HWND, title: &str, text: &str) {
    let mut nid = base(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO | NIIF_RESPECT_QUIET_TIME;
    nid.szInfoTitle = fixed(title);
    nid.szInfo = fixed(text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Shows the tray menu and returns the chosen command (0 if dismissed).
pub fn menu(hwnd: HWND, paused: bool) -> u32 {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let flags = if paused { MF_STRING | MF_CHECKED } else { MF_STRING };
        let _ = AppendMenuW(menu, flags, CMD_PAUSE as usize, PCWSTR(wide("Pozastavit").as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, PCWSTR(wide("Ukončit").as_ptr()));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0, pt.x, pt.y, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as u32
    }
}
