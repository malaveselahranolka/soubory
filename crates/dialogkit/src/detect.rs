//! Recognises Windows file dialogs and finds where our button belongs.
use crate::util::*;
use windows::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{Com::*, SystemInformation::*, Threading::*, Variant::VARIANT},
        UI::{Accessibility::*, HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    core::*,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Vista+ Common Item Dialog (IFileOpenDialog / IFileSaveDialog).
    Item,
    /// Explorer-style GetOpenFileName / GetSaveFileName.
    Legacy,
}

/// Classifies a top-level window. Children appear a little after the window shows, so callers
/// retry a few times before giving up.
pub fn classify(h: HWND) -> Option<Kind> {
    unsafe {
        if !IsWindow(Some(h)).as_bool() || !IsWindowVisible(h).as_bool() || class_of(h) != "#32770" {
            return None;
        }
    }
    let kids = descendants(h);
    let has = |c: &str| kids.iter().any(|&k| class_of(k) == c);
    if !has("SHELLDLL_DefView") {
        return None;
    }
    Some(if has("DUIViewWndClassName") { Kind::Item } else { Kind::Legacy })
}

pub fn is_dialog_shell(h: HWND) -> bool {
    class_of(h) == "#32770"
}

#[derive(Debug, Clone)]
pub struct Target {
    pub pid: u32,
    pub exe: String,
    pub is32: bool,
    /// The target runs at a higher integrity level (elevated): UIPI blocks hooks and messages.
    pub above_us: bool,
}

pub fn target(h: HWND) -> Target {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    let mut t = Target { pid, exe: String::new(), is32: false, above_us: true };
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return t };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        if QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            t.exe = full.rsplit('\\').next().unwrap_or("").to_string();
        }
        let mut machine = IMAGE_FILE_MACHINE::default();
        if IsWow64Process2(process, &mut machine, None).is_ok() {
            t.is32 = machine != IMAGE_FILE_MACHINE_UNKNOWN;
        }
        if let (Some(theirs), Some(ours)) = (integrity_of(process), integrity_of(GetCurrentProcess())) {
            t.above_us = theirs > ours;
        }
        let _ = CloseHandle(process);
    }
    t
}

fn integrity_of(process: HANDLE) -> Option<u32> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let mut size = 0u32;
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut size);
        let mut buf = vec![0u8; size as usize];
        let ok = GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr() as *mut _), size, &mut size).is_ok();
        let _ = CloseHandle(token);
        if !ok {
            return None;
        }
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        Some(*GetSidSubAuthority(label.Label.Sid, (count - 1) as u32))
    }
}

pub fn uia() -> Option<IUIAutomation> {
    unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok() }
}

/// Screen rectangle where our button goes.
/// * Item dialogs: right after the last button of the left command group ("Organize", "New folder").
/// * Legacy dialogs: under the Places bar, or above the bottom-left corner.
pub fn anchor(h: HWND, kind: Kind, automation: &IUIAutomation) -> Option<RECT> {
    match kind {
        Kind::Item => item_anchor(h, automation),
        Kind::Legacy => legacy_anchor(h),
    }
}

fn item_anchor(h: HWND, automation: &IUIAutomation) -> Option<RECT> {
    unsafe {
        let root = automation.ElementFromHandle(h).ok()?;
        let cond = automation.CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from("FolderBandModuleInner")).ok()?;
        let module = root.FindFirst(TreeScope_Descendants, &cond).ok()?;
        let module_rect = module.CurrentBoundingRectangle().ok()?;
        let all = module.FindAll(TreeScope_Descendants, &automation.CreateTrueCondition().ok()?).ok()?;
        let mut best: Option<RECT> = None;
        for i in 0..all.Length().unwrap_or(0) {
            let Ok(e) = all.GetElement(i) else { continue };
            // The left group uses AJOSplitButton; view and help buttons sit on the far right.
            if e.CurrentClassName().map(|c| c.to_string()).unwrap_or_default() != "AJOSplitButton" {
                continue;
            }
            let Ok(r) = e.CurrentBoundingRectangle() else { continue };
            if r.right > r.left && best.is_none_or(|b| r.right > b.right) {
                best = Some(r);
            }
        }
        let b = best.unwrap_or(RECT { left: module_rect.left, top: module_rect.top + 3, right: module_rect.left, bottom: module_rect.bottom - 3 });
        Some(RECT { left: b.right, top: b.top, right: b.right, bottom: b.bottom })
    }
}

fn legacy_anchor(h: HWND) -> Option<RECT> {
    unsafe {
        let scale: f32 = GetDpiForWindow(h) as f32 / 96.0;
        let height = (28.0 * scale).round() as i32;
        // Places bar is ctl1 (0x4A0); place the button right under it.
        if let Ok(places) = GetDlgItem(Some(h), 0x4A0) {
            let mut r = RECT::default();
            if IsWindowVisible(places).as_bool() && GetWindowRect(places, &mut r).is_ok() {
                let top = r.bottom + (8.0 * scale) as i32;
                return Some(RECT { left: r.left, top, right: r.left, bottom: top + height });
            }
        }
        let mut r = RECT::default();
        GetWindowRect(h, &mut r).ok()?;
        let left = r.left + (12.0 * scale) as i32;
        let bottom = r.bottom - (14.0 * scale) as i32;
        Some(RECT { left, top: bottom - height, right: left, bottom })
    }
}

/// Address-bar fallback when the hook cannot run: returns a real path only when the dialog shows one.
pub fn address_bar_path(h: HWND) -> Option<String> {
    let kids = descendants(h);
    let crumb = kids.iter().copied().find(|&k| class_of(k) == "Breadcrumb Parent")?;
    let text = descendants(crumb).into_iter().find(|&k| class_of(k) == "ToolbarWindow32").map(text_of)?;
    let value = text.split_once(": ").map(|x| x.1.to_string()).unwrap_or(text);
    let b = value.as_bytes();
    let looks_like_path = (b.len() >= 3 && b[1] == b':' && b[2] == b'\\') || value.starts_with("\\\\");
    (looks_like_path && std::path::Path::new(&value).is_dir()).then_some(value)
}
