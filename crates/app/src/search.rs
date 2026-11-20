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
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
}

struct Reply {
    generation: u64,
    hits: Vec<Hit>,
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
    let roots = match dev_root {
        Some(r) => vec![r],
        None if test_run => return,
        None => fm_index::win::service::default_roots(),
    };
    if roots.is_empty() {
        return;
    }
    let data = crate::prefs::data_dir().join("index");
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

/// At exit: stops the watchers and writes the snapshot.
pub fn stop() {
    let svc = ST.with(|s| s.borrow_mut().svc.take());
    if let Some(svc) = svc.and_then(|a| Arc::try_unwrap(a).ok()) {
        svc.stop();
    }
}

pub fn forget(hwnd: HWND) {
    ST.with(|s| s.borrow_mut().tokens.remove(&(hwnd.0 as isize)));
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
    std::thread::spawn(move || {
        if let Some(hits) = svc.search(&text, scope.as_deref(), hidden, LIMIT, &token) {
            if !token.is_cancelled() {
                let r = Box::into_raw(Box::new(Reply { generation, hits }));
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
    let entries = reply
        .hits
        .into_iter()
        .map(|h| Entry {
            name: h.name,
            is_dir: h.is_dir,
            size: h.size,
            modified: h.mtime.max(0) as u64,
            created: 0,
            attrs: h.attrs,
            full: Some(PathBuf::from(h.path)),
        })
        .collect();
    set_entries(app, entries);
    app.invalidate();
}

/// A column header was clicked: sort the results in place.
pub fn resort(app: &mut App) {
    let entries = std::mem::take(&mut app.tab_mut().entries);
    set_entries(app, entries);
}

/// Replaces the results, keeping the selection on the same paths (live updates).
fn set_entries(app: &mut App, mut entries: Vec<Entry>) {
    let folders_first = app.prefs.folders_first;
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
    t.entries = entries;
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
    app.clamp_scroll();
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
