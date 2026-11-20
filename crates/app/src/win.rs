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
/// Open the command lines other launches handed over (PENDING).
const WM_APP_OPEN: u32 = WM_APP + 20;
/// `fm --quit` from another process (hub).
const WM_APP_QUIT: u32 = WM_APP + 22;
/// Open the pickers other apps' Open dialogs asked for (PICKS).
const WM_APP_PICK: u32 = WM_APP + 23;

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
/// Ends the window in which fm's own Explorer launch is let through (hub).
const TIMER_EXPLORER: usize = 0xE1;
/// Hub timer: reads the Recent folder's shortcuts ahead of the first visit.
const TIMER_RECENTS: usize = 0xE2;
const TIMER_FS: usize = 1;
const TIMER_NOTE: usize = 2;
/// A close that came during a menu, drag or shell dialog, tried again (nested()).
const TIMER_CLOSE: usize = 3;
/// A picker checks that its host still waits (pick::tick).
const TIMER_PICK: usize = 0xE3;
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
/// The resident fm's door for other launches (open_door); solo test runs have none.
const DOOR_CLASS: PCWSTR = w!("FmDoor");
const COPY_OPEN: usize = 0x666D_0001;
/// `fm --quit`: close every window and end the process (installers, instead of a hard kill).
const COPY_QUIT: usize = 0x666D_0003;

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

/// The user's environment as it is now (Explorer does the same): programs fm starts get today's
/// PATH, not the one from sign-in or from the terminal that started fm.
pub fn refresh_env() {
    use windows::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
    use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock, SetEnvironmentStringsW};
    // fm's own data stays where this process started it.
    let _ = crate::prefs::data_dir();
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token).is_err() {
            return;
        }
        let mut block = std::ptr::null_mut();
        if CreateEnvironmentBlock(&mut block, Some(token), false).is_ok() {
            let _ = SetEnvironmentStringsW(PCWSTR(block as *const u16));
            let _ = DestroyEnvironmentBlock(block);
        }
        let _ = CloseHandle(token);
    }
}

/// `fm --quit`: asks a running fm to close its windows and exit (it finishes running copies and
/// saves prefs and the index first). Never starts one; returns at once.
pub fn ask_quit() {
    unsafe {
        let Ok(m) = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("Local\\fm.main")) else { return };
        let _ = CloseHandle(m);
        let Ok(door) = FindWindowExW(Some(HWND_MESSAGE), None, DOOR_CLASS, None) else { return };
        let cds = COPYDATASTRUCT { dwData: COPY_QUIT, cbData: 0, lpData: std::ptr::null_mut() };
        let mut res = 0usize;
        let _ = SendMessageTimeoutW(door, WM_COPYDATA, WPARAM(0), LPARAM(&cds as *const COPYDATASTRUCT as isize), SMTO_ABORTIFHUNG, 5000, Some(&mut res));
    }
}

/// How a launch joins the single instance (`Local\fm.main`).
pub enum Join {
    /// No fm runs: this process becomes the resident one.
    Owner,
    /// The running fm got the command line (or, for `--background`, simply runs).
    HandedOver,
    /// One runs but cannot be reached (elevated, quitting, hung): this process runs on its own,
    /// for its window only.
    Unjoinable,
}

/// A second launch hands its command line and folder to the resident fm, which opens what it
/// names. An elevated launch never becomes the resident one (a normal launch could not reach
/// it): it hands over to a normal fm or runs alone.
pub fn hand_over(cwd: &Path, raw: &str, background: bool, elevated: bool) -> Join {
    join(cwd, raw, background, elevated, false)
}

fn join(cwd: &Path, raw: &str, background: bool, elevated: bool, retried: bool) -> Join {
    use std::os::windows::ffi::OsStrExt;
    unsafe {
        let name = w!("Local\\fm.main");
        if elevated {
            match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, name) {
                Ok(m) => {
                    let _ = CloseHandle(m);
                }
                Err(_) => return Join::Unjoinable,
            }
        } else {
            match CreateMutexW(None, false, name) {
                Err(_) => return Join::Unjoinable,
                // The owner's handle stays open for the life of the process: it marks the instance.
                Ok(_) if GetLastError() != ERROR_ALREADY_EXISTS => return Join::Owner,
                // Others let go, so a new resident can start once this one has ended.
                Ok(m) => {
                    let _ = CloseHandle(m);
                }
            }
        }
        if background {
            return Join::HandedOver;
        }
        // The resident fm answers on its own thread, so a busy window never makes this wait.
        for _ in 0..30 {
            if let Ok(door) = FindWindowExW(Some(HWND_MESSAGE), None, DOOR_CLASS, None) {
                let mut pid = 0u32;
                GetWindowThreadProcessId(door, Some(&mut pid));
                let _ = AllowSetForegroundWindow(pid);
                // "<cwd>\0<raw>": relative paths resolve against the sender's folder.
                let s: Vec<u16> = cwd.as_os_str().encode_wide().chain([0]).chain(raw.encode_utf16()).collect();
                let cds = COPYDATASTRUCT { dwData: COPY_OPEN, cbData: (s.len() * 2) as u32, lpData: s.as_ptr() as *mut c_void };
                let mut res = 0usize;
                let ok = SendMessageTimeoutW(door, WM_COPYDATA, WPARAM(0), LPARAM(&cds as *const COPYDATASTRUCT as isize), SMTO_ABORTIFHUNG, 5000, Some(&mut res));
                if ok.0 != 0 && res != 0 {
                    return Join::HandedOver;
                }
                if ok.0 != 0 && !retried {
                    // Turned away: that fm is quitting. Once it is gone, this launch is the new one.
                    if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                        let _ = WaitForSingleObject(h, 15000);
                        let _ = CloseHandle(h);
                    }
                    return join(cwd, raw, background, elevated, true);
                }
                return Join::Unjoinable;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Join::Unjoinable
    }
}

/// Run as administrator over a normal session (the full half of a split token). With UAC off
/// or as the built-in Administrator every process has that token, and that is the normal one.
pub fn is_elevated() -> bool {
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION_TYPE, TOKEN_QUERY, TokenElevationType, TokenElevationTypeFull};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut t = TOKEN_ELEVATION_TYPE::default();
        let mut n = 0u32;
        let ok = GetTokenInformation(token, TokenElevationType, Some(&mut t as *mut _ as *mut c_void), std::mem::size_of::<TOKEN_ELEVATION_TYPE>() as u32, &mut n).is_ok();
        let _ = CloseHandle(token);
        ok && t == TokenElevationTypeFull
    }
}

/// The resident fm's door: a message-only window on its own thread that takes handed-over
/// command lines and `--quit`, answers at once and passes them to the UI thread (the hub). A
/// window stuck on a slow share then never makes a launch time out and start a second fm.
fn open_door() {
    std::thread::spawn(|| unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(door_proc),
            hInstance: hinst.into(),
            lpszClassName: DOOR_CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        if CreateWindowExW(WINDOW_EX_STYLE(0), DOOR_CLASS, None, WINDOW_STYLE(0), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None).is_err() {
            return;
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    });
}

unsafe extern "system" fn door_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if msg != WM_COPYDATA || l.0 == 0 {
            return DefWindowProcW(hwnd, msg, w, l);
        }
        let cds = &*(l.0 as *const COPYDATASTRUCT);
        let hub = Some(HWND(HUB_ANY.load(std::sync::atomic::Ordering::SeqCst) as *mut c_void));
        match cds.dwData {
            COPY_QUIT => {
                let _ = PostMessageW(hub, WM_APP_QUIT, WPARAM(0), LPARAM(0));
                LRESULT(1)
            }
            COPY_OPEN => {
                let n = cds.cbData as usize / 2;
                let text = if n == 0 || cds.lpData.is_null() {
                    String::new()
                } else {
                    String::from_utf16_lossy(std::slice::from_raw_parts(cds.lpData as *const u16, n))
                };
                let (cwd, raw) = text.split_once('\0').unwrap_or(("", &text));
                let Ok(mut p) = PENDING.lock() else { return LRESULT(0) };
                if QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
                    return LRESULT(0);
                }
                p.push((PathBuf::from(cwd), raw.to_string()));
                let _ = PostMessageW(hub, WM_APP_OPEN, WPARAM(0), LPARAM(0));
                LRESULT(1)
            }
            // Another app's Open dialog (fm_dialog_shim): the explicit ACK tells it a picker comes.
            fm_dialog_shim::proto::COPY_PICK => {
                let words = if cds.cbData < 2 || cds.lpData.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2)
                };
                let Some(req) = fm_dialog_shim::proto::Request::decode(words) else { return LRESULT(0) };
                let Ok(mut p) = PICKS.lock() else { return LRESULT(0) };
                if QUITTING.load(std::sync::atomic::Ordering::SeqCst) {
                    return LRESULT(0);
                }
                p.push(req.pipe);
                let _ = PostMessageW(hub, WM_APP_PICK, WPARAM(0), LPARAM(0));
                LRESULT(fm_dialog_shim::proto::ACK)
            }
            _ => LRESULT(0),
        }
    }
}

/// The message loop of the process. `resident`: it stays when the last window closes, as Explorer
/// does (folder opens are instant and the index stays warm); "Ukončit Soubory" ends it.
pub fn run(raw: &str, cwd: &Path, background: bool, resident: bool, pick: Option<&str>) -> i32 {
    RESIDENT.store(resident, std::sync::atomic::Ordering::SeqCst);
    unsafe {
        crate::gfx::prewarm();
        let _ = windows::Win32::System::Ole::OleInitialize(None);
        crate::win::trace("ole");
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        allow_dark_menus();
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let (icon, small) = app_icons();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: MAIN_CLASS,
            hIcon: icon,
            hIconSm: small,
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
        if resident {
            open_door();
            // Message-only windows get no broadcasts: this hidden one hears the session end.
            let _ = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                HUB_CLASS,
                None,
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinst.into()),
                None,
            );
        }

        // After OleInitialize: shell places (`::{…}`, `shell:`) parse through COM.
        let first = match pick {
            Some(pipe) => crate::pick::start(pipe),
            None if background => None,
            None => open_target(crate::target::parse(raw, cwd), None),
        };
        if first.is_none() && !resident {
            return 1;
        }
        // A started picker has no index: searches filter the folder's listing (search.rs).
        if pick.is_none() {
            crate::search::start();
        }
        // Recents reads every shortcut in Windows' Recent folder the first time; do that while
        // nobody waits for it.
        if !dev_run() && pick.is_none() {
            SetTimer(Some(hub()), TIMER_RECENTS, 3000, None);
        }
        if let Some(app) = first.and_then(|h| app_of(h).as_mut()) {
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

fn taskbar_created() -> u32 {
    static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *ID.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

thread_local! {
    /// How many fm window messages are being handled on this thread, one inside another.
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A message handled inside another fm window message: a menu, dialog, drag or COM call is
/// running its own message loop with some window's App up the stack. Whatever would free or
/// rework an App (a close, a selection from outside) waits until that loop is over.
pub fn nested() -> bool {
    DEPTH.with(|d| d.get()) > 1
}

/// One level of DEPTH for a frame that holds an App: wndproc, the edit subclass, OLE's drop calls.
pub struct Busy(());

impl Busy {
    pub fn new() -> Busy {
        DEPTH.with(|d| d.set(d.get() + 1));
        Busy(())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

static RESIDENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Command lines handed over by other launches (door thread), opened by WM_APP_OPEN on the UI
/// thread: (sender's folder, line).
static PENDING: std::sync::Mutex<Vec<(PathBuf, String)>> = std::sync::Mutex::new(Vec::new());

/// Pipes of pick requests the door took (fm_dialog_shim), opened by WM_APP_PICK on the UI thread.
static PICKS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The process ends once no window is left, unless resident; a copy or move still running keeps
/// it (windowless) until it ends.
fn quit_if_done() {
    if windows().is_empty()
        && OPS.load(std::sync::atomic::Ordering::SeqCst) == 0
        && !RESIDENT.load(std::sync::atomic::Ordering::SeqCst)
    {
        // A handed-over line not opened yet opens first (its window then ends the process).
        let Ok(p) = PENDING.lock() else { return };
        if !p.is_empty() {
            return;
        }
        QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(p);
        unsafe { PostQuitMessage(0) };
    }
}

/// Set (under the PENDING lock) once the message loop is ending: the door turns launches away.
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// "Ukončit Soubory": every window closes and the process ends (a resident one too).
pub fn quit_all(_: &mut App) {
    quit_everything();
}

fn quit_everything() {
    RESIDENT.store(false, std::sync::atomic::Ordering::SeqCst);
    let open = windows();
    for &h in &open {
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    // No window to close: the last-window check never runs, so end here (or when copies end).
    if open.is_empty() {
        quit_if_done();
    }
}

/// What a command line names: a window on it, Explorer for places only Explorer shows, or a
/// message when nothing is there.
pub fn open_target(t: crate::target::Target, near: Option<HWND>) -> Option<HWND> {
    use crate::target::Target;
    match t {
        Target::Open { loc, select } => {
            let h = open_window(loc, near, None)?;
            if let Some(app) = unsafe { app_of(h).as_mut() } {
                if select.is_some() {
                    app.reload(select.as_deref());
                }
            }
            unsafe {
                let _ = SetForegroundWindow(h);
            }
            Some(h)
        }
        Target::Explorer(args) => {
            launch_explorer(&args);
            None
        }
        Target::Missing(text) => {
            unsafe {
                MessageBoxW(None, &HSTRING::from(text), w!("Soubory"), MB_ICONWARNING | MB_SETFOREGROUND);
            }
            None
        }
    }
}

/// Signalled for a few seconds while fm itself starts Explorer, so the dialogkit watcher (which
/// replaces new Explorer windows by fm) leaves that one alone.
fn explorer_event() -> Option<HANDLE> {
    thread_local! {
        static EV: std::cell::Cell<Option<HANDLE>> = const { std::cell::Cell::new(None) };
    }
    EV.with(|c| {
        if c.get().is_none() {
            c.set(unsafe { CreateEventW(None, true, false, w!("Local\\fm.explorer-allow")) }.ok());
        }
        c.get()
    })
}

/// Starts Explorer itself (never a folder verb, which would come back to fm) with `args`.
pub fn launch_explorer(args: &str) {
    if let Some(e) = explorer_event() {
        unsafe {
            let _ = SetEvent(e);
            SetTimer(Some(hub()), TIMER_EXPLORER, 8000, None);
        }
    }
    let exe = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("explorer.exe");
    let mut cmd = std::process::Command::new(exe);
    // Explorer reads its own command line (no escapes): pass it verbatim.
    std::os::windows::process::CommandExt::raw_arg(&mut cmd, args);
    if cmd.spawn().is_err() {
        crate::prefs::log("explorer.exe could not be started");
    }
}

/// "Otevřít v Průzkumníkovi": the escape hatch to Explorer for what this window shows.
pub fn open_in_explorer(app: &mut App) {
    let t = app.tab();
    let focused = t.sel.focus.filter(|&f| t.sel.is(f) && f < t.entries.len()).map(|f| t.path_of(f));
    launch_explorer(&crate::target::explorer_args(&t.loc, focused.as_deref()));
}

/// Opens a main window on `loc`. `near`: cascade from that window (Ctrl+N, a second launch);
/// otherwise it goes where the last window was closed. `pick`: a picker, centred over its host's
/// window and foreground.
pub fn open_window(loc: Loc, near: Option<HWND>, pick: Option<crate::pick::Picker>) -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let mut app = Box::new(App::new(loc));
        crate::win::trace("app_new");
        crate::prefs::load(&mut app);
        // Its listing (file types, hidden files) and layout (bottom bar) follow from the request.
        let select = pick.as_ref().map(|p| p.req.name.clone()).filter(|n| !n.is_empty());
        if let Some(p) = pick {
            app.prefs.show_hidden |= p.req.hidden;
            app.pick = Some(p);
        }
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
        if app.pick.is_some() {
            if select.is_some() {
                app.reload(select.as_deref());
            }
            place_picker(app);
            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(hwnd);
            SetTimer(Some(hwnd), TIMER_PICK, 1000, None);
            // The host stopped waiting meanwhile: nobody to pick for.
            if app.pick.as_mut().is_some_and(|p| !p.ready()) {
                close(app);
            }
        } else if near.is_some_and(|n| GetWindowRect(n, &mut near_rect).is_ok() && !IsIconic(n).as_bool()) {
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

/// A picker is dialog-sized and centred over its host's window, inside that monitor's work area.
fn place_picker(app: &App) {
    unsafe {
        let owner = HWND(app.pick.as_ref().map_or(0, |p| p.req.owner) as *mut c_void);
        let mut r = RECT::default();
        let over_owner = !owner.is_invalid() && IsWindow(Some(owner)).as_bool() && !IsIconic(owner).as_bool() && GetWindowRect(owner, &mut r).is_ok();
        let mon = MonitorFromWindow(if over_owner { owner } else { app.hwnd }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let work = mi.rcWork;
        if !over_owner {
            r = work;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let s = dx as f32 / 96.0;
        let w = ((940.0 * s) as i32).min(work.right - work.left);
        let h = ((620.0 * s) as i32).min(work.bottom - work.top);
        let x = ((r.left + r.right) / 2 - w / 2).clamp(work.left, work.right - w);
        let y = ((r.top + r.bottom) / 2 - h / 2).clamp(work.top, work.bottom - h);
        let _ = SetWindowPos(app.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
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
            if !on_gone && i != cur && matches!(a.tabs[i].loc, Loc::Computer) {
                a.tabs[i].stale = true;
                continue;
            }
            if !on_gone && !(i == cur && matches!(a.tabs[i].loc, Loc::Computer)) {
                continue;
            }
            a.cur = i;
            if on_gone {
                if i == cur {
                    commit_rename(a, true);
                }
                a.go(Loc::Computer);
            } else if a.busy() {
                // Under a press or band: flush_deferred lists it once the rows may move.
                a.tabs[i].stale = true;
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
                quit_if_done();
                Some(LRESULT(0))
            }
            WM_APP_OPEN => {
                let near = GetForegroundWindow();
                let near = if app_of(near).is_null() { windows().last().copied() } else { Some(near) };
                let pending = PENDING.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default();
                for (cwd, raw) in pending {
                    open_target(crate::target::parse(&raw, &cwd), near);
                }
                Some(LRESULT(0))
            }
            WM_TIMER if w.0 == TIMER_EXPLORER => {
                let _ = KillTimer(Some(hwnd), TIMER_EXPLORER);
                if let Some(e) = explorer_event() {
                    let _ = windows::Win32::System::Threading::ResetEvent(e);
                }
                Some(LRESULT(0))
            }
            WM_APP_QUIT => {
                quit_everything();
                Some(LRESULT(0))
            }
            WM_APP_PICK => {
                let picks = PICKS.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default();
                for pipe in picks {
                    crate::pick::start(&pipe);
                }
                Some(LRESULT(0))
            }
            // Restart Manager (an installer) asks to close the app: not while a copy runs.
            // Environment variables changed (System settings, an installer): only the session
            // window gets this broadcast, so it runs once.
            WM_SETTINGCHANGE if l.0 != 0 && PCWSTR(l.0 as *const u16).to_string().is_ok_and(|s| s == "Environment") => {
                refresh_env();
                None
            }
            WM_QUERYENDSESSION => Some(LRESULT(
                (l.0 as u32 & ENDSESSION_CLOSEAPP == 0 || OPS.load(std::sync::atomic::Ordering::SeqCst) == 0) as isize,
            )),
            WM_ENDSESSION => {
                // Logging off, shutting down, or Restart Manager closing the app: save, then end
                // (a logoff ends the process anyway; Restart Manager may not).
                if w.0 != 0 {
                    for_each_app(|a| {
                        if !dev_run() && a.pick.is_none() {
                            crate::prefs::save(a);
                        }
                    });
                    crate::search::stop();
                    QUITTING.store(true, std::sync::atomic::Ordering::SeqCst);
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
            WM_TIMER if w.0 == TIMER_RECENTS => {
                let _ = KillTimer(Some(hwnd), TIMER_RECENTS);
                crate::shell::recent_items(200);
                Some(LRESULT(0))
            }
            WM_TIMER => {
                crate::search::tick(w.0);
                Some(LRESULT(0))
            }
            WM_APP_OPDONE => {
                crate::undo::done(Box::from_raw(l.0 as *mut crate::undo::Done));
                // The drives' fill bars follow what the operation wrote or freed.
                for_each_app(|a| {
                    crate::sidebar::refresh_usage(&mut a.side_rows);
                    a.invalidate();
                });
                Some(LRESULT(0))
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
        if !dev_keys(app, &script, val("--dev-out")) {
            return;
        }
    }
    app.invalidate();
}

/// `--dev-keys "sel:0,ctrl+d,wait:1500,dump,ctrl+z,…"`: feeds keys to the app's own key handler
/// (never synthesized input), pumping messages between steps; `dump` appends the folder listing
/// and the status note to `--dev-out`. False once a step closed the window (a picker's Enter or
/// Esc): its App is gone, and the script stops there.
fn dev_keys(app: &mut App, script: &str, out: Option<String>) -> bool {
    let mut log = String::new();
    let hwnd = app.hwnd;
    // False when the window went away; the process's WM_QUIT is posted again for the main loop.
    let pump = |ms: u64| unsafe {
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(ms) {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    PostQuitMessage(msg.wParam.0 as i32);
                    return false;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if !IsWindow(Some(hwnd)).as_bool() {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    };
    let mut alive = true;
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
            if !pump(ms) {
                alive = false;
                break;
            }
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
            let v: Vec<String> = t.entries.iter().filter(|e| e.is_dir).map(|e| if e.size_known() { format!("{}={}", e.name, e.size) } else { format!("{}=?", e.name) }).collect();
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
        } else if step == "sel" {
            let t = app.tab();
            let names: Vec<&str> = t.sel.indices().into_iter().filter_map(|i| t.entries.get(i)).map(|e| e.name.as_str()).collect();
            log += &format!("sel {:?} [{}] focus {:?}
", t.loc, names.join(" ; "), t.sel.focus.and_then(|f| t.entries.get(f)).map(|e| e.name.clone()));
        } else if step == "dump" {
            app.reload(None);
            let names: Vec<String> = app.tab().entries.iter().map(|e| e.name.clone()).collect();
            log += &format!("{} | note: {}\n", names.join(" ; "), app.status_note.as_ref().map(|n| n.0.clone()).unwrap_or_default());
        } else {
            let vk = match key {
                "del" => VK_DELETE,
                "enter" => VK_RETURN,
                "esc" => VK_ESCAPE,
                k if k.len() == 1 => VIRTUAL_KEY(k.to_ascii_uppercase().as_bytes()[0] as u16),
                _ => continue,
            };
            app.key(vk, ctrl, shift, false);
            if !pump(50) {
                alive = false;
                break;
            }
        }
    }
    if let Some(o) = out {
        let _ = std::fs::write(o, log);
    }
    alive
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

/// fm.exe's own icon (resource 1, from assets\fm.ico) at the large and small system sizes; the
/// stock folder icon if it is missing.
fn app_icons() -> (HICON, HICON) {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let load = |m: SYSTEM_METRICS_INDEX| {
            let s = GetSystemMetrics(m);
            LoadImageW(Some(hinst.into()), PCWSTR(1 as _), IMAGE_ICON, s, s, LR_DEFAULTCOLOR).map(|h| HICON(h.0)).ok()
        };
        match (load(SM_CXICON), load(SM_CXSMICON)) {
            (Some(big), Some(small)) => (big, small),
            _ => (folder_icon(), folder_icon()),
        }
    }
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
    // A picker carries its dialog's title and stays out of the shell's window list ("Show in
    // folder" must not navigate it).
    if let Some(p) = &app.pick {
        let t = wide(p.title());
        unsafe {
            let _ = SetWindowTextW(app.hwnd, PCWSTR(t.as_ptr()));
        }
        return;
    }
    // Every navigation and tab switch passes here.
    crate::shellwin::sync(app);
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
        Loc::Search { .. } => crate::fsys::known_folder(&FOLDERID_Profile).map_or(Loc::Computer, Loc::Folder),
        other => other.clone(),
    };
    if let Some(h) = open_window(loc, Some(app.hwnd), None) {
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
    // An open rename comes back with its text and selection: a system broadcast must neither
    // commit half a name nor drop it.
    let rename = rename_edit(app).and_then(|(h, row, _)| {
        let (mut a, mut b) = (0u32, 0u32);
        unsafe {
            SendMessageW(h, EM_GETSEL, Some(WPARAM(&mut a as *mut u32 as usize)), Some(LPARAM(&mut b as *mut u32 as isize)));
        }
        row.map(|r| (r, edit_text(h), (a as i32, b as i32)))
    });
    let query = edit_of(app.hwnd, EditKind::Palette).map(|(h, _)| edit_text(h));
    for k in [EditKind::Search, EditKind::Rename, EditKind::Path, EditKind::Palette] {
        destroy_edit(app.hwnd, k);
    }
    if let Some((row, text, sel)) = rename {
        create_edit(app, EditKind::Rename, row, &text, sel);
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

pub fn expand_env(s: &str) -> String {
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
                if matches!(app.tab().loc, Loc::Search { .. }) {
                    crate::search::after_rename(app);
                }
            }
        }
        Err(msg) => app.note(&msg),
    }
}

unsafe extern "system" fn edit_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM, id: usize, d: usize) -> LRESULT {
    // Reached from the message loop, not through wndproc: a dialog started from the field (the
    // palette's commands) must see itself nested.
    let r = {
        let _busy = Busy::new();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { edit_proc_inner(h, m, w, l, id, d) }))
    };
    match r {
        Ok(r) => r,
        Err(e) => {
            crate::prefs::log(&format!("panic in edit_proc (msg {m:#x}): {}", panic_text(&e)));
            unsafe { DefSubclassProc(h, m, w, l) }
        }
    }
}

unsafe fn edit_proc_inner(
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
    let r = {
        let _busy = Busy::new();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { wndproc_inner(hwnd, msg, wparam, lparam) }))
    };
    match r {
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
            quit_if_done();
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = app_of(hwnd);
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *ptr;
        // The frame covers the whole window (Mica), so Windows draws the minimize, maximize and
        // close buttons over the toolbar; DWM hit-tests, highlights and clicks them (Snap Layouts too).
        let mut dwm = LRESULT(0);
        if DwmDefWindowProc(hwnd, msg, wparam, lparam, &mut dwm).as_bool() {
            return dwm;
        }
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
            crate::shellwin::WM_APP_SHELLWIN => {
                crate::shellwin::handle(app, wparam, lparam);
                LRESULT(0)
            }
            m if m == taskbar_created() => {
                // Explorer restarted: its window list is new.
                crate::shellwin::explorer_restarted(app);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_NCCALCSIZE if wparam.0 != 0 => {
                // Keep the side and bottom resize frames, drop the caption: the client area
                // starts at the window top, under Windows' caption buttons. Also when maximized:
                // with the top frame cut off DWM stops hit-testing its buttons, so the layout
                // moves the toolbar below the off-screen frame instead (App::top_inset).
                let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
                let top = params.rgrc[0].top;
                DefWindowProcW(hwnd, msg, wparam, lparam);
                params.rgrc[0].top = top;
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
                    Hit::Drag if !path_editing(app) => LRESULT(HTCAPTION as isize),
                    _ => LRESULT(HTCLIENT as isize),
                }
            }
            WM_NCMOUSEMOVE => {
                if app.hover != Hit::None {
                    app.hover = Hit::None;
                    app.invalidate();
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_NCMOUSELEAVE | WM_MOUSELEAVE => {
                if !matches!(app.drag, Drag::None) {
                    return LRESULT(0);
                }
                app.hover = Hit::None;
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
                    app.top_inset = if app.maximized { frame_thickness(hwnd).1 as f32 / app.scale } else { 0.0 };
                    app.w = w as f32 / app.scale;
                    app.h = h as f32 / app.scale;
                    // Room for Windows' caption buttons, whose size depends on DPI and state.
                    let mut b = RECT::default();
                    if DwmGetWindowAttribute(hwnd, DWMWA_CAPTION_BUTTON_BOUNDS, (&mut b as *mut RECT).cast(), std::mem::size_of::<RECT>() as u32).is_ok()
                        && b.right > b.left
                    {
                        app.caption_w = (b.right - b.left) as f32 / app.scale;
                    }
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
                app.down_on_row = matches!(app.hit(x, y), Hit::Row(_));
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
                let moved = !ctrl && !shift && matches!(app.hit(x, y), Hit::Row(i) if !app.down_on_row || !app.tab().sel.is(i));
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
                // A drag out releases it on purpose (drag_out ends that state itself).
                if !matches!(app.drag, Drag::Out) && GetKeyState(VK_LBUTTON.0 as i32) < 0 && (!matches!(app.drag, Drag::None) || app.pressed != Hit::None) {
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
                // A live left press or drag owns the list (it would carry another item).
                if !matches!(app.drag, Drag::None) {
                    return LRESULT(0);
                }
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
                // A live left press or drag owns the list: Back/Forward would swap its rows.
                if !matches!(app.drag, Drag::None) {
                    return LRESULT(1);
                }
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
                    crate::shellwin::TIMER => crate::shellwin::replay(app),
                    TIMER_CLOSE => {
                        let _ = KillTimer(Some(hwnd), TIMER_CLOSE);
                        let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    TIMER_PICK => crate::pick::tick(app),
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
            // Alt+F4, the system and taskbar menus: closed from the message loop, never from
            // inside this call (DefWindowProc would send WM_CLOSE right here).
            WM_SYSCOMMAND if wparam.0 & 0xFFF0 == SC_CLOSE as usize => {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                LRESULT(0)
            }
            WM_CLOSE => {
                if nested() {
                    // Quit from a script, "Ukončit Soubory" or fm --quit during a menu, drag or
                    // dialog: the App must outlive that loop. Tried again shortly.
                    SetTimer(Some(hwnd), TIMER_CLOSE, 200, None);
                    return LRESULT(0);
                }
                crate::quicklook::close();
                // A picker's size, place and forced hidden files are not the user's defaults.
                if !dev_run() && app.pick.is_none() {
                    crate::prefs::save(app);
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                crate::shellwin::destroyed(hwnd);
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
