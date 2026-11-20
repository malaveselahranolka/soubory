//! Search location. Until the whole-disk index service is wired in, results come from the scope
//! folder's own listing (instant, folded match on the name).
use crate::app::{App, Loc};
use crate::fsys;
use std::path::PathBuf;
use windows::Win32::Foundation::{LPARAM, WPARAM};

pub fn start() {}
pub fn stop() {}
pub fn forget(_hwnd: windows::Win32::Foundation::HWND) {}
pub fn index_event(_w: WPARAM, _l: LPARAM) {}

pub fn default_scope(from: &Loc) -> Option<PathBuf> {
    from.folder().map(|p| p.to_path_buf())
}

pub fn run(app: &mut App) {
    let Loc::Search { text, scope } = app.tab().loc.clone() else {
        return;
    };
    let q = fm_fs::fold(text.trim());
    let terms: Vec<&str> = q.split_whitespace().collect();
    let mut hits = Vec::new();
    if let Some(dir) = &scope {
        for mut e in fsys::list_dir(dir, app.prefs.show_hidden).unwrap_or_default() {
            let f = fm_fs::fold(&e.name);
            if terms.iter().all(|t| f.contains(t)) {
                e.full = Some(dir.join(&e.name));
                hits.push(e);
            }
        }
    }
    let t = app.tab_mut();
    t.entries = hits;
    t.icon_idx = vec![i32::MIN; t.entries.len()];
    t.sel = crate::sel::Selection::new(t.entries.len());
}

pub fn message(_app: &mut App, _w: WPARAM, _l: LPARAM) {}
