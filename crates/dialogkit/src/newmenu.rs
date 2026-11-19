//! Hosts Explorer's own "New" menu (CLSID_NewMenu) for a folder, so the dialog gets exactly the
//! entries, templates, localized names and unique naming Explorer uses.
use crate::util::*;
use std::{collections::HashSet, ffi::c_void, path::Path};
use windows::{
    Win32::{
        Foundation::*,
        System::Com::*,
        UI::{Shell::Common::ITEMIDLIST, Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

const CLSID_NEW_MENU: GUID = GUID::from_u128(0xD969A300_E7FF_11d0_A93B_00A0C90F2719);
pub const FIRST_ID: u32 = 1;
const LAST_ID: u32 = 0x7FFF;

pub struct NewMenu {
    cm: IContextMenu,
    cm3: Option<IContextMenu3>,
    root: HMENU,
    /// The populated "New" submenu, shown directly as our popup.
    pub popup: HMENU,
}

impl NewMenu {
    pub fn for_folder(folder: &str) -> Result<Self> {
        unsafe {
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            SHParseDisplayName(PCWSTR(wide(folder).as_ptr()), None, &mut pidl, 0, None)?;
            let cm: IContextMenu = CoCreateInstance(&CLSID_NEW_MENU, None, CLSCTX_INPROC_SERVER)?;
            let init = cm.cast::<IShellExtInit>().and_then(|i| i.Initialize(Some(pidl), None, None));
            CoTaskMemFree(Some(pidl as *const c_void));
            init?;
            let root = CreatePopupMenu()?;
            cm.QueryContextMenu(root, 0, FIRST_ID, LAST_ID, CMF_NORMAL).ok()?;
            let cm3 = cm.cast::<IContextMenu3>().ok();
            let mut popup = HMENU::default();
            for i in 0..GetMenuItemCount(Some(root)) {
                let sub = GetSubMenu(root, i);
                if !sub.is_invalid() {
                    popup = sub;
                    // Filled lazily, exactly as Explorer does on WM_INITMENUPOPUP.
                    if let Some(cm3) = &cm3 {
                        let _ = cm3.HandleMenuMsg2(WM_INITMENUPOPUP, WPARAM(sub.0 as usize), LPARAM(i as isize), None);
                    }
                    break;
                }
            }
            if popup.is_invalid() {
                let _ = DestroyMenu(root);
                return Err(Error::new(E_FAIL, "New menu is empty here"));
            }
            Ok(Self { cm, cm3, root, popup })
        }
    }

    /// Forwards owner-draw and init messages while the popup is tracked (icons, lazy submenus).
    pub fn handle_menu_message(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        let cm3 = self.cm3.as_ref()?;
        let mut result = LRESULT(0);
        unsafe { cm3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)).ok().map(|_| result) }
    }

    pub fn invoke(&self, id: u32, owner: HWND) -> Result<()> {
        let info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            hwnd: owner,
            lpVerb: PCSTR((id - FIRST_ID) as usize as *const u8),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        unsafe { self.cm.InvokeCommand(&info) }
    }
}

impl Drop for NewMenu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.root);
        }
    }
}

/// Lower-cased names in a folder, to find what an invoked command created.
pub fn names(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect())
        .unwrap_or_default()
}

/// The newest entry whose lower-cased name is not in `before`.
pub fn created_since(dir: &Path, before: &HashSet<String>) -> Option<String> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| !before.contains(&e.file_name().to_string_lossy().to_lowercase()))
        .max_by_key(|e| e.metadata().and_then(|m| m.created()).ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
}
