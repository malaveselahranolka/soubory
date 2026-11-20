//! Undo (Ctrl+Z) and redo (Ctrl+Y) for what fm itself did: renames, new folders, duplicates,
//! the moves and copies it ran (paste, "New Folder with Selection"), and moving to the Recycle
//! Bin, which is undone by restoring the items from the bin (the bin's own Restore command).
//! Shared by all windows, like Explorer's.
use crate::app::App;
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::HWND,
        System::Com::CoTaskMemFree,
        UI::{
            Shell::{Common::*, *},
            WindowsAndMessaging::{CreatePopupMenu, DestroyMenu},
        },
    },
    core::*,
};

const DEPTH: usize = 100;

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Rename { from: PathBuf, to: PathBuf },
    /// Items fm created; undone by moving them to the Recycle Bin.
    Created(Vec<PathBuf>),
    /// (from, to) pairs fm moved.
    Moved(Vec<(PathBuf, PathBuf)>),
    /// Items fm moved to the Recycle Bin (their original paths).
    Recycled(Vec<PathBuf>),
    /// Several steps undone together, last first.
    Group(Vec<Op>),
}

impl Op {
    fn inverse(&self) -> Op {
        match self {
            Op::Rename { from, to } => Op::Rename { from: to.clone(), to: from.clone() },
            Op::Created(p) => Op::Recycled(p.clone()),
            Op::Recycled(p) => Op::Created(p.clone()),
            Op::Moved(v) => Op::Moved(v.iter().map(|(a, b)| (b.clone(), a.clone())).collect()),
            Op::Group(v) => Op::Group(v.iter().rev().map(Op::inverse).collect()),
        }
    }

    fn describe(&self) -> String {
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match self {
            Op::Rename { to, .. } => format!("přejmenování na „{}“", name(to)),
            Op::Created(p) if p.len() == 1 => format!("vytvoření „{}“", name(&p[0])),
            Op::Created(p) => format!("vytvoření {} položek", p.len()),
            Op::Recycled(p) if p.len() == 1 => format!("přesun „{}“ do koše", name(&p[0])),
            Op::Recycled(p) => format!("přesun {} položek do koše", p.len()),
            Op::Moved(v) if v.len() == 1 => format!("přesun „{}“", name(&v[0].0)),
            Op::Moved(v) => format!("přesun {} položek", v.len()),
            Op::Group(v) => v.first().map(Op::describe).unwrap_or_default(),
        }
    }
}

#[derive(Default)]
struct Stacks {
    undo: Vec<Op>,
    redo: Vec<Op>,
}

thread_local! {
    static S: RefCell<Stacks> = RefCell::new(Stacks::default());
}

/// Records something fm just did. A new action clears redo (as everywhere).
pub fn push(op: Op) {
    S.with(|s| {
        let mut s = s.borrow_mut();
        s.redo.clear();
        s.undo.push(op);
        if s.undo.len() > DEPTH {
            s.undo.remove(0);
        }
    });
}

pub fn undo(app: &mut App) {
    step(app, true);
}

pub fn redo(app: &mut App) {
    step(app, false);
}

fn step(app: &mut App, back: bool) {
    let op = S.with(|s| {
        let mut s = s.borrow_mut();
        if back { s.undo.pop() } else { s.redo.pop() }
    });
    let Some(op) = op else {
        app.note(if back { "Není co vrátit." } else { "Není co opakovat." });
        return;
    };
    let todo = if back { op.inverse() } else { op.clone() };
    match perform(app.hwnd, &todo) {
        Ok(()) => {
            S.with(|s| {
                let mut s = s.borrow_mut();
                if back { s.redo.push(op.clone()) } else { s.undo.push(op.clone()) }
            });
            app.note(&format!("{}: {}", if back { "Vráceno" } else { "Opakováno" }, op.describe()));
        }
        Err(msg) => app.note(&msg),
    }
    app.reload(None);
    app.invalidate();
}

/// Renames run right here (instant, errors shown at once). Everything else runs in order on one
/// STA worker thread, so a group (move the items back, then remove their folder) never races
/// itself; the shell shows progress and conflicts there.
fn perform(owner: HWND, op: &Op) -> std::result::Result<(), String> {
    if let Op::Rename { .. } = op {
        return rename_step(op);
    }
    if !applies(op) {
        return Err("Položky už na místě nejsou.".into());
    }
    let (op, owner) = (op.clone(), owner.0 as usize);
    std::thread::spawn(move || unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        run_sync(HWND(owner as *mut c_void), &op);
        windows::Win32::System::Com::CoUninitialize();
    });
    Ok(())
}

fn rename_step(op: &Op) -> std::result::Result<(), String> {
    let Op::Rename { from, to } = op else { return Ok(()) };
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if from.parent() == to.parent() {
        crate::shell::rename(from, &name)
    } else {
        Err("Tuto změnu už nelze vrátit.".into())
    }
}

/// Whether any part of `op` still has something to act on (bin contents are checked later).
fn applies(op: &Op) -> bool {
    match op {
        Op::Rename { from, .. } => from.exists(),
        Op::Recycled(p) => p.iter().any(|p| p.exists()),
        Op::Created(_) => true,
        Op::Moved(v) => v.iter().any(|(from, _)| from.exists()),
        Op::Group(v) => v.iter().any(applies),
    }
}

fn run_sync(owner: HWND, op: &Op) {
    use crate::shell::{Op as F, file_op_sync};
    match op {
        Op::Rename { .. } => {
            if let Err(e) = rename_step(op) {
                crate::prefs::log(&format!("undo rename: {e}"));
            }
        }
        Op::Recycled(paths) => {
            let live: Vec<PathBuf> = paths.iter().filter(|p| p.exists()).cloned().collect();
            if !live.is_empty() {
                file_op_sync(owner, &live, None, F::Recycle);
            }
        }
        Op::Created(paths) => {
            if restore(owner, paths) == 0 {
                crate::prefs::log("undo: nothing to restore from the Recycle Bin");
            }
        }
        Op::Moved(pairs) => {
            // Grouped by destination folder: one shell operation each.
            let mut by_dest: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
            for (from, to) in pairs {
                if let (true, Some(d)) = (from.exists(), to.parent()) {
                    by_dest.entry(d.to_path_buf()).or_default().push(from.clone());
                }
            }
            for (dest, items) in by_dest {
                file_op_sync(owner, &items, Some(&dest), F::Move);
            }
        }
        Op::Group(v) => {
            for o in v {
                run_sync(owner, o);
            }
        }
    }
}

/// What an IFileOperation fm ran actually did, reported from its thread (via the hub).
pub struct Done {
    pub op: Op,
}

pub fn done(d: Box<Done>) {
    let empty = match &d.op {
        Op::Created(v) | Op::Recycled(v) => v.is_empty(),
        Op::Moved(v) => v.is_empty(),
        _ => false,
    };
    if !empty {
        push(d.op);
    }
}

// ---- Recycle Bin restore ------------------------------------------------------------------------------

/// Restores the most recently deleted bin item for each of `paths` with the bin's own Restore
/// command (conflicts and progress as in Explorer). Returns how many were found.
pub fn restore(owner: HWND, paths: &[PathBuf]) -> usize {
    let wanted: HashMap<String, ()> = paths.iter().map(|p| (key(p), ())).collect();
    unsafe {
        let Ok(desktop) = SHGetDesktopFolder() else { return 0 };
        let Ok(bin_id) = SHGetKnownFolderIDList(&FOLDERID_RecycleBinFolder, 0, None) else { return 0 };
        let bin: Result<IShellFolder> = desktop.BindToObject(bin_id, None);
        CoTaskMemFree(Some(bin_id as *const c_void));
        let Ok(bin) = bin else { return 0 };
        let mut en: Option<IEnumIDList> = None;
        let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | SHCONTF_INCLUDEHIDDEN.0) as u32;
        if bin.EnumObjects(owner, flags, &mut en).is_err() {
            return 0;
        }
        let Some(en) = en else { return 0 };
        // original path -> (deleted at, child id); the newest deletion wins.
        let mut best: HashMap<String, (u64, *mut ITEMIDLIST)> = HashMap::new();
        let mut all: Vec<*mut ITEMIDLIST> = Vec::new();
        loop {
            let mut ids = [std::ptr::null_mut::<ITEMIDLIST>(); 1];
            let mut got = 0u32;
            if en.Next(&mut ids, Some(&mut got)).0 != 0 || got == 0 {
                break;
            }
            let child = ids[0];
            all.push(child);
            let Some(parsing) = display(&bin, child, SHGDN_FORPARSING) else { continue };
            let Some((orig, when)) = info_record(Path::new(&parsing)) else { continue };
            let k = key(&orig);
            if wanted.contains_key(&k) && best.get(&k).is_none_or(|b| when > b.0) {
                best.insert(k, (when, child));
            }
        }
        let picked: Vec<*const ITEMIDLIST> = best.values().map(|b| b.1 as *const ITEMIDLIST).collect();
        let mut n = 0;
        if !picked.is_empty() {
            if let Ok(cm) = bin.GetUIObjectOf::<IContextMenu>(owner, &picked, None) {
                if let Ok(m) = CreatePopupMenu() {
                    let _ = cm.QueryContextMenu(m, 0, 1, 0x7FFF, CMF_NORMAL);
                    if crate::shell::invoke_verb(&cm, owner, "undelete") {
                        n = picked.len();
                    }
                    let _ = DestroyMenu(m);
                }
            }
        }
        for id in all {
            CoTaskMemFree(Some(id as *const c_void));
        }
        n
    }
}

fn key(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

unsafe fn display(f: &IShellFolder, id: *const ITEMIDLIST, flags: SHGDNF) -> Option<String> {
    unsafe {
        let mut sr = STRRET::default();
        f.GetDisplayNameOf(id, flags, &mut sr).ok()?;
        let mut out = PWSTR::null();
        StrRetToStrW(&mut sr, Some(id), &mut out).ok()?;
        let s = out.to_string().ok();
        CoTaskMemFree(Some(out.0 as *const c_void));
        s
    }
}

/// The bin keeps each item as `$R…` next to a `$I…` record: version, size, deletion time,
/// then the original path (version 2: length-prefixed; version 1: fixed 260 chars).
fn info_record(r_path: &Path) -> Option<(PathBuf, u64)> {
    let name = r_path.file_name()?.to_string_lossy().into_owned();
    let rest = name.strip_prefix("$R")?;
    let data = std::fs::read(r_path.with_file_name(format!("$I{rest}"))).ok()?;
    parse_info(&data)
}

fn parse_info(d: &[u8]) -> Option<(PathBuf, u64)> {
    let u64_at = |o: usize| d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    let version = u64_at(0)?;
    let when = u64_at(16)?;
    let units: Vec<u16> = match version {
        2 => {
            let len = u32::from_le_bytes(d.get(24..28)?.try_into().ok()?) as usize;
            d.get(28..28 + len * 2)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect()
        }
        1 => d.get(24..24 + 520)?.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect(),
        _ => return None,
    };
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    Some((PathBuf::from(String::from_utf16_lossy(&units[..end])), when))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_v2(path: &str, when: u64) -> Vec<u8> {
        let units: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut d = Vec::new();
        d.extend(2u64.to_le_bytes());
        d.extend(1234u64.to_le_bytes());
        d.extend(when.to_le_bytes());
        d.extend((units.len() as u32).to_le_bytes());
        for u in units {
            d.extend(u.to_le_bytes());
        }
        d
    }

    #[test]
    fn parses_bin_records() {
        let (p, w) = parse_info(&record_v2(r"C:\Users\x\Žluťoučký kůň.txt", 99)).unwrap();
        assert_eq!(p, PathBuf::from(r"C:\Users\x\Žluťoučký kůň.txt"));
        assert_eq!(w, 99);
        let mut v1 = vec![0u8; 24 + 520];
        v1[0] = 1;
        v1[16] = 7;
        for (i, u) in r"D:\a.txt".encode_utf16().enumerate() {
            v1[24 + i * 2..26 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        assert_eq!(parse_info(&v1).unwrap(), (PathBuf::from(r"D:\a.txt"), 7));
        assert!(parse_info(&[1, 2, 3]).is_none());
        let mut bad = record_v2("C:\\x", 1);
        bad[24] = 0xFF; // length past the end
        assert!(parse_info(&bad).is_none());
    }

    #[test]
    fn inverse_round_trips() {
        let ops = [
            Op::Rename { from: "a".into(), to: "b".into() },
            Op::Created(vec!["x".into()]),
            Op::Moved(vec![("a".into(), "d/a".into())]),
            Op::Group(vec![Op::Created(vec!["f".into()]), Op::Moved(vec![("a".into(), "f/a".into())])]),
        ];
        for op in ops {
            assert_eq!(op.inverse().inverse(), op);
        }
    }
}
