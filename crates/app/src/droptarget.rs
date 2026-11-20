//! Drops onto the window. The folder under the cursor hands us its own shell drop target, which does
//! the real work (copy/move/link choice, name conflicts, progress, undo, files dragged out of a
//! browser or a mail); we only pick the target, highlight it, autoscroll, and spring-load folders
//! the way Finder does. Dropping onto a tag in the sidebar tags the items instead.
use crate::app::{App, Hit, Loc, View};
use crate::geom::Rect;
use crate::sidebar::Row;
use std::{
    cell::RefCell,
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, POINT, POINTL},
    Graphics::Gdi::ScreenToClient,
    System::{
        Com::*,
        Ole::*,
        SystemServices::MODIFIERKEYS_FLAGS,
    },
    UI::{
        Shell::{CLSID_DragDropHelper, DragQueryFileW, HDROP, IDropTargetHelper},
        WindowsAndMessaging::{KillTimer, SetTimer},
    },
};
use windows_core::{Ref, Result, implement};

pub const TIMER: usize = 0xD0;
/// Hover time before a folder opens under the dragged items (Finder's default is about this).
const SPRING: Duration = Duration::from_millis(750);

#[derive(Clone, PartialEq, Debug)]
enum Want {
    None,
    /// Into this folder, or onto this item (an app, a .zip).
    Path(PathBuf),
    Tag(usize),
}

struct Cur {
    hit: Hit,
    want: Want,
    inner: Option<IDropTarget>,
    since: Instant,
    sprung: bool,
}

#[derive(Default)]
struct State {
    data: Option<IDataObject>,
    cur: Option<Cur>,
    helper: Option<IDropTargetHelper>,
    /// Last cursor position (client DIPs), key state and allowed effects, for the spring timer.
    at: (f32, f32),
    keys: u32,
    allowed: u32,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
}

pub fn register(hwnd: HWND) {
    let t: IDropTarget = Target { hwnd: hwnd.0 as isize }.into();
    unsafe {
        if let Err(e) = RegisterDragDrop(hwnd, &t) {
            crate::prefs::log(&format!("RegisterDragDrop failed: {e}"));
        }
    }
}

pub fn revoke(hwnd: HWND) {
    unsafe {
        let _ = RevokeDragDrop(hwnd);
    }
}

#[implement(IDropTarget)]
struct Target {
    hwnd: isize,
}

impl Target {
    fn app(&self) -> Option<&'static mut App> {
        unsafe { crate::win::app_of(HWND(self.hwnd as *mut std::ffi::c_void)).as_mut() }
    }
}

fn client(app: &App, pt: &POINTL) -> (f32, f32) {
    let mut p = POINT { x: pt.x, y: pt.y };
    unsafe {
        let _ = ScreenToClient(app.hwnd, &mut p);
    }
    (p.x as f32 / app.scale, p.y as f32 / app.scale)
}

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let Some(app) = self.app() else { return Ok(()) };
        let data = data.cloned();
        let helper: Option<IDropTargetHelper> =
            unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).ok() };
        let allowed = unsafe { effect.as_ref().map(|e| e.0).unwrap_or(0) };
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.data = data.clone();
            s.helper = helper.clone();
            s.cur = None;
            s.allowed = allowed;
        });
        app.hover = Hit::None;
        unsafe {
            let _ = SetTimer(Some(app.hwnd), TIMER, 100, None);
        }
        let e = over(app, keys.0, pt, allowed);
        unsafe {
            if let Some(out) = effect.as_mut() {
                *out = DROPEFFECT(e);
            }
            if let (Some(h), Some(d)) = (&helper, &data) {
                let _ = h.DragEnter(app.hwnd, d, &POINT { x: pt.x, y: pt.y }, DROPEFFECT(e));
            }
        }
        Ok(())
    }

    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let Some(app) = self.app() else { return Ok(()) };
        let allowed = unsafe { effect.as_ref().map(|e| e.0).unwrap_or(0) };
        let e = over(app, keys.0, pt, allowed);
        unsafe {
            if let Some(out) = effect.as_mut() {
                *out = DROPEFFECT(e);
            }
            if let Some(h) = ST.with(|s| s.borrow().helper.clone()) {
                let _ = h.DragOver(&POINT { x: pt.x, y: pt.y }, DROPEFFECT(e));
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        let (cur, helper) = ST.with(|s| {
            let mut s = s.borrow_mut();
            s.data = None;
            (s.cur.take(), s.helper.take())
        });
        unsafe {
            if let Some(t) = cur.and_then(|c| c.inner) {
                let _ = t.DragLeave();
            }
            if let Some(h) = helper {
                let _ = h.DragLeave();
            }
        }
        if let Some(app) = self.app() {
            end(app);
        }
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<IDataObject>,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        let Some(app) = self.app() else { return Ok(()) };
        let allowed = unsafe { effect.as_ref().map(|e| e.0).unwrap_or(0) };
        // Make sure the target matches the drop point, then take everything out of the state:
        // the shell's Drop runs progress and conflict dialogs that pump messages.
        over(app, keys.0, pt, allowed);
        let (cur, helper) = ST.with(|s| {
            let mut s = s.borrow_mut();
            s.data = None;
            (s.cur.take(), s.helper.take())
        });
        end(app);
        let data = data.cloned();
        let mut result = DROPEFFECT_NONE;
        if let (Some(cur), Some(d)) = (cur, &data) {
            match (&cur.want, cur.inner) {
                (Want::Tag(i), _) => {
                    let paths = hdrop_paths(d);
                    if !paths.is_empty() {
                        crate::tags::add(&paths, *i);
                        crate::shell::tags_changed(app);
                        let name = crate::theme::TAGS[*i].0;
                        app.note(&format!("Štítek {name}: {}", count(paths.len())));
                        result = DROPEFFECT_LINK;
                    }
                }
                (_, Some(t)) => {
                    let mut e = DROPEFFECT(allowed);
                    unsafe {
                        if t.Drop(d, keys, *pt, &mut e).is_ok() {
                            result = e;
                        }
                    }
                }
                _ => {}
            }
        }
        unsafe {
            if let Some(out) = effect.as_mut() {
                *out = result;
            }
            if let (Some(h), Some(d)) = (helper, &data) {
                let _ = h.Drop(d, &POINT { x: pt.x, y: pt.y }, result);
            }
        }
        app.invalidate();
        Ok(())
    }
}

fn count(n: usize) -> String {
    match n {
        1 => "1 položka".into(),
        2..=4 => format!("{n} položky"),
        _ => format!("{n} položek"),
    }
}

fn end(app: &mut App) {
    unsafe {
        let _ = KillTimer(Some(app.hwnd), TIMER);
    }
    app.drop_hl = None;
    app.invalidate();
}

/// Resolves the target under the cursor, switches inner targets when it changed, and returns the
/// effect to show.
fn over(app: &mut App, keys: u32, pt: &POINTL, allowed: u32) -> u32 {
    let (x, y) = client(app, pt);
    autoscroll(app, x, y);
    let (hit, want, hl) = resolve(app, x, y);
    let data = ST.with(|s| {
        let mut s = s.borrow_mut();
        s.at = (x, y);
        s.keys = keys;
        s.allowed = allowed;
        s.data.clone()
    });
    let same = ST.with(|s| s.borrow().cur.as_ref().is_some_and(|c| c.hit == hit && c.want == want));
    let mut effect = DROPEFFECT(allowed);
    if same {
        let inner = ST.with(|s| s.borrow().cur.as_ref().and_then(|c| c.inner.clone()));
        if let Some(t) = inner {
            unsafe {
                if t.DragOver(MODIFIERKEYS_FLAGS(keys), *pt, &mut effect).is_err() {
                    effect = DROPEFFECT_NONE;
                }
            }
        }
    } else {
        let old = ST.with(|s| s.borrow_mut().cur.take());
        if let Some(t) = old.and_then(|c| c.inner) {
            unsafe {
                let _ = t.DragLeave();
            }
        }
        let inner = match (&want, &data) {
            (Want::Path(p), Some(d)) => crate::shell::drop_target(app.hwnd, p).and_then(|t| unsafe {
                t.DragEnter(d, MODIFIERKEYS_FLAGS(keys), *pt, &mut effect).ok().map(|_| t)
            }),
            _ => None,
        };
        ST.with(|s| {
            s.borrow_mut().cur = Some(Cur {
                hit,
                want: want.clone(),
                inner,
                since: Instant::now(),
                sprung: false,
            })
        });
    }
    let e = match &want {
        Want::None => 0,
        Want::Tag(_) if allowed & DROPEFFECT_LINK.0 != 0 => DROPEFFECT_LINK.0,
        Want::Tag(_) => allowed & DROPEFFECT_COPY.0,
        Want::Path(_) => {
            if ST.with(|s| s.borrow().cur.as_ref().is_some_and(|c| c.inner.is_some())) {
                effect.0
            } else {
                0
            }
        }
    };
    let hl = if e != 0 { hl } else { None };
    if app.drop_hl != hl {
        app.drop_hl = hl;
        app.invalidate();
    }
    e
}

/// Spring-loaded folders: called from the window's timer while a drag hovers over us.
pub fn tick(app: &mut App) {
    let due = ST.with(|s| {
        let mut s = s.borrow_mut();
        let Some(c) = s.cur.as_mut() else { return None };
        if c.sprung || c.since.elapsed() < SPRING || !springs(app, c.hit) {
            return None;
        }
        c.sprung = true;
        Some(c.hit)
    });
    let Some(hit) = due else { return };
    match hit {
        Hit::Row(i) if app.tab().view == View::Columns => {
            app.tab_mut().sel.select_only(i);
            crate::views::sync_columns(app);
        }
        Hit::Row(i) => {
            let p = app.tab().path_of(i);
            app.go(Loc::Folder(p));
        }
        Hit::ColRow(k, r) => crate::views::column_click(app, k, Some(r)),
        Hit::SideRow(k) => {
            if let Row::Item { loc, .. } = &app.side_rows[k] {
                let loc = loc.clone();
                app.go(loc);
            }
        }
        Hit::Tab(k) => app.switch_tab(k),
        Hit::Crumb(k) => {
            if let Some(l) = crate::paint::crumb_path(app, k) {
                app.go(l);
            }
        }
        _ => {}
    }
    app.invalidate();
    // Re-resolve right away so the highlight and the inner target follow the new contents.
    let (at, keys, allowed) = ST.with(|s| {
        let s = s.borrow();
        (s.at, s.keys, s.allowed)
    });
    let pt = POINTL {
        x: (at.0 * app.scale) as i32,
        y: (at.1 * app.scale) as i32,
    };
    let mut p = POINT { x: pt.x, y: pt.y };
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(app.hwnd, &mut p);
    }
    over(app, keys, &POINTL { x: p.x, y: p.y }, allowed);
}

fn springs(app: &App, hit: Hit) -> bool {
    match hit {
        Hit::Row(i) => app.tab().entries.get(i).is_some_and(|e| e.is_dir),
        Hit::ColRow(..) => true, // resolve() only keeps folder rows as ColRow targets
        Hit::SideRow(k) => matches!(&app.side_rows[k], Row::Item { loc: Loc::Folder(_), .. }),
        Hit::Tab(k) => k != app.cur,
        Hit::Crumb(_) => true,
        _ => false,
    }
}

fn autoscroll(app: &mut App, x: f32, y: f32) {
    let b = app.lay.body;
    if !b.contains(x, y) || app.tab().view == View::Columns {
        return;
    }
    const EDGE: f32 = 24.0;
    let d = if y < b.y + EDGE {
        -(b.y + EDGE - y) * 0.5
    } else if y > b.b() - EDGE {
        (y - (b.b() - EDGE)) * 0.5
    } else {
        0.0
    };
    if d != 0.0 {
        let before = app.tab().scroll;
        app.tab_mut().scroll += d;
        app.clamp_scroll();
        if app.tab().scroll != before {
            app.invalidate();
        }
    }
}

/// Items that take drops themselves (Explorer: drop onto an app opens the files with it).
fn takes_drops(p: &std::path::Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "exe" | "zip" | "bat" | "cmd" | "lnk")
}

/// What is under (x, y): the hit, what a drop there does, and what to highlight (`true` = the
/// whole folder view rather than one item).
fn resolve(app: &App, x: f32, y: f32) -> (Hit, Want, Option<(Rect, bool)>) {
    let here = || match app.tab().loc.folder() {
        Some(p) => (Hit::Empty, Want::Path(p.to_path_buf()), Some((app.lay.body, true))),
        None => (Hit::None, Want::None, None),
    };
    let h = app.hit(x, y);
    match h {
        Hit::Row(i) => {
            let t = app.tab();
            let Some(e) = t.entries.get(i) else { return here() };
            let p = t.path_of(i);
            if e.is_dir || takes_drops(&p) {
                (h, Want::Path(p), Some((app.item_rect(i), false)))
            } else {
                here()
            }
        }
        Hit::ColRow(k, r) => {
            let Some(pane) = crate::views::pane(app, k) else { return here() };
            match pane.entries.get(r) {
                Some(e) if e.is_dir => (
                    h,
                    Want::Path(e.path_in(&pane.dir)),
                    Some((crate::views::col_row_rect(app, k, r), false)),
                ),
                _ => (
                    Hit::ColEmpty(k),
                    Want::Path(pane.dir.clone()),
                    Some((crate::views::col_rect(app, k), true)),
                ),
            }
        }
        Hit::ColEmpty(k) => match crate::views::pane(app, k) {
            Some(pane) => (h, Want::Path(pane.dir.clone()), Some((crate::views::col_rect(app, k), true))),
            None => here(),
        },
        Hit::SideRow(k) => {
            let r = crate::paint::side_row_rect(app, k);
            match &app.side_rows[k] {
                Row::Item { loc: Loc::Folder(p), .. } => (h, Want::Path(p.clone()), Some((r, false))),
                Row::Tag(i) => (h, Want::Tag(*i), Some((r, false))),
                _ => (Hit::None, Want::None, None),
            }
        }
        Hit::Tab(k) | Hit::TabClose(k) => match app.tabs[k].loc.folder() {
            Some(p) => (Hit::Tab(k), Want::Path(p.to_path_buf()), crate::paint::tab_rect(app, k).map(|r| (r, false))),
            None => (Hit::Tab(k), Want::None, None),
        },
        Hit::Crumb(k) => match crate::paint::crumb_path(app, k) {
            Some(Loc::Folder(p)) => (h, Want::Path(p), crate::paint::crumb_rect(app, k).map(|r| (r, false))),
            _ => (Hit::None, Want::None, None),
        },
        Hit::Empty | Hit::Header(_) => here(),
        _ if app.lay.body.contains(x, y) => here(),
        _ => (Hit::None, Want::None, None),
    }
}

/// File paths in a data object (CF_HDROP); empty for virtual items such as a browser's image.
fn hdrop_paths(d: &IDataObject) -> Vec<PathBuf> {
    let fmt = FORMATETC {
        cfFormat: 15, // CF_HDROP
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe {
        let Ok(mut m) = d.GetData(&fmt) else { return Vec::new() };
        let h = HDROP(m.u.hGlobal.0);
        let n = DragQueryFileW(h, u32::MAX, None);
        let out = (0..n)
            .map(|i| {
                let len = DragQueryFileW(h, i, None) as usize;
                let mut buf = vec![0u16; len + 1];
                DragQueryFileW(h, i, Some(&mut buf));
                PathBuf::from(OsString::from_wide(&buf[..len]))
            })
            .collect();
        ReleaseStgMedium(&mut m);
        out
    }
}

/// `--dev-drop SRC WHERE HOLD_MS drop|leave [--dev-out FILE]`: drives this drop target the way
/// OLE would (DragEnter, DragOver every 50 ms, Drop or DragLeave) without synthesizing input.
/// WHERE is `row:N`, `tag:N` or `empty`. Results go to FILE.
pub fn dev(app: &mut App, args: &[String]) {
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage};
    let val = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let Some(i) = args.iter().position(|a| a == "--dev-drop") else { return };
    let (Some(src), Some(at), Some(hold), Some(mode)) =
        (args.get(i + 1), args.get(i + 2), args.get(i + 3), args.get(i + 4))
    else {
        return;
    };
    let out = val("--dev-out");
    let mut log = String::new();
    let hold: u64 = hold.parse().unwrap_or(0);
    let p = if let Some(n) = at.strip_prefix("row:").and_then(|n| n.parse::<usize>().ok()) {
        let r = app.item_rect(n);
        (r.x + 12.0, r.cy())
    } else if let Some(n) = at.strip_prefix("tag:").and_then(|n| n.parse::<usize>().ok()) {
        let k = app.side_rows.iter().position(|r| matches!(r, Row::Tag(t) if *t == n)).unwrap_or(0);
        let r = crate::paint::side_row_rect(app, k);
        (r.cx(), r.cy())
    } else {
        let b = app.lay.body;
        (b.cx(), b.b() - 8.0)
    };
    let mut sp = POINT { x: (p.0 * app.scale) as i32, y: (p.1 * app.scale) as i32 };
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(app.hwnd, &mut sp);
    }
    let pt = POINTL { x: sp.x, y: sp.y };
    let Some(data) = crate::shell::data_object(app.hwnd, &[PathBuf::from(src)]) else {
        log += "no data object\n";
        if let Some(o) = out { let _ = std::fs::write(o, log); }
        return;
    };
    let t: IDropTarget = Target { hwnd: app.hwnd.0 as isize }.into();
    let all = DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK;
    unsafe {
        let mut e = all;
        let _ = t.DragEnter(&data, MODIFIERKEYS_FLAGS(1), pt, &mut e); // MK_LBUTTON, like a real drag
        log += &format!("enter effect={} hl={:?}\n", e.0, app.drop_hl.map(|h| h.1));
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(hold) {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(Duration::from_millis(50));
            let mut e = all;
            let _ = t.DragOver(MODIFIERKEYS_FLAGS(1), pt, &mut e);
        }
        log += &format!("loc after hold={:?}\n", app.tab().loc);
        if mode == "drop" {
            let mut e = all;
            let r = t.Drop(&data, MODIFIERKEYS_FLAGS(0), pt, &mut e);
            log += &format!("drop {:?} effect={}\n", r.is_ok(), e.0);
        } else {
            let _ = t.DragLeave();
            log += "leave\n";
        }
    }
    log += &format!("hl after={:?}\n", app.drop_hl);
    if at.starts_with("tag:") {
        let sp = [PathBuf::from(src)];
        log += &format!("tag mask={}\n", crate::tags::mask(&sp[0]));
        crate::tags::clear(&sp); // dev runs must not leave tags behind
    }
    if let Some(o) = out {
        let _ = std::fs::write(o, log);
    }
}
