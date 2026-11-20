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
        System::{LibraryLoader::*, Threading::*},
        UI::{Controls::*, HiDpi::*, Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

pub const WM_APP_FS: u32 = WM_APP + 10;
pub const WM_APP_THUMB: u32 = WM_APP + 11;
pub const WM_APP_SEARCH: u32 = WM_APP + 12;
pub const WM_APP_INDEX: u32 = WM_APP + 13;
const TIMER_FS: usize = 1;
const TIMER_NOTE: usize = 2;
const EDIT_SEARCH: usize = 100;
const EDIT_RENAME: usize = 101;
const EDIT_PATH: usize = 102;

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Search,
    Rename,
    Path,
}

struct Edit {
    hwnd: HWND,
    kind: EditKind,
    index: usize,
    font: HFONT,
    brush: HBRUSH,
    bg: COLORREF,
    fg: COLORREF,
}

struct Watch {
    stop: HANDLE,
    thread: Option<std::thread::JoinHandle<()>>,
}

thread_local! {
    static EDITS: RefCell<Vec<Edit>> = const { RefCell::new(Vec::new()) };
    static WATCH: RefCell<Option<Watch>> = const { RefCell::new(None) };
    static APP: RefCell<*mut App> = const { RefCell::new(std::ptr::null_mut()) };
    static SEARCH_PREV: RefCell<Option<Loc>> = const { RefCell::new(None) };
}

pub fn app_ptr() -> *mut App {
    APP.with(|a| *a.borrow())
}

pub fn run(start: Loc) -> i32 {
    unsafe {
        let _ = windows::Win32::System::Ole::OleInitialize(None);
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        allow_dark_menus();
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class = w!("FmMainWindow");
        let icon = folder_icon();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            hIcon: icon,
            hIconSm: icon,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let mut app = Box::new(App::new(start));
        crate::prefs::load(&mut app);
        app.side_rows = crate::sidebar::build(&app.prefs.favorites);
        app.reload(None);
        let app_raw: *mut App = &mut *app;
        APP.with(|a| *a.borrow_mut() = app_raw);

        // Initial size in DIPs, scaled for the primary monitor.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let s = dx as f32 / 96.0;
        let hwnd = CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW,
            class,
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
        let Ok(hwnd) = hwnd else { return 1 };
        app.hwnd = hwnd;
        setup_dwm(&mut app);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
        set_title(&app);
        watch_current(&mut app);
        crate::droptarget::register(hwnd);
        let restored = crate::prefs::restore_window(&app);
        let maximized = restored && app.prefs.window.is_some_and(|w| w.1);
        let _ = ShowWindow(hwnd, if maximized { SW_SHOWMAXIMIZED } else if restored { SW_SHOWNORMAL } else { SW_SHOWDEFAULT });
        crate::search::start(&mut app);
        dev_flags(&mut app);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        stop_watch();
        crate::search::stop();
        APP.with(|a| *a.borrow_mut() = std::ptr::null_mut());
        drop(app);
        msg.wParam.0 as i32
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
            _ => crate::app::View::List,
        };
        app.set_view(view);
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
        if let Some((h, _)) = edit_of(EditKind::Search) {
            let w = wide(&t);
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
    app.invalidate();
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

pub fn new_window(app: &App) {
    if let (Ok(exe), Some(dir)) = (std::env::current_exe(), app.tab().loc.folder()) {
        let _ = std::process::Command::new(exe).arg(dir).spawn();
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
        if g.attach(app.hwnd, w, h).is_err() {
            app.gfx = Some(g);
            return;
        }
    }
    let g = Rc::new(g);
    app.gfx_ref = Some(g.clone());
    let ok = g.frame(app.scale, |c| paint::paint(app, c));
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
    position_edits(app);
    crate::quicklook::follow(app);
}

// ---- folder watching -------------------------------------------------------------------------------------

fn stop_watch() {
    WATCH.with(|w| {
        if let Some(mut old) = w.borrow_mut().take() {
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
    });
}

/// Watches the current folder (not its subtree) and posts WM_APP_FS on any change.
pub fn watch_current(app: &mut App) {
    if app.offscreen || app.hwnd.is_invalid() {
        return;
    }
    stop_watch();
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
        WATCH.with(|w| {
            *w.borrow_mut() = Some(Watch {
                stop,
                thread: Some(thread),
            })
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
    destroy_edit(kind);
    let t = &app.theme;
    let (bg, fg) = match kind {
        EditKind::Search | EditKind::Path => (paint::over(t.field, t.chrome), t.ink),
        EditKind::Rename => (t.card, t.ink),
    };
    let r = edit_rect(app, kind, index);
    let id = match kind {
        EditKind::Search => EDIT_SEARCH,
        EditKind::Rename => EDIT_RENAME,
        EditKind::Path => EDIT_PATH,
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
                hwnd,
                kind,
                index,
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

fn destroy_edit(kind: EditKind) {
    let gone = EDITS.with(|e| {
        let mut v = e.borrow_mut();
        v.iter().position(|x| x.kind == kind).map(|i| v.remove(i))
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

fn edit_of(kind: EditKind) -> Option<(HWND, usize)> {
    EDITS.with(|e| {
        e.borrow()
            .iter()
            .find(|x| x.kind == kind)
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

pub fn search_active(_app: &App) -> bool {
    edit_of(EditKind::Search).is_some()
}

pub fn renaming_index(_app: &App) -> Option<usize> {
    edit_of(EditKind::Rename).map(|e| e.1)
}

/// Where the rename field sits, so paint can draw its frame around the EDIT.
pub fn rename_rect(app: &App) -> Option<crate::geom::Rect> {
    edit_of(EditKind::Rename).map(|(_, i)| edit_rect(app, EditKind::Rename, i))
}

/// Quick Look forwards navigation keys to the main window's selection.
pub fn forward_key(vk: VIRTUAL_KEY) {
    let p = app_ptr();
    if p.is_null() {
        return;
    }
    let app = unsafe { &mut *p };
    app.key(vk, false, false, false);
    app.invalidate();
}

pub fn path_editing() -> bool {
    edit_of(EditKind::Path).is_some()
}

pub fn focus_search(app: &mut App) {
    if let Some((h, _)) = edit_of(EditKind::Search) {
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
    destroy_edit(EditKind::Search);
    app.search_text.clear();
    app.focus = Focus::Content;
    unsafe {
        let _ = SetFocus(Some(app.hwnd));
    }
    if matches!(app.tab().loc, Loc::Search { .. }) {
        let prev = SEARCH_PREV.with(|p| p.borrow_mut().take());
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
            let prev = SEARCH_PREV.with(|p| p.borrow_mut().take());
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
            SEARCH_PREV.with(|p| *p.borrow_mut() = Some(other.clone()));
            crate::search::default_scope(other)
        }
    };
    let t = app.tab_mut();
    t.loc = Loc::Search { text, scope };
    t.scroll = 0.0;
    crate::search::run(app);
    set_title(app);
    app.relayout();
    app.invalidate();
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
    destroy_edit(EditKind::Path);
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

fn commit_rename(app: &mut App, cancel: bool) {
    let Some((h, i)) = edit_of(EditKind::Rename) else {
        return;
    };
    let text = edit_text(h);
    // Only take focus back when the edit still has it (not when the user clicked into search).
    let had_focus = unsafe { GetFocus() } == h;
    destroy_edit(EditKind::Rename);
    if had_focus {
        unsafe {
            let _ = SetFocus(Some(app.hwnd));
        }
    }
    app.invalidate();
    if cancel || i >= app.tab().entries.len() {
        return;
    }
    let old = app.tab().path_of(i);
    let old_name = app.tab().entries[i].name.clone();
    let new_name = text.trim_end().to_string();
    if new_name == old_name || new_name.is_empty() {
        return;
    }
    match crate::shell::rename(&old, &new_name) {
        Ok(()) => {
            if matches!(app.tab().loc, Loc::Folder(_)) {
                app.reload(Some(&new_name));
            } else {
                let t = app.tab_mut();
                t.entries[i].name = new_name.clone();
                if let Some(f) = &mut t.entries[i].full {
                    f.set_file_name(&new_name);
                }
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
    let app = app_ptr();
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
                EDIT_PATH => {
                    if !is_char && vk == VK_RETURN {
                        let t = edit_text(h);
                        path_entered(app, &t);
                        return LRESULT(0);
                    }
                    if !is_char && vk == VK_ESCAPE {
                        destroy_edit(EditKind::Path);
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
        (WM_KILLFOCUS, EDIT_PATH) => {
            let r = unsafe { DefSubclassProc(h, m, w, l) };
            destroy_edit(EditKind::Path);
            app.invalidate();
            return r;
        }
        (WM_KILLFOCUS, EDIT_SEARCH) => {
            if app.focus == Focus::Search {
                app.focus = Focus::Content;
            }
            if app.search_text.is_empty() {
                let r = unsafe { DefSubclassProc(h, m, w, l) };
                destroy_edit(EditKind::Search);
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
            let _ = EnableNonClientDpiScaling(hwnd);
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let ptr = app_ptr();
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &mut *ptr;
        if let Some(r) = crate::shell::menu_message(hwnd, msg, wparam, lparam) {
            return r;
        }
        match msg {
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
                    Hit::Drag if !path_editing() => LRESULT(HTCAPTION as isize),
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
                    if edit_of(EditKind::Rename).is_some() {
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
                for k in [EditKind::Search, EditKind::Rename, EditKind::Path] {
                    destroy_edit(k);
                }
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
                    if let Some((h, _)) = edit_of(EditKind::Search) {
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
                let (x, y) = xy(lparam, app.scale);
                let (ctrl, shift, _) = mods();
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
                let (x, y) = xy(lparam, app.scale);
                let (ctrl, shift, _) = mods();
                SetCapture(hwnd);
                app.mouse_down(x, y, ctrl, shift);
                app.drag = Drag::None;
                app.double_click(x, y);
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
                    if edit_of(EditKind::Rename).is_some() {
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
                if id == EDIT_SEARCH && code == EN_CHANGE {
                    if let Some((h, _)) = edit_of(EditKind::Search) {
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
                    TIMER_FS => {
                        let _ = KillTimer(Some(hwnd), TIMER_FS);
                        if edit_of(EditKind::Rename).is_none() {
                            crate::thumbs::refresh();
                            app.reload(None);
                            crate::views::refresh_columns(app);
                            app.invalidate();
                        }
                    }
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
            WM_APP_THUMB => {
                crate::thumbs::arrived(app, wparam, lparam);
                LRESULT(0)
            }
            WM_APP_SEARCH | WM_APP_INDEX => {
                crate::search::message(app, msg, wparam, lparam);
                LRESULT(0)
            }
            WM_SETTINGCHANGE
            | WM_THEMECHANGED
            | WM_SYSCOLORCHANGE
            | WM_DWMCOLORIZATIONCOLORCHANGED => {
                app.theme = crate::theme::Theme::detect();
                setup_dwm(app);
                for k in [EditKind::Search, EditKind::Rename, EditKind::Path] {
                    destroy_edit(k);
                }
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
                crate::droptarget::revoke(hwnd);
                PostQuitMessage(0);
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
