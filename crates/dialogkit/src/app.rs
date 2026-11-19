//! Run mode: watches the foreground window, shows the button on file dialogs and runs the New menu.
//!
//! Re-entrancy rule: no `STATE` borrow is ever held across a call that can pump messages
//! (TrackPopupMenu, the hook round-trip, shell invokes). WinEvent callbacks arriving meanwhile only
//! mark the state dirty and are re-evaluated afterwards.
use crate::{detect, detect::Kind, inject::Injector, newmenu, newmenu::NewMenu, pill, tray, util::*};
use fm_dialog_hook::{CMD_QUERY, CMD_SELECT, FolderReply, parse_query_reply};
use std::{cell::RefCell, collections::HashSet, path::Path, time::Duration};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, DataExchange::*, LibraryLoader::GetModuleHandleW, Threading::*},
        UI::{Accessibility::*, HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::*,
};

const HOTKEY_ID: i32 = 0x464D;
const TIMER_CLASSIFY: usize = 1;
const TIMER_REANCHOR: usize = 2;
const TIMER_SELECT: usize = 3;
/// Posted by the x86 worker thread with a `Box<String>` reply.
const WM_APP_X86_REPLY: u32 = WM_APP + 3;
/// Budget for reading the folder before the menu opens; a hung dialog falls back to the address bar.
const QUERY_TIMEOUT: Duration = Duration::from_millis(600);
/// Longest default name the New menu produces ("Nová komprimovaná složka (metoda ZIP) (99).zip" ≈ 46).
const NAME_RESERVE: usize = 48;

#[derive(Clone)]
struct Tracked {
    hwnd: HWND,
    kind: Kind,
    target: detect::Target,
    rect: RECT,
}

#[derive(Default)]
struct State {
    main: HWND,
    tracked: Option<Tracked>,
    candidate: Option<(HWND, u32)>,
    paused: bool,
    busy: bool,
    dirty: bool,
    hotkey: bool,
    warned: HashSet<u32>,
    taskbar_created: u32,
    /// WinEvent hooks scoped to the tracked dialog's thread (moves, minimize, destroy).
    dialog_hooks: Vec<HWINEVENTHOOK>,
    pending_select: Option<PendingSelect>,
}

struct PendingSelect {
    hook: Option<HHOOK>,
    name: String,
    exe: String,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static INJECTOR: RefCell<Option<Injector>> = const { RefCell::new(None) };
    static UIA: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
    static ACTIVE_MENU: RefCell<Option<NewMenu>> = const { RefCell::new(None) };
    static ATOMS: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

pub fn run() -> i32 {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mutex = CreateMutexW(None, true, w!("Local\\fm.dialogkit"));
        if mutex.is_err() || GetLastError() == ERROR_ALREADY_EXISTS {
            return 0; // already running
        }
        if CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_err() {
            return 1;
        }
        let Ok(hinst) = GetModuleHandleW(None) else { return 1 };
        let class = wide("FmDialogKitMain");
        let wc = WNDCLASSW { lpfnWndProc: Some(main_proc), hInstance: hinst.into(), lpszClassName: PCWSTR(class.as_ptr()), ..Default::default() };
        RegisterClassW(&wc);
        let Ok(main) = CreateWindowExW(WS_EX_TOOLWINDOW, PCWSTR(class.as_ptr()), w!("fm-dialogkit"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst.into()), None) else {
            return 1;
        };
        // UIPI: let the hook DLL inside lower-or-equal processes reply to us.
        let _ = ChangeWindowMessageFilterEx(main, WM_COPYDATA, MSGFLT_ALLOW, None);
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        with(|s| {
            s.main = main;
            s.taskbar_created = taskbar_created;
        });
        match Injector::new(main) {
            Ok(i) => INJECTOR.with(|x| *x.borrow_mut() = Some(i)),
            Err(e) => log(format!("injector unavailable: {e}")),
        }
        UIA.with(|u| *u.borrow_mut() = detect::uia());
        if let Err(e) = pill::create(main) {
            log(format!("pill: {e}"));
            return 1;
        }
        tray::add(main, false);
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        // Only the foreground hook is global; the rest are scoped to the tracked dialog in attach().
        let hooks = [SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_event), 0, 0, flags)];
        log("started");
        on_foreground(GetForegroundWindow());
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for h in hooks {
            let _ = UnhookWinEvent(h);
        }
        tray::remove(main);
        log("stopped");
        0
    }
}

unsafe extern "system" fn on_event(_: HWINEVENTHOOK, event: u32, hwnd: HWND, id_object: i32, id_child: i32, _: u32, _: u32) {
    if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    let busy = with(|s| {
        if s.busy {
            s.dirty = true;
        }
        s.busy
    });
    if busy {
        return;
    }
    let tracked = with(|s| s.tracked.as_ref().map(|t| t.hwnd));
    match event {
        EVENT_SYSTEM_FOREGROUND => on_foreground(hwnd),
        EVENT_OBJECT_LOCATIONCHANGE if tracked == Some(hwnd) => on_moved(hwnd),
        EVENT_SYSTEM_MINIMIZESTART if tracked == Some(hwnd) => pill::hide(),
        EVENT_SYSTEM_MINIMIZEEND if tracked == Some(hwnd) => reanchor(),
        EVENT_OBJECT_DESTROY | EVENT_OBJECT_HIDE if tracked == Some(hwnd) => detach(),
        _ => {}
    }
}

fn is_ours(hwnd: HWND) -> bool {
    hwnd == with(|s| s.main) || Some(hwnd) == pill::hwnd()
}

fn on_foreground(hwnd: HWND) {
    if std::env::var_os("FM_DIALOGKIT_TRACE").is_some() {
        log(format!("foreground {:?} class={}", hwnd.0, class_of(hwnd)));
    }
    if with(|s| s.paused) || hwnd.is_invalid() {
        return detach();
    }
    if is_ours(hwnd) {
        return;
    }
    if with(|s| s.tracked.as_ref().is_some_and(|t| t.hwnd == hwnd)) {
        return reanchor();
    }
    match detect::classify(hwnd) {
        Some(kind) => attach(hwnd, kind),
        None if detect::is_dialog_shell(hwnd) => {
            // Dialog children appear shortly after the window becomes foreground.
            detach();
            let main = with(|s| {
                s.candidate = Some((hwnd, 0));
                s.main
            });
            unsafe { SetTimer(Some(main), TIMER_CLASSIFY, 100, None) };
        }
        None => detach(),
    }
}

fn attach(hwnd: HWND, kind: Kind) {
    let target = detect::target(hwnd);
    log(format!("attach {:?} dialog in {} (pid {}, 32-bit={}, above={})", kind, target.exe, target.pid, target.is32, target.above_us));
    if target.above_us {
        let first = with(|s| s.warned.insert(target.pid));
        if first {
            log(format!("skip elevated dialog in {} (pid {})", target.exe, target.pid));
        }
        return detach();
    }
    let mut rect = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rect);
    }
    unhook_dialog();
    let (pid, tid) = (target.pid, unsafe { GetWindowThreadProcessId(hwnd, None) });
    let flags = WINEVENT_OUTOFCONTEXT;
    let hooks = unsafe {
        vec![
            SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, None, Some(on_event), pid, tid, flags),
            SetWinEventHook(EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, None, Some(on_event), pid, tid, flags),
            SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, None, Some(on_event), pid, tid, flags),
        ]
    };
    with(|s| {
        s.tracked = Some(Tracked { hwnd, kind, target, rect });
        s.dialog_hooks = hooks;
    });
    set_hotkey(true);
    reanchor();
}

fn unhook_dialog() {
    for h in with(|s| std::mem::take(&mut s.dialog_hooks)) {
        unsafe {
            let _ = UnhookWinEvent(h);
        }
    }
}

fn detach() {
    unhook_dialog();
    with(|s| {
        s.tracked = None;
        s.candidate = None;
    });
    set_hotkey(false);
    pill::hide();
}

fn set_hotkey(on: bool) {
    let (main, has) = with(|s| (s.main, s.hotkey));
    unsafe {
        if on && !has {
            // Ctrl+N: "new" – free inside file dialogs. (Ctrl+Alt would be AltGr on Czech keyboards.)
            let ok = RegisterHotKey(Some(main), HOTKEY_ID, MOD_CONTROL | MOD_NOREPEAT, u32::from(b'N')).is_ok();
            with(|s| s.hotkey = ok);
        } else if !on && has {
            let _ = UnregisterHotKey(Some(main), HOTKEY_ID);
            with(|s| s.hotkey = false);
        }
    }
}

fn style_for(kind: Kind) -> pill::Style {
    if high_contrast() {
        pill::Style::HighContrast
    } else if kind == Kind::Legacy {
        pill::Style::Classic
    } else if apps_use_dark_theme() {
        pill::Style::Dark
    } else {
        pill::Style::Light
    }
}

fn reanchor() {
    let Some(t) = with(|s| s.tracked.clone()) else { return pill::hide() };
    unsafe {
        if IsIconic(t.hwnd).as_bool() || !IsWindowVisible(t.hwnd).as_bool() {
            return pill::hide();
        }
    }
    let anchor = UIA.with(|u| u.borrow().as_ref().and_then(|a| detect::anchor(t.hwnd, t.kind, a)));
    let Some(anchor) = anchor else {
        log(format!("no anchor for {:?} dialog in {}", t.kind, t.target.exe));
        return pill::hide();
    };
    pill::place(anchor, style_for(t.kind), monitor_scale(t.hwnd));
}

/// Moves follow the dialog immediately; a resize re-reads the command bar layout once it settles.
fn on_moved(hwnd: HWND) {
    let mut now = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut now);
    }
    let (old, main) = with(|s| {
        let old = s.tracked.as_ref().map(|t| t.rect);
        if let Some(t) = s.tracked.as_mut() {
            t.rect = now;
        }
        (old, s.main)
    });
    let Some(old) = old else { return };
    if rect_w(&old) == rect_w(&now) && rect_h(&old) == rect_h(&now) {
        if let (Some(p), Some(r)) = (pill::hwnd(), pill::rect()) {
            let (dx, dy) = (now.left - old.left, now.top - old.top);
            unsafe {
                let _ = SetWindowPos(p, None, r.left + dx, r.top + dy, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
            pill::shift(dx, dy);
        }
    } else {
        pill::hide();
        unsafe { SetTimer(Some(main), TIMER_REANCHOR, 60, None) };
    }
}

fn on_timer(id: usize) {
    let main = with(|s| s.main);
    unsafe {
        let _ = KillTimer(Some(main), id);
    }
    match id {
        TIMER_REANCHOR => reanchor(),
        TIMER_SELECT => finish_select(Err("timeout".into())),
        TIMER_CLASSIFY => {
            let Some((hwnd, tries)) = with(|s| s.candidate) else { return };
            if unsafe { GetForegroundWindow() } != hwnd {
                return;
            }
            if let Some(kind) = detect::classify(hwnd) {
                with(|s| s.candidate = None);
                attach(hwnd, kind);
            } else if tries < 5 {
                with(|s| s.candidate = Some((hwnd, tries + 1)));
                unsafe { SetTimer(Some(main), TIMER_CLASSIFY, 120, None) };
            }
        }
        _ => {}
    }
}

fn query_folder(t: &Tracked) -> std::result::Result<FolderReply, String> {
    let reply = INJECTOR.with(|i| {
        i.borrow().as_ref().map(|i| i.call(t.hwnd, t.target.is32, CMD_QUERY as isize, QUERY_TIMEOUT))
    });
    match reply.unwrap_or_else(|| Err("injector unavailable".into())).and_then(|r| parse_query_reply(&r)) {
        Ok(f) => Ok(f),
        Err(e) => {
            log(format!("hook query failed in {}: {e}", t.target.exe));
            detect::address_bar_path(t.hwnd).map(|p| FolderReply { parsing_name: p.clone(), fs_path: Some(p) }).ok_or(e)
        }
    }
}

/// Starts select-and-rename without blocking; the result comes back via WM_COPYDATA / x86 reply.
fn start_select(t: &Tracked, name: &str) {
    let atom = unsafe { GlobalAddAtomW(PCWSTR(wide(name).as_ptr())) };
    if atom == 0 {
        return;
    }
    let lparam = CMD_SELECT as isize | ((atom as isize) << 8);
    let main = with(|s| s.main);
    let started = INJECTOR.with(|i| {
        let i = i.borrow();
        let i = i.as_ref().ok_or("injector unavailable".to_string())?;
        if t.target.is32 != cfg!(target_pointer_width = "32") {
            i.start_other_bitness(t.hwnd, lparam, Duration::from_millis(3000), main, WM_APP_X86_REPLY).map(|_| None)
        } else {
            i.start(t.hwnd, lparam).map(Some)
        }
    });
    match started {
        Ok(hook) => {
            with(|s| s.pending_select = Some(PendingSelect { hook, name: name.to_string(), exe: t.target.exe.clone() }));
            unsafe { SetTimer(Some(main), TIMER_SELECT, 4000, None) };
        }
        Err(e) => log(format!("select start failed: {e}")),
    }
    // The DLL copies the name out of the atom when the request is handled; keep it until then.
    ATOMS.with(|a| a.borrow_mut().push(atom));
}

fn finish_select(reply: std::result::Result<String, String>) {
    let main = with(|s| s.main);
    unsafe {
        let _ = KillTimer(Some(main), TIMER_SELECT);
    }
    let Some(p) = with(|s| s.pending_select.take()) else { return };
    if let Some(h) = p.hook {
        unsafe {
            let _ = UnhookWindowsHookEx(h);
        }
    }
    for atom in ATOMS.with(|a| std::mem::take(&mut *a.borrow_mut())) {
        unsafe { GlobalDeleteAtom(atom) };
    }
    match reply {
        Ok(r) if r == "OK" => {}
        other => {
            log(format!("select '{}' failed in {}: {other:?}", p.name, p.exe));
            balloon(&format!("Vytvořeno: {}", p.name), "Dialog položku nezobrazuje – nejspíš ji skrývá filtr typů souborů.");
        }
    }
}

#[allow(dead_code)] // used by --act, which waits for the result
fn select_in_dialog(t: &Tracked, name: &str) -> std::result::Result<(), String> {
    let atom = unsafe { GlobalAddAtomW(PCWSTR(wide(name).as_ptr())) };
    if atom == 0 {
        return Err("atom".into());
    }
    let lparam = CMD_SELECT as isize | ((atom as isize) << 8);
    let reply = INJECTOR.with(|i| i.borrow().as_ref().map(|i| i.call(t.hwnd, t.target.is32, lparam, Duration::from_millis(4000))));
    unsafe { GlobalDeleteAtom(atom) };
    match reply.unwrap_or_else(|| Err("injector unavailable".into())) {
        Ok(r) if r == "OK" => Ok(()),
        Ok(r) => Err(r),
        Err(e) => Err(e),
    }
}

fn info_menu(text: &str) -> Option<HMENU> {
    unsafe {
        let m = CreatePopupMenu().ok()?;
        let _ = AppendMenuW(m, MF_STRING | MF_GRAYED, 1, PCWSTR(wide(text).as_ptr()));
        Some(m)
    }
}

fn open_menu() {
    if with(|s| s.pending_select.is_some()) {
        return; // previous item is still being selected
    }
    let Some(t) = with(|s| s.tracked.clone()) else { return };
    let main = with(|s| {
        s.busy = true;
        s.main
    });
    pill::set_pressed(true);
    let folder = query_folder(&t);
    let fs_dir = folder.as_ref().ok().and_then(|f| f.fs_path.clone());
    // Most apps cannot open paths of 260+ characters, so refuse rather than create such an item.
    let too_long = fs_dir.as_deref().is_some_and(|d| d.encode_utf16().count() + 1 + NAME_RESERVE >= 260);
    let menu = if too_long { None } else { fs_dir.as_deref().map(NewMenu::for_folder) };
    let (popup, info) = match &menu {
        Some(Ok(m)) => (m.popup, None),
        Some(Err(e)) => {
            log(format!("New menu failed for {fs_dir:?}: {e}"));
            (HMENU::default(), info_menu("Tady nelze nic vytvořit"))
        }
        None if too_long => (HMENU::default(), info_menu("Cesta ke složce je příliš dlouhá")),
        None if folder.is_ok() => (HMENU::default(), info_menu("Ve virtuální složce nelze vytvářet")),
        None => (HMENU::default(), info_menu("Složku dialogu se nepodařilo zjistit")),
    };
    ACTIVE_MENU.with(|m| *m.borrow_mut() = menu.and_then(|r| r.ok()));
    let shown = info.unwrap_or(popup);
    let cmd = unsafe {
        let r = pill::rect().unwrap_or_default();
        let _ = SetForegroundWindow(main);
        let params = TPMPARAMS { cbSize: std::mem::size_of::<TPMPARAMS>() as u32, rcExclude: r };
        let cmd = TrackPopupMenuEx(shown, (TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_VERTICAL).0, r.left, r.bottom, main, Some(&params));
        let _ = PostMessageW(Some(main), WM_NULL, WPARAM(0), LPARAM(0));
        if let Some(i) = info {
            let _ = DestroyMenu(i);
        }
        cmd.0 as u32
    };
    pill::set_pressed(false);
    unsafe {
        let _ = SetForegroundWindow(t.hwnd);
    }
    if cmd != 0 && info.is_none() {
        if let Some(dir) = fs_dir.as_deref() {
            create_and_select(&t, Path::new(dir), cmd, main);
        }
    }
    ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
    let dirty = with(|s| {
        s.busy = false;
        std::mem::take(&mut s.dirty)
    });
    if dirty {
        on_foreground(unsafe { GetForegroundWindow() });
    }
}

fn create_and_select(t: &Tracked, dir: &Path, cmd: u32, owner: HWND) {
    let before = newmenu::names(dir);
    let invoked = ACTIVE_MENU.with(|m| m.borrow().as_ref().map(|m| m.invoke(cmd, owner)));
    if let Some(Err(e)) = invoked {
        log(format!("invoke failed: {e}"));
        return balloon("Nepodařilo se vytvořit položku", &e.message());
    }
    let Some(name) = newmenu::created_since(dir, &before) else {
        return; // e.g. the shortcut wizard: nothing new yet
    };
    start_select(t, &name);
}

fn balloon(title: &str, text: &str) {
    tray::balloon(with(|s| s.main), title, text);
}

unsafe extern "system" fn main_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match m {
            WM_COPYDATA if crate::inject::on_copydata(l) => {
                // Synchronous waits take the reply themselves; anything left is an async select reply.
                if with(|s| s.pending_select.is_some()) {
                    if let Some(r) = crate::inject::take_reply() {
                        finish_select(Ok(r));
                    }
                }
                return LRESULT(1);
            }
            WM_APP_X86_REPLY => {
                let text = *Box::from_raw(l.0 as *mut String);
                finish_select(Ok(text));
                return LRESULT(0);
            }
            WM_INITMENUPOPUP | WM_MEASUREITEM | WM_DRAWITEM | WM_MENUCHAR => {
                if let Some(r) = ACTIVE_MENU.with(|x| x.borrow().as_ref().and_then(|x| x.handle_menu_message(m, w, l))) {
                    return r;
                }
            }
            WM_HOTKEY if w.0 as i32 == HOTKEY_ID => {
                open_menu();
                return LRESULT(0);
            }
            pill::WM_APP_PILL_PRESSED => {
                open_menu();
                return LRESULT(0);
            }
            WM_TIMER => {
                on_timer(w.0);
                return LRESULT(0);
            }
            tray::WM_APP_TRAY => {
                let event = (l.0 & 0xffff) as u32;
                if event == WM_RBUTTONUP || event == WM_LBUTTONUP || event == WM_CONTEXTMENU {
                    let paused = with(|s| s.paused);
                    match tray::menu(h, paused) {
                        tray::CMD_PAUSE => {
                            with(|s| s.paused = !paused);
                            tray::set_paused(h, !paused);
                            on_foreground(GetForegroundWindow());
                        }
                        tray::CMD_QUIT => PostQuitMessage(0),
                        _ => {}
                    }
                }
                return LRESULT(0);
            }
            _ if m != 0 && m == with(|s| s.taskbar_created) => {
                tray::add(h, with(|s| s.paused));
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(h, m, w, l)
    }
}

/// `--act <query|list|new <index>> --hwnd <H>`: scripted access for tests; prints one line.
pub fn act_cli(args: &[String]) -> i32 {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let Some(hwnd) = get("--hwnd").and_then(|s| s.parse::<usize>().ok()).map(|v| HWND(v as *mut _)) else {
        println!("ERR\tusage: --act <query|list|new N|anchor> --hwnd H");
        return 2;
    };
    let Some(kind) = detect::classify(hwnd) else {
        println!("ERR\tnot a file dialog");
        return 1;
    };
    let target = detect::target(hwnd);
    let t = Tracked { hwnd, kind, target, rect: RECT::default() };
    let sink = unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = wide("FmDialogKitActSink");
        let wc = WNDCLASSW { lpfnWndProc: Some(main_proc), hInstance: hinst.into(), lpszClassName: PCWSTR(class.as_ptr()), ..Default::default() };
        RegisterClassW(&wc);
        CreateWindowExW(Default::default(), PCWSTR(class.as_ptr()), w!(""), Default::default(), 0, 0, 0, 0, Some(HWND_MESSAGE), None, Some(hinst.into()), None).unwrap_or_default()
    };
    match Injector::new(sink) {
        Ok(i) => INJECTOR.with(|x| *x.borrow_mut() = Some(i)),
        Err(e) => {
            println!("ERR\t{e}");
            return 1;
        }
    }
    let start = std::time::Instant::now();
    let folder = query_folder(&t);
    match args.first().map(String::as_str) {
        Some("query") => match folder {
            Ok(f) => {
                println!("OK	{}	{}	{} ms", f.fs_path.unwrap_or_default(), f.parsing_name, start.elapsed().as_millis());
                0
            }
            Err(e) => {
                println!("ERR	{e}");
                1
            }
        },
        Some("preview") => preview(hwnd, kind, get("--out").unwrap_or_else(|| "preview.bmp".into())),
        Some("anchor") => {
            let a = detect::uia().and_then(|u| detect::anchor(hwnd, kind, &u));
            println!("{kind:?} {a:?} window_dpi={} monitor_scale={}", unsafe { GetDpiForWindow(hwnd) }, monitor_scale(hwnd));
            0
        }
        Some(cmd @ ("list" | "new")) => {
            let Some(dir) = folder.ok().and_then(|f| f.fs_path) else {
                println!("ERR\tno filesystem folder");
                return 1;
            };
            let menu = match NewMenu::for_folder(&dir) {
                Ok(m) => m,
                Err(e) => {
                    println!("ERR\t{e}");
                    return 1;
                }
            };
            let items = menu_items(menu.popup);
            if cmd == "list" {
                for (i, (id, text)) in items.iter().enumerate() {
                    println!("{i}\t{id}\t{text}");
                }
                return 0;
            }
            let Some((id, text)) = args.get(1).and_then(|s| s.parse::<usize>().ok()).and_then(|i| items.get(i)) else {
                println!("ERR\tbad index");
                return 2;
            };
            ACTIVE_MENU.with(|m| *m.borrow_mut() = Some(menu));
            let before = newmenu::names(Path::new(&dir));
            let invoked = ACTIVE_MENU.with(|m| m.borrow().as_ref().map(|m| m.invoke(*id, sink)));
            let created = newmenu::created_since(Path::new(&dir), &before);
            let selected = created.as_deref().map(|n| select_in_dialog(&t, n));
            println!("{text}\tinvoke={invoked:?}\tcreated={created:?}\tselected={selected:?}\t{} ms", start.elapsed().as_millis());
            0
        }
        _ => {
            println!("ERR\tunknown action");
            2
        }
    }
}

/// Renders the dialog (PrintWindow) with the button composited at its real anchor, normal and
/// hovered side by side, into a 32-bit BMP. Needs no foreground and captures nothing else on screen.
fn preview(hwnd: HWND, kind: Kind, out: String) -> i32 {
    use windows::Win32::{Graphics::Gdi::*, Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow}};
    let main = unsafe { CreateWindowExW(WS_EX_TOOLWINDOW, w!("STATIC"), w!(""), WS_POPUP, 0, 0, 0, 0, None, None, None, None).unwrap_or_default() };
    if pill::create(main).is_err() {
        println!("ERR\tpill");
        return 1;
    }
    let Some(anchor) = detect::uia().and_then(|u| detect::anchor(hwnd, kind, &u)) else {
        println!("ERR\tno anchor");
        return 1;
    };
    unsafe {
        let mut r = RECT::default();
        let _ = GetWindowRect(hwnd, &mut r);
        let (w, h) = (rect_w(&r), rect_h(&r));
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let header = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w * 2,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let bmi = BITMAPINFO { bmiHeader: header, ..Default::default() };
        let mut bits = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else { return 1 };
        let old = SelectObject(mem, dib.into());
        // PrintWindow returns a DPI-unaware window at its logical size; map our physical anchor onto it.
        let window_scale = GetDpiForWindow(hwnd) as f32 / 96.0;
        let f = monitor_scale(hwnd) / window_scale.max(1.0);
        let m = |v: i32| (v as f32 / f).round() as i32;
        let local = RECT { left: m(anchor.left - r.left), top: m(anchor.top - r.top), right: m(anchor.right - r.left), bottom: m(anchor.bottom - r.top) };
        let scale = window_scale.max(1.0);
        for (i, hover) in [false, true].into_iter().enumerate() {
            let part = CreateCompatibleDC(Some(screen));
            let pbmi = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biWidth: w, ..header }, ..Default::default() };
            let mut pbits = std::ptr::null_mut();
            if let Ok(pdib) = CreateDIBSection(Some(part), &pbmi, DIB_RGB_COLORS, &mut pbits, None, 0) {
                let pold = SelectObject(part, pdib.into());
                let _ = PrintWindow(hwnd, part, PRINT_WINDOW_FLAGS(2));
                pill::preview(part, local, style_for(kind), scale.max(1.0), hover);
                let _ = BitBlt(mem, i as i32 * w, 0, w, h, Some(part), 0, 0, SRCCOPY);
                SelectObject(part, pold);
                let _ = DeleteObject(pdib.into());
            }
            let _ = DeleteDC(part);
        }
        // BMP: file header + info header + top-down 32-bit pixels.
        let pixels = std::slice::from_raw_parts(bits as *const u8, (w * 2 * h * 4) as usize);
        let mut file = Vec::with_capacity(pixels.len() + 54);
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + pixels.len() as u32).to_le_bytes());
        file.extend_from_slice(&[0, 0, 0, 0]);
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&(w * 2).to_le_bytes());
        file.extend_from_slice(&(-h).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&[0u8; 24]);
        file.extend_from_slice(pixels);
        let ok = std::fs::write(&out, file).is_ok();
        SelectObject(mem, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        println!("{}\t{out}\tanchor={anchor:?}", if ok { "OK" } else { "ERR" });
        0
    }
}

fn menu_items(menu: HMENU) -> Vec<(u32, String)> {
    let mut out = vec![];
    unsafe {
        for j in 0..GetMenuItemCount(Some(menu)) {
            let id = GetMenuItemID(menu, j);
            let mut buf = [0u16; 256];
            let n = GetMenuStringW(menu, j as u32, Some(&mut buf), MF_BYPOSITION);
            let text = String::from_utf16_lossy(&buf[..n.max(0) as usize]).replace('&', "");
            if id != u32::MAX && id != 0 && !text.is_empty() {
                out.push((id, text));
            }
        }
    }
    out
}
