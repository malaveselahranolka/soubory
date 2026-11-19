//! Spike: host Explorer's own "New" menu (CLSID_NewMenu) out-of-process for a folder, list its
//! entries, optionally invoke one and report which item appeared on disk.
//! Usage: newmenu <dir> [entry-index]
use std::{collections::HashSet, ffi::c_void};
use windows::{
    Win32::{
        Foundation::*,
        System::Com::*,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

const CLSID_NEW_MENU: GUID = GUID::from_u128(0xD969A300_E7FF_11d0_A93B_00A0C90F2719);

fn listing(dir: &str) -> HashSet<String> {
    std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
}

fn main() -> Result<()> {
    let dir = std::env::args().nth(1).expect("dir").replace('/', "\\");
    let pick: Option<usize> = std::env::args().nth(2).and_then(|s| s.parse().ok());
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let wdir: Vec<u16> = dir.encode_utf16().chain([0]).collect();
        let pidl = ILCreateFromPathW(PCWSTR(wdir.as_ptr()));
        let cm: IContextMenu = CoCreateInstance(&CLSID_NEW_MENU, None, CLSCTX_INPROC_SERVER)?;
        cm.cast::<IShellExtInit>()?.Initialize(Some(pidl), None, None)?;
        let menu = CreatePopupMenu()?;
        cm.QueryContextMenu(menu, 0, 1, 0x7FFF, CMF_NORMAL).ok()?;
        let cm3: IContextMenu3 = cm.cast()?;
        let mut entries = vec![];
        for i in 0..GetMenuItemCount(Some(menu)) {
            let sub = GetSubMenu(menu, i);
            if sub.is_invalid() {
                continue;
            }
            // The New submenu is filled lazily, exactly like Explorer does on WM_INITMENUPOPUP.
            let _ = cm3.HandleMenuMsg2(WM_INITMENUPOPUP, WPARAM(sub.0 as usize), LPARAM(i as isize), None);
            for j in 0..GetMenuItemCount(Some(sub)) {
                let id = GetMenuItemID(sub, j);
                let mut buf = [0u16; 256];
                let n = GetMenuStringW(sub, j as u32, Some(&mut buf), MF_BYPOSITION);
                let text = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
                if id != u32::MAX && id != 0 && !text.is_empty() {
                    entries.push((id, text));
                }
            }
        }
        for (k, (id, t)) in entries.iter().enumerate() {
            println!("{k:2}: id={id:<5} {t}");
        }
        if let Some(k) = pick {
            let (id, text) = &entries[k];
            let before = listing(&dir);
            let info = CMINVOKECOMMANDINFO {
                cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                lpVerb: PCSTR((id - 1) as usize as *const u8),
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            };
            let r = cm.InvokeCommand(&info);
            let created: Vec<_> = listing(&dir).difference(&before).cloned().collect();
            println!("invoke '{text}' -> {r:?}, created: {created:?}");
        }
        let _ = DestroyMenu(menu);
        CoTaskMemFree(Some(pidl as *const c_void));
    }
    Ok(())
}
