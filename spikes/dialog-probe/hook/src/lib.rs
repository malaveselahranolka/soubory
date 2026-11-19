//! Injected into a file dialog's UI thread via WH_GETMESSAGE. Commands arrive as a registered message
//! posted to the dialog: wParam = reply window, lParam = cmd | (atom << 8).
//!   cmd 1: reply with the current folder.  cmd 2: select the item named by the global atom and start rename.
//! Replies go back via WM_COPYDATA. Everything runs on the dialog's own UI thread (STA), never blocks it.
use std::{ffi::c_void, sync::OnceLock};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::CoTaskMemFree, DataExchange::*},
        UI::{Shell::Common::ITEMIDLIST, Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

pub const TAG: usize = 0x4657_4450;
const CWM_GETISHELLBROWSER: u32 = WM_USER + 7;
const RETRY_TIMER: usize = 0x4657;

fn cmd_msg() -> u32 {
    static M: OnceLock<u32> = OnceLock::new();
    *M.get_or_init(|| unsafe { RegisterWindowMessageW(w!("FinderWin.DialogCmd")) })
}

struct Pending {
    dlg: HWND,
    reply: HWND,
    name: Vec<u16>,
    tries: u32,
}
// Only touched on the dialog UI thread.
static mut PENDING: Option<Pending> = None;

#[unsafe(no_mangle)]
pub unsafe extern "system" fn GetMsgProc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 && wparam.0 == PM_REMOVE.0 as usize {
            let msg = &mut *(lparam.0 as *mut MSG);
            if msg.message == cmd_msg() {
                let (dlg, reply, lp) = (msg.hwnd, HWND(msg.wParam.0 as *mut c_void), msg.lParam.0 as usize);
                msg.message = WM_NULL;
                // A panic must never unwind into the host app.
                let _ = std::panic::catch_unwind(|| handle(dlg, reply, (lp & 0xff) as u8, (lp >> 8) as u16));
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

unsafe fn handle(dlg: HWND, reply: HWND, cmd: u8, atom: u16) {
    unsafe {
        match cmd {
            1 => send_reply(reply, dlg, &folder_of(dlg).unwrap_or_else(|e| format!("ERR {e}"))),
            2 => {
                let mut buf = [0u16; 256];
                let n = GlobalGetAtomNameW(atom, &mut buf) as usize;
                if n == 0 {
                    return send_reply(reply, dlg, "ERR atom");
                }
                PENDING = Some(Pending { dlg, reply, name: buf[..n].iter().copied().chain([0]).collect(), tries: 0 });
                retry(dlg, 0, 0, 0);
            }
            _ => send_reply(reply, dlg, "ERR cmd"),
        }
    }
}

/// The new item shows up in the view only after the shell's change notification is processed on this
/// thread, so selection is retried from a timer instead of blocking.
unsafe extern "system" fn retry(_: HWND, _: u32, _: usize, _: u32) {
    unsafe {
        #[allow(static_mut_refs)]
        let Some(p) = PENDING.as_mut() else { return };
        p.tries += 1;
        match select_edit(p.dlg, &p.name) {
            Ok(()) => finish(format!("OK selected after {} tries", p.tries)),
            Err(_) if p.tries < 40 => {
                SetTimer(Some(p.dlg), RETRY_TIMER, 50, Some(retry));
            }
            Err(e) => finish(format!("ERR select: {e}")),
        }
    }
}

unsafe fn finish(text: String) {
    unsafe {
        #[allow(static_mut_refs)]
        if let Some(p) = PENDING.take() {
            let _ = KillTimer(Some(p.dlg), RETRY_TIMER);
            send_reply(p.reply, p.dlg, &text);
        }
    }
}

unsafe fn browser(dlg: HWND) -> std::result::Result<IShellView, String> {
    unsafe {
        let raw = SendMessageW(dlg, CWM_GETISHELLBROWSER, None, None);
        let ptr = raw.0 as *mut c_void;
        let sb = IShellBrowser::from_raw_borrowed(&ptr).ok_or("no IShellBrowser")?;
        sb.QueryActiveShellView().map_err(|e| e.to_string())
    }
}

unsafe fn folder_of(dlg: HWND) -> std::result::Result<String, String> {
    unsafe {
        let fv: IFolderView = browser(dlg)?.cast().map_err(|e| e.to_string())?;
        let item: IShellItem = fv.GetFolder().map_err(|e| e.to_string())?;
        Ok(format!(
            "fs={} | parse={}",
            disp(&item, SIGDN_FILESYSPATH).unwrap_or_default(),
            disp(&item, SIGDN_DESKTOPABSOLUTEPARSING).unwrap_or_default()
        ))
    }
}

unsafe fn select_edit(dlg: HWND, name: &[u16]) -> Result<()> {
    unsafe {
        let view = browser(dlg).map_err(|e| Error::new(E_FAIL, e))?;
        let fv: IFolderView = view.cast()?;
        let folder: IShellFolder = fv.GetFolder()?;
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        folder.ParseDisplayName(dlg, None, PCWSTR(name.as_ptr()), None, &mut pidl, std::ptr::null_mut())?;
        // SelectItem is a silent no-op until the view actually contains the item, so look for it first.
        let mut found = false;
        for i in 0..fv.ItemCount(SVGIO_ALLVIEW).unwrap_or(0) {
            let Ok(it) = fv.Item(i) else { continue };
            let hr = folder.CompareIDs(LPARAM(0), it, pidl);
            CoTaskMemFree(Some(it as *const c_void));
            if hr.is_ok() && (hr.0 & 0xffff) == 0 {
                found = true;
                break;
            }
        }
        let r = if found {
            view.SelectItem(pidl, (SVSI_EDIT.0 | SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0 | SVSI_ENSUREVISIBLE.0 | SVSI_FOCUSED.0) as u32)
        } else {
            Err(Error::new(HRESULT(0x8000000Au32 as i32), "not in view yet"))
        };
        CoTaskMemFree(Some(pidl as *const c_void));
        r
    }
}

fn disp(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    unsafe {
        let p = item.GetDisplayName(sigdn).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

unsafe fn send_reply(to: HWND, from: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let cds = COPYDATASTRUCT { dwData: TAG, cbData: (wide.len() * 2) as u32, lpData: wide.as_ptr() as *mut c_void };
    unsafe {
        SendMessageTimeoutW(
            to,
            WM_COPYDATA,
            WPARAM(from.0 as usize),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG,
            2000,
            None,
        );
    }
}
