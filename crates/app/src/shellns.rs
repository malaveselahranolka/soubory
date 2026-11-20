//! Shell namespace locations that are not plain folders: the Recycle Bin, Network, phones and
//! other devices, any virtual folder. Items keep their absolute ID lists, and the folder's own
//! shell objects supply context menus (Restore, Empty Recycle Bin…), icons and verbs, as in
//! Explorer. Listing runs on a worker thread because network and device folders can be slow.
use crate::app::{App, Loc};
use crate::fsys::Entry;
use std::{ffi::c_void, path::PathBuf, sync::Arc};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES,
        System::{
            Com::*,
            SystemServices::*,
            Variant::{PSTF_UTC, VARIANT, VariantToFileTime, VariantToString, VariantToUInt64},
        },
        UI::{
            Shell::{Common::*, *},
            WindowsAndMessaging::*,
        },
    },
    core::*,
};

/// An absolute shell ID list, owned (comparable, sendable between threads).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ShellId(pub Arc<[u8]>);

impl std::fmt::Debug for ShellId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ShellId({} bytes)", self.0.len())
    }
}

impl ShellId {
    /// Copies an ID list (ITEMIDLIST is byte-packed, so any alignment is fine).
    pub fn from_raw(p: *const ITEMIDLIST) -> ShellId {
        unsafe {
            let n = ILGetSize(Some(p)) as usize;
            ShellId(std::slice::from_raw_parts(p as *const u8, n).into())
        }
    }
    pub fn as_ptr(&self) -> *const ITEMIDLIST {
        self.0.as_ptr() as *const ITEMIDLIST
    }
    pub fn known(id: &GUID) -> Option<ShellId> {
        unsafe {
            let p = SHGetKnownFolderIDList(id, 0, None).ok()?;
            let s = ShellId::from_raw(p);
            CoTaskMemFree(Some(p as *const c_void));
            Some(s)
        }
    }
    /// The last item's own ID (relative to its parent folder).
    fn last(&self) -> *const ITEMIDLIST {
        unsafe { ILFindLastID(self.as_ptr()) }
    }
    fn parent(&self) -> Option<ShellId> {
        let mut v = self.0.to_vec();
        unsafe {
            let p = v.as_mut_ptr() as *mut ITEMIDLIST;
            if !ILRemoveLastID(Some(p)).as_bool() {
                return None;
            }
            let n = ILGetSize(Some(p)) as usize;
            v.truncate(n);
        }
        Some(ShellId(v.into()))
    }
    pub fn name(&self) -> String {
        unsafe {
            SHGetNameFromIDList(self.as_ptr(), SIGDN_NORMALDISPLAY)
                .ok()
                .map(|p| {
                    let s = p.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(p.0 as *const c_void));
                    s
                })
                .unwrap_or_default()
        }
    }
}

pub fn recycle_bin() -> Option<Loc> {
    let id = ShellId::known(&FOLDERID_RecycleBinFolder)?;
    Some(Loc::Shell { name: id.name(), id })
}

pub fn network() -> Option<Loc> {
    let id = ShellId::known(&FOLDERID_NetworkFolder)?;
    Some(Loc::Shell { name: id.name(), id })
}

pub fn is_network(id: &ShellId) -> bool {
    ShellId::known(&FOLDERID_NetworkFolder).as_ref() == Some(id)
}

pub fn is_recycle_bin(id: &ShellId) -> bool {
    ShellId::known(&FOLDERID_RecycleBinFolder).as_ref() == Some(id)
}

fn folder(id: &ShellId) -> Option<IShellFolder> {
    unsafe {
        let desktop = SHGetDesktopFolder().ok()?;
        if id.0.len() <= 2 {
            // The empty ID list (just its terminator) is the desktop itself.
            return Some(desktop);
        }
        desktop.BindToObject(id.as_ptr(), None).ok()
    }
}

unsafe fn display(f: &IShellFolder, child: *const ITEMIDLIST, flags: SHGDNF) -> Option<String> {
    unsafe {
        let mut sr = STRRET::default();
        f.GetDisplayNameOf(child, flags, &mut sr).ok()?;
        let mut out = PWSTR::null();
        StrRetToStrW(&mut sr, Some(child), &mut out).ok()?;
        let s = out.to_string().ok();
        CoTaskMemFree(Some(out.0 as *const c_void));
        s
    }
}

const STORAGE: GUID = GUID::from_u128(0xb725f130_47ef_101a_a5f1_02608c9eebac);
const PKEY_SIZE: PROPERTYKEY = PROPERTYKEY { fmtid: STORAGE, pid: 12 };
const PKEY_DATE_MODIFIED: PROPERTYKEY = PROPERTYKEY { fmtid: STORAGE, pid: 14 };
const PKEY_DATE_CREATED: PROPERTYKEY = PROPERTYKEY { fmtid: STORAGE, pid: 15 };
/// Bin columns (Explorer's "Original Location" / "Date Deleted").
const DELETED_FROM: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0x9b174b33_40ff_11d2_a27e_00c04fc30871), pid: 2 };
const DATE_DELETED: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0x9b174b33_40ff_11d2_a27e_00c04fc30871), pid: 3 };

unsafe fn detail(f2: &IShellFolder2, child: *const ITEMIDLIST, key: &PROPERTYKEY) -> Option<VARIANT> {
    unsafe { f2.GetDetailsEx(child, key).ok() }
}

fn as_filetime(v: &VARIANT) -> Option<u64> {
    let ft = unsafe { VariantToFileTime(v, PSTF_UTC).ok()? };
    Some(((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64)
}

fn as_u64(v: &VARIANT) -> Option<u64> {
    unsafe { VariantToUInt64(v).ok() }
}

fn as_string(v: &VARIANT) -> Option<String> {
    let mut buf = [0u16; 1024];
    unsafe { VariantToString(v, &mut buf).ok()? };
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n]))
}

/// Lists a shell folder (call on an STA thread).
pub fn list(owner: HWND, id: &ShellId, hidden: bool) -> std::result::Result<Vec<Entry>, String> {
    let f = folder(id).ok_or_else(|| "Umístění nelze otevřít.".to_string())?;
    let f2: Option<IShellFolder2> = f.cast().ok();
    let bin = is_recycle_bin(id);
    let mut flags = SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0;
    if hidden {
        flags |= SHCONTF_INCLUDEHIDDEN.0;
    }
    let mut out = Vec::new();
    unsafe {
        let mut en: Option<IEnumIDList> = None;
        let hr = f.EnumObjects(owner, flags as u32, &mut en);
        if hr.is_err() {
            return Err(format!("Obsah nelze načíst ({}).", hr.message()));
        }
        let Some(en) = en else { return Ok(out) }; // S_FALSE: nothing to list
        loop {
            let mut ids = [std::ptr::null_mut::<ITEMIDLIST>(); 1];
            let mut got = 0u32;
            if en.Next(&mut ids, Some(&mut got)).0 != 0 || got == 0 {
                break;
            }
            let child = ids[0] as *const ITEMIDLIST;
            let mut a = (SFGAO_FOLDER | SFGAO_FILESYSTEM | SFGAO_HIDDEN | SFGAO_STREAM).0;
            let _ = f.GetAttributesOf(&[child], &mut a);
            let name = display(&f, child, SHGDN_INFOLDER).unwrap_or_default();
            let parsing = display(&f, child, SHGDN_FORPARSING);
            let abs = ILCombine(Some(id.as_ptr()), Some(child));
            let shell = (!abs.is_null()).then(|| ShellId::from_raw(abs));
            if !abs.is_null() {
                ILFree(Some(abs));
            }
            let is_dir = a & SFGAO_FOLDER.0 != 0 && a & SFGAO_STREAM.0 == 0;
            let real = a & SFGAO_FILESYSTEM.0 != 0;
            let mut e = Entry {
                name,
                is_dir,
                size: 0,
                modified: 0,
                created: 0,
                attrs: if is_dir { 0x10 } else { 0 } | if a & SFGAO_HIDDEN.0 != 0 { 2 } else { 0 },
                full: parsing.filter(|_| real).map(PathBuf::from),
                shell,
                origin: None,
                sized: false,
            };
            if let Some(f2) = &f2 {
                if let Some(v) = detail(f2, child, &PKEY_SIZE) {
                    e.size = as_u64(&v).unwrap_or(0);
                }
                if let Some(v) = detail(f2, child, &PKEY_DATE_CREATED) {
                    e.created = as_filetime(&v).unwrap_or(0);
                }
                let when = if bin { DATE_DELETED } else { PKEY_DATE_MODIFIED };
                if let Some(v) = detail(f2, child, &when) {
                    e.modified = as_filetime(&v).unwrap_or(0);
                }
                if bin {
                    e.origin = detail(f2, child, &DELETED_FROM).and_then(|v| as_string(&v));
                }
            }
            CoTaskMemFree(Some(child as *const c_void));
            out.push(e);
        }
    }
    Ok(out)
}

struct Listed {
    id: ShellId,
    result: std::result::Result<Vec<Entry>, String>,
}

/// Starts listing the tab's shell location; results arrive as WM_APP_SHELLLIST.
pub fn start_list(app: &mut App) {
    let Loc::Shell { id, .. } = app.tab().loc.clone() else { return };
    let hidden = app.prefs.show_hidden;
    let hwnd = app.hwnd.0 as usize;
    if app.offscreen {
        let r = list(HWND::default(), &id, hidden);
        finish(app, Listed { id, result: r });
        return;
    }
    // No window yet: open_window lists again once it exists.
    if app.hwnd.is_invalid() {
        return;
    }
    app.tab_mut().loading = true;
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = list(HWND(hwnd as *mut c_void), &id, hidden);
        let b = Box::into_raw(Box::new(Listed { id, result }));
        if PostMessageW(Some(HWND(hwnd as *mut c_void)), crate::win::WM_APP_SHELLLIST, WPARAM(0), LPARAM(b as isize)).is_err() {
            drop(Box::from_raw(b));
        }
        CoUninitialize();
    });
}

pub fn message(app: &mut App, l: LPARAM) {
    let listed = unsafe { Box::from_raw(l.0 as *mut Listed) };
    finish(app, *listed);
    app.invalidate();
}

fn finish(app: &mut App, listed: Listed) {
    let same = matches!(&app.tab().loc, Loc::Shell { id, .. } if *id == listed.id);
    if !same {
        return;
    }
    let folders_first = app.prefs.folders_first;
    let t = app.tab_mut();
    t.loading = false;
    // By shell ID: display names repeat (several deleted "report.docx" in the Bin).
    let keep: std::collections::HashSet<ShellId> = t.sel.indices().iter().filter_map(|&i| t.entries.get(i).and_then(|e| e.shell.clone())).collect();
    let focus = t.sel.focus.and_then(|i| t.entries.get(i)).and_then(|e| e.shell.clone());
    match listed.result {
        Ok(mut v) => {
            crate::fsys::sort(&mut v, t.sort, t.asc, folders_first);
            t.entries = v;
            t.error = None;
        }
        Err(e) => {
            t.entries.clear();
            t.error = Some(e);
        }
    }
    crate::groups::apply(t, folders_first);
    t.icon_idx = vec![i32::MIN; t.entries.len()];
    t.sel = crate::sel::Selection::new(t.entries.len());
    let again: Vec<usize> = t.entries.iter().enumerate().filter(|(_, e)| e.shell.as_ref().is_some_and(|s| keep.contains(s))).map(|(i, _)| i).collect();
    if !again.is_empty() {
        t.sel.set_many(again, &[]);
    }
    if let Some(i) = focus.and_then(|f| t.entries.iter().position(|e| e.shell.as_ref() == Some(&f))) {
        t.sel.focus = Some(i);
        t.sel.anchor = Some(i);
    }
    app.clamp_scroll();
    // Only after a regroup or view change, not on every watcher refresh.
    if std::mem::take(&mut app.tab_mut().reveal_on_load) {
        app.reveal_focus();
    }
}

/// Opens an item of a shell location: folders inside the app, anything else with its verb.
pub fn open(app: &mut App, i: usize, new_tab: bool) {
    let Some(e) = app.tab().entries.get(i).cloned() else { return };
    if e.is_dir {
        let loc = match &e.full {
            Some(p) if p.is_dir() => Loc::Folder(p.clone()),
            _ => match &e.shell {
                Some(id) => Loc::Shell { id: id.clone(), name: e.name.clone() },
                None => return,
            },
        };
        if new_tab { app.new_tab(loc) } else { app.go(loc) }
        return;
    }
    if let Some(id) = &e.shell {
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST,
            hwnd: app.hwnd,
            lpIDList: id.as_ptr() as *mut c_void,
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        unsafe {
            let _ = ShellExecuteExW(&mut sei);
        }
    }
}

/// The context menu of the selected items, from their folder (Restore, Delete, Properties…).
pub fn item_menu(app: &mut App, pt: POINT) {
    let ids: Vec<ShellId> = app.tab().sel.indices().iter().filter_map(|&i| app.tab().entries.get(i)?.shell.clone()).collect();
    let Some(parent) = ids.first().and_then(ShellId::parent) else { return };
    let Some(f) = folder(&parent) else { return };
    let children: Vec<*const ITEMIDLIST> = ids.iter().map(ShellId::last).collect();
    unsafe {
        let Ok(cm) = f.GetUIObjectOf::<IContextMenu>(app.hwnd, &children, None) else { return };
        show(app, &cm, pt, CMF_NORMAL);
    }
}

/// Right-click on empty space: the folder's own background menu (Empty Recycle Bin, view…).
pub fn background_menu(app: &mut App, pt: POINT) {
    let Loc::Shell { id, .. } = app.tab().loc.clone() else { return };
    let Some(f) = folder(&id) else { return };
    unsafe {
        let Ok(cm) = f.CreateViewObject::<IContextMenu>(app.hwnd) else { return };
        show(app, &cm, pt, CMF_NORMAL | CMF_NODEFAULT);
    }
}

unsafe fn show(app: &mut App, cm: &IContextMenu, pt: POINT, flags: u32) {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        if cm.QueryContextMenu(m, 0, 1, 0x7FFF, flags).is_ok() {
            let cmd = crate::shell::with_active(&[cm], || crate::shell::track(app, m, pt));
            if cmd > 0 {
                crate::shell::invoke(cm, app.hwnd, cmd - 1, None, Some(pt));
            }
        }
        let _ = DestroyMenu(m);
    }
    app.reload(None);
}

/// Live refresh: the shell tells us when the location changes (items deleted to the bin, a
/// device appears). Returns the registration to pass to `unwatch`.
pub fn watch(hwnd: HWND, id: &ShellId) -> u32 {
    let entry = SHChangeNotifyEntry { pidl: id.as_ptr() as *mut ITEMIDLIST, fRecursive: false.into() };
    unsafe {
        SHChangeNotifyRegister(
            hwnd,
            SHCNRF_ShellLevel | SHCNRF_InterruptLevel,
            SHCNE_ALLEVENTS.0 as i32,
            crate::win::WM_APP_FS,
            1,
            &entry,
        )
    }
}

pub fn unwatch(reg: u32) {
    if reg != 0 {
        unsafe {
            let _ = SHChangeNotifyDeregister(reg);
        }
    }
}

/// Icon index for an item that has no plain path.
pub fn icon_index(id: &ShellId) -> i32 {
    let mut sfi = SHFILEINFOW::default();
    unsafe {
        let ok = SHGetFileInfoW(
            PCWSTR(id.as_ptr() as *const u16),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut sfi),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_PIDL | SHGFI_SYSICONINDEX,
        );
        if ok == 0 { -1 } else { sfi.iIcon }
    }
}
