//! Explorer window watcher: hands File Explorer windows to fm.
//!
//! Some apps start `explorer.exe /select,<file>` or `explorer.exe <folder>` directly, which no
//! registry verb can redirect. While HKCU\Software\fm "Redirect" names an existing fm.exe, each
//! Explorer window is decided once, when it is first shown: a file-system folder (or This PC,
//! Recycle Bin, Network) opens in fm and the Explorer window is closed; anything else stays.
//! Windows already open when the watcher starts, shown while Redirect is off, or opened from an
//! Explorer window the user kept (Ctrl+N, a torn-off tab) are never touched.
//!
//! The COM calls into Explorer (and into fm, whose windows are in the same shell list) cannot be
//! timed out, so they run on a worker thread: a stuck window delays redirects, never the New
//! button. A decision that comes too late, or a window that moved on meanwhile, keeps Explorer.
use crate::{detect, util::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    os::windows::fs::MetadataExt,
    path::Path,
    sync::{
        Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN,
        System::{Com::*, Registry::*, Threading::*, Variant::VARIANT},
        UI::{Accessibility::*, Shell::{Common::ITEMIDLIST, *}, WindowsAndMessaging::*},
    },
    core::*,
};

const TICK: Duration = Duration::from_millis(100);
/// Polling stops this long after the window appeared; Explorer then stays.
const GIVE_UP: Duration = Duration::from_secs(3);
/// Never launch fm or close Explorer later than this (a call may have stalled since the last check).
const TOO_LATE: Duration = Duration::from_millis(3500);
/// Explorer applies `/select` once the folder is enumerated. Without a selection, decide only after
/// the item count has stopped changing for this long; longer while nothing is listed yet (a slow
/// share's first batch, or a truly empty folder).
const QUIET: Duration = Duration::from_millis(400);
const EMPTY_QUIET: Duration = Duration::from_secs(1);

const SHELL_TARGETS: [(GUID, &str); 3] = [
    (FOLDERID_ComputerFolder, "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"),
    (FOLDERID_RecycleBinFolder, "::{645FF040-5081-101B-9F08-00AA002F954E}"),
    (FOLDERID_NetworkFolder, "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}"),
];

/// A newly shown Explorer window, sent to the worker.
struct Job {
    hwnd: usize,
    since: Instant,
}

thread_local! {
    /// Every Explorer window seen so far (UI thread); pruned once they are gone.
    static SEEN: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
    static WORKER: RefCell<Option<Sender<Job>>> = const { RefCell::new(None) };
    /// The last two top-level foreground windows, newest first.
    static FOREGROUND: Cell<[usize; 2]> = const { Cell::new([0; 2]) };
}

/// Windows the worker has not decided yet (shared with it).
static UNDECIDED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn set_undecided(hwnd: HWND, on: bool) {
    if let Ok(mut v) = UNDECIDED.lock() {
        v.retain(|&h| h != hwnd.0 as usize);
        if on {
            v.push(hwnd.0 as usize);
        }
    }
}

fn is_undecided(hwnd: HWND) -> bool {
    UNDECIDED.lock().is_ok_and(|v| v.contains(&(hwnd.0 as usize)))
}

/// A panic must not unwind into a WinEvent callback or end the worker: that would abort the helper.
fn guard(f: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        log("explorer: watcher panicked");
    }
}

/// Watches windows being shown, globally and out of context; app.rs unhooks it on exit.
pub fn hook() -> HWINEVENTHOOK {
    unsafe { SetWinEventHook(EVENT_OBJECT_SHOW, EVENT_OBJECT_SHOW, None, Some(on_show), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS) }
}

/// Called by app.rs on every foreground change, before anything can drop the event.
pub fn foreground(hwnd: HWND) {
    let h = unsafe { GetAncestor(hwnd, GA_ROOT) }.0 as usize;
    FOREGROUND.with(|f| {
        let [newest, _] = f.get();
        if h != 0 && h != newest {
            f.set([h, newest]);
        }
    });
}

/// The window in front before `hwnd` is an Explorer window the user kept (still open and not
/// waiting to be replaced): they opened this one from it. An app's `explorer.exe` has the app there.
fn opened_from_kept_explorer(hwnd: HWND) -> bool {
    let before = FOREGROUND.with(Cell::get).into_iter().map(|h| HWND(h as *mut _)).find(|&h| h != hwnd && !h.is_invalid());
    before.is_some_and(|b| {
        unsafe { IsWindow(Some(b)) }.as_bool()
            && class_of(b) == "CabinetWClass"
            && !is_undecided(b)
            && detect::target(b).exe.eq_ignore_ascii_case("explorer.exe")
    })
}

/// Explorer windows already open when the watcher starts are the user's: never touch them.
pub fn spare_open_windows() {
    foreground(unsafe { GetForegroundWindow() }); // the first foreground event comes only later
    let mut after = None;
    while let Ok(h) = unsafe { FindWindowExW(None, after, w!("CabinetWClass"), None) } {
        first_sight(h);
        after = Some(h);
    }
}

/// Records `hwnd`; true only the first time.
fn first_sight(hwnd: HWND) -> bool {
    SEEN.with(|s| {
        let mut s = s.borrow_mut();
        s.retain(|&h| unsafe { IsWindow(Some(HWND(h as *mut _))).as_bool() });
        s.insert(hwnd.0 as usize)
    })
}

unsafe extern "system" fn on_show(_: HWINEVENTHOOK, _: u32, hwnd: HWND, id_object: i32, id_child: i32, _: u32, _: u32) {
    if id_object == OBJID_WINDOW.0 && id_child == CHILDID_SELF as i32 {
        guard(|| shown(hwnd));
    }
}

/// Decided at the first show, so fm's escape hatch is sampled while fm is opening the window.
fn shown(hwnd: HWND) {
    if class_of(hwnd) != "CabinetWClass" || !first_sight(hwnd) || redirect_target().is_none() {
        return;
    }
    let t = detect::target(hwnd);
    if !t.exe.eq_ignore_ascii_case("explorer.exe") || t.above_us {
        return;
    }
    if explorer_allowed() {
        return log("explorer: window allowed by fm");
    }
    if opened_from_kept_explorer(hwnd) {
        return log("explorer: opened from a kept Explorer window, left alone");
    }
    let job = Job { hwnd: hwnd.0 as usize, since: Instant::now() };
    set_undecided(hwnd, true);
    WORKER.with(|w| {
        let mut w = w.borrow_mut();
        if w.is_none() {
            let (tx, rx) = channel();
            if std::thread::Builder::new().name("explorer-watch".into()).spawn(move || worker(rx)).is_ok() {
                *w = Some(tx);
            }
        }
        if !w.as_ref().is_some_and(|tx| tx.send(job).is_ok()) {
            *w = None; // the next window starts a new worker
            set_undecided(hwnd, false);
            log("explorer: no worker, window left alone");
        }
    });
}

struct Pending {
    hwnd: HWND,
    since: Instant,
    count: i32,
    count_since: Instant,
}

/// Polls the waiting windows every 100 ms in its own multithreaded apartment (no message loop,
/// so nothing re-enters).
fn worker(jobs: Receiver<Job>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut pending: Vec<Pending> = Vec::new();
    let new = |j: Job| Pending { hwnd: HWND(j.hwnd as *mut _), since: j.since, count: i32::MIN, count_since: j.since };
    loop {
        if pending.is_empty() {
            let Ok(job) = jobs.recv() else { return };
            pending.push(new(job));
        }
        std::thread::sleep(TICK);
        pending.extend(jobs.try_iter().map(new));
        guard(|| {
            pending.retain_mut(|p| {
                let done = step(p);
                if done {
                    set_undecided(p.hwnd, false);
                }
                !done
            })
        });
    }
}

/// One poll of a waiting window; true once it is decided.
fn step(p: &mut Pending) -> bool {
    if !unsafe { IsWindow(Some(p.hwnd)) }.as_bool() {
        return true;
    }
    if p.since.elapsed() >= GIVE_UP {
        log("explorer: not settled within 3 s, left alone");
        return true;
    }
    if tabs(p.hwnd) > 1 {
        log("explorer: window has several tabs, left alone");
        return true;
    }
    // No folder view yet: don't search the shell list (each entry is a call into its owner).
    if child_count(p.hwnd, "SHELLDLL_DefView") == 0 {
        return false;
    }
    let Some((app, view)) = browser_of(p.hwnd).and_then(|b| folder_view(&b).map(|v| (b, v))) else {
        return false; // not navigated yet
    };
    // `/select` is applied once the folder is enumerated: take a selection as soon as it shows,
    // otherwise wait until the item count has been still for a while.
    let selection = first_selected(&view);
    let count = unsafe { view.ItemCount(SVGIO_ALLVIEW) }.unwrap_or(-1);
    if count != p.count {
        (p.count, p.count_since) = (count, Instant::now());
    }
    let quiet = if count > 0 { QUIET } else { EMPTY_QUIET };
    if selection.is_none() && p.count_since.elapsed() < quiet {
        return false;
    }
    let Some(folder) = folder_of(&view) else { return false };
    // An empty view of a directory with visible entries is still being listed; a /select may follow.
    if count <= 0 && selection.is_none() && matches!(&folder, Folder::Fs(d) if has_visible_entry(d)) {
        return false; // GIVE_UP then leaves it to Explorer
    }
    replace(p, &app, folder, selection);
    true
}

/// One ShellTabWindowClass child per tab: closing the window closes them all.
fn tabs(hwnd: HWND) -> usize {
    child_count(hwnd, "ShellTabWindowClass")
}

fn child_count(hwnd: HWND, class: &str) -> usize {
    descendants(hwnd).into_iter().filter(|&c| class_of(c) == class).count()
}

/// Hidden entries (desktop.ini) are not listed by default, so they do not count. On Windows the
/// entry metadata comes from the directory listing itself.
fn has_visible_entry(dir: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut it| it.any(|e| e.and_then(|e| e.metadata()).is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN.0 == 0)))
}

#[derive(Debug, PartialEq)]
enum Folder {
    Fs(String),
    Shell(&'static str),
    Other,
}

/// Launches fm first, then closes Explorer if the window is still on time and on the same place.
fn replace(p: &Pending, app: &IWebBrowserApp, folder: Folder, selection: Option<String>) {
    let Some(fm) = redirect_target() else { return };
    let cmd = match &folder {
        Folder::Fs(dir) => command_line(&fm, dir, selection.as_deref()),
        Folder::Shell(target) => command_line(&fm, target, None),
        Folder::Other => return log("explorer: not a file-system folder, left alone"),
    };
    // Before the deadline check, so the ping counts against it.
    if fm_unresponsive() {
        return log("explorer: fm is not responding, Explorer stays");
    }
    if p.since.elapsed() >= TOO_LATE {
        return log("explorer: decided too late, left alone");
    }
    if let Err(e) = launch(&fm, &cmd) {
        return log(format!("explorer: starting fm failed ({e}), Explorer stays"));
    }
    // No paths in the log: it would keep a record of every file revealed with "Show in folder".
    log(format!("explorer: started fm (select={})", selection.is_some()));
    if std::env::var_os("FM_DIALOGKIT_TRACE").is_some() {
        log(format!("explorer: {cmd}"));
    }
    let unchanged = unsafe { IsWindow(Some(p.hwnd)) }.as_bool()
        && tabs(p.hwnd) <= 1
        && folder_view(app).and_then(|v| folder_of(&v)).as_ref() == Some(&folder);
    if !unchanged || p.since.elapsed() >= TOO_LATE {
        return log("explorer: window changed meanwhile (navigated, new tab or late), Explorer stays");
    }
    match unsafe { app.Quit() } {
        Ok(()) => log(format!("explorer: replaced after {} ms ({} items)", p.since.elapsed().as_millis(), p.count)),
        Err(e) => log(format!("explorer: closing the window failed: {e}")),
    }
}

/// The resident fm takes a hand-over on its door thread even while its UI thread is stuck (a
/// dropped share), and would show nothing until it recovers: then Explorer stays. Its top-level
/// FmHub lives on that UI thread; FindWindowExW(None, ..) skips the message-only FmHub every fm
/// process has. No resident fm (a cold start) is no reason to wait. Needs a real fm to test.
fn fm_unresponsive() -> bool {
    unsafe {
        let Ok(hub) = FindWindowExW(None, None, w!("FmHub"), None) else { return false };
        SendMessageTimeoutW(hub, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 300, None).0 == 0
    }
}

fn launch(fm: &str, cmd: &str) -> Result<()> {
    let app = wide(fm);
    let mut cmd = wide(cmd);
    let si = STARTUPINFOW { cb: std::mem::size_of::<STARTUPINFOW>() as u32, ..Default::default() };
    let mut pi = PROCESS_INFORMATION::default();
    unsafe {
        // May be refused (we are not in the foreground); then fm only flashes on the taskbar.
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        CreateProcessW(PCWSTR(app.as_ptr()), Some(PWSTR(cmd.as_mut_ptr())), None, None, false, PROCESS_CREATION_FLAGS(0), None, PCWSTR::null(), &si, &mut pi)?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(())
}

/// `"<fm>" /select,"<file>"` or `"<fm>" "<target>"`, parsed back by CommandLineToArgvW rules.
pub fn command_line(fm: &str, target: &str, select: Option<&str>) -> String {
    match select {
        Some(file) => format!("{} /select,{}", quote(fm), quote(file)),
        None => format!("{} {}", quote(fm), quote(target)),
    }
}

/// Backslashes before the closing quote are doubled, so the drive root `C:\` is passed as `"C:\\"`;
/// a bare `"C:\"` would read as `C:"`. Windows paths cannot contain `"`, so nothing else is escaped.
fn quote(arg: &str) -> String {
    let trailing = arg.len() - arg.trim_end_matches('\\').len();
    format!("\"{arg}{}\"", "\\".repeat(trailing))
}

/// fm.exe to hand windows to: HKCU\Software\fm "Redirect", only while that file exists. Read on
/// every decision because scripts add and remove it. Debug builds take `FM_REDIRECT_TEST` instead,
/// so the watcher can be tried without touching the registry.
fn redirect_target() -> Option<String> {
    let test = if cfg!(debug_assertions) { std::env::var("FM_REDIRECT_TEST").ok() } else { None };
    let path = test.or_else(registry_redirect)?;
    Path::new(&path).is_file().then_some(path)
}

fn registry_redirect() -> Option<String> {
    let key = wide(r"Software\fm");
    let name = wide("Redirect");
    let mut buf = [0u16; 1024];
    let mut size = std::mem::size_of_val(&buf) as u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size)) };
    if r.is_err() {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n])).filter(|s| !s.is_empty())
}

/// fm signals this event for a few seconds when the user explicitly asks for Explorer.
fn explorer_allowed() -> bool {
    unsafe {
        let Ok(event) = OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("Local\\fm.explorer-allow")) else { return false };
        let signaled = WaitForSingleObject(event, 0) == WAIT_OBJECT_0;
        let _ = CloseHandle(event);
        signaled
    }
}

/// The window's browser object from the shell's window list. Each entry's HWND() is a call into
/// its owner (another Explorer window, or fm), so the newest entries go first and the search stops
/// at the match; only the matching CabinetWClass window gets any further call.
fn browser_of(hwnd: HWND) -> Option<IWebBrowserApp> {
    unsafe {
        let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        (0..windows.Count().ok()?)
            .rev()
            .filter_map(|i| windows.Item(&VARIANT::from(i)).ok()?.cast::<IWebBrowserApp>().ok())
            .find(|b| b.HWND().is_ok_and(|h| h.0 == hwnd.0 as isize))
    }
}

/// None until the window has navigated and created its view.
fn folder_view(app: &IWebBrowserApp) -> Option<IFolderView> {
    unsafe {
        let browser: IShellBrowser = app.cast::<IServiceProvider>().ok()?.QueryService(&SID_STopLevelBrowser).ok()?;
        browser.QueryActiveShellView().ok()?.cast().ok()
    }
}

fn folder_of(view: &IFolderView) -> Option<Folder> {
    unsafe {
        let persist = view.GetFolder::<IPersistFolder2>().ok()?;
        let pidl = persist.GetCurFolder().ok()?;
        if pidl.is_null() {
            return None;
        }
        let class = persist.GetClassID().unwrap_or_default();
        if std::env::var_os("FM_DIALOGKIT_TRACE").is_some() {
            log(format!("explorer: folder class {class:?}"));
        }
        let folder = classify(pidl, &class);
        CoTaskMemFree(Some(pidl as *const _));
        Some(folder)
    }
}

/// What fm should open for an absolute PIDL whose folder object has class `class`.
fn classify(pidl: *const ITEMIDLIST, class: &GUID) -> Folder {
    // SHGetNameFromIDList is SHGetPathFromIDListW without the MAX_PATH limit; a .zip "folder" has a
    // file path, so require a real directory.
    let dir = unsafe { SHGetNameFromIDList(pidl, SIGDN_FILESYSPATH) }.ok().and_then(take).filter(|d| Path::new(d).is_dir());
    match dir {
        // The plain file-system folder, or a known folder's own view of its directory (the Desktop
        // root, the profile). Any other class on a directory shows something else: Fonts, God Mode,
        // desktop.ini junctions. Fonts is a known folder, so it is excluded by name.
        Some(dir) if *class == ShellFSFolder || (is_known_folder(pidl) && !is_known(pidl, &FOLDERID_Fonts)) => Folder::Fs(dir),
        Some(_) => Folder::Other,
        None => SHELL_TARGETS.iter().find(|(id, _)| is_known(pidl, id)).map_or(Folder::Other, |(_, t)| Folder::Shell(t)),
    }
}

fn is_known_folder(pidl: *const ITEMIDLIST) -> bool {
    unsafe { CoCreateInstance::<_, IKnownFolderManager>(&KnownFolderManager, None, CLSCTX_INPROC_SERVER).is_ok_and(|m| m.FindFolderFromIDList(pidl).is_ok()) }
}

fn is_known(pidl: *const ITEMIDLIST, id: &GUID) -> bool {
    unsafe {
        SHGetKnownFolderIDList(id, 0, None).is_ok_and(|known| {
            let same = ILIsEqual(pidl, known).as_bool();
            CoTaskMemFree(Some(known as *const _));
            same
        })
    }
}

fn first_selected(view: &IFolderView) -> Option<String> {
    unsafe {
        let items: IShellItemArray = view.Items(SVGIO_SELECTION).ok()?;
        if items.GetCount().ok()? == 0 {
            return None;
        }
        take(items.GetItemAt(0).ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?)
    }
}

/// Copies and frees a CoTaskMem string.
fn take(p: PWSTR) -> Option<String> {
    unsafe {
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FM: &str = r"C:\Program Files\fm\fm.exe";

    /// Parses like the receiving process does (CommandLineToArgvW = MSVC argv rules).
    fn argv(cmd: &str) -> Vec<String> {
        let w = wide(cmd);
        let mut n = 0;
        unsafe {
            let a = CommandLineToArgvW(PCWSTR(w.as_ptr()), &mut n);
            assert!(!a.is_null());
            let out = (0..n as usize).map(|i| (*a.add(i)).to_string().unwrap()).collect();
            let _ = LocalFree(Some(HLOCAL(a as *mut _)));
            out
        }
    }

    #[test]
    fn folder_with_spaces() {
        let cmd = command_line(FM, r"C:\Users\Jan Novák\Stažené soubory", None);
        assert_eq!(cmd, r#""C:\Program Files\fm\fm.exe" "C:\Users\Jan Novák\Stažené soubory""#);
        assert_eq!(argv(&cmd), [FM, r"C:\Users\Jan Novák\Stažené soubory"]);
    }

    #[test]
    fn drive_root_keeps_its_backslash() {
        let cmd = command_line(FM, r"C:\", None);
        assert_eq!(cmd, r#""C:\Program Files\fm\fm.exe" "C:\\""#);
        assert_eq!(argv(&cmd), [FM, r"C:\"]);
        assert_eq!(argv(&command_line(FM, r"\\server\share\", None)), [FM, r"\\server\share\"]);
    }

    #[test]
    fn select_file() {
        let cmd = command_line(FM, r"C:\a b", Some(r"C:\a b\c d.txt"));
        assert_eq!(cmd, r#""C:\Program Files\fm\fm.exe" /select,"C:\a b\c d.txt""#);
        assert_eq!(argv(&cmd), [FM, r"/select,C:\a b\c d.txt"]);
        assert_eq!(argv(&command_line(FM, r"C:\", Some(r"D:\"))), [FM, r"/select,D:\"]);
    }

    #[test]
    fn shell_target_is_one_argument() {
        let cmd = command_line(FM, SHELL_TARGETS[0].1, None);
        assert_eq!(argv(&cmd), [FM, "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"]);
    }

    #[test]
    fn hidden_entries_do_not_count() {
        use windows::Win32::Storage::FileSystem::SetFileAttributesW;
        let d = std::env::temp_dir().join(format!("fm-test-visible-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let ds = d.to_string_lossy().into_owned();
        assert!(!has_visible_entry(&ds));
        let ini = d.join("desktop.ini");
        std::fs::write(&ini, b"x").unwrap();
        unsafe { SetFileAttributesW(PCWSTR(wide(&ini.to_string_lossy()).as_ptr()), FILE_ATTRIBUTE_HIDDEN).unwrap() };
        assert!(!has_visible_entry(&ds));
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        assert!(has_visible_entry(&ds));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn classifies_places() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // With the class of the folder object the shell binds for the place, as Explorer's view has.
        let of = |name: &str| unsafe {
            let w = wide(name);
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None).unwrap();
            let class = SHBindToObject(None, pidl, None).and_then(|p: IPersist| p.GetClassID()).unwrap_or_default();
            let f = classify(pidl, &class);
            CoTaskMemFree(Some(pidl as *const _));
            f
        };
        let windows = std::env::var("WINDIR").unwrap();
        assert!(matches!(of(&windows), Folder::Fs(p) if p.eq_ignore_ascii_case(&windows)));
        assert_eq!(of(r"C:\"), Folder::Fs(r"C:\".into()));
        // Known folders whose folder class is not the plain one but which show the directory.
        let profile = std::env::var("USERPROFILE").unwrap();
        assert!(matches!(of(&profile), Folder::Fs(p) if p.eq_ignore_ascii_case(&profile)));
        assert!(matches!(of("shell:Desktop"), Folder::Fs(_)));
        // A God Mode junction shows All Tasks, not the (empty) directory.
        let god = std::env::temp_dir().join(format!("fm-test-{}.{{ED7BA470-8E54-465E-825C-99712043E01C}}", std::process::id()));
        std::fs::create_dir_all(&god).unwrap();
        let god_kind = of(&god.to_string_lossy());
        std::fs::remove_dir(&god).unwrap();
        assert_eq!(god_kind, Folder::Other);
        // Fonts is a directory with its own view; Control Panel has no path.
        assert_eq!(of(&format!(r"{windows}\Fonts")), Folder::Other);
        assert_eq!(of("shell:Fonts"), Folder::Other);
        assert_eq!(of("::{26EE0668-A00A-44D7-9371-BEB064C98683}"), Folder::Other);
        // A file is not a folder to open.
        assert_eq!(of(&format!(r"{windows}\explorer.exe")), Folder::Other);
        for (_, target) in SHELL_TARGETS {
            assert_eq!(of(target), Folder::Shell(target), "{target}");
        }
    }
}
