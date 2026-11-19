//! Race-safe file and directory creation with unique numeric suffixes.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::name::{format_candidate, validate_name};

/// Maximum number of suffix attempts before giving up with an error.
pub const MAX_CREATE_ATTEMPTS: usize = 10_000;

/// Creates a new empty file in `dir` named `stem.ext` (or `stem` if `ext` is empty),
/// retrying with ` (2)`, ` (3)`, ... only on `AlreadyExists`.
///
/// Uses `OpenOptions::create_new(true)` so it is race-safe and never overwrites
/// an existing file. Returns an error after [`MAX_CREATE_ATTEMPTS`] collisions.
pub fn create_new_file(dir: &Path, stem: &str, ext: &str) -> io::Result<PathBuf> {
    create_unique(dir, stem, ext, |path| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(|_| ())
    })
}

/// Creates a new directory in `dir` named `stem`, retrying with ` (2)`, ` (3)`, ...
/// only on `AlreadyExists`.
///
/// Uses `std::fs::create_dir` so it is race-safe and never overwrites an existing
/// entry. Returns an error after [`MAX_CREATE_ATTEMPTS`] collisions.
pub fn create_new_dir(dir: &Path, stem: &str) -> io::Result<PathBuf> {
    create_unique(dir, stem, "", |path| fs::create_dir(path))
}

fn create_unique(
    dir: &Path,
    stem: &str,
    ext: &str,
    mut op: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    for attempt in 1..=MAX_CREATE_ATTEMPTS {
        let candidate = format_candidate(stem, ext, attempt);
        validate_name(&candidate)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let path = dir.join(&candidate);
        match op(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("nepodařilo se vytvořit unikátní položku ani po {MAX_CREATE_ATTEMPTS} pokusech"),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{create_new_dir, create_new_file};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time went backwards")
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("fm_fs_test_{tag}_{}_{nanos}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn create_new_file_preserves_existing_and_increments() {
        let dir = TempDir::new("preserve");
        let p1 = create_new_file(&dir.path, "Nový soubor", "txt").expect("first file");
        assert_eq!(
            p1.file_name().and_then(|n| n.to_str()),
            Some("Nový soubor.txt")
        );

        let sentinel = b"original bytes must not be overwritten";
        fs::write(&p1, sentinel).expect("write sentinel");

        let p2 = create_new_file(&dir.path, "Nový soubor", "txt").expect("second file");
        assert_eq!(
            p2.file_name().and_then(|n| n.to_str()),
            Some("Nový soubor (2).txt")
        );
        assert_eq!(fs::read(&p1).expect("read p1"), sentinel);
        assert!(fs::read(&p2).expect("read p2").is_empty());

        let d1 = create_new_dir(&dir.path, "Nová složka").expect("first dir");
        assert_eq!(d1.file_name().and_then(|n| n.to_str()), Some("Nová složka"));
        let d2 = create_new_dir(&dir.path, "Nová složka").expect("second dir");
        assert_eq!(
            d2.file_name().and_then(|n| n.to_str()),
            Some("Nová složka (2)")
        );
    }

    #[test]
    fn create_new_file_concurrent_8_threads() {
        let dir = TempDir::new("concurrent");
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::with_capacity(8);

        for _ in 0..8 {
            let b = Arc::clone(&barrier);
            let p = dir.path.clone();
            handles.push(thread::spawn(move || {
                b.wait();
                create_new_file(&p, "Nový soubor", "txt").expect("concurrent create")
            }));
        }

        let created: HashSet<PathBuf> = handles
            .into_iter()
            .map(|h| h.join().expect("thread join"))
            .collect();
        assert_eq!(created.len(), 8);
    }

    #[test]
    fn create_new_file_rejects_missing_parent_immediately() {
        let dir = TempDir::new("missing");
        let missing = dir.path.join("does_not_exist");
        let err = create_new_file(&missing, "Nový soubor", "txt").expect_err("should fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
