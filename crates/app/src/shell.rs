//! Shell integration: open, context menus (Explorer's own, with every extension), verbs,
//! IFileOperation delete, rename, new folder, properties, drag out, Recents.
use crate::app::{App, Loc, View};
use crate::fsys::{self, Entry, wide};
use std::{
    cell::RefCell,
    collections::HashSet,
    ffi::c_void,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{
            Com::*,
            DataExchange::*,
            Memory::*,
            Ole::*,
        },
        UI::{
            Input::KeyboardAndMouse::GetKeyState,
            Shell::{Common::ITEMIDLIST, *},
            WindowsAndMessaging::*,
        },
    },
    core::*,
};

const CLSID_NEW_MENU: GUID = GUID::from_u128(0xD969A300_E7FF_11d0_A93B_00A0C90F2719);
const NEW_FIRST: u32 = 1;
const NEW_LAST: u32 = 0x0FFF;
const SHELL_FIRST: u32 = 0x1000;
const SHELL_LAST: u32 = 0x7FFF;
const OWN: u32 = 0x8000;
const CMIC_MASK_UNICODE: u32 = 0x4000;
const CMIC_MASK_ASYNCOK: u32 = 0x0010_0000;

// Our own menu command ids (OWN + n).
pub(crate) const C_QUICKLOOK: u32 = OWN + 1;
pub(crate) const C_NEW_TAB: u32 = OWN + 2;
pub(crate) const C_COPY_PATH: u32 = OWN + 3;
pub(crate) const C_FAVORITE: u32 = OWN + 4;
pub(crate) const C_NEW_FOLDER: u32 = OWN + 5;
pub(crate) const C_PASTE: u32 = OWN + 6;
pub(crate) const C_TERMINAL: u32 = OWN + 7;
pub(crate) const C_PROPS: u32 = OWN + 8;
pub(crate) const C_HIDDEN: u32 = OWN + 9;
pub(crate) const C_FOLDERS_FIRST: u32 = OWN + 10;
pub(crate) const C_PREVIEW: u32 = OWN + 11;
pub(crate) const C_SIDEBAR: u32 = OWN + 12;
pub(crate) const C_GOTO: u32 = OWN + 13;
pub(crate) const C_NEW_WINDOW: u32 = OWN + 14;
pub(crate) const C_VIEW0: u32 = OWN + 20; // +0..4: icons, list, columns, gallery, tiles
pub(crate) const C_SORT0: u32 = OWN + 30; // +0..4
pub(crate) const C_TAG0: u32 = OWN + 40; // +0..6
pub(crate) const C_TAG_CLEAR: u32 = OWN + 47;
pub(crate) const C_OPEN: u32 = OWN + 50;
pub(crate) const C_REMOVE_FAV: u32 = OWN + 51;
pub(crate) const C_EJECT: u32 = OWN + 52;
pub(crate) const C_DUPLICATE: u32 = OWN + 53;
pub(crate) const C_NEW_FOLDER_WITH: u32 = OWN + 54;
pub(crate) const C_SELECT_ALL: u32 = OWN + 55;
pub(crate) const C_REFRESH: u32 = OWN + 56;
pub(crate) const C_MAP_DRIVE: u32 = OWN + 57;
pub(crate) const C_UNMAP_DRIVE: u32 = OWN + 58;
pub(crate) const C_SAVE_SEARCH: u32 = OWN + 59;
pub(crate) const C_REMOVE_SEARCH: u32 = OWN + 60;
pub(crate) const C_GROUP0: u32 = OWN + 61; // +0..6, groups::KEYS order

thread_local! {
    /// Context menus that are being tracked; their owner-draw/init messages are forwarded.
    static ACTIVE: RefCell<Vec<IContextMenu>> = const { RefCell::new(Vec::new()) };
}

/// Forwards menu messages (icons, lazy "Send to"/"New" submenus) to the shell while a menu is open.
pub fn menu_message(_hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        return None;
    }
    ACTIVE.with(|a| {
        for cm in a.borrow().iter() {
            unsafe {
                if let Ok(cm3) = cm.cast::<IContextMenu3>() {
                    let mut r = LRESULT(0);
                    if cm3
                        .HandleMenuMsg2(msg, wparam, lparam, Some(&mut r))
                        .is_ok()
                        && msg != WM_INITMENUPOPUP
                    {
                        return Some(r);
                    }
                } else if let Ok(cm2) = cm.cast::<IContextMenu2>() {
                    let _ = cm2.HandleMenuMsg(msg, wparam, lparam);
                }
            }
        }
        if msg == WM_INITMENUPOPUP {
            Some(LRESULT(0))
        } else {
            None
        }
    })
}

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const c_void)) };
    }
}

fn pidl(p: &Path) -> Option<Pidl> {
    let w = wide(&p.to_string_lossy());
    let mut out: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut out, 0, None).ok()? };
    Some(Pidl(out))
}

/// Parent IShellFolder plus child ids for items that share the first item's parent.
fn parent_and_children(
    paths: &[PathBuf],
) -> Option<(IShellFolder, Vec<Pidl>, Vec<*const ITEMIDLIST>)> {
    let mut owned = Vec::new();
    let mut children = Vec::new();
    let mut folder: Option<IShellFolder> = None;
    let first_parent = paths.first()?.parent().map(Path::to_path_buf);
    for p in paths {
        if p.parent().map(Path::to_path_buf) != first_parent {
            continue;
        }
        let full = pidl(p)?;
        unsafe {
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            let f: IShellFolder = SHBindToParent(
                full.0,
                Some(&mut child as *mut *mut ITEMIDLIST as *mut *mut ITEMIDLIST),
            )
            .ok()?;
            children.push(child as *const ITEMIDLIST);
            if folder.is_none() {
                folder = Some(f);
            }
        }
        owned.push(full); // child points inside `full`
    }
    Some((folder?, owned, children))
}

fn item_cm(hwnd: HWND, paths: &[PathBuf]) -> Option<IContextMenu> {
    let (folder, _owned, children) = parent_and_children(paths)?;
    unsafe { folder.GetUIObjectOf(hwnd, &children, None).ok() }
}

fn folder_shellfolder(dir: &Path) -> Option<IShellFolder> {
    let p = pidl(dir)?;
    unsafe { SHBindToObject(None, p.0, None).ok() }
}

fn background_cm(hwnd: HWND, dir: &Path) -> Option<IContextMenu> {
    let f = folder_shellfolder(dir)?;
    unsafe { f.CreateViewObject(hwnd).ok() }
}

fn new_menu(dir: &Path) -> Option<IContextMenu> {
    unsafe {
        let p = pidl(dir)?;
        let cm: IContextMenu =
            CoCreateInstance(&CLSID_NEW_MENU, None, CLSCTX_INPROC_SERVER).ok()?;
        cm.cast::<IShellExtInit>()
            .ok()?
            .Initialize(Some(p.0), None, None)
            .ok()?;
        Some(cm)
    }
}

fn append(m: HMENU, id: u32, text: &str, checked: bool, enabled: bool) {
    let w = wide(text);
    let mut flags = MF_STRING;
    if checked {
        flags |= MF_CHECKED;
    }
    if !enabled {
        flags |= MF_GRAYED;
    }
    unsafe {
        let _ = AppendMenuW(m, flags, id as usize, PCWSTR(w.as_ptr()));
    }
}

fn sep(m: HMENU) {
    unsafe {
        let _ = AppendMenuW(m, MF_SEPARATOR, 0, None);
    }
}

fn submenu(m: HMENU, text: &str) -> HMENU {
    unsafe {
        let s = CreatePopupMenu().unwrap_or_default();
        let w = wide(text);
        let _ = AppendMenuW(m, MF_POPUP, s.0 as usize, PCWSTR(w.as_ptr()));
        s
    }
}

pub(crate) fn track(app: &App, m: HMENU, pt: POINT) -> u32 {
    unsafe {
        let _ = SetForegroundWindow(app.hwnd);
        let r = TrackPopupMenuEx(
            m,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            pt.x,
            pt.y,
            app.hwnd,
            None,
        );
        let _ = PostMessageW(Some(app.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        r.0 as u32
    }
}

fn verb_of(cm: &IContextMenu, offset: u32) -> String {
    let mut buf = [0u16; 128];
    unsafe {
        if cm
            .GetCommandString(
                offset as usize,
                GCS_VERBW,
                None,
                PSTR(buf.as_mut_ptr() as *mut u8),
                buf.len() as u32,
            )
            .is_ok()
        {
            let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
            return String::from_utf16_lossy(&buf[..n]);
        }
    }
    String::new()
}

pub(crate) fn invoke(cm: &IContextMenu, hwnd: HWND, offset: u32, dir: Option<&Path>, pt: Option<POINT>) {
    let dir_w = dir.map(|d| wide(&d.to_string_lossy()));
    let dir_a: Option<Vec<u8>> = dir.map(|d| d.to_string_lossy().bytes().chain([0]).collect());
    let mut info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_ASYNCOK,
        hwnd,
        lpVerb: PCSTR(offset as usize as *const u8),
        lpVerbW: PCWSTR(offset as usize as *const u16),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    if let (Some(w), Some(a)) = (&dir_w, &dir_a) {
        info.lpDirectoryW = PCWSTR(w.as_ptr());
        info.lpDirectory = PCSTR(a.as_ptr());
    }
    if let Some(p) = pt {
        info.fMask |= CMIC_MASK_PTINVOKE;
        info.ptInvoke = p;
    }
    if unsafe { GetKeyState(0x11) } < 0 {
        info.fMask |= CMIC_MASK_CONTROL_DOWN;
    }
    if unsafe { GetKeyState(0x10) } < 0 {
        info.fMask |= CMIC_MASK_SHIFT_DOWN;
    }
    unsafe {
        let _ =
            cm.InvokeCommand(&info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO);
    }
}

pub fn invoke_verb(cm: &IContextMenu, hwnd: HWND, verb: &str) -> bool {
    let a: Vec<u8> = verb.bytes().chain([0]).collect();
    let w = wide(verb);
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE,
        hwnd,
        lpVerb: PCSTR(a.as_ptr()),
        lpVerbW: PCWSTR(w.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        cm.InvokeCommand(&info as *const CMINVOKECOMMANDINFOEX as *const CMINVOKECOMMANDINFO)
            .is_ok()
    }
}

pub(crate) fn with_active<R>(cms: &[&IContextMenu], f: impl FnOnce() -> R) -> R {
    ACTIVE.with(|a| a.borrow_mut().extend(cms.iter().map(|c| (*c).clone())));
    let r = f();
    ACTIVE.with(|a| a.borrow_mut().clear());
    r
}

// ---- public commands ---------------------------------------------------------------------------------------

pub fn open_path(hwnd: HWND, path: &Path) {
    let w = wide(&path.to_string_lossy());
    let dir = path.parent().map(|d| wide(&d.to_string_lossy()));
    unsafe {
        ShellExecuteW(
            Some(hwnd),
            PCWSTR::null(),
            PCWSTR(w.as_ptr()),
            PCWSTR::null(),
            dir.as_ref()
                .map(|d| PCWSTR(d.as_ptr()))
                .unwrap_or(PCWSTR::null()),
            SW_SHOWNORMAL,
        );
    }
}

/// Right-click on items: our Finder-style entries, then Explorer's full menu.
pub fn item_menu(app: &mut App, pt: POINT) {
    if matches!(app.tab().loc, Loc::Shell { .. }) {
        crate::shellns::item_menu(app, pt);
        return;
    }
    let paths = app.tab().selected_paths();
    if paths.is_empty() {
        return;
    }
    let shift = unsafe { GetKeyState(0x10) } < 0;
    let Some(cm) = item_cm(app.hwnd, &paths) else {
        return;
    };
    let single_dir = paths.len() == 1 && paths[0].is_dir();
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let mut flags = CMF_NORMAL | CMF_CANRENAME;
        if shift {
            flags |= CMF_EXTENDEDVERBS;
        }
        if cm
            .QueryContextMenu(m, 0, SHELL_FIRST, SHELL_LAST, flags)
            .is_err()
        {
            let _ = DestroyMenu(m);
            return;
        }
        // Our items at the top, Finder style.
        let top = CreatePopupMenu().unwrap_or_default();
        append(top, C_QUICKLOOK, "Rychlý náhled\tMezerník", false, true);
        if single_dir {
            append(top, C_NEW_TAB, "Otevřít v nové kartě", false, true);
        }
        let tags = submenu(top, "Štítky");
        let current = crate::tags::mask_of_all(&paths);
        for (i, (_, name)) in crate::theme::TAGS.iter().enumerate() {
            append(tags, C_TAG0 + i as u32, name, current & (1 << i) != 0, true);
        }
        sep(tags);
        append(tags, C_TAG_CLEAR, "Odebrat štítky", false, current != 0);
        append(
            top,
            C_COPY_PATH,
            "Kopírovat cestu\tCtrl+Shift+C",
            false,
            true,
        );
        append(top, C_DUPLICATE, "Duplikovat\tCtrl+D", false, true);
        if paths.len() > 1 {
            append(
                top,
                C_NEW_FOLDER_WITH,
                &format!("Nová složka s výběrem ({} položek)", paths.len()),
                false,
                true,
            );
        }
        if single_dir {
            let fav = app.prefs.favorites.iter().any(|f| f == &paths[0]);
            append(
                top,
                C_FAVORITE,
                if fav {
                    "Odebrat z oblíbených"
                } else {
                    "Přidat do oblíbených"
                },
                false,
                true,
            );
        }
        // Move our items before the shell's.
        let n = GetMenuItemCount(Some(top));
        for k in (0..n).rev() {
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_SUBMENU | MIIM_STRING | MIIM_STATE | MIIM_FTYPE,
                ..Default::default()
            };
            let mut buf = [0u16; 256];
            info.dwTypeData = PWSTR(buf.as_mut_ptr());
            info.cch = buf.len() as u32;
            let _ = GetMenuItemInfoW(top, k as u32, true, &mut info);
            let _ = InsertMenuItemW(m, 0, true, &info);
        }
        let _ = InsertMenuW(m, n as u32, MF_BYPOSITION | MF_SEPARATOR, 0, None);
        // The submenu now belongs to `m`; detach it from `top` before destroying `top`.
        while GetMenuItemCount(Some(top)) > 0 {
            let _ = RemoveMenu(top, 0, MF_BYPOSITION);
        }
        let _ = DestroyMenu(top);

        let cmd = with_active(&[&cm], || track(app, m, pt));
        let _ = DestroyMenu(m);
        match cmd {
            0 => {}
            C_QUICKLOOK => crate::quicklook::toggle(app),
            C_NEW_TAB => app.new_tab(Loc::Folder(paths[0].clone())),
            C_COPY_PATH => copy_path(app),
            C_DUPLICATE => duplicate(app),
            C_NEW_FOLDER_WITH => new_folder_with_selection(app),
            C_FAVORITE => toggle_favorite(app, &paths[0]),
            C_TAG_CLEAR => {
                crate::tags::clear(&paths);
                tags_changed(app);
            }
            c if (C_TAG0..C_TAG0 + 7).contains(&c) => {
                crate::tags::toggle(&paths, (c - C_TAG0) as usize);
                tags_changed(app);
            }
            c if (SHELL_FIRST..=SHELL_LAST).contains(&c) => {
                let off = c - SHELL_FIRST;
                let verb = verb_of(&cm, off);
                if verb.eq_ignore_ascii_case("rename") {
                    crate::win::begin_rename(app);
                } else if verb.eq_ignore_ascii_case("delete") {
                    delete(app, shift);
                } else {
                    let dir = app.tab().loc.folder().map(Path::to_path_buf);
                    invoke(&cm, app.hwnd, off, dir.as_deref(), Some(pt));
                }
            }
            _ => {}
        }
    }
}

/// Right-click on empty space: New, Paste, view options, then the folder's shell extensions.
pub fn background_menu(app: &mut App, pt: POINT) {
    if matches!(app.tab().loc, Loc::Shell { .. }) {
        crate::shellns::background_menu(app, pt);
        return;
    }
    let Some(dir) = app.tab().loc.folder().map(Path::to_path_buf) else {
        more_menu_at(app, pt);
        return;
    };
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        // View and sort.
        view_items(app, m);
        sep(m);
        append(m, C_REFRESH, "Aktualizovat\tF5", false, true);
        append(m, C_PASTE, "Vložit\tCtrl+V", false, can_paste());
        sep(m);
        append(m, C_NEW_FOLDER, "Nová složka\tCtrl+Shift+N", false, true);
        let newcm = new_menu(&dir);
        if let Some(nc) = &newcm {
            if nc
                .QueryContextMenu(
                    m,
                    GetMenuItemCount(Some(m)) as u32,
                    NEW_FIRST,
                    NEW_LAST,
                    CMF_NORMAL,
                )
                .is_err()
            {
                // Leave it out.
            }
        }
        sep(m);
        append(m, C_TERMINAL, "Otevřít v Terminálu", false, true);
        append(
            m,
            C_SELECT_ALL,
            "Vybrat vše\tCtrl+A",
            false,
            !app.tab().entries.is_empty(),
        );
        // Shell background extensions ("Open with Code", "Git Bash here"…).
        let bg = background_cm(app.hwnd, &dir);
        let mut bg_ok = false;
        if let Some(b) = &bg {
            sep(m);
            let pos = GetMenuItemCount(Some(m)) as u32;
            bg_ok = b
                .QueryContextMenu(m, pos, SHELL_FIRST, SHELL_LAST, CMF_NORMAL | CMF_NODEFAULT)
                .is_ok();
            dedupe_background(m, pos);
        }
        sep(m);
        append(m, C_PROPS, "Vlastnosti\tAlt+Enter", false, true);
        tidy_separators(m);

        let mut active: Vec<&IContextMenu> = Vec::new();
        if let Some(nc) = &newcm {
            active.push(nc);
        }
        if let (Some(b), true) = (&bg, bg_ok) {
            active.push(b);
        }
        let before = names(&dir);
        let cmd = with_active(&active, || track(app, m, pt));
        let _ = DestroyMenu(m);
        match cmd {
            0 => {}
            c if (NEW_FIRST..=NEW_LAST).contains(&c) => {
                if let Some(nc) = &newcm {
                    invoke(nc, app.hwnd, c - NEW_FIRST, Some(&dir), None);
                    // Select the created item and start renaming it, like Explorer.
                    if let Some(name) = created_since(&dir, &before) {
                        app.reload(Some(&name));
                        crate::win::begin_rename(app);
                    }
                }
            }
            c if (SHELL_FIRST..=SHELL_LAST).contains(&c) => {
                if let Some(b) = &bg {
                    invoke(b, app.hwnd, c - SHELL_FIRST, Some(&dir), Some(pt));
                }
            }
            c => own_command(app, c),
        }
    }
}

/// Removes background entries we already offer ourselves (View, Sort, Refresh, Paste, New, Properties).
fn dedupe_background(m: HMENU, from: u32) {
    unsafe {
        let mut k = GetMenuItemCount(Some(m)) - 1;
        while k >= from as i32 {
            let id = GetMenuItemID(m, k);
            let mut buf = [0u16; 128];
            let n = GetMenuStringW(m, k as u32, Some(&mut buf), MF_BYPOSITION);
            let text = String::from_utf16_lossy(&buf[..n.max(0) as usize])
                .replace('&', "")
                .to_lowercase();
            let dup = [
                "zobrazit",
                "seřadit",
                "řadit",
                "seskupit",
                "aktualizovat",
                "vložit",
                "nový",
                "vlastnosti",
                "view",
                "sort",
                "group",
                "refresh",
                "paste",
                "new",
                "properties",
                "přizpůsobit",
            ]
            .iter()
            .any(|d| text.starts_with(d));
            if dup && (id == u32::MAX || (SHELL_FIRST..=SHELL_LAST).contains(&id)) {
                let _ = DeleteMenu(m, k as u32, MF_BYPOSITION);
            }
            k -= 1;
        }
    }
}

/// No leading, trailing or doubled separators.
fn tidy_separators(m: HMENU) {
    unsafe {
        let mut prev_sep = true;
        let mut k = 0;
        while k < GetMenuItemCount(Some(m)) {
            let is_sep = GetMenuState(m, k as u32, MF_BYPOSITION) & MF_SEPARATOR.0 != 0;
            if is_sep && prev_sep {
                let _ = DeleteMenu(m, k as u32, MF_BYPOSITION);
                continue;
            }
            prev_sep = is_sep;
            k += 1;
        }
        let n = GetMenuItemCount(Some(m));
        if n > 0 && GetMenuState(m, (n - 1) as u32, MF_BYPOSITION) & MF_SEPARATOR.0 != 0 {
            let _ = DeleteMenu(m, (n - 1) as u32, MF_BYPOSITION);
        }
    }
}

fn view_items(app: &App, m: HMENU) {
    let v = submenu(m, "Zobrazení");
    let cur = app.tab().view;
    for (k, (name, view)) in [
        ("Ikony\tCtrl+1", View::Icons),
        ("Seznam\tCtrl+2", View::List),
        ("Sloupce\tCtrl+3", View::Columns),
        ("Galerie\tCtrl+4", View::Gallery),
        ("Dlaždice\tCtrl+5", View::Tiles),
    ]
    .iter()
    .enumerate()
    {
        append(v, C_VIEW0 + k as u32, name, cur == *view, true);
    }
    let s = submenu(m, "Řadit podle");
    let t = app.tab();
    for (k, (name, key)) in [
        ("Název", fsys::SortKey::Name),
        ("Datum změny", fsys::SortKey::Modified),
        ("Datum vytvoření", fsys::SortKey::Created),
        ("Velikost", fsys::SortKey::Size),
        ("Druh", fsys::SortKey::Kind),
    ]
    .iter()
    .enumerate()
    {
        append(s, C_SORT0 + k as u32, name, t.sort == *key, true);
    }
    sep(s);
    append(
        s,
        C_FOLDERS_FIRST,
        "Složky nahoře",
        app.prefs.folders_first,
        true,
    );
    let g = submenu(m, "Seskupit podle");
    for (k, (key, name, ..)) in crate::groups::KEYS.iter().enumerate() {
        if k == 1 {
            sep(g);
        }
        append(g, C_GROUP0 + k as u32, name, t.group == *key, true);
    }
}

/// Toolbar "⋯" menu.
/// Explorer's address-bar chevron: the subfolders of crumb `k` (drives for "Tento počítač"),
/// the one on the current path checked. Opens upward from the path bar.
pub fn crumb_menu(app: &mut App, k: usize) {
    let Some(loc) = crate::paint::crumb_path(app, k) else { return };
    let here = crate::paint::crumb_path(app, k + 1);
    let mut items: Vec<(String, Loc)> = match &loc {
        Loc::Computer => fsys::volumes().into_iter().map(|v| (v.label, Loc::Folder(v.root))).collect(),
        Loc::Folder(p) => subfolders(p, app.prefs.show_hidden),
        _ => return,
    };
    if !matches!(loc, Loc::Computer) {
        items.sort_by(|a, b| fsys::name_cmp(&a.0, &b.0));
    }
    // shortcut: a native menu stays usable up to about a thousand items; Explorer scrolls further.
    items.truncate(1000);
    let Some((x, y)) = crate::paint::chevron_anchor(app, k) else { return };
    let pt = crate::win::to_screen(app, x, y);
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        if items.is_empty() {
            append(m, 0, "Žádné podsložky", false, false);
        }
        for (i, (name, l)) in items.iter().enumerate() {
            append(m, i as u32 + 1, &name.replace('&', "&&"), here.as_ref() == Some(l), true);
        }
        let _ = SetForegroundWindow(app.hwnd);
        let r = TrackPopupMenuEx(m, (TPM_RETURNCMD | TPM_BOTTOMALIGN | TPM_LEFTALIGN).0, pt.x, pt.y, app.hwnd, None);
        let _ = PostMessageW(Some(app.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        if let Some((_, l)) = (r.0 as usize).checked_sub(1).and_then(|i| items.get(i)) {
            app.go(l.clone());
        }
    }
}

/// Subfolders by name, without opening any of them (no OneDrive hydration).
fn subfolders(dir: &Path, hidden: bool) -> Vec<(String, Loc)> {
    use std::os::windows::fs::MetadataExt;
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    // DirEntry::metadata comes from the directory listing itself; no item is opened.
    rd.flatten()
        .filter(|e| {
            e.metadata().is_ok_and(|m| {
                let a = m.file_attributes();
                a & FILE_ATTRIBUTE_DIRECTORY.0 != 0 && (hidden || a & FILE_ATTRIBUTE_HIDDEN.0 == 0)
            })
        })
        .map(|e| (e.file_name().to_string_lossy().into_owned(), Loc::Folder(e.path())))
        .collect()
}

pub fn more_menu(app: &mut App) {
    let r = app.lay.more;
    let pt = crate::win::to_screen(app, r.x, r.b() + 4.0);
    more_menu_at(app, pt);
}

fn more_menu_at(app: &mut App, pt: POINT) {
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let in_folder = app.tab().loc.folder().is_some();
        append(
            m,
            C_NEW_FOLDER,
            "Nová složka\tCtrl+Shift+N",
            false,
            in_folder,
        );
        append(m, C_NEW_TAB, "Nová karta\tCtrl+T", false, true);
        append(m, C_NEW_WINDOW, "Nové okno\tCtrl+N", false, in_folder);
        sep(m);
        view_items(app, m);
        append(
            m,
            C_HIDDEN,
            "Zobrazit skryté soubory\tCtrl+Shift+.",
            app.prefs.show_hidden,
            true,
        );
        append(
            m,
            C_PREVIEW,
            "Náhledový panel\tCtrl+Shift+P",
            app.preview_on,
            true,
        );
        append(
            m,
            C_SIDEBAR,
            "Postranní panel\tCtrl+/",
            app.sidebar_on,
            true,
        );
        sep(m);
        if let Loc::Search { text, scope } = &app.tab().loc {
            let saved = app.prefs.saved.iter().any(|(t, s)| t == text.trim() && s == scope);
            append(m, C_SAVE_SEARCH, "Uložit hledání", saved, !text.trim().is_empty() && !saved);
        }
        append(m, C_GOTO, "Přejít na složku…\tCtrl+L", false, true);
        append(m, C_TERMINAL, "Otevřít v Terminálu", false, in_folder);
        append(m, C_PROPS, "Vlastnosti složky", false, in_folder);
        sep(m);
        append(m, C_MAP_DRIVE, "Připojit síťovou jednotku…", false, true);
        append(m, C_UNMAP_DRIVE, "Odpojit síťovou jednotku…", false, true);
        let cmd = track(app, m, pt);
        let _ = DestroyMenu(m);
        own_command(app, cmd);
    }
}

/// Commands the palette runs by the same IDs as the More menu.
pub(crate) mod cmd {
    pub(crate) use super::{
        C_GOTO as GOTO, C_HIDDEN as HIDDEN, C_MAP_DRIVE as MAP_DRIVE, C_NEW_FOLDER as NEW_FOLDER, C_NEW_TAB as NEW_TAB,
        C_NEW_WINDOW as NEW_WINDOW, C_PREVIEW as PREVIEW, C_PROPS as PROPS, C_REFRESH as REFRESH, C_SAVE_SEARCH as SAVE_SEARCH,
        C_SELECT_ALL as SELECT_ALL, C_SIDEBAR as SIDEBAR, C_SORT0 as SORT0, C_TERMINAL as TERMINAL, C_UNMAP_DRIVE as UNMAP_DRIVE,
        C_VIEW0 as VIEW0, C_GROUP0 as GROUP0,
    };
}

pub fn run_command(app: &mut App, cmd: u32) {
    own_command(app, cmd);
}

fn own_command(app: &mut App, cmd: u32) {
    match cmd {
        C_NEW_FOLDER => new_folder(app),
        C_NEW_TAB => {
            let loc = app.tab().loc.clone();
            app.new_tab(loc);
        }
        C_NEW_WINDOW => crate::win::new_window(app),
        C_PASTE => paste(app),
        C_TERMINAL => open_terminal(app),
        C_PROPS => {
            if let Some(d) = app.tab().loc.folder() {
                show_properties(app.hwnd, d);
            }
        }
        C_HIDDEN => app.toggle_hidden(),
        C_FOLDERS_FIRST => {
            app.prefs.folders_first = !app.prefs.folders_first;
            app.reload(None);
            app.invalidate();
        }
        C_PREVIEW => {
            app.preview_on = !app.preview_on;
            app.relayout();
            app.invalidate();
        }
        C_SIDEBAR => {
            app.sidebar_on = !app.sidebar_on;
            app.relayout();
            app.invalidate();
        }
        C_GOTO => crate::win::go_to_folder(app),
        C_SAVE_SEARCH => save_search(app),
        // Windows' own dialogs; the drive watcher updates every sidebar when a drive appears.
        C_MAP_DRIVE => unsafe {
            let _ = windows::Win32::NetworkManagement::WNet::WNetConnectionDialog(app.hwnd, windows::Win32::NetworkManagement::WNet::RESOURCETYPE_DISK.0);
        },
        C_UNMAP_DRIVE => unsafe {
            let _ = windows::Win32::NetworkManagement::WNet::WNetDisconnectDialog(Some(app.hwnd), windows::Win32::NetworkManagement::WNet::RESOURCETYPE_DISK.0);
        },
        C_REFRESH => {
            app.reload(None);
            app.invalidate();
        }
        C_SELECT_ALL => {
            app.tab_mut().sel.select_all();
            app.invalidate();
        }
        c if (C_VIEW0..C_VIEW0 + 5).contains(&c) => app.set_view(
            [View::Icons, View::List, View::Columns, View::Gallery, View::Tiles][(c - C_VIEW0) as usize],
        ),
        c if (C_GROUP0..C_GROUP0 + crate::groups::KEYS.len() as u32).contains(&c) => {
            app.set_group(crate::groups::KEYS[(c - C_GROUP0) as usize].0);
        }
        c if (C_SORT0..C_SORT0 + 5).contains(&c) => {
            let key = [
                fsys::SortKey::Name,
                fsys::SortKey::Modified,
                fsys::SortKey::Created,
                fsys::SortKey::Size,
                fsys::SortKey::Kind,
            ][(c - C_SORT0) as usize];
            app.set_sort(key);
        }
        _ => {}
    }
}

pub fn sidebar_menu(app: &mut App, i: usize, pt: POINT) {
    let Some(crate::sidebar::Row::Item { loc, eject, .. }) = app.side_rows.get(i).cloned() else {
        return;
    };
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        append(m, C_OPEN, "Otevřít", false, true);
        append(m, C_NEW_TAB, "Otevřít v nové kartě", false, true);
        let is_fav = matches!(&loc, Loc::Folder(p) if app.prefs.favorites.contains(p));
        if is_fav {
            append(m, C_REMOVE_FAV, "Odebrat z oblíbených", false, true);
        }
        if matches!(loc, Loc::Search { .. }) {
            append(m, C_REMOVE_SEARCH, "Odebrat z postranního panelu", false, true);
        }
        if eject {
            append(m, C_EJECT, "Vysunout", false, true);
        }
        if loc.folder().is_some() {
            sep(m);
            append(m, C_PROPS, "Vlastnosti", false, true);
        }
        let cmd = track(app, m, pt);
        let _ = DestroyMenu(m);
        match cmd {
            C_OPEN => match loc {
                Loc::Search { text, scope } => app.open_search(text, scope),
                other => app.go(other),
            },
            C_NEW_TAB => app.new_tab(loc),
            C_REMOVE_FAV => {
                if let Loc::Folder(p) = &loc {
                    toggle_favorite(app, p);
                }
            }
            C_REMOVE_SEARCH => {
                if let Loc::Search { text, scope } = &loc {
                    app.prefs.saved.retain(|(t, s)| !(t == text && s == scope));
                    crate::prefs::save(app);
                    sidebar_changed(app);
                }
            }
            C_EJECT => {
                if let Loc::Folder(p) = &loc {
                    if let Some(cm) = item_cm(app.hwnd, std::slice::from_ref(p)) {
                        if let Ok(m2) = CreatePopupMenu() {
                            let _ = cm.QueryContextMenu(m2, 0, SHELL_FIRST, SHELL_LAST, CMF_NORMAL);
                            invoke_verb(&cm, app.hwnd, "eject");
                            let _ = DestroyMenu(m2);
                        }
                    }
                    app.side_rows = crate::sidebar::build(&app.prefs);
                    app.invalidate();
                }
            }
            C_PROPS => {
                if let Some(p) = loc.folder() {
                    show_properties(app.hwnd, p);
                }
            }
            _ => {}
        }
    }
}

fn toggle_favorite(app: &mut App, p: &Path) {
    if let Some(k) = app.prefs.favorites.iter().position(|f| f == p) {
        app.prefs.favorites.remove(k);
    } else {
        app.prefs.favorites.push(p.to_path_buf());
    }
    crate::prefs::save(app);
    sidebar_changed(app);
}

/// Favorites and saved searches are shared by every window.
fn sidebar_changed(app: &App) {
    let (favs, saved) = (app.prefs.favorites.clone(), app.prefs.saved.clone());
    crate::win::for_each_app(|a| {
        a.prefs.favorites = favs.clone();
        a.prefs.saved = saved.clone();
        a.side_rows = crate::sidebar::build(&a.prefs);
        a.invalidate();
    });
}

/// "Uložit hledání": the current search becomes a sidebar item in every window.
pub fn save_search(app: &mut App) {
    let Loc::Search { text, scope } = app.tab().loc.clone() else { return };
    let item = (text.replace(['\t', '\r', '\n'], " ").trim().to_string(), scope);
    if item.0.is_empty() || app.prefs.saved.contains(&item) {
        return;
    }
    app.prefs.saved.push(item);
    crate::prefs::save(app);
    sidebar_changed(app);
    app.note("Hledání je uložené v postranním panelu.");
}

pub fn properties(app: &mut App) {
    let paths = app.tab().selected_paths();
    match paths.len() {
        0 => {
            if let Some(d) = app.tab().loc.folder() {
                show_properties(app.hwnd, d);
            }
        }
        1 => show_properties(app.hwnd, &paths[0]),
        _ => unsafe {
            if let Some((folder, _o, children)) = parent_and_children(&paths) {
                if let Ok(dobj) = folder.GetUIObjectOf::<IDataObject>(app.hwnd, &children, None) {
                    let _ = SHMultiFileProperties(&dobj, 0);
                }
            }
        },
    }
}

fn show_properties(hwnd: HWND, p: &Path) {
    let w = wide(&p.to_string_lossy());
    unsafe {
        let _ = SHObjectProperties(
            Some(hwnd),
            SHOP_FILEPATH,
            PCWSTR(w.as_ptr()),
            PCWSTR::null(),
        );
    }
}

pub fn verb_on_selection(app: &mut App, verb: &str) {
    let paths = app.tab().selected_paths();
    if paths.is_empty() {
        return;
    }
    if let Some(cm) = item_cm(app.hwnd, &paths) {
        unsafe {
            if let Ok(m) = CreatePopupMenu() {
                let _ = cm.QueryContextMenu(m, 0, SHELL_FIRST, SHELL_LAST, CMF_NORMAL);
                invoke_verb(&cm, app.hwnd, verb);
                let _ = DestroyMenu(m);
            }
        }
        app.note(match verb {
            "copy" => "Zkopírováno do schránky",
            "cut" => "Vyjmuto — vložte klávesami Ctrl+V",
            _ => "",
        });
    }
}

/// File paths on the clipboard and whether they were cut (Preferred DropEffect = move).
/// None for anything that is not plain files (the shell's paste handles those).
fn clipboard_files(hwnd: HWND) -> Option<(Vec<PathBuf>, bool)> {
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let read = || -> Option<(Vec<PathBuf>, bool)> {
            let h = GetClipboardData(15).ok()?; // CF_HDROP
            let drop = HDROP(h.0);
            let n = DragQueryFileW(drop, u32::MAX, None);
            let mut paths = Vec::new();
            for i in 0..n {
                let len = DragQueryFileW(drop, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                DragQueryFileW(drop, i, Some(&mut buf));
                paths.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
            }
            let fmt = RegisterClipboardFormatW(w!("Preferred DropEffect"));
            let moving = GetClipboardData(fmt)
                .ok()
                .map(|g| {
                    let hg = HGLOBAL(g.0);
                    let p = GlobalLock(hg) as *const u32;
                    let v = if p.is_null() { 0 } else { *p };
                    let _ = GlobalUnlock(hg);
                    v & DROPEFFECT_MOVE.0 != 0
                })
                .unwrap_or(false);
            (!paths.is_empty()).then_some((paths, moving))
        };
        let r = read();
        let _ = CloseClipboard();
        r
    }
}

fn can_paste() -> bool {
    unsafe { IsClipboardFormatAvailable(15).is_ok() } // CF_HDROP
}

pub fn paste(app: &mut App) {
    let Some(dir) = app.tab().loc.folder().map(Path::to_path_buf) else {
        return;
    };
    if !can_paste() {
        app.note("Ve schránce nejsou žádné soubory.");
        return;
    }
    if let Some((paths, moving)) = clipboard_files(app.hwnd) {
        if !moving && paths.iter().all(|p| p.parent() == Some(dir.as_path())) {
            duplicate_into(app, &dir, paths);
        } else if moving && paths.iter().all(|p| p.parent() == Some(dir.as_path())) {
            // Cut and pasted back into the same folder: nothing to do.
        } else {
            file_op(app.hwnd, paths, Some(dir), if moving { Op::Move } else { Op::Copy }, true);
            if moving {
                // A cut is used up by its paste (Explorer).
                unsafe {
                    if OpenClipboard(Some(app.hwnd)).is_ok() {
                        let _ = EmptyClipboard();
                        let _ = CloseClipboard();
                    }
                }
            }
        }
        return;
    }
    if let Some(bg) = background_cm(app.hwnd, &dir) {
        unsafe {
            if let Ok(m) = CreatePopupMenu() {
                let _ = bg.QueryContextMenu(m, 0, SHELL_FIRST, SHELL_LAST, CMF_NORMAL);
                if !invoke_verb(&bg, app.hwnd, "paste") {
                    app.note("Vložení se nepodařilo.");
                }
                let _ = DestroyMenu(m);
            }
        }
    }
}

/// Explorer's "Copy as path": quoted full paths, one per line.
pub fn copy_path(app: &mut App) {
    let paths = app.tab().selected_paths();
    let text = if paths.is_empty() {
        app.tab()
            .loc
            .folder()
            .map(|d| format!("\"{}\"", d.display()))
            .unwrap_or_default()
    } else {
        paths
            .iter()
            .map(|p| format!("\"{}\"", p.display()))
            .collect::<Vec<_>>()
            .join("\r\n")
    };
    if set_clipboard_text(app.hwnd, &text) {
        app.note("Cesta zkopírována");
    }
}

fn set_clipboard_text(hwnd: HWND, text: &str) -> bool {
    let w = wide(text);
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let bytes = w.len() * 2;
        let ok = (|| -> Option<()> {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                return None;
            }
            std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
            let _ = GlobalUnlock(h);
            SetClipboardData(13, Some(HANDLE(h.0))).ok()?; // CF_UNICODETEXT
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

/// Deletes the selection to the Recycle Bin (or permanently with Shift), on a worker thread with
/// the shell's own progress and confirmation UI.
pub fn delete(app: &mut App, permanent: bool) {
    let paths = app.tab().selected_paths();
    if paths.is_empty()
        || !matches!(
            app.tab().loc,
            Loc::Folder(_) | Loc::Search { .. } | Loc::Recents | Loc::Tag(_)
        )
    {
        return;
    }
    // Keep the cursor near where the deleted items were.
    let next = app.tab().sel.indices().last().map(|&i| i + 1);
    let next_name = next
        .and_then(|i| app.tab().entries.get(i))
        .map(|e| e.name.clone());
    file_op(
        app.hwnd,
        paths,
        None,
        if permanent {
            Op::DeletePermanent
        } else {
            Op::Recycle
        },
        true,
    );
    if let Some(n) = next_name {
        app.tab_mut().sel.clear();
        if let Some(i) = app.tab().entries.iter().position(|e| e.name == n) {
            app.tab_mut().sel.focus = Some(i);
        }
    }
}

#[derive(Clone, Copy)]
pub enum Op {
    Recycle,
    DeletePermanent,
    Copy,
    Move,
}

/// Runs an IFileOperation on its own STA thread; the shell shows progress, conflicts and errors.
/// With `record`, what actually happened goes onto the undo stack when it finishes.
pub fn file_op(owner: HWND, paths: Vec<PathBuf>, dest: Option<PathBuf>, op: Op, record: bool) {
    let owner_raw = owner.0 as usize;
    let hub = crate::win::hub().0 as usize;
    let guard = crate::win::OpGuard::new();
    std::thread::spawn(move || unsafe {
        let _guard = guard;
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let done = file_op_sync(HWND(owner_raw as *mut c_void), &paths, dest.as_deref(), op);
        if record {
            if let Some(op) = done {
                let b = Box::into_raw(Box::new(crate::undo::Done { op }));
                if PostMessageW(Some(HWND(hub as *mut c_void)), crate::win::WM_APP_OPDONE, WPARAM(0), LPARAM(b as isize)).is_err() {
                    drop(Box::from_raw(b));
                }
            }
        }
        CoUninitialize();
    });
}

/// The same, on the calling (STA) thread. Returns what happened, for undo.
pub fn file_op_sync(owner: HWND, paths: &[PathBuf], dest: Option<&Path>, op: Op) -> Option<crate::undo::Op> {
    let target = |p: &PathBuf| dest.zip(p.file_name()).map(|(d, n)| d.join(n));
    let existed: Vec<bool> = paths.iter().map(|p| target(p).is_some_and(|t| t.exists())).collect();
    let run = || -> Result<()> {
        unsafe {
            let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            fo.SetOwnerWindow(owner)?;
            let flags = match op {
                Op::Recycle => FOF_ALLOWUNDO.0 as u32 | FOFX_RECYCLEONDELETE.0 | FOF_WANTNUKEWARNING.0 as u32,
                Op::DeletePermanent => FOF_WANTNUKEWARNING.0 as u32,
                Op::Copy | Op::Move => FOF_ALLOWUNDO.0 as u32 | FOF_NOCONFIRMMKDIR.0 as u32,
            };
            fo.SetOperationFlags(FILEOPERATION_FLAGS(flags))?;
            let dest_item: Option<IShellItem> = match dest {
                Some(d) => Some(SHCreateItemFromParsingName(PCWSTR(wide(&d.to_string_lossy()).as_ptr()), None)?),
                None => None,
            };
            for p in paths {
                let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide(&p.to_string_lossy()).as_ptr()), None)?;
                match op {
                    Op::Recycle | Op::DeletePermanent => fo.DeleteItem(&item, None)?,
                    Op::Copy => fo.CopyItem(&item, dest_item.as_ref(), PCWSTR::null(), None)?,
                    Op::Move => fo.MoveItem(&item, dest_item.as_ref(), PCWSTR::null(), None)?,
                }
            }
            fo.PerformOperations()
        }
    };
    if let Err(e) = run() {
        crate::prefs::log(&format!("file operation failed: {e}"));
    }
    match op {
        Op::Recycle => Some(crate::undo::Op::Recycled(paths.iter().filter(|p| !p.exists()).cloned().collect())),
        Op::DeletePermanent => None,
        Op::Move => Some(crate::undo::Op::Moved(
            paths
                .iter()
                .filter_map(|p| target(p).map(|t| (p.clone(), t)))
                .filter(|(p, t)| !p.exists() && t.exists())
                .collect(),
        )),
        Op::Copy => Some(crate::undo::Op::Created(
            paths
                .iter()
                .zip(existed)
                .filter_map(|(p, before)| target(p).filter(|t| !before && t.exists()))
                .collect(),
        )),
    }
}

pub fn rename(old: &Path, new_name: &str) -> std::result::Result<(), String> {
    fm_fs::validate_name(new_name).map_err(|e| e.to_string())?;
    let Some(parent) = old.parent() else {
        return Err("Kořen disku nelze přejmenovat.".into());
    };
    let new = parent.join(new_name);
    let (wo, wn) = (fsys::wide_path(old), fsys::wide_path(&new));
    unsafe {
        if let Err(e) = MoveFileExW(PCWSTR(wo.as_ptr()), PCWSTR(wn.as_ptr()), MOVE_FILE_FLAGS(0)) {
            let code = (e.code().0 & 0xFFFF) as u32;
            return Err(match code {
                183 | 80 => format!("Položka s názvem „{new_name}“ už existuje."),
                5 => "K přejmenování nemáte oprávnění.".into(),
                32 => "Soubor je otevřený v jiné aplikaci.".into(),
                _ => format!("Přejmenování se nepodařilo ({}).", e.message()),
            });
        }
        let event = if new.is_dir() {
            SHCNE_RENAMEFOLDER
        } else {
            SHCNE_RENAMEITEM
        };
        let (a, b) = (wide(&old.to_string_lossy()), wide(&new.to_string_lossy()));
        SHChangeNotify(
            event,
            SHCNF_PATHW,
            Some(a.as_ptr() as *const c_void),
            Some(b.as_ptr() as *const c_void),
        );
    }
    crate::tags::renamed(old, &new);
    Ok(())
}

pub fn new_folder(app: &mut App) {
    let Some(dir) = app.tab().loc.folder().map(Path::to_path_buf) else {
        app.note("Tady nelze vytvořit složku.");
        return;
    };
    match fm_fs::create_new_dir(&dir, "Nová složka") {
        Ok(p) => {
            notify_created(&p, true);
            crate::undo::push(crate::undo::Op::Created(vec![p.clone()]));
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            if app.tab().view == View::Columns || app.tab().view == View::Gallery {
                app.set_view(View::List);
            }
            app.reload(name.as_deref());
            crate::win::begin_rename(app);
        }
        Err(e) => app.note(&format!(
            "Složku nelze vytvořit: {}",
            crate::app::describe_io(&e)
        )),
    }
}

fn notify_created(p: &Path, dir: bool) {
    let w = wide(&p.to_string_lossy());
    unsafe {
        SHChangeNotify(
            if dir { SHCNE_MKDIR } else { SHCNE_CREATE },
            SHCNF_PATHW,
            Some(w.as_ptr() as *const c_void),
            None,
        )
    };
}

/// Finder's "New Folder with Selection": creates a folder and moves the selection into it.
pub fn new_folder_with_selection(app: &mut App) {
    let Some(dir) = app.tab().loc.folder().map(Path::to_path_buf) else {
        return;
    };
    let paths = app.tab().selected_paths();
    if paths.is_empty() {
        return;
    }
    match fm_fs::create_new_dir(&dir, "Nová složka s položkami") {
        Ok(p) => {
            notify_created(&p, true);
            crate::undo::push(crate::undo::Op::Group(vec![
                crate::undo::Op::Created(vec![p.clone()]),
                crate::undo::Op::Moved(
                    paths.iter().filter_map(|s| s.file_name().map(|n| (s.clone(), p.join(n)))).collect(),
                ),
            ]));
            file_op(app.hwnd, paths, Some(p.clone()), Op::Move, false);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            app.reload(name.as_deref());
            crate::win::begin_rename(app);
        }
        Err(e) => app.note(&crate::app::describe_io(&e)),
    }
}

/// Finder's Duplicate (Ctrl+D): "name - kopie.ext" next to the original, via the shell copy engine.
pub fn duplicate(app: &mut App) {
    let Some(dir) = app.tab().loc.folder().map(Path::to_path_buf) else {
        return;
    };
    let paths = app.tab().selected_paths();
    duplicate_into(app, &dir, paths);
}

/// Copies `paths` into `dir` as "name - kopie" (", 2", …), which is what copying an item into
/// its own folder does in Explorer. The copies go onto the undo stack.
fn duplicate_into(app: &mut App, dir: &Path, paths: Vec<PathBuf>) {
    let mut made = Vec::new();
    let mut taken = names(dir);
    for p in paths {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_dir = p.is_dir();
        let (stem, ext) = match name.rfind('.') {
            Some(i) if i > 0 && !is_dir => (name[..i].to_string(), name[i + 1..].to_string()),
            _ => (name.clone(), String::new()),
        };
        let target = fm_fs::unique_name(
            |n| taken.contains(&n.to_lowercase()),
            &format!("{stem} - kopie"),
            &ext,
        );
        taken.insert(target.to_lowercase());
        made.push(dir.join(&target));
        std::thread::spawn({
            let (p, dir, owner) = (p.clone(), dir.to_path_buf(), app.hwnd.0 as usize);
            let guard = crate::win::OpGuard::new();
            move || unsafe {
                let _guard = guard;
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let run = || -> Result<()> {
                    let fo: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
                    fo.SetOwnerWindow(HWND(owner as *mut c_void))?;
                    fo.SetOperationFlags(FILEOPERATION_FLAGS(FOF_ALLOWUNDO.0 as u32))?;
                    let item: IShellItem = SHCreateItemFromParsingName(
                        PCWSTR(wide(&p.to_string_lossy()).as_ptr()),
                        None,
                    )?;
                    let dest: IShellItem = SHCreateItemFromParsingName(
                        PCWSTR(wide(&dir.to_string_lossy()).as_ptr()),
                        None,
                    )?;
                    let tw = wide(&target);
                    fo.CopyItem(&item, &dest, PCWSTR(tw.as_ptr()), None)?;
                    fo.PerformOperations()
                };
                let _ = run();
                CoUninitialize();
            }
        });
    }
    if !made.is_empty() {
        crate::undo::push(crate::undo::Op::Created(made));
    }
}

pub fn open_terminal(app: &mut App) {
    let Some(dir) = app.tab().loc.folder().map(Path::to_path_buf) else {
        return;
    };
    let ok = std::process::Command::new("wt.exe")
        .args(["-d", &dir.to_string_lossy()])
        .spawn()
        .is_ok();
    if !ok {
        let _ = std::process::Command::new("powershell.exe")
            .current_dir(&dir)
            .spawn();
    }
}

/// Drag the selection out (to Explorer, a browser, the desktop…) with the shell's own data object.
/// A tag location lists what carries the tag, so it reloads; elsewhere only the dots change.
/// Tags live in one store for all windows: every window showing a tag place or grouped by tags
/// lists again (background tabs do it when shown).
pub fn tags_changed(app: &mut App) {
    fn refresh(a: &mut App) {
        if matches!(a.tab().loc, Loc::Tag(_)) || a.groups_by_tags() {
            a.reload(None);
        }
        a.invalidate();
    }
    refresh(app);
    crate::win::for_each_other_app(app.hwnd, refresh);
}

/// The shell's drop target for dropping onto `path` (into a folder or drive, onto an app or a .zip).
pub fn drop_target(hwnd: HWND, path: &Path) -> Option<IDropTarget> {
    let (folder, _owned, children) = parent_and_children(&[path.to_path_buf()])?;
    unsafe { folder.GetUIObjectOf::<IDropTarget>(hwnd, &children, None).ok() }
}

/// The shell's data object for `paths` (what a drag out of the window carries).
pub fn data_object(hwnd: HWND, paths: &[PathBuf]) -> Option<IDataObject> {
    let (folder, _owned, children) = parent_and_children(paths)?;
    unsafe { folder.GetUIObjectOf::<IDataObject>(hwnd, &children, None).ok() }
}

pub fn drag_out(app: &mut App, i: usize) {
    if !app.tab().sel.is(i) {
        app.tab_mut().sel.select_only(i);
    }
    let paths = app.tab().selected_paths();
    let Some((folder, _o, children)) = parent_and_children(&paths) else {
        return;
    };
    unsafe {
        let Ok(dobj) = folder.GetUIObjectOf::<IDataObject>(app.hwnd, &children, None) else {
            return;
        };
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
        let _ = SHDoDragDrop(
            Some(app.hwnd),
            &dobj,
            None,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        );
    }
    app.drag = crate::app::Drag::None;
    app.pressed = crate::app::Hit::None;
    app.invalidate();
}

pub fn timer(_app: &mut App, _id: usize) {}

// ---- Recents -------------------------------------------------------------------------------------------------

/// Recently opened files from Windows' Recent folder, newest first. Folders are left out (Finder).
pub fn recent_items(limit: usize) -> Vec<Entry> {
    let Some(dir) = fsys::known_folder(&FOLDERID_Recent) else {
        return Vec::new();
    };
    let mut links: Vec<(u64, PathBuf)> = fsys::list_dir(&dir, true)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.name.to_lowercase().ends_with(".lnk"))
        .map(|e| (e.modified, dir.join(&e.name)))
        .collect();
    links.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    unsafe {
        let Ok(sl) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER)
        else {
            return out;
        };
        let Ok(pf) = sl.cast::<IPersistFile>() else {
            return out;
        };
        for (opened, lnk) in links {
            if out.len() >= limit {
                break;
            }
            let w = wide(&lnk.to_string_lossy());
            if pf.Load(PCWSTR(w.as_ptr()), STGM_READ).is_err() {
                continue;
            }
            let mut buf = [0u16; 1024];
            if sl
                .GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
                .is_err()
            {
                continue;
            }
            let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
            if n == 0 {
                continue;
            }
            let target = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
            if !seen.insert(target.to_string_lossy().to_lowercase()) {
                continue;
            }
            let tw = wide(&target.to_string_lossy());
            let mut data = WIN32_FILE_ATTRIBUTE_DATA::default();
            if GetFileAttributesExW(
                PCWSTR(tw.as_ptr()),
                GetFileExInfoStandard,
                (&mut data as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
            )
            .is_err()
            {
                continue;
            }
            if data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                continue;
            }
            let _ = opened;
            out.push(Entry {
                name: target
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                is_dir: false,
                size: ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64,
                modified: ((data.ftLastWriteTime.dwHighDateTime as u64) << 32)
                    | data.ftLastWriteTime.dwLowDateTime as u64,
                created: ((data.ftCreationTime.dwHighDateTime as u64) << 32)
                    | data.ftCreationTime.dwLowDateTime as u64,
                attrs: data.dwFileAttributes,
                full: Some(target),
                shell: None,
                origin: None,
                sized: false,
            });
        }
    }
    out
}

pub fn names(dir: &Path) -> HashSet<String> {
    fsys::list_dir(dir, true)
        .map(|v| v.into_iter().map(|e| e.name.to_lowercase()).collect())
        .unwrap_or_default()
}

pub fn created_since(dir: &Path, before: &HashSet<String>) -> Option<String> {
    fsys::list_dir(dir, true)
        .ok()?
        .into_iter()
        .filter(|e| !before.contains(&e.name.to_lowercase()))
        .max_by_key(|e| e.created)
        .map(|e| e.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subfolders_lists_only_visible_folders() {
        let d = std::env::temp_dir().join(format!("fm-subfolders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for f in ["Beta", "alfa", "Skrytá"] {
            std::fs::create_dir_all(d.join(f)).unwrap();
        }
        std::fs::write(d.join("soubor.txt"), "x").unwrap();
        let w = wide(&d.join("Skrytá").to_string_lossy());
        unsafe { SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_ATTRIBUTE_HIDDEN).unwrap() };
        let names = |hidden| {
            let mut v: Vec<String> = subfolders(&d, hidden).into_iter().map(|(n, _)| n).collect();
            v.sort();
            v
        };
        assert_eq!(names(false), ["Beta", "alfa"]);
        assert_eq!(names(true), ["Beta", "Skrytá", "alfa"]);
        assert!(subfolders(&d.join("chybí"), true).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
