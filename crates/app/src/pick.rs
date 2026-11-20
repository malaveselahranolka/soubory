//! Option 2's picker (docs/ARCHITECTURE.md §2): an fm window that picks files for another app's
//! Open dialog. `fm_dialog_shim.dll` in that app hands the request to the resident fm's door, or
//! starts `fm --pick <pipe>`; both end in [`start`]. The window is a normal fm window (sidebar,
//! views, search) plus a bottom bar: the file types, "Zrušit" and the dialog's OK button.
//!
//! The pipe (protocol in `fm_dialog_shim::proto`): read the request, say Ready once the window is
//! up, answer exactly once. Every way a shown picker closes answers (Drop); before Ready it only
//! lets go, and the host shows the system dialog instead.
use crate::app::{App, Hit, Loc};
use crate::fsys::{Entry, wide};
use crate::geom::Rect;
use crate::gfx::{Align, Canvas, Font};
use crate::theme::{Color, Theme};
use fm_dialog_shim::proto::{self, Msg, Request};
use std::{
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::Pipes::*,
        UI::{Shell::FOLDERID_Documents, WindowsAndMessaging::*},
    },
    core::PCWSTR,
};

pub const H_TYPES: u8 = 0;
pub const H_CANCEL: u8 = 1;
pub const H_OK: u8 = 2;
const BTN_H: f32 = 28.0;

/// The folder of the last pick: where the next picker opens unless the app names one.
/// shortcut: kept in memory only (the resident fm lives all session); persist it if users miss it after a restart.
static LAST: Mutex<Option<PathBuf>> = Mutex::new(None);

pub struct Picker {
    pub req: Request,
    /// The host's pipe; invalid once answered or gone.
    pipe: HANDLE,
    /// Ready was sent: from now on every close answers.
    ready: bool,
    /// The file type shown (index into `req.filters`).
    pub filter: usize,
}

impl Drop for Picker {
    fn drop(&mut self) {
        if self.ready {
            self.answer(Msg::Cancelled);
        } else {
            self.let_go();
        }
    }
}

impl Picker {
    /// A picker that answers nobody (`fm --render --pick`, tests).
    pub fn offline(req: Request) -> Picker {
        Picker::new(req, HANDLE::default())
    }

    fn new(req: Request, pipe: HANDLE) -> Picker {
        let filter = (req.index as usize).saturating_sub(1).min(req.filters.len().saturating_sub(1));
        Picker { req, pipe, ready: false, filter }
    }

    pub fn title(&self) -> &str {
        if self.req.title.trim().is_empty() { "Otevřít" } else { &self.req.title }
    }

    pub fn ok_label(&self) -> &str {
        if self.req.ok.trim().is_empty() { "Otevřít" } else { &self.req.ok }
    }

    fn send(&self, m: &Msg) -> bool {
        let b = proto::to_bytes(&m.encode());
        !self.pipe.is_invalid() && unsafe { WriteFile(self.pipe, Some(&b), None, None) }.is_ok()
    }

    /// The window is up. False when the host gave up meanwhile: the window then closes.
    pub fn ready(&mut self) -> bool {
        if self.pipe.is_invalid() {
            return true;
        }
        self.ready = self.send(&Msg::Ready);
        if !self.ready {
            self.let_go();
        }
        self.ready
    }

    /// The one answer. The host's window may take the foreground back once ours closes.
    pub fn answer(&mut self, m: Msg) {
        if self.pipe.is_invalid() {
            return;
        }
        self.send(&m);
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(HWND(self.req.owner as *mut _), Some(&mut pid));
            if pid != 0 {
                let _ = AllowSetForegroundWindow(pid);
            }
        }
        self.let_go();
    }

    fn let_go(&mut self) {
        if !self.pipe.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.pipe);
            }
        }
        self.pipe = HANDLE::default();
    }

    /// The host still waits (its end of the pipe is open).
    fn alive(&self) -> bool {
        self.pipe.is_invalid() || unsafe { PeekNamedPipe(self.pipe, None, 0, None, None, None) }.is_ok()
    }
}

/// Opens a picker for the request waiting on `pipe`. None when there is nothing to pick for (the
/// host gave up, the request is unreadable, the window did not open); the host then shows the
/// system dialog.
pub fn start(pipe: &str) -> Option<HWND> {
    if !proto::valid_pipe(pipe) {
        crate::prefs::log("pick: not a pick pipe");
        return None;
    }
    let h = connect(pipe)?;
    let Some(req) = read_request(h).filter(|r| r.pipe == pipe) else {
        crate::prefs::log("pick: no request on the pipe");
        unsafe {
            let _ = CloseHandle(h);
        }
        return None;
    };
    let loc = start_loc(&req, LAST.lock().ok().and_then(|l| l.clone()));
    crate::win::open_window(loc, None, Some(Picker::new(req, h)))
}

fn connect(pipe: &str) -> Option<HANDLE> {
    let w = wide(pipe);
    for _ in 0..2 {
        match unsafe { CreateFileW(PCWSTR(w.as_ptr()), (GENERIC_READ | GENERIC_WRITE).0, FILE_SHARE_NONE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None) } {
            Ok(h) => {
                let _ = unsafe { SetNamedPipeHandleState(h, Some(&PIPE_READMODE_MESSAGE), None, None) };
                return Some(h);
            }
            Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() => unsafe {
                let _ = WaitNamedPipeW(PCWSTR(w.as_ptr()), 1000);
            },
            Err(_) => break,
        }
    }
    crate::prefs::log("pick: the pipe is gone");
    None
}

/// The request, the pipe's first message (the shim writes it as soon as we are connected).
fn read_request(h: HANDLE) -> Option<Request> {
    let start = Instant::now();
    loop {
        let (mut avail, mut left) = (0u32, 0u32);
        unsafe { PeekNamedPipe(h, None, 0, None, Some(&mut avail), Some(&mut left)) }.ok()?;
        if avail > 0 {
            let mut buf = vec![0u8; left.max(avail) as usize];
            let mut n = 0u32;
            unsafe { ReadFile(h, Some(&mut buf), Some(&mut n), None) }.ok()?;
            return Request::decode(&proto::from_bytes(&buf[..n as usize])?);
        }
        if start.elapsed() > Duration::from_secs(3) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// SetFolder, else the last picker's folder, else SetDefaultFolder, else Documents (Windows' order).
fn start_loc(req: &Request, last: Option<PathBuf>) -> Loc {
    [Some(PathBuf::from(&req.folder)), last, Some(PathBuf::from(&req.default_folder))]
        .into_iter()
        .flatten()
        .find(|p| p.is_absolute() && p.is_dir())
        .or_else(|| crate::fsys::known_folder(&FOLDERID_Documents))
        .map_or(Loc::Computer, Loc::Folder)
}

// ---- file types ----------------------------------------------------------------------------------

/// The patterns of a filter spec (`*.png; *.JPG`), lowercased; None when it shows everything.
pub fn patterns(spec: &str) -> Option<Vec<String>> {
    let v: Vec<String> = spec.split(';').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect();
    (!v.is_empty() && !v.iter().any(|p| p == "*" || p == "*.*")).then_some(v)
}

/// Folders always show, so the user can get anywhere; files only when a pattern matches.
pub fn shows(patterns: &[String], e: &Entry) -> bool {
    let name = e.name.to_lowercase();
    e.is_dir || patterns.iter().any(|p| wild(p, &name))
}

/// The patterns of the file type a picker window shows; None outside pickers and for "all files".
pub fn filter_of(app: &App) -> Option<Vec<String>> {
    let p = app.pick.as_ref()?;
    patterns(&p.req.filters.get(p.filter)?.1)
}

/// `*` and `?` wildcards, as in a file dialog's file types.
fn wild(p: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (p.chars().collect(), s.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < s.len() {
        if i < p.len() && (p[i] == '?' || p[i] == s[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(st) = star {
            i = st + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

// ---- picking ---------------------------------------------------------------------------------------

/// A file that can be handed back: on disk, not a folder or a shortcut to one, not a shell item.
fn is_file(app: &App, i: usize) -> bool {
    let t = app.tab();
    let Some(e) = t.entries.get(i) else { return false };
    if e.is_dir || e.shell.is_some() || matches!(t.loc, Loc::Shell { .. }) {
        return false;
    }
    let p = t.path_of(i);
    p.is_absolute() && crate::shell::link_folder(&p).is_none()
}

/// What App::open does in a picker: a file is picked; None leaves folders to App::open.
pub fn open(app: &mut App, i: usize) -> Option<()> {
    is_file(app, i).then(|| accept(app, &[i]))
}

/// Enter and the OK button: the selected files, or into the one selected folder.
pub fn accept_selection(app: &mut App) {
    let sel = app.tab().sel.indices();
    accept(app, &sel);
}

fn accept(app: &mut App, items: &[usize]) {
    if let [i] = *items {
        if !is_file(app, i) {
            app.open(i, false);
            return;
        }
    }
    if items.is_empty() {
        return;
    }
    if items.iter().any(|&i| !is_file(app, i)) {
        app.note("Vyberte jen soubory.");
        return;
    }
    let paths: Vec<PathBuf> = items.iter().map(|&i| app.tab().path_of(i)).collect();
    if paths.iter().any(|p| !p.is_file()) {
        app.note("Soubor už neexistuje.");
        return;
    }
    if paths.len() > 1 && app.pick.as_ref().is_some_and(|p| !p.req.multi) {
        app.note("Vyberte jeden soubor.");
        return;
    }
    let Some(p) = app.pick.as_mut() else { return };
    if let Ok(mut last) = LAST.lock() {
        *last = paths[0].parent().map(PathBuf::from);
    }
    p.answer(Msg::Picked(paths.iter().map(|p| p.to_string_lossy().into_owned()).collect()));
    crate::win::close(app);
}

/// Esc, "Zrušit", closing the window.
pub fn cancel(app: &mut App) {
    if let Some(p) = app.pick.as_mut() {
        p.answer(Msg::Cancelled);
    }
    crate::win::close(app);
}

/// A single-select dialog keeps one item selected (Ctrl/Shift clicks, Ctrl+A, a band).
pub fn clamp(app: &mut App) {
    if app.pick.as_ref().is_none_or(|p| p.req.multi) || app.tab().sel.count() < 2 {
        return;
    }
    let t = app.tab_mut();
    if let Some(f) = t.sel.focus.or_else(|| t.sel.indices().first().copied()) {
        t.sel.select_only(f);
    }
}

/// Once a second: the host may have gone (closed, crashed, gave up); its picker goes too.
pub fn tick(app: &mut App) {
    if let Some(p) = app.pick.as_mut() {
        if !p.alive() {
            p.let_go();
            crate::win::close(app);
        }
    }
}

// ---- bottom bar ----------------------------------------------------------------------------------

pub struct Parts {
    pub types: Option<Rect>,
    pub cancel: Rect,
    pub ok: Rect,
}

/// The bar's controls: the file types aligned with the content, the buttons on the right.
pub fn parts(app: &App) -> Option<Parts> {
    let p = app.pick.as_ref()?;
    let bar = app.lay.pick?;
    let m = |s: &str, f: Font| app.gfx().map_or(s.chars().count() as f32 * 7.0, |g| g.measure(s, f));
    let y = bar.y + (bar.h - BTN_H) / 2.0;
    let ok_w = (m(p.ok_label(), Font::BodyStrong) + 32.0).max(96.0);
    let cancel_w = (m("Zrušit", Font::Body) + 32.0).max(88.0);
    let ok = Rect::new(bar.r() - 16.0 - ok_w, y, ok_w, BTN_H);
    let cancel = Rect::new(ok.x - 8.0 - cancel_w, y, cancel_w, BTN_H);
    let types = p.req.filters.get(p.filter).map(|(name, spec)| {
        let x = app.lay.card.x + 12.0;
        let w = (m(type_label(name, spec), Font::Body) + 44.0).clamp(120.0, 320.0).min(cancel.x - 16.0 - x).max(40.0);
        Rect::new(x, y, w, BTN_H)
    });
    Some(Parts { types, cancel, ok })
}

fn type_label<'a>(name: &'a str, spec: &'a str) -> &'a str {
    if name.trim().is_empty() { spec } else { name }
}

pub fn hit(app: &App, x: f32, y: f32) -> Hit {
    let Some(p) = parts(app) else { return Hit::None };
    if p.types.is_some_and(|r| r.contains(x, y)) {
        Hit::Pick(H_TYPES)
    } else if p.cancel.contains(x, y) {
        Hit::Pick(H_CANCEL)
    } else if p.ok.contains(x, y) {
        Hit::Pick(H_OK)
    } else {
        // Chrome, like the toolbar: it moves the window.
        Hit::Drag
    }
}

pub fn click(app: &mut App, k: u8) {
    match k {
        H_TYPES => types_menu(app),
        H_CANCEL => cancel(app),
        H_OK => accept_selection(app),
        _ => {}
    }
}

/// The file types as a native menu over the field, the shown one checked.
fn types_menu(app: &mut App) {
    let (Some(Parts { types: Some(r), .. }), Some(p)) = (parts(app), app.pick.as_ref()) else { return };
    let pt = crate::win::to_screen(app, r.x, r.y - 2.0);
    let chosen = unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        for (k, (name, spec)) in p.req.filters.iter().enumerate() {
            crate::shell::append(m, k as u32 + 1, &type_label(name, spec).replace('&', "&&"), k == p.filter, true);
        }
        let _ = SetForegroundWindow(app.hwnd);
        let r = TrackPopupMenuEx(m, (TPM_RETURNCMD | TPM_BOTTOMALIGN | TPM_LEFTALIGN).0, pt.x, pt.y, app.hwnd, None);
        let _ = PostMessageW(Some(app.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        (r.0 as usize).checked_sub(1)
    };
    if let (Some(k), Some(p)) = (chosen, app.pick.as_mut()) {
        p.filter = k;
        app.reload(None);
        crate::views::refresh_columns(app);
        app.invalidate();
    }
}

pub fn paint(app: &App, c: &mut Canvas, t: &Theme) {
    let (Some(parts), Some(p)) = (parts(app), app.pick.as_ref()) else { return };
    let hot = |k: u8| app.hover == Hit::Pick(k);
    let down = |k: u8| hot(k) && app.pressed == Hit::Pick(k);
    let field = crate::paint::over(t.field, t.chrome);
    let state = |c: &mut Canvas, r: Rect, k: u8| {
        if down(k) {
            c.round(r, 6.0, t.press);
        } else if hot(k) {
            c.round(r, 6.0, t.hover);
        }
    };
    if let (Some(r), Some((name, spec))) = (parts.types, p.req.filters.get(p.filter)) {
        c.round(r, 6.0, field);
        state(c, r, H_TYPES);
        c.round_stroke(r, 6.0, t.edge, 0.0);
        let label = c.fit_path(type_label(name, spec), Font::Body, r.w - 36.0);
        c.text(&label, r.cut_left(10.0).cut_right(26.0), Font::Body, t.ink, Align::Start);
        c.icon(crate::paint::g::DOWN, crate::phosphor::Weight::Bold, r.right(26.0), 12.0, t.ink2);
    }
    c.round(parts.cancel, 6.0, field);
    state(c, parts.cancel, H_CANCEL);
    c.round_stroke(parts.cancel, 6.0, t.edge, 0.0);
    c.text("Zrušit", parts.cancel, Font::Body, t.ink, Align::Center);
    // The default button, dimmed until something is selected.
    let on = app.tab().sel.count() > 0;
    if on {
        c.round(parts.ok, 6.0, t.accent);
        if hot(H_OK) {
            c.round(parts.ok, 6.0, Color { a: if down(H_OK) { 0.22 } else { 0.12 }, ..t.on_accent });
        }
    } else {
        c.round(parts.ok, 6.0, field);
        c.round_stroke(parts.ok, 6.0, t.edge, 0.0);
    }
    c.text(p.ok_label(), parts.ok, Font::BodyStrong, if on { t.on_accent } else { t.ink4 }, Align::Center);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool) -> Entry {
        Entry { name: name.into(), is_dir: dir, size: 0, modified: 0, created: 0, attrs: 0, full: None, shell: None, origin: None, sized: false }
    }

    #[test]
    fn file_types() {
        let p = patterns(" *.PNG ; *.jpg;;").unwrap();
        assert_eq!(p, ["*.png", "*.jpg"]);
        assert!(shows(&p, &entry("Léto.png", false)) && shows(&p, &entry("A.JPG", false)));
        assert!(!shows(&p, &entry("a.png.txt", false)) && !shows(&p, &entry("png", false)));
        assert!(shows(&p, &entry("Fotky.txt", true)), "folders always show");
        assert_eq!(patterns("*.*"), None);
        assert_eq!(patterns("*"), None);
        assert_eq!(patterns(""), None);
        let q = patterns("report-??.pdf;*.tar.gz").unwrap();
        assert!(shows(&q, &entry("report-07.pdf", false)) && !shows(&q, &entry("report-7.pdf", false)));
        assert!(shows(&q, &entry("a.b.tar.gz", false)) && !shows(&q, &entry("a.tar.gzip", false)));
        assert!(wild("*", "") && wild("a*", "a") && !wild("a?", "a"));
    }

    #[test]
    fn request_to_window() {
        let dir = std::env::temp_dir().join(format!("fm-pick-unit-{}", std::process::id()));
        let other = dir.join("jiná");
        std::fs::create_dir_all(&other).unwrap();
        let d = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let req = |folder: String, default_folder: String| Request { folder, default_folder, ..Default::default() };
        // SetFolder wins, then the last picker's folder, then SetDefaultFolder.
        assert_eq!(start_loc(&req(d(&dir), d(&other)), Some(other.clone())), Loc::Folder(dir.clone()));
        assert_eq!(start_loc(&req(d(&dir.join("chybí")), d(&dir)), Some(other.clone())), Loc::Folder(other.clone()));
        assert_eq!(start_loc(&req(String::new(), d(&other)), None), Loc::Folder(other.clone()));
        assert!(matches!(start_loc(&req("relativní".into(), String::new()), None), Loc::Folder(_) | Loc::Computer));
        // The file type index is 1-based and clamped; labels default to Czech.
        let mut r = Request { index: 2, filters: vec![("A".into(), "*.a".into()), ("B".into(), "*.b".into())], ..Default::default() };
        assert_eq!(Picker::offline(r.clone()).filter, 1);
        r.index = 9;
        assert_eq!(Picker::offline(r.clone()).filter, 1);
        r.index = 0;
        let p = Picker::offline(r);
        assert_eq!((p.filter, p.title(), p.ok_label()), (0, "Otevřít", "Otevřít"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn picker_window_lists_and_selects_as_asked() {
        let dir = std::env::temp_dir().join(format!("fm-pick-app-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("složka")).unwrap();
        for f in ["a.png", "b.txt", "c.jpg"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let mut app = App::new(Loc::Folder(dir.clone()));
        app.offscreen = true;
        let req = Request { filters: vec![("Obrázky".into(), "*.png;*.jpg".into()), ("Vše".into(), "*.*".into())], index: 1, ..Default::default() };
        app.pick = Some(Picker::offline(req));
        app.reload(None);
        let names = |app: &App| app.tab().entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&app), ["složka", "a.png", "c.jpg"], "filtered, folders kept");
        app.pick.as_mut().unwrap().filter = 1;
        app.reload(None);
        assert_eq!(names(&app).len(), 4, "all files");
        // Single select: Ctrl+A leaves one item.
        app.key(windows::Win32::UI::Input::KeyboardAndMouse::VK_A, true, false, false);
        assert_eq!(app.tab().sel.count(), 1);
        app.pick.as_mut().unwrap().req.multi = true;
        app.key(windows::Win32::UI::Input::KeyboardAndMouse::VK_A, true, false, false);
        assert_eq!(app.tab().sel.count(), 4);
        // Only files are picked; one folder is opened instead.
        assert!(!is_file(&app, 0) && is_file(&app, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
