//! fm windows in the shell's window list (IShellWindows), registered the way Explorer registers
//! its own. "Show in folder" (SHOpenFolderAndSelectItems: browsers' downloads, Office, Start's
//! "Open file location") looks there first: it finds the fm window already showing the folder
//! and asks it to select the item, and while it waits for a new window it waits for ours. Tools
//! that walk `Shell.Application.Windows()` (scripts, launchers) see fm's folders too.
//!
//! One object per window answers as Explorer's browser does: IWebBrowser2 (HWND, LocationURL,
//! Navigate, Quit), IServiceProvider, IShellBrowser and IShellView (SelectItem). Calls come from
//! other processes at any time, also inside our modal loops, so they never touch the App: they
//! post to the window. Pattern after the Files launcher (OpenInFolder.cpp, MIT).
use crate::app::{App, Loc};
use crate::shellns::ShellId;
use std::{cell::RefCell, collections::HashMap, ffi::c_void};
use windows::Win32::{
    Foundation::*,
    System::{
        Com::*,
        Ole::*,
        Threading::GetCurrentThreadId,
        Variant::{InitVariantFromBuffer, VARIANT},
    },
    UI::{
        Shell::{Common::ITEMIDLIST, *},
        WindowsAndMessaging::*,
    },
};
use windows_core::{BOOL, BSTR, ComObject, GUID, HRESULT, IUnknownImpl, Interface, PCWSTR, Ref, Result, implement};

/// Posted to a window: wparam SELECT (lparam: Box<(ShellId, flags)>) or NAVIGATE (Box<String>).
pub const WM_APP_SHELLWIN: u32 = WM_APP + 21;
const SELECT: usize = 0;
const NAVIGATE: usize = 1;
/// sync() was asked again while it was calling out: run it once that call is over.
const RESYNC: usize = 2;
/// Window timer: held calls are tried again (replay).
pub const TIMER: usize = 4;

struct Reg {
    obj: ComObject<Browser>,
    cookie: i32,
    at: ShellId,
    /// The list that issued the cookie (a restarted Explorer has a new one).
    sw: IShellWindows,
    /// LIST_GEN when registered: older entries belong to a list Explorer took with it.
    list: u32,
}

thread_local! {
    static REGS: RefCell<HashMap<isize, Reg>> = RefCell::new(HashMap::new());
    static WINDOWS: RefCell<Option<IShellWindows>> = const { RefCell::new(None) };
    /// Calls into the list pump messages, so sync() can be entered again from inside one.
    static IN_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// SELECT / NAVIGATE that came inside a menu or dialog: (window, kind, payload).
    static HELD: RefCell<Vec<(isize, usize, isize)>> = const { RefCell::new(Vec::new()) };
    static LIST_GEN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The shell's window list. Kept, and made again when Explorer restarted (its old proxy fails).
fn shell_windows(fresh: bool) -> Option<IShellWindows> {
    WINDOWS.with(|w| {
        if fresh || w.borrow().is_none() {
            *w.borrow_mut() = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok() };
        }
        w.borrow().clone()
    })
}

/// The shell location `loc` shows, if Explorer could show it too.
fn location(loc: &Loc) -> Option<ShellId> {
    match loc {
        Loc::Folder(p) => unsafe {
            let w = crate::fsys::wide(&p.to_string_lossy());
            let id = ILCreateFromPathW(PCWSTR(w.as_ptr()));
            (!id.is_null()).then(|| {
                let s = ShellId::from_raw(id);
                ILFree(Some(id));
                s
            })
        },
        Loc::Computer => ShellId::known(&FOLDERID_ComputerFolder),
        Loc::Shell { id, .. } => Some(id.clone()),
        _ => None,
    }
}

/// The same place: a folder's ID list carries its timestamps, so bytes differ after any change.
fn same(a: &ShellId, b: &ShellId) -> bool {
    a == b || unsafe { ILIsEqual(a.as_ptr(), b.as_ptr()).as_bool() }
}

/// An ID list as the VARIANT IShellWindows takes (a byte array).
fn variant(id: &ShellId) -> Option<VARIANT> {
    unsafe { InitVariantFromBuffer(id.0.as_ptr() as *const c_void, id.0.len() as u32).ok() }
}

/// After every navigation and tab switch: the window's entry follows what it shows (and goes
/// while it shows something Explorer has no location for: Recents, tags, a search).
pub fn sync(app: &App) {
    if app.offscreen || app.hwnd.is_invalid() {
        return;
    }
    if IN_SYNC.with(|b| b.replace(true)) {
        unsafe {
            let _ = PostMessageW(Some(app.hwnd), WM_APP_SHELLWIN, WPARAM(RESYNC), LPARAM(1));
        }
        return;
    }
    sync_now(app);
    IN_SYNC.with(|b| b.set(false));
}

fn sync_now(app: &App) {
    let key = app.hwnd.0 as isize;
    let Some(at) = location(&app.tab().loc) else {
        forget(app.hwnd);
        return;
    };
    let list = LIST_GEN.with(|g| g.get());
    let moved = REGS.with(|r| r.borrow().get(&key).map(|reg| (reg.list != list || !same(&reg.at, &at)).then(|| (reg.obj.clone(), reg.cookie, reg.sw.clone()))));
    match moved {
        Some(None) => {}
        Some(Some(_)) if REGS.with(|r| r.borrow().get(&key).is_some_and(|reg| reg.list != list)) => {
            drop_reg(app.hwnd);
            register(app.hwnd, at, false);
        }
        Some(Some((obj, cookie, sw))) => {
            *obj.folder.borrow_mut() = Some(at.clone());
            if variant(&at).is_some_and(|v| unsafe { sw.OnNavigate(cookie, &v).is_ok() }) {
                REGS.with(|r| r.borrow_mut().get_mut(&key).map(|reg| reg.at = at));
            } else {
                // Explorer restarted: the entry went with it (revoked anyway, in case not).
                drop_reg(app.hwnd);
                register(app.hwnd, at, true);
            }
        }
        None => register(app.hwnd, at, false),
    }
}

fn register(hwnd: HWND, at: ShellId, fresh: bool) {
    let Some(v) = variant(&at) else { return };
    let Some(sw) = shell_windows(fresh) else { return };
    let obj = ComObject::new(Browser { hwnd, folder: RefCell::new(Some(at.clone())) });
    let Ok(disp) = obj.cast::<IDispatch>() else { return };
    unsafe {
        // Explorer's order: pending (by thread, so a caller waiting for the folder finds it),
        // then the window, which completes it (usually under the same cookie).
        let pending = sw.RegisterPending(GetCurrentThreadId() as i32, &v, &VARIANT::default(), SWC_BROWSER).ok();
        let Ok(cookie) = sw.Register(&disp, hwnd.0 as isize as i32, SWC_BROWSER) else {
            if let Some(p) = pending {
                let _ = sw.Revoke(p);
            }
            *obj.folder.borrow_mut() = None;
            if !fresh {
                register(hwnd, at, true);
            }
            return;
        };
        if let Some(p) = pending.filter(|&p| p != cookie) {
            let _ = sw.Revoke(p);
        }
        let _ = sw.OnNavigate(cookie, &v);
        // Calls are serialized by IN_SYNC, so the window has no entry yet.
        drop_reg(hwnd);
        let list = LIST_GEN.with(|g| g.get());
        REGS.with(|r| r.borrow_mut().insert(hwnd.0 as isize, Reg { obj, cookie, at, sw, list }));
    }
}

/// Removes the window's entry, revoking it in the list that issued it.
fn drop_reg(hwnd: HWND) {
    let Some(reg) = REGS.with(|r| r.borrow_mut().remove(&(hwnd.0 as isize))) else { return };
    // Calls still arriving (the shell holds the object a while) find nothing to act on.
    *reg.obj.folder.borrow_mut() = None;
    unsafe {
        let _ = reg.sw.Revoke(reg.cookie);
    }
}

/// The window closed (or shows no shell location): its entry goes.
pub fn forget(hwnd: HWND) {
    drop_reg(hwnd);
}

/// The window is going: its entry, and calls still held for it (their payloads freed).
pub fn destroyed(hwnd: HWND) {
    drop_reg(hwnd);
    let key = hwnd.0 as isize;
    let gone: Vec<(isize, usize, isize)> = HELD.with(|h| {
        let mut h = h.borrow_mut();
        let (gone, keep) = h.drain(..).partition(|e| e.0 == key);
        *h = keep;
        gone
    });
    for (_, kind, l) in gone {
        unsafe {
            match kind {
                SELECT => drop(Box::from_raw(l as *mut (ShellId, u32))),
                NAVIGATE => drop(Box::from_raw(l as *mut String)),
                _ => {}
            }
        }
    }
}

/// TIMER: calls held during a menu or dialog, handled now (or held again while it lasts).
pub fn replay(app: &mut App) {
    unsafe {
        let _ = KillTimer(Some(app.hwnd), TIMER);
    }
    let key = app.hwnd.0 as isize;
    let mine: Vec<(isize, usize, isize)> = HELD.with(|h| {
        let mut h = h.borrow_mut();
        let (mine, keep) = h.drain(..).partition(|e| e.0 == key);
        *h = keep;
        mine
    });
    for (_, kind, l) in mine {
        handle(app, WPARAM(kind), LPARAM(l));
    }
}

/// Explorer restarted ("TaskbarCreated"): the list is new and empty.
pub fn explorer_restarted(app: &App) {
    thread_local! {
        static LAST: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
    }
    // Every window gets the broadcast; the list is made again once for all of them.
    let now = std::time::Instant::now();
    if LAST.with(|l| l.replace(Some(now))).is_none_or(|t| now.duration_since(t).as_secs() >= 3) {
        WINDOWS.with(|w| *w.borrow_mut() = None);
        LIST_GEN.with(|g| g.set(g.get().wrapping_add(1)));
    }
    sync(app);
}

/// The window's side of a posted call.
pub fn handle(app: &mut App, w: WPARAM, l: LPARAM) {
    if l.0 == 0 {
        return;
    }
    if w.0 == RESYNC {
        sync(app);
        return;
    }
    if crate::win::nested() {
        // Inside a menu or dialog: its command must still act on what it was opened for.
        HELD.with(|h| h.borrow_mut().push((app.hwnd.0 as isize, w.0, l.0)));
        unsafe {
            SetTimer(Some(app.hwnd), TIMER, 200, None);
        }
        return;
    }
    match w.0 {
        SELECT => {
            let (id, flags) = *unsafe { Box::from_raw(l.0 as *mut (ShellId, u32)) };
            select(app, &id, flags);
        }
        NAVIGATE => {
            let text = *unsafe { Box::from_raw(l.0 as *mut String) };
            let cwd = app.tab().loc.folder().map(std::path::Path::to_path_buf).unwrap_or_default();
            match crate::target::parse(&text, &cwd) {
                crate::target::Target::Open { loc, select } => app.go_select(loc, select),
                crate::target::Target::Explorer(a) => crate::win::launch_explorer(&a),
                crate::target::Target::Missing(m) => app.note(&m),
            }
        }
        _ => {}
    }
    unsafe {
        if IsIconic(app.hwnd).as_bool() {
            let _ = ShowWindow(app.hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(app.hwnd);
    }
}

/// SelectItem: the item is found the way the current place lists it (a name in a folder, a
/// drive root in This PC, an ID list in a shell place).
fn select(app: &mut App, id: &ShellId, flags: u32) {
    if flags & SVSI_SELECT.0 as u32 == 0 || app.busy() {
        return;
    }
    let path = crate::shellns::fs_path(id);
    let find = |app: &App| {
        let t = app.tab();
        t.entries.iter().position(|e| match &t.loc {
            Loc::Folder(_) => path.as_ref().and_then(|p| p.file_name()).is_some_and(|n| e.name == n.to_string_lossy()),
            Loc::Computer => path.as_ref().is_some_and(|p| e.full.as_ref() == Some(p)),
            _ => e.shell.as_ref() == Some(id),
        })
    };
    let only = flags & SVSI_DESELECTOTHERS.0 as u32 != 0 || app.tab().sel.count() == 0;
    let mut at = find(app);
    if at.is_none() && matches!(app.tab().loc, Loc::Folder(_)) {
        // Just created (a download that finished): not listed yet.
        app.reload(None);
        at = find(app);
    }
    let Some(i) = at else { return };
    let sel = &mut app.tab_mut().sel;
    if only {
        sel.select_only(i);
    } else if !sel.is(i) {
        sel.toggle(i);
    }
    if flags & (SVSI_FOCUSED.0 as u32 | SVSI_ENSUREVISIBLE.0 as u32) != 0 || only {
        app.tab_mut().sel.focus = Some(i);
        app.reveal(i);
    }
    if flags & SVSI_EDIT.0 as u32 == SVSI_EDIT.0 as u32 {
        crate::win::begin_rename(app);
    }
    app.invalidate();
}

fn post(hwnd: HWND, kind: usize, payload: *mut c_void, free: impl FnOnce()) -> Result<()> {
    unsafe {
        if PostMessageW(Some(hwnd), WM_APP_SHELLWIN, WPARAM(kind), LPARAM(payload as isize)).is_err() {
            free();
            return Err(E_FAIL.into());
        }
        // While the caller still holds the foreground right it may have passed on.
        let _ = SetForegroundWindow(hwnd);
    }
    Ok(())
}

fn post_navigate(hwnd: HWND, text: String) -> Result<()> {
    if text.trim().is_empty() {
        return Err(E_INVALIDARG.into());
    }
    let p = Box::into_raw(Box::new(text));
    post(hwnd, NAVIGATE, p as *mut c_void, || drop(unsafe { Box::from_raw(p) }))
}

#[implement(IWebBrowser2, IServiceProvider, IShellBrowser, IShellView)]
struct Browser {
    hwnd: HWND,
    /// What the window shows (absolute), for SelectItem and LocationURL; None once it closed.
    folder: RefCell<Option<ShellId>>,
}

impl Browser {
    fn folder(&self) -> Result<ShellId> {
        self.folder.borrow().clone().ok_or_else(|| E_FAIL.into())
    }
}

/// This object as another of its interfaces.
fn cast_self<I: Interface>(this: &Browser_Impl) -> Result<I> {
    let mut p = std::ptr::null_mut();
    unsafe {
        this.QueryInterface(&I::IID, &mut p).ok()?;
        Ok(I::from_raw(p))
    }
}

fn not_impl<T>() -> Result<T> {
    Err(E_NOTIMPL.into())
}

/// Type information from a registered type library: scripts reach these objects through
/// IDispatch (`.HWND`, `.LocationURL`, `.Document.Folder`), and DispInvoke maps names to the
/// interface methods below.
fn type_info_of(lib: &GUID, major: u16, minor: u16, iid: &GUID) -> Result<ITypeInfo> {
    thread_local! {
        static TI: RefCell<HashMap<u128, ITypeInfo>> = RefCell::new(HashMap::new());
    }
    let key = iid.to_u128();
    if let Some(t) = TI.with(|t| t.borrow().get(&key).cloned()) {
        return Ok(t);
    }
    let t = unsafe { LoadRegTypeLib(lib, major, minor, 0)?.GetTypeInfoOfGuid(iid)? };
    TI.with(|m| m.borrow_mut().insert(key, t.clone()));
    Ok(t)
}

/// SHDocVw: IWebBrowser2.
fn type_info() -> Result<ITypeInfo> {
    const LIBID_SHDOCVW: GUID = GUID::from_u128(0xeab22ac0_30c1_11cf_a7eb_0000c05bae0b);
    type_info_of(&LIBID_SHDOCVW, 1, 1, &IWebBrowser2::IID)
}

/// Shell32: IShellFolderViewDual (a window's `.Document`).
fn view_type_info() -> Result<ITypeInfo> {
    const LIBID_SHELL32: GUID = GUID::from_u128(0x50a7e9b0_70ef_11d1_b75a_00a0c90564fe);
    type_info_of(&LIBID_SHELL32, 1, 0, &IShellFolderViewDual::IID)
}

impl IDispatch_Impl for Browser_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> {
        Ok(type_info().is_ok() as u32)
    }
    fn GetTypeInfo(&self, itinfo: u32, _: u32) -> Result<ITypeInfo> {
        if itinfo != 0 {
            return Err(DISP_E_BADINDEX.into());
        }
        type_info()
    }
    fn GetIDsOfNames(&self, _: *const GUID, names: *const PCWSTR, n: u32, _: u32, ids: *mut i32) -> Result<()> {
        unsafe { DispGetIDsOfNames(&type_info()?, names, n, ids) }
    }
    fn Invoke(&self, id: i32, _: *const GUID, _: u32, flags: DISPATCH_FLAGS, params: *const DISPPARAMS, result: *mut VARIANT, excep: *mut EXCEPINFO, argerr: *mut u32) -> Result<()> {
        let ti = type_info().map_err(|_| windows_core::Error::from(DISP_E_MEMBERNOTFOUND))?;
        let this: IWebBrowser2 = cast_self(self)?;
        unsafe { DispInvoke(this.as_raw(), &ti, id, flags.0, params as *mut DISPPARAMS, result, excep, argerr) }
    }
}

fn dispatch_err<T>() -> Result<T> {
    Err(windows_core::Error::from(DISP_E_MEMBERNOTFOUND))
}

/// A window's `.Document`: Explorer's folder view as scripts see it (`.Folder.Self.Path`,
/// `.Title`, `.Items()`: the place the window shows; the selection is not offered). The shell
/// also reaches the view through it (IShellView, IServiceProvider), so those go to the browser.
#[implement(IShellFolderViewDual, IShellView, IServiceProvider)]
struct View {
    browser: ComObject<Browser>,
}

impl IDispatch_Impl for View_Impl {
    fn GetTypeInfoCount(&self) -> Result<u32> {
        Ok(view_type_info().is_ok() as u32)
    }
    fn GetTypeInfo(&self, itinfo: u32, _: u32) -> Result<ITypeInfo> {
        if itinfo != 0 {
            return Err(DISP_E_BADINDEX.into());
        }
        view_type_info()
    }
    fn GetIDsOfNames(&self, _: *const GUID, names: *const PCWSTR, n: u32, _: u32, ids: *mut i32) -> Result<()> {
        unsafe { DispGetIDsOfNames(&view_type_info()?, names, n, ids) }
    }
    fn Invoke(&self, id: i32, _: *const GUID, _: u32, flags: DISPATCH_FLAGS, params: *const DISPPARAMS, result: *mut VARIANT, excep: *mut EXCEPINFO, argerr: *mut u32) -> Result<()> {
        let ti = view_type_info().map_err(|_| windows_core::Error::from(DISP_E_MEMBERNOTFOUND))?;
        let mut p = std::ptr::null_mut();
        unsafe {
            self.QueryInterface(&IShellFolderViewDual::IID, &mut p).ok()?;
            let this = IShellFolderViewDual::from_raw(p);
            DispInvoke(this.as_raw(), &ti, id, flags.0, params as *mut DISPPARAMS, result, excep, argerr)
        }
    }
}

impl IShellFolderViewDual_Impl for View_Impl {
    fn Application(&self) -> Result<IDispatch> {
        unsafe { CoCreateInstance::<_, IShellDispatch>(&Shell, None, CLSCTX_INPROC_SERVER)?.cast() }
    }
    fn Parent(&self) -> Result<IDispatch> {
        self.browser.cast()
    }
    fn Folder(&self) -> Result<Folder> {
        let id = self.browser.folder()?;
        // NameSpace takes a path or a parsing name (`::{CLSID}` for This PC, the Recycle Bin).
        let name = crate::shellns::fs_path(&id).map(|p| p.to_string_lossy().into_owned()).or_else(|| crate::shellns::parsing_name(&id));
        let name = name.ok_or_else(|| windows_core::Error::from(E_FAIL))?;
        let shell: IShellDispatch = unsafe { CoCreateInstance(&Shell, None, CLSCTX_INPROC_SERVER)? };
        unsafe { shell.NameSpace(&VARIANT::from(BSTR::from(name))) }
    }
    fn SelectedItems(&self) -> Result<FolderItems> {
        dispatch_err()
    }
    fn FocusedItem(&self) -> Result<FolderItem> {
        dispatch_err()
    }
    fn SelectItem(&self, _: *const VARIANT, _: i32) -> Result<()> {
        dispatch_err()
    }
    fn PopupItemMenu(&self, _: Ref<FolderItem>, _: &VARIANT, _: &VARIANT) -> Result<BSTR> {
        dispatch_err()
    }
    fn Script(&self) -> Result<IDispatch> {
        dispatch_err()
    }
    fn ViewOptions(&self) -> Result<i32> {
        Ok(0)
    }
}

impl IServiceProvider_Impl for View_Impl {
    fn QueryService(&self, sid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if ppv.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        if unsafe { self.QueryInterface(riid, ppv) }.is_ok() {
            return Ok(());
        }
        // The browser answers every service by what it implements.
        let _ = sid;
        let unk: windows_core::IUnknown = self.browser.cast()?;
        unsafe { unk.query(riid, ppv).ok() }
    }
}

impl IOleWindow_Impl for View_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        Ok(self.browser.hwnd)
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> Result<()> {
        not_impl()
    }
}

impl IShellView_Impl for View_Impl {
    fn TranslateAccelerator(&self, _: *const MSG) -> HRESULT {
        S_FALSE
    }
    fn EnableModeless(&self, _: BOOL) -> Result<()> {
        not_impl()
    }
    fn UIActivate(&self, _: u32) -> Result<()> {
        not_impl()
    }
    fn Refresh(&self) -> Result<()> {
        not_impl()
    }
    fn CreateViewWindow(&self, _: Ref<IShellView>, _: *const FOLDERSETTINGS, _: Ref<IShellBrowser>, _: *const RECT) -> Result<HWND> {
        not_impl()
    }
    fn DestroyViewWindow(&self) -> Result<()> {
        not_impl()
    }
    fn GetCurrentInfo(&self) -> Result<FOLDERSETTINGS> {
        not_impl()
    }
    fn AddPropertySheetPages(&self, _: u32, _: windows::Win32::UI::Controls::LPFNSVADDPROPSHEETPAGE, _: LPARAM) -> Result<()> {
        not_impl()
    }
    fn SaveViewState(&self) -> Result<()> {
        not_impl()
    }
    fn SelectItem(&self, item: *const ITEMIDLIST, flags: u32) -> Result<()> {
        let v: IShellView = self.browser.cast()?;
        unsafe { v.SelectItem(item, flags) }
    }
    fn GetItemObject(&self, _: &_SVGIO, _: *const GUID, _: *mut *mut c_void) -> Result<()> {
        not_impl()
    }
}

impl IWebBrowser_Impl for Browser_Impl {
    fn GoBack(&self) -> Result<()> {
        not_impl()
    }
    fn GoForward(&self) -> Result<()> {
        not_impl()
    }
    fn GoHome(&self) -> Result<()> {
        not_impl()
    }
    fn GoSearch(&self) -> Result<()> {
        not_impl()
    }
    fn Navigate(&self, url: &BSTR, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT) -> Result<()> {
        post_navigate(self.hwnd, url.to_string())
    }
    fn Refresh(&self) -> Result<()> {
        not_impl()
    }
    fn Refresh2(&self, _: *const VARIANT) -> Result<()> {
        not_impl()
    }
    fn Stop(&self) -> Result<()> {
        not_impl()
    }
    fn Application(&self) -> Result<IDispatch> {
        cast_self(self)
    }
    fn Parent(&self) -> Result<IDispatch> {
        cast_self(self)
    }
    fn Container(&self) -> Result<IDispatch> {
        cast_self(self)
    }
    fn Document(&self) -> Result<IDispatch> {
        ComObject::new(View { browser: self.to_object() }).cast()
    }
    fn TopLevelContainer(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_TRUE)
    }
    fn Type(&self) -> Result<BSTR> {
        not_impl()
    }
    fn Left(&self) -> Result<i32> {
        not_impl()
    }
    fn SetLeft(&self, _: i32) -> Result<()> {
        not_impl()
    }
    fn Top(&self) -> Result<i32> {
        not_impl()
    }
    fn SetTop(&self, _: i32) -> Result<()> {
        not_impl()
    }
    fn Width(&self) -> Result<i32> {
        not_impl()
    }
    fn SetWidth(&self, _: i32) -> Result<()> {
        not_impl()
    }
    fn Height(&self) -> Result<i32> {
        not_impl()
    }
    fn SetHeight(&self, _: i32) -> Result<()> {
        not_impl()
    }
    fn LocationName(&self) -> Result<BSTR> {
        Ok(BSTR::from(self.folder()?.name()))
    }
    fn LocationURL(&self) -> Result<BSTR> {
        // Explorer's form: a file URL for a folder, empty for a virtual place.
        let Some(p) = crate::shellns::fs_path(&self.folder()?) else { return Ok(BSTR::new()) };
        let w = crate::fsys::wide(&p.to_string_lossy());
        let mut buf = vec![0u16; 4096];
        let mut n = buf.len() as u32;
        unsafe { UrlCreateFromPathW(PCWSTR(w.as_ptr()), windows_core::PWSTR(buf.as_mut_ptr()), &mut n, 0)? };
        Ok(BSTR::from_wide(&buf[..(n as usize).min(buf.len())]))
    }
    fn Busy(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_FALSE)
    }
}

impl IWebBrowserApp_Impl for Browser_Impl {
    fn Quit(&self) -> Result<()> {
        unsafe { PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
    }
    fn ClientToWindow(&self, _: *mut i32, _: *mut i32) -> Result<()> {
        not_impl()
    }
    fn PutProperty(&self, _: &BSTR, _: &VARIANT) -> Result<()> {
        not_impl()
    }
    fn GetProperty(&self, _: &BSTR) -> Result<VARIANT> {
        not_impl()
    }
    fn Name(&self) -> Result<BSTR> {
        Ok(BSTR::from("Soubory"))
    }
    fn HWND(&self) -> Result<SHANDLE_PTR> {
        Ok(SHANDLE_PTR(self.hwnd.0 as isize))
    }
    fn FullName(&self) -> Result<BSTR> {
        let exe = std::env::current_exe().map_err(|_| windows_core::Error::from(E_FAIL))?;
        Ok(BSTR::from(exe.to_string_lossy().as_ref()))
    }
    fn Path(&self) -> Result<BSTR> {
        let exe = std::env::current_exe().map_err(|_| windows_core::Error::from(E_FAIL))?;
        let dir = exe.parent().map(|d| format!("{}\\", d.display())).unwrap_or_default();
        Ok(BSTR::from(dir))
    }
    fn Visible(&self) -> Result<VARIANT_BOOL> {
        Ok(if unsafe { IsWindowVisible(self.hwnd).as_bool() } { VARIANT_TRUE } else { VARIANT_FALSE })
    }
    fn SetVisible(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn StatusBar(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetStatusBar(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn StatusText(&self) -> Result<BSTR> {
        not_impl()
    }
    fn SetStatusText(&self, _: &BSTR) -> Result<()> {
        not_impl()
    }
    fn ToolBar(&self) -> Result<i32> {
        not_impl()
    }
    fn SetToolBar(&self, _: i32) -> Result<()> {
        not_impl()
    }
    fn MenuBar(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetMenuBar(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn FullScreen(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetFullScreen(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
}

impl IWebBrowser2_Impl for Browser_Impl {
    fn Navigate2(&self, url: *const VARIANT, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT) -> Result<()> {
        // A path or URL (an ID list is not taken).
        let v = unsafe { url.as_ref() }.ok_or_else(|| windows_core::Error::from(E_INVALIDARG))?;
        let mut buf = vec![0u16; 32768];
        unsafe { windows::Win32::System::Variant::VariantToString(v, &mut buf)? };
        let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
        post_navigate(self.hwnd, String::from_utf16_lossy(&buf[..n]))
    }
    fn QueryStatusWB(&self, _: OLECMDID) -> Result<OLECMDF> {
        not_impl()
    }
    fn ExecWB(&self, _: OLECMDID, _: OLECMDEXECOPT, _: *const VARIANT, _: *mut VARIANT) -> Result<()> {
        not_impl()
    }
    fn ShowBrowserBar(&self, _: *const VARIANT, _: *const VARIANT, _: *const VARIANT) -> Result<()> {
        not_impl()
    }
    fn ReadyState(&self) -> Result<READYSTATE> {
        Ok(READYSTATE_COMPLETE)
    }
    fn Offline(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetOffline(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn Silent(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetSilent(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn RegisterAsBrowser(&self) -> Result<VARIANT_BOOL> {
        Ok(VARIANT_TRUE)
    }
    fn SetRegisterAsBrowser(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn RegisterAsDropTarget(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetRegisterAsDropTarget(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn TheaterMode(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetTheaterMode(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn AddressBar(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetAddressBar(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
    fn Resizable(&self) -> Result<VARIANT_BOOL> {
        not_impl()
    }
    fn SetResizable(&self, _: VARIANT_BOOL) -> Result<()> {
        not_impl()
    }
}

impl IServiceProvider_Impl for Browser_Impl {
    fn QueryService(&self, _: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        // Whatever service is named (top-level browser, folder view…), this object is it.
        if ppv.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { self.QueryInterface(riid, ppv).ok() }
    }
}

impl IOleWindow_Impl for Browser_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        Ok(self.hwnd)
    }
    fn ContextSensitiveHelp(&self, _: BOOL) -> Result<()> {
        not_impl()
    }
}

impl IShellBrowser_Impl for Browser_Impl {
    fn InsertMenusSB(&self, _: HMENU, _: *mut OLEMENUGROUPWIDTHS) -> Result<()> {
        not_impl()
    }
    fn SetMenuSB(&self, _: HMENU, _: isize, _: HWND) -> Result<()> {
        not_impl()
    }
    fn RemoveMenusSB(&self, _: HMENU) -> Result<()> {
        not_impl()
    }
    fn SetStatusTextSB(&self, _: &PCWSTR) -> Result<()> {
        not_impl()
    }
    fn EnableModelessSB(&self, _: BOOL) -> Result<()> {
        not_impl()
    }
    fn TranslateAcceleratorSB(&self, _: *const MSG, _: u16) -> Result<()> {
        not_impl()
    }
    fn BrowseObject(&self, _: *const ITEMIDLIST, _: u32) -> Result<()> {
        not_impl()
    }
    fn GetViewStateStream(&self, _: u32) -> Result<IStream> {
        not_impl()
    }
    fn GetControlWindow(&self, _: u32) -> Result<HWND> {
        not_impl()
    }
    fn SendControlMsg(&self, _: u32, _: u32, _: WPARAM, _: LPARAM, _: *mut LRESULT) -> Result<()> {
        not_impl()
    }
    fn QueryActiveShellView(&self) -> Result<IShellView> {
        cast_self(self)
    }
    fn OnViewWindowActive(&self, _: Ref<IShellView>) -> Result<()> {
        not_impl()
    }
    fn SetToolbarItems(&self, _: *const windows::Win32::UI::Controls::TBBUTTON, _: u32, _: u32) -> Result<()> {
        not_impl()
    }
}

impl IShellView_Impl for Browser_Impl {
    fn TranslateAccelerator(&self, _: *const MSG) -> HRESULT {
        S_FALSE
    }
    fn EnableModeless(&self, _: BOOL) -> Result<()> {
        not_impl()
    }
    fn UIActivate(&self, _: u32) -> Result<()> {
        not_impl()
    }
    fn Refresh(&self) -> Result<()> {
        not_impl()
    }
    fn CreateViewWindow(&self, _: Ref<IShellView>, _: *const FOLDERSETTINGS, _: Ref<IShellBrowser>, _: *const RECT) -> Result<HWND> {
        not_impl()
    }
    fn DestroyViewWindow(&self) -> Result<()> {
        not_impl()
    }
    fn GetCurrentInfo(&self) -> Result<FOLDERSETTINGS> {
        not_impl()
    }
    fn AddPropertySheetPages(&self, _: u32, _: windows::Win32::UI::Controls::LPFNSVADDPROPSHEETPAGE, _: LPARAM) -> Result<()> {
        not_impl()
    }
    fn SaveViewState(&self) -> Result<()> {
        not_impl()
    }
    fn SelectItem(&self, item: *const ITEMIDLIST, flags: u32) -> Result<()> {
        if item.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let folder = self.folder()?;
        let id = unsafe {
            let full = ILCombine(Some(folder.as_ptr()), Some(item));
            if full.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            let id = ShellId::from_raw(full);
            ILFree(Some(full));
            id
        };
        let p = Box::into_raw(Box::new((id, flags)));
        post(self.hwnd, SELECT, p as *mut c_void, || drop(unsafe { Box::from_raw(p) }))
    }
    fn GetItemObject(&self, _: &_SVGIO, _: *const GUID, _: *mut *mut c_void) -> Result<()> {
        not_impl()
    }
}
