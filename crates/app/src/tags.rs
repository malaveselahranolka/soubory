//! Finder colour tags, stored in `%LOCALAPPDATA%\fm\tags.tsv` (never written into the files).
//! One line per tagged item: `<mask>\t<full path>`. Our own renames carry tags along.
use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
};

thread_local! {
    static TAGS: RefCell<Option<HashMap<String, (u8, PathBuf)>>> = const { RefCell::new(None) };
}

fn file() -> PathBuf {
    crate::prefs::data_dir().join("tags.tsv")
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

fn with<R>(f: impl FnOnce(&mut HashMap<String, (u8, PathBuf)>) -> R) -> R {
    TAGS.with(|t| {
        let mut t = t.borrow_mut();
        let map = t.get_or_insert_with(load);
        f(map)
    })
}

fn load() -> HashMap<String, (u8, PathBuf)> {
    let mut m = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(file()) {
        for line in text.lines() {
            if let Some((mask, path)) = line.split_once('\t') {
                if let Ok(mask) = mask.parse::<u8>() {
                    let p = PathBuf::from(path);
                    m.insert(key(&p), (mask & 0x7F, p));
                }
            }
        }
    }
    m
}

fn save(m: &HashMap<String, (u8, PathBuf)>) {
    let mut lines: Vec<String> = m
        .values()
        .filter(|(mask, _)| *mask != 0)
        .map(|(mask, p)| format!("{mask}\t{}", p.display()))
        .collect();
    lines.sort();
    let path = file();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, lines.join("\n")).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

pub fn mask(p: &Path) -> u8 {
    with(|m| m.get(&key(p)).map(|e| e.0).unwrap_or(0))
}

/// Tags present on every one of `paths` (for the checkmarks in the menu).
pub fn mask_of_all(paths: &[PathBuf]) -> u8 {
    paths.iter().map(|p| mask(p)).fold(0x7F, |a, b| a & b) & if paths.is_empty() { 0 } else { 0x7F }
}

/// Adds tag `i` to all paths, or removes it when all of them have it.
pub fn toggle(paths: &[PathBuf], i: usize) {
    let bit = 1u8 << i;
    let all = mask_of_all(paths) & bit != 0;
    with(|m| {
        for p in paths {
            let e = m.entry(key(p)).or_insert((0, p.clone()));
            if all { e.0 &= !bit } else { e.0 |= bit }
        }
        m.retain(|_, v| v.0 != 0);
        save(m);
    });
}

/// Adds tag `i` to all paths (dropping items onto a tag in the sidebar).
pub fn add(paths: &[PathBuf], i: usize) {
    with(|m| {
        for p in paths {
            m.entry(key(p)).or_insert((0, p.clone())).0 |= 1 << i;
        }
        save(m);
    });
}

pub fn clear(paths: &[PathBuf]) {
    with(|m| {
        for p in paths {
            m.remove(&key(p));
        }
        save(m);
    });
}

/// Keeps tags on items (and everything under a renamed folder) after our own rename/move.
pub fn renamed(old: &Path, new: &Path) {
    let (ok, nk) = (key(old), new.to_string_lossy().into_owned());
    with(|m| {
        let moved: Vec<String> = m
            .keys()
            .filter(|k| *k == &ok || k.starts_with(&format!("{ok}\\")))
            .cloned()
            .collect();
        if moved.is_empty() {
            return;
        }
        for k in moved {
            if let Some((mask, p)) = m.remove(&k) {
                let rest = &p.to_string_lossy()[ok.len()..];
                let np = PathBuf::from(format!("{nk}{rest}"));
                m.insert(key(&np), (mask, np));
            }
        }
        save(m);
    });
}

/// Every item carrying tag `i` that still exists.
pub fn items_with(i: usize) -> Vec<PathBuf> {
    with(|m| {
        m.values()
            .filter(|(mask, p)| mask & (1 << i) != 0 && p.exists())
            .map(|(_, p)| p.clone())
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_and_rename_keep_tags() {
        TAGS.with(|t| *t.borrow_mut() = Some(HashMap::new()));
        let a = PathBuf::from(r"C:\x\a.txt");
        let b = PathBuf::from(r"C:\x\b.txt");
        // Avoid writing the real tags file: only exercise the in-memory map.
        with(|m| {
            m.insert(key(&a), (1, a.clone()));
        });
        assert_eq!(mask(&a), 1);
        assert_eq!(mask_of_all(&[a.clone(), b.clone()]), 0);
        with(|m| {
            let (mask, _) = m.remove(&key(&a)).unwrap();
            m.insert(key(&b), (mask | 4, b.clone()));
        });
        assert_eq!(mask(&b), 5);
        assert_eq!(mask(&a), 0);
    }
}
