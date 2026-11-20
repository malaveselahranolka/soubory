//! Win32 window: custom title bar over Mica, composition swap chain, input routing, child EDITs
//! for search / rename / go-to-folder, folder watching.
use crate::app::{App, Cap, Drag, Focus, Hit, Loc};
use crate::fsys::wide;

use crate::paint;
use std::{
    cell::RefCell,
    ffi::c_void,
    path::{Path, PathBuf},
    rc::Rc,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Dwm::*, Gdi::*},
        Storage::FileSystem::*,
        System::{DataExchange::COPYDATASTRUCT, LibraryLoader::*, Threading::*},
        UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

pub const WM_APP_FS: u32 = WM_APP + 10;
pub const WM_APP_THUMB: u32 = WM_APP + 11;
pub const WM_APP_SEARCH: u32 = WM_APP + 12;
/// Folder sizes from the index (search.rs).
pub const WM_APP_SIZES: u32 = WM_APP + 18;
/// An action a UI Automation client asked for (uia.rs).
pub const WM_APP_UIA: u32 = WM_APP + 19;
pub const WM_APP_INDEX: u32 = WM_APP + 13;
pub const WM_APP_OPDONE: u32 = WM_APP + 14;
pub const WM_APP_SHELLLIST: u32 = WM_APP + 15;
pub const WM_APP_DRIVES: u32 = WM_APP + 16;
/// The last running file operation finished (see [`OpGuard`]).
const WM_APP_OPS_IDLE: u32 = WM_APP + 17;

/// Work still running off the windows: fm's own copy/move/delete threads ([`OpGuard`]) and the
/// shell's (a drop from Explorer, Properties, Extract All) through the process reference below.
/// Closing the last window does not end the process while any runs, as in Explorer.
static OPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// The hub for threads that cannot read the UI thread's [`hub`].
static HUB_ANY: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

fn ops_add() {
    OPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

fn ops_release() -> u32 {
    let left = OPS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) - 1;
    if left == 0 {
        let hub = HWND(HUB_ANY.load(std::sync::atomic::Ordering::SeqCst) as *mut c_void);
        unsafe {
            let _ = PostMessageW(Some(hub), WM_APP_OPS_IDLE, WPARAM(0), LPARAM(0));
        }
    }
    left as u32
}

/// Held by a worker thread for the length of one file operation; create it on the UI thread
/// before the worker starts.
pub struct OpGuard(());

impl OpGuard {
    pub fn new() -> OpGuard {
        ops_add();
        OpGuard(())
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        ops_release();
    }
}

/// The process reference shell threads take with `SHGetInstanceExplorer` (`CTF_PROCESS_REF`)
/// and release when done. A static object: its count is [`OPS`].
#[repr(C)]
struct ProcRef {
    vtbl: *const windows::core::IUnknown_Vtbl,
}
// SAFETY: immutable; its methods touch only atomics.
unsafe impl Sync for ProcRef {}

static PROC_REF_VTBL: windows::core::IUnknown_Vtbl = windows::core::IUnknown_Vtbl {
    QueryInterface: proc_ref_qi,
    AddRef: proc_ref_add,
    Release: proc_ref_release,
};
static PROC_REF: ProcRef = ProcRef { vtbl: &PROC_REF_VTBL };

unsafe extern "system" fn proc_ref_qi(this: *mut c_void, iid: *const windows::core::GUID, out: *mut *mut c_void) -> windows::core::HRESULT {
    if out.is_null() {
        return windows::Win32::Foundation::E_POINTER;
    }
    unsafe {
        if !iid.is_null() && *iid == <windows::core::IUnknown as windows::core::Interface>::IID {
            ops_add();
            *out = this;
            return windows::Win32::Foundation::S_OK;
        }
        *out = std::ptr::null_mut();
    }
    windows::Win32::Foundation::E_NOINTERFACE
}

unsafe extern "system" fn proc_ref_add(_: *mut c_void) -> u32 {
    OPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u32 + 1
}

unsafe extern "system" fn proc_ref_release(_: *mut c_void) -> u32 {
    ops_release()
}

/// Offers [`PROC_REF`] to shell threads started in this process.
fn register_proc_ref() {
    let raw = &PROC_REF as *const ProcRef as *mut c_void;
    unsafe {
        // Borrowed: SHSetInstanceExplorer keeps the pointer without a reference of its own.
        if let Some(unk) = windows::core::IUnknown::from_raw_borrowed(&raw) {
            windows::Win32::UI::Shell::SHSetInstanceExplorer(unk);
        }
    }
}

/// Hub timer that coalesces a burst of drive notifications (one USB stick sends several).
const TIMER_DRIVES: usize = 0xD1;
const TIMER_FS: usize = 1;
const TIMER_NOTE: usize = 2;
const EDIT_SEARCH: usize = 100;
const EDIT_RENAME: usize = 101;
const EDIT_PATH: usize = 102;
const EDIT_BATCH_A: usize = 103;
const EDIT_BATCH_B: usize = 104;
const EDIT_PALETTE: usize = 105;

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Search,
    Rename,
    Path,
    BatchA,
    BatchB,
    Palette,
}

struct Edit {
    /// The main window the edit belongs to.
    owner: HWND,
    hwnd: HWND,
    kind: EditKind,
    index: usize,
    /// Rename: the item's path (the list can change under the field).
    path: PathBuf,
    font: HFONT,
    brush: HBRUSH,
    bg: COLORREF,
    fg: COLORREF,
}

struct Watch {
    stop: HANDLE,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Shell locations are watched through SHChangeNotifyRegister instead.
    shell: u32,
}

thread_local! {
    static EDITS: RefCell<Vec<Edit>> = const { RefCell::new(Vec::new()) };
    static WATCHES: RefCell<Vec<(isize, Watch)>> = const { RefCell::new(Vec::new()) };
    /// Our main windows, oldest first. Each one's App lives behind GWLP_USERDATA.
    static WINDOWS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    /// Message-only window for process-wide events: thumbnails, the index, a second launch.
    static HUB: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

const MAIN_CLASS: PCWSTR = w!("FmMainWindow");
const HUB_CLASS: PCWSTR = w!("FmHub");
const COPY_OPEN: usize = 0x666D_0001;

/// The App behind one of our main windows; null for any other window.
pub fn app_of(hwnd: HWND) -> *mut App {
    if hwnd.is_invalid() || !WINDOWS.with(|w| w.borrow().contains(&(hwnd.0 as isize))) {
        return std::ptr::null_mut();
    }
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App }
}

pub fn windows() -> Vec<HWND> {
    WINDOWS.with(|w| w.borrow().iter().map(|&h| HWND(h as *mut c_void)).collect())
}

/// Runs `f` on every open window's App (prefs and favorites changes, index events).
pub fn for_each_app(mut f: impl FnMut(&mut App)) {
    for h in windows() {
        if let Some(app) = unsafe { app_of(h).as_mut() } {
            f(app);
        }
    }
}

/// `f` for every window but `except` (whose App the caller already holds).
pub fn for_each_other_app(except: HWND, mut f: impl FnMut(&mut App)) {
    for h in windows().into_iter().filter(|&h| h != except) {
        if let Some(app) = unsafe { app_of(h).as_mut() } {
            f(app);
        }
    }
}

pub fn invalidate_all() {
    for_each_app(|a| a.invalidate());
}

pub fn hub() -> HWND {
    HUB.with(|h| HWND(h.get() as *mut c_void))
}

/// Single instance (`Local\fm.main`): a second launch hands its folder to the running process,
/// which opens a window for it. Returns true when handed over (this process should exit).
pub fn hand_over(path: Option<&Path>) -> bool {
    use std::os::windows::ffi::OsStrExt;
    unsafe {
        // The handle stays open for the life of the process; that is what marks the instance.
        let _ = CreateMutexW(None, false, w!("Local\\fm.main"));
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return false;
        }
        for _ in 0..30 {
            if let Ok(hub) = FindWindowExW(Some(HWND_MESSAGE), None, HUB_CLASS, None) {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hub, Some(&mut pid));
                let _ = AllowSetForegroundWindow(pid);
                let s: Vec<u16> = path.map(|p| p.as_os_str().encode_wide().collect()).unwrap_or_default();
                let cds = COPYDATASTRUCT {
                    dwData: COPY_OPEN,
                    cbData: (s.len() * 2) as u32,
                    lpData: s.as_ptr() as *mut c_void,
                };
                let mut res = 0usize;
                let ok = SendMessageTimeoutW(
                    hub,
                    WM_COPYDATA,
                    WPARAM(0),
                    LPARAM(&cds as *const COPYDATASTRUCT as isize),
                    SMTO_ABORTIFHUNG,
                    5000,
                    Some(&mut res),
                );
                return ok.0 != 0 && res != 0;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }
}

pub fn run(start: Loc) -> i32 {
    unsafe {
        crate::gfx::prewarm();
        let _ = windows::Win32::System::Ole::OleInitialize(None);
        crate::win::trace("ole");
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        allow_dark_menus();
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let icon = folder_icon();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: MAIN_CLASS,
            hIcon: icon,
            hIconSm: icon,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hub_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(hub_proc),
            hInstance: hinst.into(),
            lpszClassName: HUB_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&hub_wc);
        if let Ok(h) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            HUB_CLASS,
            None,
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        ) {
            HUB.with(|c| c.set(h.0 as isize));
            HUB_ANY.store(h.0 as isize, std::sync::atomic::Ordering::SeqCst);
            register_proc_ref();
            watch_drives(h);
        }

        let Some(first) = open_window(start, None) else { return 1 };
        crate::search::start();
        if let Some(app) = app_of(first).as_mut() {
            dev_flags(app);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // Files copied here must stay pasteable after we are gone (Explorer never exits). First,
        // so a paste elsewhere does not wait for the snapshot; retried while another app holds
        // the clipboard open.
        for _ in 0..10 {
            if windows::Win32::System::Ole::OleFlushClipboard().is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        crate::search::stop();
        msg.wParam.0 as i32
    }
}

/// Opens a main window on `loc`. `near`: cascade from that window (Ctrl+N, a second launch);
/// otherwise it goes where the last window was closed.
pub fn open_window(loc: Loc, near: Option<HWND>) -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let mut app = Box::new(App::new(loc));
        crate::win::trace("app_new");
        crate::prefs::load(&mut app);
        // App::new built the sidebar from default prefs; only favorites and saved searches differ.
        if !app.prefs.favorites.is_empty() || !app.prefs.saved.is_empty() {
            app.side_rows = crate::sidebar::build(&app.prefs);
        }
        app.reload(None);
        let app_raw: *mut App = Box::into_raw(app);
        crate::win::trace("prefs_sidebar_reload");

        // Initial size in DIPs, scaled for the primary monitor.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let s = dx as f32 / 96.0;
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW,
            MAIN_CLASS,
            w!("Soubory"),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            (1120.0 * s) as i32,
            (720.0 * s) as i32,
            None,
            None,
            Some(hinst.into()),
            Some(app_raw as *const c_void),
        );
        // shortcut: if creation fails before WM_NCCREATE the App leaks; it never does in practice.
        let Ok(hwnd) = hwnd else { return None };
        crate::win::trace("create_window");
        let app = &mut *app_raw;
        setup_dwm(app);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
        set_title(app);
        watch_current(app);
        // reload ran before the window existed; async replies need its handle.
        if matches!(app.tab().loc, Loc::Search { .. } | Loc::Shell { .. }) {
            app.reload(None);
        } else {
            crate::search::request_sizes(app);
        }
        crate::win::trace("dwm_title_watch");
        crate::droptarget::register(hwnd);
        let mut near_rect = RECT::default();
        if near.is_some_and(|n| GetWindowRect(n, &mut near_rect).is_ok() && !IsIconic(n).as_bool()) {
            // Explorer and Finder offset a new window down and right of the current one.
            let off = (26.0 * app.scale) as i32;
            let _ = SetWindowPos(
                hwnd,
                None,
                near_rect.left + off,
                near_rect.top + off,
                near_rect.right - near_rect.left,
                near_rect.bottom - near_rect.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        } else {
            let restored = crate::prefs::restore_window(app);
            let maximized = restored && app.prefs.window.is_some_and(|w| w.1);
            let _ = ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else if restored { SW_SHOWNORMAL } else { SW_SHOWDEFAULT });
        }
        crate::win::trace("shown");
        Some(hwnd)
    }
}

/// Drives and media come and go (USB sticks, mapped network drives, eject, a disc): every
/// sidebar and "Tento počítač" follows, as in Explorer.
fn watch_drives(hub: HWND) {
    let Some(pc) = crate::shellns::ShellId::known(&FOLDERID_ComputerFolder) else { return };
    let entry = SHChangeNotifyEntry { pidl: pc.as_ptr() as *mut windows::Win32::UI::Shell::Common::ITEMIDLIST, fRecursive: false.into() };
    let events = SHCNE_DRIVEADD.0 | SHCNE_DRIVEREMOVED.0 | SHCNE_MEDIAINSERTED.0 | SHCNE_MEDIAREMOVED.0;
    let reg = unsafe { SHChangeNotifyRegister(hub, SHCNRF_ShellLevel | SHCNRF_InterruptLevel, events as i32, WM_APP_DRIVES, 1, &entry) };
    trace(if reg != 0 { "drive_watch" } else { "drive_watch_failed" });
    // The registration lives as long as the process; keep its ID list alive with it.
    std::mem::forget(pc);
}

fn drives_changed() {
    crate::fsys::forget_volume_names();
    let roots: Vec<PathBuf> = crate::fsys::volumes().into_iter().map(|v| v.root).collect();
    let gone = |loc: &Loc| {
        loc.folder().and_then(|p| p.ancestors().last()).is_some_and(|root| {
            root.to_string_lossy().len() == 3 && !roots.iter().any(|r| r.as_os_str().eq_ignore_ascii_case(root.as_os_str()))
        })
    };
    for_each_app(|a| {
        a.side_rows = crate::sidebar::build(&a.prefs);
        // Tabs on a drive that went away go to "Tento počítač", like Explorer; a rename there is
        // cancelled first so its edit does not outlive the item.
        let (cur, focus) = (a.cur, a.focus);
        for i in 0..a.tabs.len() {
            let on_gone = gone(&a.tabs[i].loc);
            if !on_gone && !(i == cur && matches!(a.tabs[i].loc, Loc::Computer)) {
                continue;
            }
            a.cur = i;
            if on_gone {
                if i == cur {
                    commit_rename(a, true);
                }
                a.go(Loc::Computer);
            } else {
                a.reload(None);
            }
        }
        // Layout, search field, focus, watch and title are per window: back to the visible tab.
        a.cur = cur;
        a.focus = focus;
        sync_search_field(a);
        a.relayout();
        watch_current(a);
        set_title(a);
        a.invalidate();
    });
}

unsafe extern "system" fn hub_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        match msg {
            WM_APP_THUMB => {
                crate::thumbs::arrived(w, l);
                invalidate_all();
                Some(LRESULT(0))
            }
            WM_APP_INDEX => {
                crate::search::index_event(w, l);
                Some(LRESULT(0))
            }
            WM_APP_OPS_IDLE => {
                if windows().is_empty() && OPS.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                    PostQuitMessage(0);
                }
                Some(LRESULT(0))
            }
            WM_APP_DRIVES => {
                SetTimer(Some(hwnd), TIMER_DRIVES, 300, None);
                Some(LRESULT(0))
            }
            WM_TIMER if w.0 == TIMER_DRIVES => {
                let _ = KillTimer(Some(hwnd), TIMER_DRIVES);
                drives_changed();
                Some(LRESULT(0))
            }
            WM_TIMER => {
                crate::search::tick(w.0);
                Some(LRESULT(0))
            }
            WM_APP_OPDONE => {
                crate::undo::done(Box::from_raw(l.0 as *mut crate::undo::Done));
                Some(LRESULT(0))
            }
            WM_COPYDATA => {
                let cds = &*(l.0 as *const COPYDATASTRUCT);
                if cds.dwData != COPY_OPEN {
                    return Some(LRESULT(0));
                }
                let n = cds.cbData as usize / 2;
                let text = if n == 0 || cds.lpData.is_null() {
                    String::new()
                } else {
                    String::from_utf16_lossy(std::slice::from_raw_parts(cds.lpData as *const u16, n))
                };
                let loc = crate::start_loc(&text);
                let near = GetForegroundWindow();
                let near = if app_of(near).is_null() { windows().last().copied() } else { Some(near) };
                if let Some(h) = open_window(loc, near) {
                    let _ = SetForegroundWindow(h);
                }
                Some(LRESULT(1))
            }
            _ => None,
        }
    }));
    match r {
        Ok(Some(r)) => r,
        Ok(None) => unsafe { DefWindowProcW(hwnd, msg, w, l) },
        Err(e) => {
            crate::prefs::log(&format!("panic in hub (msg {msg:#x}): {}", panic_text(&e)));
            LRESULT(0)
        }
    }
}

/// Test hooks for scripts/dev (no synthesized input): --dev-select N, --dev-rename,
/// --dev-search TEXT, --dev-goto, --dev-view icons|list|columns|gallery.
fn dev_flags(app: &mut App) {
    let args: Vec<String> = std::env::args().collect();
    let val = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    if let Some(v) = val("--dev-view") {
        let view = match v.as_str() {
            "icons" => crate::app::View::Icons,
            "columns" => crate::app::View::Columns,
            "gallery" => crate::app::View::Gallery,
            "tiles" => crate::app::View::Tiles,
            _ => crate::app::View::List,
        };
        app.set_view(view);
    }
    if let Some(k) = val("--dev-sort") {
        use crate::fsys::SortKey;
        let key = match k.as_str() {
            "modified" => SortKey::Modified,
            "created" => SortKey::Created,
            "size" => SortKey::Size,
            "kind" => SortKey::Kind,
            _ => SortKey::Name,
        };
        app.set_sort(key);
    }
    if let Some(g) = val("--dev-group") {
        app.set_group(crate::groups::GroupKey::from_pref(&g));
    }
    if let Some(y) = val("--dev-scroll").and_then(|v| v.parse::<f32>().ok()) {
        app.tab_mut().scroll = y;
        app.clamp_scroll();
    }
    if let Some(n) = val("--dev-select").and_then(|v| v.parse::<usize>().ok()) {
        if n < app.tab().entries.len() {
            app.tab_mut().sel.select_only(n);
        }
    }
    if args.iter().any(|a| a == "--dev-rename") {
        begin_rename(app);
    }
    if let Some(t) = val("--dev-search") {
        focus_search(app);
        if let Some((h, _)) = edit_of(app.hwnd, EditKind::Search) {
            let w = wide(&t);
            unsafe {
                let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
            }
        }
    }
    if let Some(q) = val("--dev-palette") {
        crate::palette::open(app);
        if let Some((h, _)) = edit_of(app.hwnd, EditKind::Palette) {
            let w = wide(&q);
            unsafe {
                let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
            }
        }
    }
    if args.iter().any(|a| a == "--dev-goto") {
        go_to_folder(app);
    }
    if args.iter().any(|a| a == "--dev-ql") {
        crate::quicklook::toggle(app);
    }
    crate::droptarget::dev(app, &args);
    // Clicks the sidebar row with this label through the real hit test and click handlers.
    if let Some(label) = val("--dev-side") {
        let row = app.side_rows.iter().position(|r| matches!(r, crate::sidebar::Row::Item { label: l, .. } if *l == label));
        if let Some(r) = row.map(|i| crate::paint::side_row_rect(app, i)) {
            app.mouse_down(r.cx(), r.cy(), false, false);
            app.mouse_up(r.cx(), r.cy(), false, false);
        }
    }
    if let Some(script) = val("--dev-keys") {
        dev_keys(app, &script, val("--dev-out"));
    }
    app.invalidate();
}

/// `--dev-keys "sel:0,ctrl+d,wait:1500,dump,ctrl+z,…"`: feeds keys to the app's own key handler
/// (never synthesized input), pumping messages between steps; `dump` appends the folder listing
/// and the status note to `--dev-out`.
fn dev_keys(app: &mut App, script: &str, out: Option<String>) {
    let mut log = String::new();
    let pump = |ms: u64| unsafe {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(ms) {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    for step in script.split(',').map(str::trim) {
        let (mods, key) = match step.rsplit_once('+') {
            Some((m, k)) => (m, k),
            None => ("", step),
        };
        let (ctrl, shift) = (mods.contains("ctrl"), mods.contains("shift"));
        if let Some(n) = step.strip_prefix("sel:").and_then(|n| n.parse::<usize>().ok()) {
            if n < app.tab().entries.len() {
                app.tab_mut().sel.select_only(n);
            }
        } else if let Some(ms) = step.strip_prefix("wait:").and_then(|n| n.parse::<u64>().ok()) {
            pump(ms);
        } else if step == "go:bin" || step == "go:net" {
            let loc = if step == "go:bin" { crate::shellns::recycle_bin() } else { crate::shellns::network() };
            if let Some(l) = loc {
                app.go(l);
            }
        } else if let Some(spec) = step.strip_prefix("batch:") {
            // batch:Mode:a:b — opens the sheet on the selection and fills it.
            let mut it = spec.splitn(3, ':');
            let mode = match it.next() {
                Some("Add") => crate::batch::Mode::Add,
                Some("Format") => crate::batch::Mode::Format,
                _ => crate::batch::Mode::Replace,
            };
            if app.batch.is_none() {
                crate::batch::open(app);
            }
            crate::batch::set_mode(app, mode);
            if let Some(b) = app.batch.as_mut() {
                b.a = it.next().unwrap_or("").to_string();
                b.b = it.next().unwrap_or("").to_string();
            }
            batch_fields(app);
            app.invalidate();
        } else if step == "batchok" {
            crate::batch::apply(app);
        } else if step == "selall" {
            app.tab_mut().sel.select_all();
        } else if step == "dumpshell" {
            // Counts only, plus items that came from the scratch folder: never the user's own.
            let t = app.tab();
            log += &format!("loc={} count={} loading={} error={:?}\n", t.loc.title(), t.entries.len(), t.loading, t.error);
            for e in t.entries.iter().filter(|e| e.origin.as_deref().unwrap_or("").contains("scratchpad")) {
                log += &format!("  {} | from {} | deleted>0={} | shell={}\n", e.name, e.origin.clone().unwrap_or_default(), e.modified > 0, e.shell.is_some());
            }
        } else if step == "groups" {
            // Labels and sizes, then a hit-test round trip over the real window geometry.
            let gs: Vec<String> = app.shown_groups().iter().map(|g| format!("{}:{}+{}", g.label, g.start, g.len)).collect();
            log += &format!("groups [{}] scroll={}
", gs.join(" | "), app.tab().scroll);
            let b = app.lay.body;
            let pinned = app.pinned_header().map(|(_, r)| r);
            let (mut ok, mut bad) = (0, Vec::new());
            for i in app.visible_items() {
                let r = app.item_rect(i);
                let (x, y) = if app.tab().view == crate::app::View::Icons {
                    let (icon, _) = crate::paint::tile_parts(app, i);
                    (icon.cx(), icon.cy())
                } else if app.tab().view == crate::app::View::Tiles {
                    let (_, name) = crate::paint::tiles_parts(app, i);
                    (name.cx(), name.cy())
                } else {
                    (r.x + 40.0, r.cy())
                };
                if y < b.y || y > b.b() || pinned.is_some_and(|p| p.contains(x, y)) {
                    continue;
                }
                if app.hit(x, y) == crate::app::Hit::Row(i) { ok += 1 } else { bad.push(i) }
            }
            for g in 0..app.shown_groups().len() {
                let r = app.group_header_rect(g);
                let (x, y) = (r.x + 30.0, r.cy());
                if y > b.y && y < b.b() && !pinned.is_some_and(|p| p.contains(x, y)) && app.hit(x, y) != crate::app::Hit::Group(g) {
                    bad.push(10_000 + g);
                }
            }
            log += &format!("hit ok={ok} bad={bad:?} pinned={:?}
", app.pinned_header().map(|(g, r)| (g, r.y)));
        } else if step == "sizes" {
            let t = app.tab();
            let v: Vec<String> = t.entries.iter().filter(|e| e.is_dir).map(|e| format!("{}={}", e.name, e.size)).collect();
            log += &format!("sizes [{}] order [{}]\n", v.join(" "), t.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(" "));
        } else if step == "procref" {
            // What a shell thread with CTF_PROCESS_REF sees.
            let before = OPS.load(std::sync::atomic::Ordering::SeqCst);
            let held = unsafe { windows::Win32::UI::Shell::SHGetInstanceExplorer() };
            let during = OPS.load(std::sync::atomic::Ordering::SeqCst);
            let ok = held.is_ok();
            drop(held);
            log += &format!("procref ok={ok} ops {before} -> {during} -> {}\n", OPS.load(std::sync::atomic::Ordering::SeqCst));
        } else if step == "close" {
            // Last step: the window goes once the message loop runs.
            unsafe {
                let _ = PostMessageW(Some(app.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            log += &format!("close with ops={}\n", OPS.load(std::sync::atomic::Ordering::SeqCst));
            break;
        } else if step == "dump" {
            app.reload(None);
            let names: Vec<String> = app.tab().entries.iter().map(|e| e.name.clone()).collect();
            log += &format!("{} | note: {}\n", names.join(" ; "), app.status_note.as_ref().map(|n| n.0.clone()).unwrap_or_default());
        } else {
            let vk = match key {
                "del" => VK_DELETE,
                k if k.len() == 1 => VIRTUAL_KEY(k.to_ascii_uppercase().as_bytes()[0] as u16),
                _ => continue,
            };
            app.key(vk, ctrl, shift, false);
            pump(50);
        }
    }
    if let Some(o) = out {
        let _ = std::fs::write(o, log);
    }
}

/// `--dev-startup FILE`: phase timings (ms since process creation) go to FILE.trace.
pub fn trace(label: &str) {
    static OUT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let out = OUT.get_or_init(|| {
        let args: Vec<String> = std::env::args().collect();
        args.iter().position(|a| a == "--dev-startup").and_then(|i| args.get(i + 1)).cloned()
    });
    let Some(out) = out else { return };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(format!("{out}.trace")) {
        let _ = writeln!(f, "{label}: {}", since_start_ms());
    }
}

fn since_start_ms() -> u64 {
    unsafe {
        let (mut c, mut e, mut k, mut u) = Default::default();
        if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).is_err() {
            return 0;
        }
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let now = (unix.as_nanos() / 100) as u64 + 116_444_736_000_000_000;
        let created: FILETIME = c;
        let t = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        now.saturating_sub(t) / 10_000
    }
}

/// `--dev-startup FILE`: after the first presented frame, writes the ms since the process was
/// created (the cold-start budget is 300 ms).
fn startup_probe() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::Relaxed) {
        return;
    }
    let args: Vec<String> = std::env::args().collect();
    let Some(out) = args.iter().position(|a| a == "--dev-startup").and_then(|i| args.get(i + 1)) else { return };
    unsafe {
        let (mut c, mut e, mut k, mut u) = Default::default();
        if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).is_ok() {
            let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            let now = (unix.as_nanos() / 100) as u64 + 116_444_736_000_000_000; // FILETIME ticks
            let created: FILETIME = c;
            let t = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
            let _ = std::fs::write(out, format!("{}", now.saturating_sub(t) / 10_000));
        }
    }
}

/// Dev/test runs (`--dev-*`) must not overwrite the user's saved window and view.
fn dev_run() -> bool {
    std::env::args().any(|a| a.starts_with("--dev-"))
}

fn folder_icon() -> HICON {
    unsafe {
        let mut sii = SHSTOCKICONINFO {
            cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
            ..Default::default()
        };
        if SHGetStockIconInfo(SIID_FOLDER, SHGSI_ICON | SHGSI_LARGEICON, &mut sii).is_ok() {
            sii.hIcon
        } else {
            HICON::default()
        }
    }
}

/// Undocumented uxtheme ordinals 135/136 (SetPreferredAppMode, FlushMenuThemes): dark native
/// menus in dark mode. Absent on old builds, then menus stay light.
fn allow_dark_menus() {
    unsafe {
        let Ok(ux) = LoadLibraryExW(w!("uxtheme.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) else {
            return;
        };
        if let Some(f) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
            let set: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set(1); // AllowDark
        }
        if let Some(f) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
            let flush: extern "system" fn() = std::mem::transmute(f);
            flush();
        }
    }
}

fn setup_dwm(app: &mut App) {
    unsafe {
        let dark = BOOL::from(app.theme.is_dark());
        let _ = DwmSetWindowAttribute(
            app.hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const BOOL).cast(),
            4,
        );
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            app.hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&corner as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            4,
        );
        let hc = app.theme.mode == crate::theme::Mode::HighContrast;
        let backdrop = if hc { DWMSBT_NONE } else { DWMSBT_MAINWINDOW };
        app.mica = !hc
            && DwmSetWindowAttribute(
                app.hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                (&backdrop as *const DWM_SYSTEMBACKDROP_TYPE).cast(),
                4,
            )
            .is_ok();
        let m = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(app.hwnd, &m);
    }
}

pub fn set_title(app: &App) {
    if app.offscreen || app.hwnd.is_invalid() {
        return;
    }
    let t = wide(&app.tab().loc.title());
    unsafe {
        let _ = SetWindowTextW(app.hwnd, PCWSTR(t.as_ptr()));
    }
}

pub fn close(app: &App) {
    unsafe {
        let _ = PostMessageW(Some(app.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

pub fn caption(app: &mut App, c: Cap) {
    unsafe {
        match c {
            Cap::Min => {
                let _ = ShowWindow(app.hwnd, SW_MINIMIZE);
            }
            Cap::Max => {
                let _ = ShowWindow(
                    app.hwnd,
                    if IsZoomed(app.hwnd).as_bool() {
                        SW_RESTORE
                    } else {
                        SW_MAXIMIZE
                    },
                );
            }
            Cap::Close => close(app),
        }
    }
}

pub fn wheel_lines() -> u32 {
    let mut n = 3u32;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&mut n as *mut u32).cast()),
            Default::default(),
        );
    }
    n.clamp(1, 20)
}

pub fn to_screen(app: &App, x: f32, y: f32) -> POINT {
    let mut p = POINT {
        x: (x * app.scale) as i32,
        y: (y * app.scale) as i32,
    };
    unsafe {
        let _ = ClientToScreen(app.hwnd, &mut p);
    }
    p
}

/// Ctrl+N: a new window on the same folder (Explorer), in this process.
pub fn new_window(app: &App) {
    let loc = match &app.tab().loc {
        Loc::Search { .. } => crate::start_loc(""),
        other => other.clone(),
    };
    if let Some(h) = open_window(loc, Some(app.hwnd)) {
        unsafe {
            let _ = SetForegroundWindow(h);
        }
    }
}

// ---- painting ------------------------------------------------------------------------------------------

fn paint_now(app: &mut App) {
    let Some(mut g) = app.gfx.take() else { return };
    if !g.has_device() {
        let (w, h) = (
            (app.w * app.scale).round() as u32,
            (app.h * app.scale).round() as u32,
        );
        crate::win::trace("paint_begin");
        let attached = g.attach(app.hwnd, w, h);
        crate::win::trace("gfx_attach");
        if attached.is_err() {
            app.gfx = Some(g);
            return;
        }
    }
    let g = Rc::new(g);
    app.gfx_ref = Some(g.clone());
    let ok = g.frame(app.scale, |c| paint::paint(app, c));
    crate::win::trace("frame");
    app.gfx_ref = None;
    let mut g = Rc::try_unwrap(g)
        .ok()
        .expect("gfx is shared only during a frame");
    if !ok {
        if let Some(dc) = g.dc() {
            app.icons.forget_device(dc.as_raw() as usize);
        }
        crate::thumbs::forget_device(app);
        g.drop_device();
        unsafe {
            let _ = InvalidateRect(Some(app.hwnd), None, false);
        }
    }
    app.gfx = Some(g);
    if ok {
        startup_probe();
    }
    position_edits(app);
    crate::quicklook::follow(app);
}

// ---- folder watching -------------------------------------------------------------------------------------

fn stop_watch(hwnd: HWND) {
    let gone = WATCHES.with(|w| {
        let mut v = w.borrow_mut();
        v.iter().position(|(h, _)| *h == hwnd.0 as isize).map(|i| v.remove(i).1)
    });
    {
        if let Some(mut old) = gone {
            crate::shellns::unwatch(old.shell);
            unsafe {
                let _ = SetEvent(old.stop);
            }
            if let Some(t) = old.thread.take() {
                let _ = t.join();
            }
            unsafe {
                let _ = CloseHandle(old.stop);
            }
        }
    }
}

/// Watches the current folder (not its subtree) and posts WM_APP_FS on any change.
pub fn watch_current(app: &mut App) {
    if app.offscreen || app.hwnd.is_invalid() {
        return;
    }
    stop_watch(app.hwnd);
    if let Loc::Shell { id, .. } = &app.tab().loc {
        let reg = crate::shellns::watch(app.hwnd, id);
        if let Ok(stop) = unsafe { CreateEventW(None, true, false, None) } {
            WATCHES.with(|w| w.borrow_mut().push((app.hwnd.0 as isize, Watch { stop, thread: None, shell: reg })));
        }
        return;
    }
    let Some(dir) = app.tab().loc.folder().map(Path::to_path_buf) else {
        return;
    };
    let hwnd = app.hwnd.0 as usize;
    unsafe {
        let Ok(stop) = CreateEventW(None, true, false, None) else {
            return;
        };
        let stop_raw = stop.0 as usize;
        let thread = std::thread::spawn(move || {
            let w = crate::fsys::wide_path(&dir);
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_ATTRIBUTES;
            let Ok(ch) = FindFirstChangeNotificationW(PCWSTR(w.as_ptr()), false, filter) else {
                return;
            };
            let stop = HANDLE(stop_raw as *mut c_void);
            loop {
                let r = WaitForMultipleObjects(&[stop, ch], false, INFINITE);
                if r != WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
                    break;
                }
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_APP_FS,
                    WPARAM(0),
                    LPARAM(0),
                );
                if FindNextChangeNotification(ch).is_err() {
                    break;
                }
            }
            let _ = FindCloseChangeNotification(ch);
        });
        WATCHES.with(|w| {
            w.borrow_mut().push((
                hwnd as isize,
                Watch {
                    stop,
                    thread: Some(thread),
                    shell: 0,
                },
            ))
        });
    }
}

// ---- child edits -----------------------------------------------------------------------------------------

fn colorref(c: crate::theme::Color) -> COLORREF {
    COLORREF(
        (c.r * 255.0).round() as u32
            | (((c.g * 255.0).round() as u32) << 8)
            | (((c.b * 255.0).round() as u32) << 16),
    )
}

fn ui_font(app: &App, px: f32) -> HFONT {
    unsafe {
        CreateFontW(
            -((px * app.scale).round() as i32),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            w!("Segoe UI Variable Text"),
        )
    }
}

fn edit_rect(app: &App, kind: EditKind, index: usize) -> crate::geom::Rect {
    use crate::geom::Rect;
    match kind {
        EditKind::BatchA => crate::batch::edit_rect(app, 0),
        EditKind::BatchB => crate::batch::edit_rect(app, 1),
        EditKind::Palette => crate::palette::edit_rect(app),
        EditKind::Search => app
            .lay
            .search
            .cut_left(30.0)
            .cut_right(8.0)
            .center(app.lay.search.w - 38.0, 18.0),
        EditKind::Path => {
            let t = app.lay.title;
            Rect::new(t.x + 8.0, t.cy() - 9.0, t.w - 16.0, 18.0)
        }
        EditKind::Rename => match app.tab().view {
            crate::app::View::Icons => {
                let (_, name) = paint::tile_parts(app, index);
                Rect::new(name.x, name.y, name.w, 18.0)
            }
            crate::app::View::Tiles => {
                let (_, name) = paint::tiles_parts(app, index);
                Rect::new(name.x - 3.0, name.y, name.w + 3.0, 18.0)
            }
            _ => {
                let r = app.row_rect(index);
                let name_x = app.lay.body.x + crate::layout::INSET + 28.0;
                let w = paint::name_col_width(app) - 36.0;
                Rect::new(name_x - 3.0, r.cy() - 9.0, w.max(80.0), 18.0)
            }
        },
    }
}

fn create_edit(
    app: &mut App,
    kind: EditKind,
    index: usize,
    text: &str,
    sel: (i32, i32),
) -> Option<HWND> {
    destroy_edit(app.hwnd, kind);
    let t = &app.theme;
    let (bg, fg) = match kind {
        EditKind::Search | EditKind::Path => (paint::over(t.field, t.chrome), t.ink),
        EditKind::Rename => (t.card, t.ink),
        EditKind::BatchA | EditKind::BatchB => (paint::over(t.field, t.card), t.ink),
        EditKind::Palette => (t.card, t.ink),
    };
    let r = edit_rect(app, kind, index);
    let id = match kind {
        EditKind::Search => EDIT_SEARCH,
        EditKind::Rename => EDIT_RENAME,
        EditKind::Path => EDIT_PATH,
        EditKind::BatchA => EDIT_BATCH_A,
        EditKind::BatchB => EDIT_BATCH_B,
        EditKind::Palette => EDIT_PALETTE,
    };
    let w = wide(text);
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED,
            w!("EDIT"),
            PCWSTR(w.as_ptr()),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            (r.x * app.scale).round() as i32,
            (r.y * app.scale).round() as i32,
            (r.w * app.scale).round() as i32,
            (r.h * app.scale).round() as i32,
            Some(app.hwnd),
            Some(HMENU(id as *mut c_void)),
            None,
            None,
        )
        .ok()?;
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        let font = ui_font(app, 13.0);
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        SendMessageW(
            hwnd,
            EM_SETMARGINS,
            Some(WPARAM((EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize)),
            Some(LPARAM(0)),
        );
        let _ = SetWindowSubclass(hwnd, Some(edit_proc), id, 0);
        if kind == EditKind::Path {
            let _ = SHAutoComplete(hwnd, SHACF_FILESYS_DIRS | SHACF_AUTOSUGGEST_FORCE_ON);
        }
        let bgc = colorref(bg);
        let brush = CreateSolidBrush(bgc);
        EDITS.with(|e| {
            e.borrow_mut().push(Edit {
                owner: app.hwnd,
                hwnd,
                kind,
                index,
                path: if kind == EditKind::Rename { app.tab().path_of(index) } else { PathBuf::new() },
                font,
                brush,
                bg: bgc,
                fg: colorref(fg),
            })
        });
        SendMessageW(
            hwnd,
            EM_SETSEL,
            Some(WPARAM(sel.0 as usize)),
            Some(LPARAM(sel.1 as isize)),
        );
        let _ = SetFocus(Some(hwnd));
        Some(hwnd)
    }
}

fn destroy_edit(owner: HWND, kind: EditKind) {
    let gone = EDITS.with(|e| {
        let mut v = e.borrow_mut();
        v.iter().position(|x| x.owner == owner && x.kind == kind).map(|i| v.remove(i))
    });
    if let Some(ed) = gone {
        unsafe {
            let _ = RemoveWindowSubclass(ed.hwnd, Some(edit_proc), 0);
            let _ = DestroyWindow(ed.hwnd);
            let _ = DeleteObject(ed.font.into());
            let _ = DeleteObject(ed.brush.into());
        }
    }
}

fn edit_of(owner: HWND, kind: EditKind) -> Option<(HWND, usize)> {
    EDITS.with(|e| {
        e.borrow()
            .iter()
            .find(|x| x.owner == owner && x.kind == kind)
            .map(|x| (x.hwnd, x.index))
    })
}

fn edit_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, &mut buf);
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

fn position_edits(app: &App) {
    let list: Vec<(HWND, EditKind, usize)> = EDITS.with(|e| {
        e.borrow()
            .iter()
            .filter(|x| x.owner == app.hwnd)
            .map(|x| (x.hwnd, x.kind, x.index))
            .collect()
    });
    for (h, kind, index) in list {
        let r = edit_rect(app, kind, index);
        unsafe {
            let _ = SetWindowPos(
                h,
                None,
                (r.x * app.scale).round() as i32,
                (r.y * app.scale).round() as i32,
                (r.w * app.scale).round() as i32,
                (r.h * app.scale).round() as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// (Re)creates the batch rename sheet's fields for its current mode, or removes them.
/// After a DPI or theme change: the fields are recreated on demand with the new font and
/// colours; the palette's comes back at once with its query.
fn rebuild_edits(app: &mut App) {
    // Commits like a resize would; destroying the field would drop the typed name.
    end_rename(app);
    let query = edit_of(app.hwnd, EditKind::Palette).map(|(h, _)| edit_text(h));
    for k in [EditKind::Search, EditKind::Rename, EditKind::Path, EditKind::Palette] {
        destroy_edit(app.hwnd, k);
    }
    if app.palette.is_some() {
        palette_field(app, true);
        if let (Some(q), Some((h, _))) = (query, edit_of(app.hwnd, EditKind::Palette)) {
            let w = wide(&q);
            let n = q.encode_utf16().count();
            unsafe {
                // EN_CHANGE filters the list again.
                let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
                SendMessageW(h, EM_SETSEL, Some(WPARAM(n)), Some(LPARAM(n as isize)));
            }
        }
    }
}

/// Shows (and focuses) or removes the palette's text field.
pub fn palette_field(app: &mut App, on: bool) {
    if !on {
        // Only when the field still has the focus: not when the user clicked elsewhere.
        let had_focus = edit_of(app.hwnd, EditKind::Palette).is_some_and(|(h, _)| unsafe { GetFocus() } == h);
        destroy_edit(app.hwnd, EditKind::Palette);
        if had_focus {
            unsafe {
                let _ = SetFocus(Some(app.hwnd));
            }
        }
        return;
    }
    let h = match edit_of(app.hwnd, EditKind::Palette) {
        Some((h, _)) => Some(h),
        None => create_edit(app, EditKind::Palette, 0, "", (0, 0)),
    };
    if let Some(h) = h {
        unsafe {
            let _ = SetFocus(Some(h));
        }
    }
}

pub fn batch_fields(app: &mut App) {
    destroy_edit(app.hwnd, EditKind::BatchA);
    destroy_edit(app.hwnd, EditKind::BatchB);
    let Some(b) = &app.batch else {
        unsafe {
            let _ = SetFocus(Some(app.hwnd));
        }
        return;
    };
    let (a, bt, mode) = (b.a.clone(), b.b.clone(), b.mode);
    if crate::batch::has_b(mode) {
        let n = bt.encode_utf16().count() as i32;
        create_edit(app, EditKind::BatchB, 0, &bt, (n, n));
    }
    let n = a.encode_utf16().count() as i32;
    create_edit(app, EditKind::BatchA, 0, &a, (0, n));
}

pub fn search_active(app: &App) -> bool {
    edit_of(app.hwnd, EditKind::Search).is_some()
}

pub fn renaming_index(app: &App) -> Option<usize> {
    rename_edit(app).and_then(|e| e.1)
}

/// A rename field is open in this window.
pub fn renaming(app: &App) -> bool {
    edit_of(app.hwnd, EditKind::Rename).is_some()
}

/// The open rename field, the row of the item it was opened on and that item's path. The row
/// follows the item when the list changed under the field; None when the item is gone.
fn rename_edit(app: &App) -> Option<(HWND, Option<usize>, PathBuf)> {
    let (h, i, p) = EDITS.with(|e| {
        e.borrow()
            .iter()
            .find(|x| x.owner == app.hwnd && x.kind == EditKind::Rename)
            .map(|x| (x.hwnd, x.index, x.path.clone()))
    })?;
    let t = app.tab();
    let dir = t.dir();
    let at = |k: usize| t.entries.get(k).is_some_and(|e| e.path_in(&dir) == p);
    let row = if at(i) { Some(i) } else { (0..t.entries.len()).find(|&k| at(k)) };
    if let Some(r) = row.filter(|&r| r != i) {
        EDITS.with(|e| {
            if let Some(x) = e.borrow_mut().iter_mut().find(|x| x.hwnd == h) {
                x.index = r;
            }
        });
    }
    Some((h, row, p))
}

/// Where the rename field sits, so paint can draw its frame around the EDIT.
pub fn rename_rect(app: &App) -> Option<crate::geom::Rect> {
    renaming_index(app).map(|i| edit_rect(app, EditKind::Rename, i))
}

/// Quick Look forwards navigation keys to its owner window's selection.
pub fn forward_key(ql: HWND, vk: VIRTUAL_KEY) {
    let owner = unsafe { GetWindow(ql, GW_OWNER) }.unwrap_or_default();
    let Some(app) = (unsafe { app_of(owner).as_mut() }) else { return };
    app.key(vk, false, false, false);
    app.invalidate();
}

pub fn path_editing(app: &App) -> bool {
    edit_of(app.hwnd, EditKind::Path).is_some()
}

pub fn focus_search(app: &mut App) {
    if let Some((h, _)) = edit_of(app.hwnd, EditKind::Search) {
        unsafe {
            let _ = SetFocus(Some(h));
            SendMessageW(h, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
        }
    } else {
        let text = app.search_text.clone();
        let n = text.encode_utf16().count() as i32;
        create_edit(app, EditKind::Search, 0, &text, (0, n));
    }
    app.focus = Focus::Search;
    app.invalidate();
}

pub fn clear_search(app: &mut App) {
    destroy_edit(app.hwnd, EditKind::Search);
    app.search_text.clear();
    app.focus = Focus::Content;
    unsafe {
        let _ = SetFocus(Some(app.hwnd));
    }
    if matches!(app.tab().loc, Loc::Search { .. }) {
        let prev = app.search_prev.take();
        let t = app.tab_mut();
        t.loc = prev.unwrap_or(Loc::Computer);
        t.scroll = 0.0;
        app.reload(None);
    }
    app.invalidate();
}

fn search_changed(app: &mut App, text: String) {
    app.search_text = text.clone();
    if text.trim().is_empty() {
        if matches!(app.tab().loc, Loc::Search { .. }) {
            let prev = app.search_prev.take();
            let t = app.tab_mut();
            t.loc = prev.unwrap_or(Loc::Computer);
            app.reload(None);
        }
        app.invalidate();
        return;
    }
    let scope = match &app.tab().loc {
        Loc::Search { scope, .. } => scope.clone(),
        other => {
            let prev = Some(other.clone());
            let scope = crate::search::default_scope(other);
            app.search_prev = prev;
            scope
        }
    };
    let t = app.tab_mut();
    if !matches!(t.loc, Loc::Search { .. }) {
        t.by_rank = true;
    }
    t.loc = Loc::Search { text, scope };
    t.scroll = 0.0;
    crate::search::run(app);
    set_title(app);
    app.relayout();
    app.invalidate();
}

/// The search field shows the current tab's query: after Back/Forward, a tab switch or a saved
/// search it would otherwise keep a stale one, and the next keystroke would search for that.
pub fn sync_search_field(app: &mut App) {
    let want = match &app.tab().loc {
        Loc::Search { text, .. } => text.clone(),
        _ => String::new(),
    };
    if app.search_text == want {
        return;
    }
    app.search_text = want.clone();
    if let Some((h, _)) = edit_of(app.hwnd, EditKind::Search) {
        if want.is_empty() && unsafe { GetFocus() } != h {
            destroy_edit(app.hwnd, EditKind::Search);
        } else {
            // EN_CHANGE sees the text equal to search_text and does not search again.
            let w = wide(&want);
            let n = want.encode_utf16().count();
            unsafe {
                let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
                SendMessageW(h, EM_SETSEL, Some(WPARAM(n)), Some(LPARAM(n as isize)));
            }
        }
    }
}

pub fn go_to_folder(app: &mut App) {
    let text = app
        .tab()
        .loc
        .folder()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let n = text.encode_utf16().count() as i32;
    create_edit(app, EditKind::Path, 0, &text, (0, n));
    app.invalidate();
}

fn path_entered(app: &mut App, text: &str) {
    destroy_edit(app.hwnd, EditKind::Path);
    unsafe {
        let _ = SetFocus(Some(app.hwnd));
    }
    let raw = text.trim().trim_matches('"');
    if raw.is_empty() {
        return;
    }
    let expanded = expand_env(raw);
    let mut p = PathBuf::from(&expanded);
    if expanded.starts_with('~') {
        if let Some(home) = crate::fsys::known_folder(&FOLDERID_Profile) {
            p = home.join(
                expanded
                    .trim_start_matches('~')
                    .trim_start_matches(['\\', '/']),
            );
        }
    } else if p.is_relative() {
        if let Some(cur) = app.tab().loc.folder() {
            p = cur.join(&p);
        }
    }
    if p.is_dir() {
        app.go(Loc::Folder(p));
    } else if p.is_file() {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
        if let Some(parent) = p.parent() {
            app.go_select(Loc::Folder(parent.to_path_buf()), name);
        }
    } else {
        app.note(&format!("Složka „{raw}“ neexistuje."));
    }
}

fn expand_env(s: &str) -> String {
    let w = wide(s);
    let mut buf = vec![0u16; 4096];
    let n = unsafe {
        windows::Win32::System::Environment::ExpandEnvironmentStringsW(
            PCWSTR(w.as_ptr()),
            Some(&mut buf),
        )
    };
    if n == 0 || n as usize > buf.len() {
        return s.to_string();
    }
    String::from_utf16_lossy(&buf[..(n as usize).saturating_sub(1)])
}

pub fn begin_rename(app: &mut App) {
    if app.tab().sel.count() > 1 {
        crate::batch::open(app);
        return;
    }
    let t = app.tab();
    if !matches!(t.loc, Loc::Folder(_) | Loc::Search { .. } | Loc::Recents) {
        return;
    }
    let Some(i) = t
        .sel
        .focus
        .filter(|&f| t.sel.is(f))
        .or_else(|| t.sel.first())
    else {
        return;
    };
    let e = &t.entries[i];
    let name = e.name.clone();
    let stem = if e.is_dir {
        name.encode_utf16().count()
    } else {
        name.rfind('.')
            .filter(|&k| k > 0)
            .map(|k| name[..k].encode_utf16().count())
            .unwrap_or(name.encode_utf16().count())
    };
    app.tab_mut().sel.select_only(i);
    app.reveal_focus();
    create_edit(app, EditKind::Rename, i, &name, (0, stem as i32));
    app.invalidate();
}

/// Commits an open rename (before anything reorders the list under it).
pub fn end_rename(app: &mut App) {
    if edit_of(app.hwnd, EditKind::Rename).is_some() {
        commit_rename(app, false);
    }
}

fn commit_rename(app: &mut App, cancel: bool) {
    let Some((h, row, path)) = rename_edit(app) else {
        return;
    };
    let text = edit_text(h);
    // Only take focus back when the edit still has it (not when the user clicked into search).
    let had_focus = unsafe { GetFocus() } == h;
    destroy_edit(app.hwnd, EditKind::Rename);
    if had_focus {
        unsafe {
            let _ = SetFocus(Some(app.hwnd));
        }
    }
    app.invalidate();
    if !cancel {
        match row {
            Some(i) => apply_rename(app, i, text),
            None if path.file_name().is_some_and(|n| *n != *text.trim_end()) => {
                app.note("Položka mezitím zmizela, nic se nepřejmenovalo.")
            }
            None => {}
        }
    }
    // What waited for the field to close: search results, a re-sort by folder size.
    app.flush_deferred();
}

fn apply_rename(app: &mut App, i: usize, text: String) {
    let old = app.tab().path_of(i);
    let old_name = app.tab().entries[i].name.clone();
    let new_name = text.trim_end().to_string();
    if new_name == old_name || new_name.is_empty() {
        return;
    }
    match crate::shell::rename(&old, &new_name) {
        Ok(()) => {
            if let Some(parent) = old.parent() {
                crate::undo::push(crate::undo::Op::Rename { from: old.clone(), to: parent.join(&new_name) });
            }
            if matches!(app.tab().loc, Loc::Folder(_)) {
                app.reload(Some(&new_name));
            } else {
                let folders_first = app.prefs.folders_first;
                let t = app.tab_mut();
                t.entries[i].name = new_name.clone();
                if let Some(f) = &mut t.entries[i].full {
                    f.set_file_name(&new_name);
                }
                // Search and Recents patch the item in place: its sort place and group follow.
                t.resort(folders_first);
                app.reveal_focus();
            }
        }
        Err(msg) => app.note(&msg),
    }
}

unsafe extern "system" fn edit_proc(
    h: HWND,
    m: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    let app = app_of(unsafe { GetParent(h) }.unwrap_or_default());
    if app.is_null() {
        return unsafe { DefSubclassProc(h, m, w, l) };
    }
    let app = unsafe { &mut *app };
    match (m, id) {
        (WM_KEYDOWN, _) | (WM_CHAR, _) => {
            let vk = VIRTUAL_KEY(w.0 as u16);
            let is_char = m == WM_CHAR;
            match id {
                EDIT_RENAME => {
                    if !is_char && vk == VK_RETURN {
                        commit_rename(app, false);
                        return LRESULT(0);
                    }
                    if !is_char && vk == VK_ESCAPE {
                        commit_rename(app, true);
                        return LRESULT(0);
                    }
                    if is_char && (w.0 == 13 || w.0 == 27) {
                        return LRESULT(0);
                    }
                }
                EDIT_SEARCH => {
                    if !is_char && vk == VK_ESCAPE {
                        clear_search(app);
                        return LRESULT(0);
                    }
                    if !is_char && (vk == VK_RETURN || vk == VK_DOWN) {
                        app.focus = Focus::Content;
                        unsafe {
                            let _ = SetFocus(Some(app.hwnd));
                        }
                        if app.tab().sel.count() == 0 && !app.tab().entries.is_empty() {
                            app.tab_mut().sel.select_only(0);
                        }
                        app.kb = true;
                        app.invalidate();
                        return LRESULT(0);
                    }
                    if is_char && (w.0 == 13 || w.0 == 27) {
                        return LRESULT(0);
                    }
                }
                EDIT_PALETTE => {
                    // Home/End stay with the text; the list takes the rest.
                    if !is_char && !matches!(vk, VK_HOME | VK_END) && crate::palette::key(app, vk) {
                        return LRESULT(0);
                    }
                    if is_char && (w.0 == 13 || w.0 == 27) {
                        return LRESULT(0);
                    }
                }
                EDIT_BATCH_A | EDIT_BATCH_B => {
                    if !is_char && vk == VK_RETURN {
                        crate::batch::apply(app);
                        return LRESULT(0);
                    }
                    if !is_char && vk == VK_ESCAPE {
                        crate::batch::close(app);
                        return LRESULT(0);
                    }
                    if !is_char && vk == VK_TAB {
                        let other = if id == EDIT_BATCH_A { EditKind::BatchB } else { EditKind::BatchA };
                        if let Some((o, _)) = edit_of(app.hwnd, other) {
                            unsafe {
                                let _ = SetFocus(Some(o));
                                SendMessageW(o, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                            }
                        }
                        return LRESULT(0);
                    }
                    if is_char && (w.0 == 13 || w.0 == 27 || w.0 == 9) {
                        return LRESULT(0);
                    }
                }
                EDIT_PATH => {
                    if !is_char && vk == VK_RETURN {
                        let t = edit_text(h);
                        path_entered(app, &t);
                        return LRESULT(0);
                    }
                    if !is_char && vk == VK_ESCAPE {
                        destroy_edit(app.hwnd, EditKind::Path);
                        unsafe {
                            let _ = SetFocus(Some(app.hwnd));
                        }
                        app.invalidate();
                        return LRESULT(0);
                    }
                    if is_char && (w.0 == 13 || w.0 == 27) {
                        return LRESULT(0);
                    }
                }
                _ => {}
            }
            // Ctrl+A selects all (single-line EDIT does not by default).
            if !is_char && vk == VK_A && unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0 {
                unsafe { SendMessageW(h, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1))) };
                return LRESULT(0);
            }
            if is_char && w.0 == 1 {
                return LRESULT(0);
            }
        }
        (WM_KILLFOCUS, EDIT_RENAME) => {
            let r = unsafe { DefSubclassProc(h, m, w, l) };
            commit_rename(app, false);
            return r;
        }
        (WM_KILLFOCUS, EDIT_PALETTE) => {
            // A click in the window keeps it open (the click decides); another app closes it.
            let r = unsafe { DefSubclassProc(h, m, w, l) };
            if w.0 as *mut c_void != app.hwnd.0 {
                crate::palette::close(app);
            }
            return r;
        }
        (WM_KILLFOCUS, EDIT_PATH) => {
            let r = unsafe { DefSubclassProc(h, m, w, l) };
            destroy_edit(app.hwnd, EditKind::Path);
            app.invalidate();
            return r;
        }
        (WM_KILLFOCUS, EDIT_SEARCH) => {
            if app.focus == Focus::Search {
                app.focus = Focus::Content;
            }
            if app.search_text.is_empty() {
                let r = unsafe { DefSubclassProc(h, m, w, l) };
                destroy_edit(app.hwnd, EditKind::Search);
                app.invalidate();
                return r;
            }
            app.invalidate();
        }
        _ => {}
    }
    unsafe { DefSubclassProc(h, m, w, l) }
}

// ---- window procedure ---------------------------------------------------------------------------------------

fn xy(l: LPARAM, scale: f32) -> (f32, f32) {
    let x = (l.0 & 0xFFFF) as i16 as f32;
    let y = ((l.0 >> 16) & 0xFFFF) as i16 as f32;
    (x / scale, y / scale)
}

fn mods() -> (bool, bool, bool) {
    unsafe {
        (
            GetKeyState(VK_CONTROL.0 as i32) < 0,
            GetKeyState(VK_SHIFT.0 as i32) < 0,
            GetKeyState(VK_MENU.0 as i32) < 0,
        )
    }
}

fn frame_thickness(hwnd: HWND) -> (i32, i32) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let pad = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        (
            GetSystemMetricsForDpi(SM_CXFRAME, dpi) + pad,
            GetSystemMetricsForDpi(SM_CYFRAME, dpi) + pad,
        )
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { wndproc_inner(hwnd, msg, wparam, lparam) })) {
        Ok(r) => r,
        Err(e) => {
            crate::prefs::log(&format!("panic in wndproc (msg {msg:#x}): {}", panic_text(&e)));
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

pub fn panic_text(e: &Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default()
}

unsafe fn wndproc_inner(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let app = cs.lpCreateParams as *mut App;
            (*app).hwnd = hwnd;
            (*app).scale = GetDpiForWindow(hwnd) as f32 / 96.0;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as isize);
            WINDOWS.with(|w| w.borrow_mut().push(hwnd.0 as isize));
            let _ = EnableNonClientDpiScaling(hwnd);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_NCDESTROY {
            // Last message: the window's App goes with it.
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            WINDOWS.with(|w| w.borrow_mut().retain(|&h| h != hwnd.0 as isize));
            if !p.is_null() {
                drop(Box::from_raw(p));
            }
            // With a copy or move still running the process stays (windowless) until it ends.
            if windows().is_empty() && OPS.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                PostQuitMessage(0);
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = app_of(hwnd);
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *ptr;
        if let Some(r) = crate::shell::menu_message(hwnd, msg, wparam, lparam) {
            return r;
        }
        match msg {
            // Screen readers: the window's own UI Automation tree (lParam is 32-bit).
            WM_GETOBJECT if lparam.0 as i32 == crate::uia::ROOT_OBJECT_ID => crate::uia::get_object(hwnd, wparam, lparam),
            WM_APP_UIA => {
                crate::uia::act(app, lparam);
                LRESULT(0)
            }
            WM_NCCALCSIZE if wparam.0 != 0 => {
                // Keep the side and bottom resize frames, drop the caption: the client area
                // starts at the window top, where the toolbar draws its own caption buttons.
                let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
                let top = params.rgrc[0].top;
                DefWindowProcW(hwnd, msg, wparam, lparam);
                params.rgrc[0].top = top;
                if IsZoomed(hwnd).as_bool() {
                    params.rgrc[0].top += frame_thickness(hwnd).1;
                }
                LRESULT(0)
            }
            WM_NCHITTEST => {
                let d = DefWindowProcW(hwnd, msg, wparam, lparam);
                if d.0 != HTCLIENT as isize {
                    return d;
                }
                let mut p = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut p);
                if !IsZoomed(hwnd).as_bool() && p.y < frame_thickness(hwnd).1 {
                    return LRESULT(HTTOP as isize);
                }
                let (x, y) = (p.x as f32 / app.scale, p.y as f32 / app.scale);
                match app.hit(x, y) {
                    Hit::Caption(Cap::Max) => LRESULT(HTMAXBUTTON as isize),
                    Hit::Drag if !path_editing(app) => LRESULT(HTCAPTION as isize),
                    _ => LRESULT(HTCLIENT as isize),
                }
            }
            WM_NCMOUSEMOVE => {
                if wparam.0 == HTMAXBUTTON as usize {
                    if app.hover != Hit::Caption(Cap::Max) {
                        app.hover = Hit::Caption(Cap::Max);
                        track_leave(hwnd, true);
                        app.invalidate();
                    }
                    return LRESULT(0);
                }
                if app.hover != Hit::None {
                    app.hover = Hit::None;
                    app.invalidate();
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_NCLBUTTONDOWN if wparam.0 == HTMAXBUTTON as usize => {
                app.pressed = Hit::Caption(Cap::Max);
                app.invalidate();
                LRESULT(0)
            }
            WM_NCLBUTTONUP if wparam.0 == HTMAXBUTTON as usize => {
                if app.pressed == Hit::Caption(Cap::Max) {
                    caption(app, Cap::Max);
                }
                app.pressed = Hit::None;
                app.invalidate();
                LRESULT(0)
            }
            WM_NCMOUSELEAVE | WM_MOUSELEAVE => {
                if !matches!(app.drag, Drag::None) {
                    return LRESULT(0);
                }
                app.hover = Hit::None;
                if msg == WM_NCMOUSELEAVE && app.pressed == Hit::Caption(Cap::Max) {
                    app.pressed = Hit::None;
                }
                app.invalidate();
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                paint_now(app);
                let _ = EndPaint(hwnd, &ps);
                crate::uia::after_paint(app);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                let (w, h) = (
                    (lparam.0 & 0xFFFF) as u32,
                    ((lparam.0 >> 16) & 0xFFFF) as u32,
                );
                if w > 0 && h > 0 {
                    app.maximized = wparam.0 == SIZE_MAXIMIZED as usize;
                    app.w = w as f32 / app.scale;
                    app.h = h as f32 / app.scale;
                    if let Some(g) = app.gfx.as_mut() {
                        g.resize(w, h);
                    }
                    if edit_of(app.hwnd, EditKind::Rename).is_some() {
                        commit_rename(app, false);
                    }
                    app.relayout();
                    paint_now(app);
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                app.scale = (wparam.0 & 0xFFFF) as f32 / 96.0;
                let r = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                rebuild_edits(app);
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let mi = &mut *(lparam.0 as *mut MINMAXINFO);
                mi.ptMinTrackSize = POINT {
                    x: (640.0 * app.scale) as i32,
                    y: (420.0 * app.scale) as i32,
                };
                LRESULT(0)
            }
            WM_ACTIVATE => {
                app.active = (wparam.0 & 0xFFFF) != WA_INACTIVE as usize;
                app.invalidate();
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_SETFOCUS => {
                if app.focus == Focus::Search {
                    if let Some((h, _)) = edit_of(app.hwnd, EditKind::Search) {
                        let _ = SetFocus(Some(h));
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                track_leave(hwnd, false);
                let (x, y) = xy(lparam, app.scale);
                app.mouse_move(x, y);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                app.eat_dblclk = false;
                let (x, y) = xy(lparam, app.scale);
                let (ctrl, shift, _) = mods();
                // Same for a click on an item (it would select or drag whatever moved there).
                if renaming(app) && matches!(app.hit(x, y), Hit::Row(_) | Hit::Group(_) | Hit::Empty | Hit::ColRow(..) | Hit::ColEmpty(_)) {
                    end_rename(app);
                    // Nor does a double-click begun with it.
                    app.eat_dblclk = true;
                    return LRESULT(0);
                }
                SetCapture(hwnd);
                if !matches!(app.hit(x, y), Hit::Search)
                    && GetFocus() != Some(hwnd).unwrap_or_default()
                {
                    let _ = SetFocus(Some(hwnd));
                }
                app.mouse_down(x, y, ctrl, shift);
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                if std::mem::take(&mut app.eat_dblclk) {
                    return LRESULT(0);
                }
                let (x, y) = xy(lparam, app.scale);
                let (ctrl, shift, _) = mods();
                // The first click left only its item selected: another item under the pointer
                // means the list moved in between (a re-sort, live results), so this is a click.
                let moved = !ctrl && !shift && matches!(app.hit(x, y), Hit::Row(i) if !app.tab().sel.is(i));
                SetCapture(hwnd);
                app.mouse_down(x, y, ctrl, shift);
                app.drag = Drag::None;
                if !moved {
                    app.double_click(x, y);
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED => {
                // Capture lost with the button still down (a dialog came up, Alt+Tab): no
                // button-up will follow, so the press ends here. Our own release on button-up
                // comes with the button already up.
                if GetKeyState(VK_LBUTTON.0 as i32) < 0 && (!matches!(app.drag, Drag::None) || app.pressed != Hit::None) {
                    app.drag = Drag::None;
                    app.pressed = Hit::None;
                    app.flush_deferred();
                    app.invalidate();
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                let (x, y) = xy(lparam, app.scale);
                let (ctrl, shift, _) = mods();
                app.mouse_up(x, y, ctrl, shift);
                LRESULT(0)
            }
            WM_MBUTTONUP => {
                // Like the right button: closing a rename is all this click does.
                if renaming(app) {
                    end_rename(app);
                    return LRESULT(0);
                }
                let (x, y) = xy(lparam, app.scale);
                app.middle_click(x, y);
                LRESULT(0)
            }
            WM_XBUTTONUP => {
                if (wparam.0 >> 16) & 0xFFFF == XBUTTON1 as usize {
                    app.back();
                } else {
                    app.forward();
                }
                LRESULT(1)
            }
            WM_RBUTTONUP => {
                // A click that closes a rename does only that: the commit can move the rows
                // (a re-sort by the new name), so the menu would open on another item.
                if renaming(app) {
                    end_rename(app);
                    return LRESULT(0);
                }
                let (x, y) = xy(lparam, app.scale);
                let mut p = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ClientToScreen(hwnd, &mut p);
                app.right_click(x, y, p);
                LRESULT(0)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32;
                let mut p = POINT {
                    x: (lparam.0 & 0xFFFF) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut p);
                if msg == WM_MOUSEWHEEL {
                    if edit_of(app.hwnd, EditKind::Rename).is_some() {
                        commit_rename(app, false);
                    }
                    let (ctrl, _, _) = mods();
                    app.wheel(p.x as f32 / app.scale, p.y as f32 / app.scale, delta, ctrl);
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                if (lparam.0 & 0xFFFF) as u32 == HTCLIENT
                    && (app.hover == Hit::SideResize || matches!(app.drag, Drag::Sidebar { .. }))
                {
                    SetCursor(LoadCursorW(None, IDC_SIZEWE).ok());
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let (ctrl, shift, alt) = mods();
                if app.key(VIRTUAL_KEY(wparam.0 as u16), ctrl, shift, alt) {
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CHAR => {
                let (ctrl, _, alt) = mods();
                if !ctrl && !alt {
                    if let Some(ch) = char::from_u32(wparam.0 as u32) {
                        app.char_typed(ch);
                    }
                }
                LRESULT(0)
            }
            WM_SYSCHAR => {
                // Swallow Alt+letter beeps for our own shortcuts (Alt+D).
                if wparam.0 as u8 as char == 'd' || wparam.0 as u8 as char == 'D' {
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_COMMAND => {
                let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
                let id = wparam.0 & 0xFFFF;
                if (id == EDIT_BATCH_A || id == EDIT_BATCH_B) && code == EN_CHANGE {
                    let kind = if id == EDIT_BATCH_A { EditKind::BatchA } else { EditKind::BatchB };
                    if let Some((h, _)) = edit_of(app.hwnd, kind) {
                        let t = edit_text(h);
                        if let Some(b) = app.batch.as_mut() {
                            if id == EDIT_BATCH_A { b.a = t } else { b.b = t }
                        }
                        app.invalidate();
                    }
                }
                if id == EDIT_PALETTE && code == EN_CHANGE {
                    if let Some((h, _)) = edit_of(app.hwnd, EditKind::Palette) {
                        let t = edit_text(h);
                        crate::palette::filter(app, &t);
                    }
                }
                if id == EDIT_SEARCH && code == EN_CHANGE {
                    if let Some((h, _)) = edit_of(app.hwnd, EditKind::Search) {
                        let t = edit_text(h);
                        if t != app.search_text {
                            search_changed(app, t);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_CTLCOLOREDIT => {
                let child = HWND(lparam.0 as *mut c_void);
                let hdc = HDC(wparam.0 as *mut c_void);
                let found = EDITS.with(|e| {
                    e.borrow()
                        .iter()
                        .find(|x| x.hwnd == child)
                        .map(|x| (x.bg, x.fg, x.brush))
                });
                if let Some((bg, fg, brush)) = found {
                    SetTextColor(hdc, fg);
                    SetBkColor(hdc, bg);
                    return LRESULT(brush.0 as isize);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_TIMER => {
                match wparam.0 {
                    // While busy (press, drag, rename) the timer keeps ticking: the reload comes
                    // once the rows may move.
                    TIMER_FS if !app.busy() => {
                        let _ = KillTimer(Some(hwnd), TIMER_FS);
                        crate::thumbs::refresh();
                        app.reload(None);
                        crate::views::refresh_columns(app);
                        app.invalidate();
                    }
                    TIMER_FS => {}
                    TIMER_NOTE => {
                        let _ = KillTimer(Some(hwnd), TIMER_NOTE);
                        app.invalidate();
                    }
                    crate::droptarget::TIMER => crate::droptarget::tick(app),
                    _ => crate::shell::timer(app, wparam.0),
                }
                LRESULT(0)
            }
            WM_APP_FS => {
                // Coalesce bursts (a copy of 1 000 files) into one reload.
                SetTimer(Some(hwnd), TIMER_FS, 150, None);
                LRESULT(0)
            }
            WM_APP_SEARCH => {
                crate::search::message(app, wparam, lparam);
                LRESULT(0)
            }
            WM_APP_SIZES => {
                crate::search::sizes_message(app, lparam);
                LRESULT(0)
            }
            WM_APP_SHELLLIST => {
                crate::shellns::message(app, lparam);
                LRESULT(0)
            }
            WM_SETTINGCHANGE
            | WM_THEMECHANGED
            | WM_SYSCOLORCHANGE
            | WM_DWMCOLORIZATIONCOLORCHANGED => {
                app.theme = crate::theme::Theme::detect();
                setup_dwm(app);
                rebuild_edits(app);
                app.invalidate();
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CLOSE => {
                crate::quicklook::close();
                if !dev_run() {
                    crate::prefs::save(app);
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                crate::uia::forget(hwnd);
                crate::droptarget::revoke(hwnd);
                stop_watch(hwnd);
                for k in [EditKind::Search, EditKind::Rename, EditKind::Path, EditKind::BatchA, EditKind::BatchB] {
                    destroy_edit(hwnd, k);
                }
                crate::search::forget(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn track_leave(hwnd: HWND, nc: bool) {
    let mut t = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: if nc {
            TME_LEAVE | TME_NONCLIENT
        } else {
            TME_LEAVE
        },
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    unsafe {
        let _ = TrackMouseEvent(&mut t);
    }
}

/// Note timer so the transient status text disappears on its own.
pub fn arm_note_timer(app: &App) {
    if !app.offscreen {
        unsafe {
            SetTimer(Some(app.hwnd), TIMER_NOTE, 3100, None);
        }
    }
}
