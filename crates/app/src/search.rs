//! Search. The whole-disk index (fm-index) answers as you type; the scope bar picks the whole
//! computer or the folder the search started from (Finder). Without the index (offscreen renders,
//! test runs) the scope folder's own listing is filtered instead.
use crate::app::{App, Loc};
use crate::fsys::{self, Entry};
use fm_index::CancelToken;
use fm_index::win::service::{Event, Hit, Service};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    UI::WindowsAndMessaging::{KillTimer, PostMessageW, SetTimer},
};

/// Ranked results kept per query; Finder shows everything, but past this nobody scrolls.
pub const LIMIT: usize = 2000;
pub const TIMER_RERUN: usize = 0x5E;
const SERVICE_READY: usize = 1;

#[derive(Default)]
struct State {
    svc: Option<Arc<Service>>,
    /// Per window: the running query's token (a newer query cancels it) and its generation.
    tokens: HashMap<isize, (CancelToken, u64)>,
    next_gen: u64,
    scanning: bool,
    progress: usize,
    rerun_armed: bool,
    /// Folder sizes the index reported, by full path (the list view's Size column).
    sizes: HashMap<PathBuf, u64>,
    /// Per window: the running folder-size request's token.
    size_tokens: HashMap<isize, CancelToken>,
    /// Windows whose results arrived under an open rename field (run again once it closes).
    deferred: HashSet<isize>,
}

struct Sizes {
    loc: Loc,
    paths: Vec<PathBuf>,
    sizes: Vec<Option<u64>>,
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
}

struct Reply {
    generation: u64,
    hits: Vec<Hit>,
    /// Creation times by hit (the index has none), fetched when the tab sorts or groups by them.
    created: Vec<u64>,
}

/// Starts the index on a background thread: loading a snapshot of a few million entries takes
/// a second or two, and the first window must not wait for it.
pub fn start() {
    let args: Vec<String> = std::env::args().collect();
    let dev_root = args
        .iter()
        .position(|a| a == "--dev-index")
        .and_then(|i| args.get(i + 1))
        .cloned();
    // Test runs index only what they ask for, never the whole disk.
    let test_run = args.iter().any(|a| a.starts_with("--dev-") || a == "--new-instance");
    let roots = match dev_root.clone() {
        Some(r) => vec![r],
        None if test_run => return,
        None => fm_index::win::service::default_roots(),
    };
    if roots.is_empty() {
        return;
    }
    // Snapshots are named by volume: a test index of a scratch folder on C: must not replace
    // the real index of C:.
    let data = crate::prefs::data_dir().join(if dev_root.is_some() { "index-dev" } else { "index" });
    let _ = std::fs::create_dir_all(&data);
    let hub = crate::win::hub().0 as isize;
    ST.with(|s| s.borrow_mut().scanning = true);
    std::thread::spawn(move || {
        let svc = Service::start(
            roots,
            data,
            Box::new(move |ev| post(hub, 0, Box::new(ev))),
        );
        post(hub, SERVICE_READY, Box::new(svc));
    });
}

fn post<T>(hub: isize, kind: usize, v: Box<T>) {
    let p = Box::into_raw(v);
    unsafe {
        if PostMessageW(
            Some(HWND(hub as *mut std::ffi::c_void)),
            crate::win::WM_APP_INDEX,
            WPARAM(kind),
            LPARAM(p as isize),
        )
        .is_err()
        {
            drop(Box::from_raw(p));
        }
    }
}

/// At exit: stops the watchers and writes the snapshot. A query or size request still running
/// holds the service; it is cancelled and given up to two seconds to let go.
pub fn stop() {
    let (svc, tokens, size_tokens) = ST.with(|s| {
        let mut s = s.borrow_mut();
        (s.svc.take(), std::mem::take(&mut s.tokens), std::mem::take(&mut s.size_tokens))
    });
    tokens.values().for_each(|(t, _)| t.cancel());
    size_tokens.values().for_each(CancelToken::cancel);
    let Some(mut svc) = svc else { return };
    for _ in 0..100 {
        match Arc::try_unwrap(svc) {
            Ok(s) => return s.stop(),
            Err(again) => svc = again,
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    crate::prefs::log("index: a query outlived exit; the snapshot was not written");
}

/// Folder sizes the index already reported, filled in before sorting so a reload neither
/// flickers nor sorts twice.
pub fn known_sizes(entries: &mut [Entry], dir: &Path) {
    ST.with(|s| {
        let s = s.borrow();
        if s.sizes.is_empty() {
            return;
        }
        for e in entries.iter_mut().filter(|e| e.is_dir) {
            if let Some(&n) = s.sizes.get(&e.path_in(dir)) {
                e.size = n;
                e.sized = true;
            }
        }
    });
}

/// Asks the index for the sizes of the current tab's folders; they arrive as WM_APP_SIZES.
pub fn request_sizes(app: &App) {
    let Some(svc) = ST.with(|s| s.borrow().svc.clone()) else { return };
    let t = app.tab();
    // No window yet (open_window asks again once it exists): the reply would have nowhere to go.
    if matches!(t.loc, Loc::Computer | Loc::Shell { .. }) || app.hwnd.is_invalid() {
        return;
    }
    let dir = t.dir();
    // Every folder: each is one lookup and a walk of its own subtree (C:\ whole takes ~15 ms).
    let dirs: Vec<&Entry> = t.entries.iter().filter(|e| e.is_dir && e.shell.is_none()).collect();
    let paths: Vec<PathBuf> = dirs.iter().map(|e| e.path_in(&dir)).collect();
    // Never opened to check emptiness: links and junctions (they lead elsewhere, maybe a share).
    let probe: Vec<bool> = dirs.iter().map(|e| e.attrs & 0x400 == 0 || e.is_placeholder()).collect();
    if paths.is_empty() {
        return;
    }
    let key = app.hwnd.0 as isize;
    let token = ST.with(|s| {
        let mut s = s.borrow_mut();
        let token = match s.size_tokens.get(&key) {
            Some(t) => t.next(),
            None => CancelToken::new(),
        };
        s.size_tokens.insert(key, token.clone());
        token
    });
    let loc = t.loc.clone();
    std::thread::spawn(move || {
        let strs: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        let found = svc.folder_sizes(&strs, &token);
        drop(svc);
        let Some(found) = found else { return };
        // A folder the index lists nothing in is empty, or was never walked (a junction, access
        // denied): it counts as empty only when it lists as empty here; otherwise unknown.
        let mut sizes = Vec::with_capacity(found.len());
        for ((p, &probe), f) in paths.iter().zip(&probe).zip(found) {
            if token.is_cancelled() {
                return;
            }
            sizes.push(match f {
                Some((n, true)) => Some(n),
                Some((_, false)) if probe && std::fs::read_dir(p).is_ok_and(|mut d| d.next().is_none()) => Some(0),
                _ => None,
            });
        }
        if token.is_cancelled() {
            return;
        }
        let r = Box::into_raw(Box::new(Sizes { loc, paths, sizes }));
        unsafe {
            if PostMessageW(Some(HWND(key as *mut std::ffi::c_void)), crate::win::WM_APP_SIZES, WPARAM(0), LPARAM(r as isize)).is_err() {
                drop(Box::from_raw(r));
            }
        }
    });
}

/// Folder sizes for a window: every tab still showing that place takes them.
pub fn sizes_message(app: &mut App, l: LPARAM) {
    let r = unsafe { Box::from_raw(l.0 as *mut Sizes) };
    let fresh: HashMap<PathBuf, u64> = r.paths.into_iter().zip(r.sizes).filter_map(|(p, n)| Some((p, n?))).collect();
    ST.with(|s| {
        let mut s = s.borrow_mut();
        // shortcut: the cache is dropped wholesale past 100k folders; an LRU would keep the hot ones.
        if s.sizes.len() > 100_000 {
            s.sizes.clear();
        }
        s.sizes.extend(fresh.iter().map(|(p, n)| (p.clone(), *n)));
    });
    let folders_first = app.prefs.folders_first;
    let cur = app.cur;
    let mut repaint = false;
    for k in 0..app.tabs.len() {
        let t = &mut app.tabs[k];
        if t.loc != r.loc {
            continue;
        }
        let dir = t.dir();
        let mut changed = false;
        for e in t.entries.iter_mut().filter(|e| e.is_dir) {
            if let Some(&n) = fresh.get(&e.path_in(&dir)) {
                changed |= e.size != n || !e.sized;
                e.size = n;
                e.sized = true;
            }
        }
        repaint |= changed && k == cur;
        // Sorted by size: the folders move to their places. The shown tab waits for a press,
        // drag or rename to end (flush_deferred), so nothing moves under the pointer or the field.
        let sorted = t.sort == fsys::SortKey::Size && !matches!(t.loc, Loc::Recents) && !(matches!(t.loc, Loc::Search { .. }) && t.by_rank);
        if changed && sorted {
            if k == cur {
                t.resort_pending = true;
            } else {
                t.resort(folders_first);
            }
        }
    }
    app.flush_deferred();
    if repaint {
        app.invalidate();
    }
}

pub fn forget(hwnd: HWND) {
    ST.with(|s| {
        let mut s = s.borrow_mut();
        let key = hwnd.0 as isize;
        // A closed window's queries and size requests stop (closing the app waits for them).
        if let Some((t, _)) = s.tokens.remove(&key) {
            t.cancel();
        }
        if let Some(t) = s.size_tokens.remove(&key) {
            t.cancel();
        }
        s.deferred.remove(&key);
    });
}

/// A rename was applied in place in a search tab. A reply already on its way, or a rerun due
/// now, would still have the old name (the index sees the rename 50-100 ms later): drop it and
/// query again later.
pub fn after_rename(app: &App) {
    let key = app.hwnd.0 as isize;
    ST.with(|s| {
        let mut s = s.borrow_mut();
        s.next_gen += 1;
        let generation = s.next_gen;
        if let Some(e) = s.tokens.get_mut(&key) {
            *e = (e.0.next(), generation);
        }
        s.rerun_armed = true;
    });
    unsafe {
        SetTimer(Some(crate::win::hub()), TIMER_RERUN, 500, None);
    }
}

/// The window is no longer busy: run a query whose results arrived meanwhile. Through the
/// rerun timer, not at once: a rename just applied reaches the index 50-100 ms later, and an
/// earlier query would bring the old name back.
pub fn replay(app: &mut App) {
    let key = app.hwnd.0 as isize;
    if ST.with(|s| {
        let mut s = s.borrow_mut();
        let d = s.deferred.remove(&key);
        if d {
            s.rerun_armed = true;
        }
        d
    }) {
        unsafe {
            // Restarts the timer when it is already armed (it could fire inside that window).
            SetTimer(Some(crate::win::hub()), TIMER_RERUN, 500, None);
        }
    }
}

/// Hub message: the service came up, or an index event.
pub fn index_event(w: WPARAM, l: LPARAM) {
    if w.0 == SERVICE_READY {
        let svc = unsafe { Box::from_raw(l.0 as *mut Service) };
        let scanning = svc.is_scanning();
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.svc = Some(Arc::new(*svc));
            s.scanning = scanning;
        });
        rerun_all();
        return;
    }
    let ev = unsafe { Box::from_raw(l.0 as *mut Event) };
    match *ev {
        Event::Progress { entries, .. } => {
            ST.with(|s| {
                let mut s = s.borrow_mut();
                s.scanning = true;
                s.progress = entries;
            });
            crate::win::for_each_app(|a| {
                if matches!(a.tab().loc, Loc::Search { .. }) {
                    a.invalidate();
                }
            });
        }
        Event::Ready { .. } => {
            let scanning = ST.with(|s| s.borrow().svc.as_ref().is_some_and(|v| v.is_scanning()));
            ST.with(|s| s.borrow_mut().scanning = scanning);
            rerun_all();
        }
        // A root that cannot be watched retries on its own and repeats this; log, no UI.
        Event::Error { root, message } => crate::prefs::log(&format!("index {root}: {message}")),
        Event::Changed => {
            // Live results, at most twice a second.
            let arm = ST.with(|s| !std::mem::replace(&mut s.borrow_mut().rerun_armed, true));
            if arm {
                unsafe {
                    SetTimer(Some(crate::win::hub()), TIMER_RERUN, 500, None);
                }
            }
        }
    }
}

/// Hub timer.
pub fn tick(id: usize) {
    if id == TIMER_RERUN {
        unsafe {
            let _ = KillTimer(Some(crate::win::hub()), TIMER_RERUN);
        }
        ST.with(|s| s.borrow_mut().rerun_armed = false);
        rerun_all();
    }
}

fn rerun_all() {
    crate::win::for_each_app(|a| {
        if matches!(a.tab().loc, Loc::Search { .. }) {
            run(a);
            a.invalidate();
        } else {
            request_sizes(a);
        }
    });
}

/// Where a new search looks first: the whole computer once the index is there (Finder),
/// otherwise the folder it started from.
pub fn default_scope(from: &Loc) -> Option<PathBuf> {
    if indexed() { None } else { from.folder().map(Path::to_path_buf) }
}

pub fn indexed() -> bool {
    ST.with(|s| s.borrow().svc.is_some())
}

/// Index state for the scope bar, while there is something to say.
pub fn status() -> Option<String> {
    ST.with(|s| {
        let s = s.borrow();
        if !s.scanning {
            return None;
        }
        Some(if s.progress > 0 {
            format!("Indexuji disk… {}", fsys::format_count(s.progress))
        } else {
            "Indexuji disk…".to_string()
        })
    })
}

pub fn run(app: &mut App) {
    let Loc::Search { text, scope } = app.tab().loc.clone() else {
        return;
    };
    let svc = ST.with(|s| s.borrow().svc.clone());
    let Some(svc) = svc else {
        run_listing(app, &text, scope.as_deref());
        return;
    };
    if text.trim().is_empty() {
        set_entries(app, Vec::new());
        return;
    }
    // No window yet: open_window runs the query again once it exists.
    if app.hwnd.is_invalid() {
        return;
    }
    let key = app.hwnd.0 as isize;
    let (token, generation) = ST.with(|s| {
        let mut s = s.borrow_mut();
        s.next_gen += 1;
        let generation = s.next_gen;
        let token = match s.tokens.get(&key) {
            Some((t, _)) => t.next(),
            None => CancelToken::new(),
        };
        s.tokens.insert(key, (token.clone(), generation));
        (token, generation)
    });
    let hidden = app.prefs.show_hidden;
    let scope = scope.map(|p| p.to_string_lossy().into_owned());
    let t = app.tab();
    let want_created = t.sort == fsys::SortKey::Created || t.group == crate::groups::GroupKey::Created;
    std::thread::spawn(move || {
        let found = svc.search(&text, scope.as_deref(), hidden, LIMIT, &token);
        // Not held through the stats below: closing the app waits for every query's reference.
        drop(svc);
        if let Some(hits) = found {
            // Attributes only (no file is opened, placeholders stay online), at most LIMIT.
            let created = if want_created {
                let mut v = Vec::with_capacity(hits.len());
                for h in &hits {
                    if token.is_cancelled() {
                        return;
                    }
                    v.push(fsys::entry_of(Path::new(&h.path)).map_or(0, |e| e.created));
                }
                v
            } else {
                Vec::new()
            };
            if !token.is_cancelled() {
                let r = Box::into_raw(Box::new(Reply { generation, hits, created }));
                unsafe {
                    if PostMessageW(
                        Some(HWND(key as *mut std::ffi::c_void)),
                        crate::win::WM_APP_SEARCH,
                        WPARAM(0),
                        LPARAM(r as isize),
                    )
                    .is_err()
                    {
                        drop(Box::from_raw(r));
                    }
                }
            }
        }
    });
}

/// Results for a window. Stale ones (an older query, or the user left the search) are dropped.
pub fn message(app: &mut App, _w: WPARAM, l: LPARAM) {
    let reply = unsafe { Box::from_raw(l.0 as *mut Reply) };
    let current = ST.with(|s| s.borrow().tokens.get(&(app.hwnd.0 as isize)).map(|t| t.1));
    if current != Some(reply.generation) || !matches!(app.tab().loc, Loc::Search { .. }) {
        return;
    }
    // Never replace the list under a press, drag or open rename field: the drag, click or
    // rename would act on whatever moved into that row. flush_deferred runs it again.
    if app.busy() {
        ST.with(|s| s.borrow_mut().deferred.insert(app.hwnd.0 as isize));
        return;
    }
    let created = reply.created;
    let entries = reply
        .hits
        .into_iter()
        .enumerate()
        .map(|(k, h)| Entry {
            name: h.name,
            is_dir: h.is_dir,
            size: h.size,
            modified: h.mtime.max(0) as u64,
            created: created.get(k).copied().unwrap_or(0),
            attrs: h.attrs,
            full: Some(PathBuf::from(h.path)),
            shell: None,
            origin: None,
            sized: false,
        })
        .collect();
    set_entries(app, entries);
    app.invalidate();
}

/// A column header was clicked: sort the results in place.
pub fn resort(app: &mut App) {
    // A copy: set_entries maps the old selection through the old list.
    let entries = app.tab().entries.clone();
    set_entries(app, entries);
}

/// Replaces the results, keeping the selection on the same paths (live updates).
fn set_entries(app: &mut App, mut entries: Vec<Entry>) {
    let folders_first = app.prefs.folders_first;
    known_sizes(&mut entries, Path::new(""));
    let t = app.tab_mut();
    if !t.by_rank {
        fsys::sort(&mut entries, t.sort, t.asc, folders_first);
    }
    let keep: HashSet<PathBuf> = t
        .sel
        .indices()
        .into_iter()
        .filter_map(|i| t.entries.get(i).and_then(|e| e.full.clone()))
        .collect();
    let focus = t.sel.focus.and_then(|i| t.entries.get(i)).and_then(|e| e.full.clone());
    t.entries = entries;
    crate::groups::apply(t, folders_first);
    t.icon_idx = vec![i32::MIN; t.entries.len()];
    t.sel = crate::sel::Selection::new(t.entries.len());
    let again: Vec<usize> = t
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.full.as_ref().is_some_and(|p| keep.contains(p)))
        .map(|(i, _)| i)
        .collect();
    if !again.is_empty() {
        t.sel.set_many(again, &[]);
    }
    if let Some(i) = focus.and_then(|p| t.entries.iter().position(|e| e.full.as_ref() == Some(&p))) {
        t.sel.focus = Some(i);
        t.sel.anchor = Some(i);
    }
    app.clamp_scroll();
    if std::mem::take(&mut app.tab_mut().reveal_on_load) {
        app.reveal_focus();
    }
    request_sizes(app);
}

/// Without the index: the scope folder's own entries whose folded name has every term.
fn run_listing(app: &mut App, text: &str, scope: Option<&Path>) {
    let q = fm_fs::fold(text.trim());
    let terms: Vec<&str> = q.split_whitespace().collect();
    let mut hits = Vec::new();
    if let Some(dir) = scope {
        for mut e in fsys::list_dir(dir, app.prefs.show_hidden).unwrap_or_default() {
            let f = fm_fs::fold(&e.name);
            if !terms.is_empty() && terms.iter().all(|t| f.contains(t)) {
                e.full = Some(dir.join(&e.name));
                hits.push(e);
            }
        }
    }
    set_entries(app, hits);
}
