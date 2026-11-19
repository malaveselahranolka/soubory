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
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static INJECTOR: RefCell<Option<Injector>> = const { RefCell::new(None) };
    static UIA: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
    static ACTIVE_MENU: RefCell<Option<NewMenu>> = const { RefCell::new(None) };
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
        let hooks = [
            SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_event), 0, 0, flags),
            SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, None, Some(on_event), 0, 0, flags),
            SetWinEventHook(EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, None, Some(on_event), 0, 0, flags),
            SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, None, Some(on_event), 0, 0, flags),
        ];
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
    with(|s| s.tracked = Some(Tracked { hwnd, kind, target, rect }));
    set_hotkey(true);
    reanchor();
}

fn detach() {
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
    let Some(anchor) = anchor else { return pill::hide() };
    let scale = unsafe { GetDpiForWindow(t.hwnd) } as f32 / 96.0;
    pill::place(anchor, style_for(t.kind), scale.max(1.0));
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
        i.borrow().as_ref().map(|i| i.call(t.hwnd, t.target.is32, CMD_QUERY as isize, Duration::from_millis(3000)))
    });
    match reply.unwrap_or_else(|| Err("injector unavailable".into())).and_then(|r| parse_query_reply(&r)) {
        Ok(f) => Ok(f),
        Err(e) => {
            log(format!("hook query failed in {}: {e}", t.target.exe));
            detect::address_bar_path(t.hwnd).map(|p| FolderReply { parsing_name: p.clone(), fs_path: Some(p) }).ok_or(e)
        }
    }
}

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
    let Some(t) = with(|s| s.tracked.clone()) else { return };
    let main = with(|s| {
        s.busy = true;
        s.main
    });
    pill::set_pressed(true);
    let folder = query_folder(&t);
    let fs_dir = folder.as_ref().ok().and_then(|f| f.fs_path.clone());
    let menu = fs_dir.as_deref().map(NewMenu::for_folder);
    let (popup, info) = match &menu {
        Some(Ok(m)) => (m.popup, None),
        Some(Err(e)) => {
            log(format!("New menu failed for {fs_dir:?}: {e}"));
            (HMENU::default(), info_menu("Tady nelze nic vytvořit"))
        }
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
    if let Err(e) = select_in_dialog(t, &name) {
        log(format!("select '{name}' failed in {}: {e}", t.target.exe));
        balloon(&format!("Vytvořeno: {name}"), "Dialog položku nezobrazuje – nejspíš ji skrývá filtr typů souborů.");
    }
}

fn balloon(title: &str, text: &str) {
    tray::balloon(with(|s| s.main), title, text);
}

unsafe extern "system" fn main_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match m {
            WM_COPYDATA if crate::inject::on_copydata(l) => return LRESULT(1),
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
        Some("anchor") => {
            let a = detect::uia().and_then(|u| detect::anchor(hwnd, kind, &u));
            println!("{kind:?} {a:?} dpi={}", unsafe { GetDpiForWindow(hwnd) });
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
