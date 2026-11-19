//! Loaded on demand into a file dialog's UI thread through a `WH_GETMESSAGE` thread hook.
//!
//! Protocol: the helper posts [`CMD_MESSAGE`] to the dialog's top-level window with
//! `wParam` = the helper's reply window and `lParam` = `cmd | (atom << 8)`. The DLL answers with
//! `WM_COPYDATA` (`dwData` = [`REPLY_TAG`], UTF-16 text). Replies:
//! * [`CMD_QUERY`]  → `"OK\t<filesystem path or empty>\t<desktop-absolute parsing name>"`
//! * [`CMD_SELECT`] → `"OK"` once the item named by the global atom is selected in rename mode.
//! * anything failing → `"ERR\t<reason>"`.
//!
//! Everything runs on the dialog's own thread and never blocks it: selection is retried from a
//! timer until the shell view has picked up the new item. Panics never cross into the host.
#![allow(clippy::missing_safety_doc)]

use std::{ffi::c_void, sync::OnceLock};

/// Registered window message name shared with the helper.
pub const CMD_MESSAGE_NAME: &str = "FinderWin.DialogCmd.v1";
/// `COPYDATASTRUCT::dwData` of every reply.
pub const REPLY_TAG: usize = 0x4657_4431;
pub const CMD_QUERY: u8 = 1;
pub const CMD_SELECT: u8 = 2;
/// Name of the exported hook procedure.
pub const HOOK_PROC_NAME: &str = "FmDialogGetMsgProc";

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::{
        Win32::{
            Foundation::*,
            System::{Com::CoTaskMemFree, DataExchange::*, LibraryLoader::*},
            UI::{Shell::Common::ITEMIDLIST, Shell::*, WindowsAndMessaging::*},
        },
        core::*,
    };

    const CWM_GETISHELLBROWSER: u32 = WM_USER + 7;
    const RETRY_TIMER: usize = 0x464D_0001;
    const RETRY_MS: u32 = 40;
    const MAX_TRIES: u32 = 50; // 2 s: the shell view picks up new items asynchronously

    fn cmd_message() -> u32 {
        static M: OnceLock<u32> = OnceLock::new();
        *M.get_or_init(|| {
            let name: Vec<u16> = CMD_MESSAGE_NAME.encode_utf16().chain([0]).collect();
            unsafe { RegisterWindowMessageW(PCWSTR(name.as_ptr())) }
        })
    }

    struct Pending {
        dlg: HWND,
        reply: HWND,
        name: Vec<u16>,
        tries: u32,
    }

    thread_local! {
        static PENDING: std::cell::RefCell<Option<Pending>> = const { std::cell::RefCell::new(None) };
    }

    /// Keeps the DLL mapped for the life of the host: a retry timer may still point into it after
    /// the helper removed the hook, and unloading code under a live timer would crash the host.
    fn pin_self() {
        static PINNED: OnceLock<()> = OnceLock::new();
        PINNED.get_or_init(|| unsafe {
            let mut module = HMODULE::default();
            let _ = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                PCWSTR(pin_self as *const () as *const u16),
                &mut module,
            );
        });
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "system" fn FmDialogGetMsgProc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            if code >= 0 && wparam.0 == PM_REMOVE.0 as usize && lparam.0 != 0 {
                let msg = &mut *(lparam.0 as *mut MSG);
                if msg.message == cmd_message() {
                    let (dlg, reply, lp) = (msg.hwnd, HWND(msg.wParam.0 as *mut c_void), msg.lParam.0 as usize);
                    msg.message = WM_NULL;
                    let _ = std::panic::catch_unwind(|| {
                        pin_self();
                        handle(dlg, reply, (lp & 0xff) as u8, ((lp >> 8) & 0xffff) as u16)
                    });
                }
            }
            CallNextHookEx(None, code, wparam, lparam)
        }
    }

    unsafe fn handle(dlg: HWND, reply: HWND, cmd: u8, atom: u16) {
        unsafe {
            match cmd {
                CMD_QUERY => send_reply(reply, dlg, &query(dlg).unwrap_or_else(|e| format!("ERR\t{e}"))),
                CMD_SELECT => {
                    let mut buf = [0u16; 256];
                    let n = GlobalGetAtomNameW(atom, &mut buf) as usize;
                    if n == 0 {
                        return send_reply(reply, dlg, "ERR\tatom");
                    }
                    // A newer request replaces an unfinished one.
                    finish(None);
                    PENDING.with(|p| {
                        *p.borrow_mut() = Some(Pending {
                            dlg,
                            reply,
                            name: buf[..n].iter().copied().chain([0]).collect(),
                            tries: 0,
                        })
                    });
                    retry(dlg, 0, 0, 0);
                }
                _ => send_reply(reply, dlg, "ERR\tcmd"),
            }
        }
    }

    unsafe extern "system" fn retry(_: HWND, _: u32, _: usize, _: u32) {
        let _ = std::panic::catch_unwind(|| unsafe {
            let step = PENDING.with(|p| {
                let mut p = p.borrow_mut();
                let p = p.as_mut()?;
                p.tries += 1;
                Some((p.dlg, p.name.clone(), p.tries))
            });
            let Some((dlg, name, tries)) = step else { return };
            match select_edit(dlg, &name) {
                Ok(()) => finish(Some("OK".into())),
                Err(_) if tries < MAX_TRIES => {
                    SetTimer(Some(dlg), RETRY_TIMER, RETRY_MS, Some(retry));
                }
                Err(e) => finish(Some(format!("ERR\tselect: {}", e.message()))),
            }
        });
    }

    unsafe fn finish(text: Option<String>) {
        unsafe {
            if let Some(p) = PENDING.with(|p| p.borrow_mut().take()) {
                let _ = KillTimer(Some(p.dlg), RETRY_TIMER);
                send_reply(p.reply, p.dlg, text.as_deref().unwrap_or("ERR\tsuperseded"));
            }
        }
    }

    unsafe fn active_view(dlg: HWND) -> Result<IShellView> {
        unsafe {
            let raw = SendMessageW(dlg, CWM_GETISHELLBROWSER, None, None);
            let ptr = raw.0 as *mut c_void;
            let browser = IShellBrowser::from_raw_borrowed(&ptr).ok_or_else(|| Error::new(E_NOINTERFACE, "no IShellBrowser"))?;
            browser.QueryActiveShellView()
        }
    }

    unsafe fn query(dlg: HWND) -> Result<String> {
        unsafe {
            let view: IFolderView = active_view(dlg)?.cast()?;
            let item: IShellItem = view.GetFolder()?;
            let fs = display_name(&item, SIGDN_FILESYSPATH).unwrap_or_default();
            let parse = display_name(&item, SIGDN_DESKTOPABSOLUTEPARSING).unwrap_or_default();
            Ok(format!("OK\t{fs}\t{parse}"))
        }
    }

    unsafe fn select_edit(dlg: HWND, name: &[u16]) -> Result<()> {
        unsafe {
            let view = active_view(dlg)?;
            let folder_view: IFolderView = view.cast()?;
            let folder: IShellFolder = folder_view.GetFolder()?;
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            folder.ParseDisplayName(dlg, None, PCWSTR(name.as_ptr()), None, &mut pidl, std::ptr::null_mut())?;
            // SelectItem silently does nothing until the view contains the item, so look first.
            let mut present = false;
            for i in 0..folder_view.ItemCount(SVGIO_ALLVIEW).unwrap_or(0) {
                let Ok(item) = folder_view.Item(i) else { continue };
                let hr = folder.CompareIDs(LPARAM(0), item, pidl);
                CoTaskMemFree(Some(item as *const c_void));
                if hr.is_ok() && (hr.0 & 0xffff) == 0 {
                    present = true;
                    break;
                }
            }
            let result = if present {
                let flags = SVSI_EDIT.0 | SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0 | SVSI_ENSUREVISIBLE.0 | SVSI_FOCUSED.0;
                view.SelectItem(pidl, flags as u32)
            } else {
                Err(Error::new(HRESULT(0x8000_000Au32 as i32), "item not in view yet")) // E_PENDING
            };
            CoTaskMemFree(Some(pidl as *const c_void));
            result
        }
    }

    fn display_name(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
        unsafe {
            let p = item.GetDisplayName(sigdn).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const c_void));
            s
        }
    }

    unsafe fn send_reply(to: HWND, from: HWND, text: &str) {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let data = COPYDATASTRUCT {
            dwData: REPLY_TAG,
            cbData: (wide.len() * 2) as u32,
            lpData: wide.as_ptr() as *mut c_void,
        };
        unsafe {
            SendMessageTimeoutW(
                to,
                WM_COPYDATA,
                WPARAM(from.0 as usize),
                LPARAM(&data as *const _ as isize),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                3000,
                None,
            );
        }
    }
}

#[cfg(windows)]
pub use imp::FmDialogGetMsgProc;

/// Parsed reply of [`CMD_QUERY`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderReply {
    /// Real file-system folder, if the dialog shows one.
    pub fs_path: Option<String>,
    /// Shell parsing name (also set for virtual folders such as This PC).
    pub parsing_name: String,
}

/// Parses a [`CMD_QUERY`] reply.
pub fn parse_query_reply(reply: &str) -> Result<FolderReply, String> {
    let mut parts = reply.split('\t');
    match parts.next() {
        Some("OK") => {
            let fs = parts.next().unwrap_or("");
            let parse = parts.next().unwrap_or("");
            Ok(FolderReply { fs_path: (!fs.is_empty()).then(|| fs.to_string()), parsing_name: parse.to_string() })
        }
        Some("ERR") => Err(parts.next().unwrap_or("unknown").to_string()),
        _ => Err(format!("malformed reply: {reply}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_replies() {
        assert_eq!(
            parse_query_reply("OK\tC:\\x\tC:\\x").unwrap(),
            FolderReply { fs_path: Some("C:\\x".into()), parsing_name: "C:\\x".into() }
        );
        let virt = parse_query_reply("OK\t\t::{20D04FE0-3AEA-1069-A2D8-08002B30309D}").unwrap();
        assert_eq!(virt.fs_path, None);
        assert_eq!(parse_query_reply("ERR\tno IShellBrowser"), Err("no IShellBrowser".into()));
        assert!(parse_query_reply("garbage").is_err());
    }
}
