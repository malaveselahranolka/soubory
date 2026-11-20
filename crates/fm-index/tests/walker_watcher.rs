#![cfg(windows)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fm_fs::attrs::{
    ARCHIVE, DIRECTORY, HIDDEN, OFFLINE, RECALL_ON_DATA_ACCESS, RECALL_ON_OPEN, REPARSE_POINT,
};
use fm_index::win::service::{Event, Service, default_roots};
use fm_index::win::walker::{
    DirEntryRef, DirSource, MAX_WALK_DEPTH, RootInfo, WalkBatch, WalkOptions, Win32DirSource,
    merge_index, merge_index_with_report, walk_root, walk_with_source, walk_with_source_report,
    walk_with_source_streaming,
};
use fm_index::win::watcher::{
    NoopWatcherFs, NotifyAction, NotifyRecord, VolumeWatcher, WatcherConfig, WatcherFs,
    Win32WatcherFs, apply_records,
};
use fm_index::{CancelToken, EntryId, Index, SearchOptions, parse_query};
use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, SetFileAttributesW};
use windows::core::PCWSTR;

static TEST_SEQ: AtomicUsize = AtomicUsize::new(1);

struct TempTree {
    path: PathBuf,
    junctions: Vec<PathBuf>,
}

impl TempTree {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("fm_idx_{tag}_{}_{nanos}_{seq}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        Self {
            path,
            junctions: Vec::new(),
        }
    }

    fn path_str(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        for j in &self.junctions {
            let _ = fs::remove_dir(j);
        }
        let ext = to_ext_path(&self.path);
        let _ = fs::remove_dir_all(ext);
    }
}

fn to_ext_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('/', "\\");
    if s.starts_with("\\\\?\\") {
        PathBuf::from(s)
    } else if let Some(unc) = s.strip_prefix("\\\\") {
        PathBuf::from(format!("\\\\?\\UNC\\{unc}"))
    } else {
        PathBuf::from(format!("\\\\?\\{s}"))
    }
}

fn set_hidden(p: &Path) {
    let ext = to_ext_path(p);
    let mut w: Vec<u16> = ext.to_string_lossy().encode_utf16().collect();
    w.push(0);
    unsafe {
        SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_FLAGS_AND_ATTRIBUTES(HIDDEN))
            .expect("SetFileAttributesW(HIDDEN)");
    }
}

fn collect_std_fs_recursive(cur: &Path, rel_prefix: &str, out: &mut HashSet<(String, bool, u64)>) {
    let ext_cur = to_ext_path(cur);
    for entry_res in fs::read_dir(&ext_cur).expect("read_dir") {
        let entry = entry_res.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_path = cur.join(&name);
        let meta = fs::symlink_metadata(to_ext_path(&child_path)).expect("symlink_metadata");
        let attrs = meta.file_attributes();
        let is_dir = (attrs & DIRECTORY) != 0;
        let is_reparse = (attrs & REPARSE_POINT) != 0;
        let size = if is_dir { 0 } else { meta.len() };
        let rel = if rel_prefix.is_empty() {
            name.clone()
        } else {
            format!("{rel_prefix}\\{name}")
        };
        out.insert((rel.clone(), is_dir, size));
        if is_dir && !is_reparse {
            collect_std_fs_recursive(&child_path, &rel, out);
        }
    }
}

fn index_rel_triples(idx: &Index, root_str: &str) -> HashSet<(String, bool, u64)> {
    let root_id = idx.lookup(root_str).expect("root in index");
    let root_path = idx.path(root_id);
    let prefix = if root_path.ends_with('\\') {
        root_path
    } else {
        format!("{root_path}\\")
    };
    idx.iter_ids()
        .filter(|&id| id != root_id)
        .map(|id| {
            let full = idx.path(id);
            let rel = full.strip_prefix(&prefix).unwrap_or(&full).to_owned();
            let is_dir = idx.is_dir(id);
            let size = if is_dir { 0 } else { idx.meta(id).size };
            (rel, is_dir, size)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComparableEntry {
    is_dir: bool,
    size: u64,
    mtime: i64,
    parent_rel: String,
    file_id: u64,
}

fn comparable_tree(idx: &Index, root_str: &str) -> BTreeMap<String, ComparableEntry> {
    let root_id = idx.lookup(root_str).expect("root in index");
    let root_path = idx.path(root_id);
    let prefix = if root_path.ends_with('\\') {
        root_path
    } else {
        format!("{root_path}\\")
    };
    let rel_of = |id: EntryId| -> String {
        if id == root_id {
            String::new()
        } else {
            let p = idx.path(id);
            p.strip_prefix(&prefix).unwrap_or(&p).to_owned()
        }
    };

    let mut map = BTreeMap::new();
    for id in idx.iter_ids() {
        if id == root_id {
            continue;
        }
        let parent_id = idx.parent(id).expect("non-root has parent");
        let meta = idx.meta(id);
        let is_dir = idx.is_dir(id);
        map.insert(
            rel_of(id),
            ComparableEntry {
                is_dir,
                size: if is_dir { 0 } else { meta.size },
                mtime: meta.mtime,
                parent_rel: rel_of(parent_id),
                file_id: meta.file_id,
            },
        );
    }
    map
}

#[test]
fn acceptance_1_walker_matches_std_fs_with_junction_loop_long_path_diacritics_and_hidden() {
    let mut tree = TempTree::new("acc1");
    let docs = tree.path.join("Dokumenty");
    fs::create_dir_all(&docs).expect("docs dir");

    // Diacritics filename
    let cz_file = docs.join("Žluťoučký kůň.txt");
    fs::write(
        &cz_file,
        b"P\xc5\x99\xc3\xadli\xc5\xa1 \xc5\xbelu\xc5\xa5ou\xc4\x8dk\xc3\xbd k\xc5\xaf\xc5\x88",
    )
    .expect("write cz");

    // Hidden file
    let hidden_file = tree.path.join("tajny_soubor.txt");
    fs::write(&hidden_file, b"secret bytes").expect("write hidden");
    set_hidden(&hidden_file);

    // Path > 260 characters using \\?\
    let mut deep = tree.path.clone();
    for i in 0..8 {
        deep = deep.join(format!("velmi_dlouha_slozka_cislo_{i:02}_abcdefghij"));
    }
    let ext_deep = to_ext_path(&deep);
    fs::create_dir_all(&ext_deep).expect("create >260 dir");
    let deep_file = deep.join("soubor_nad_260_znaku_test.txt");
    assert!(
        deep_file.to_string_lossy().len() > 260,
        "expected path > 260 chars, got {}",
        deep_file.to_string_lossy().len()
    );
    fs::write(to_ext_path(&deep_file), b"long path payload 123").expect("write deep file");

    // Junction loop: Dokumenty\loop_to_root -> tree.path
    let junction = docs.join("loop_to_root");
    let mklink = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&tree.path)
        .output();
    match mklink {
        Ok(out) if out.status.success() => {
            tree.junctions.push(junction.clone());
        }
        Ok(out) => {
            eprintln!(
                "SKIPPING acceptance_1_walker_matches_std_fs_with_junction_loop_long_path_diacritics_and_hidden: cmd /c mklink /J failed (status {:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        Err(err) => {
            eprintln!(
                "SKIPPING acceptance_1_walker_matches_std_fs_with_junction_loop_long_path_diacritics_and_hidden: failed to run mklink: {err}"
            );
            return;
        }
    }

    let mut expected = HashSet::new();
    collect_std_fs_recursive(&tree.path, "", &mut expected);

    let root_str = tree.path_str();
    let cancel = AtomicBool::new(false);
    let (idx, stats) =
        walk_root(&root_str, WalkOptions::default(), &cancel, |_| {}).expect("walk succeeds");
    assert_eq!(stats.denied, 0);
    assert_eq!(stats.errors, 0);

    let actual = index_rel_triples(&idx, &root_str);
    assert_eq!(actual, expected, "walker tree must match std::fs::read_dir");
    assert!(actual.contains(&("Dokumenty\\loop_to_root".to_owned(), true, 0)));
    assert!(!actual.iter().any(|(p, _, _)| p.contains("loop_to_root\\")));

    let tok = CancelToken::new();
    assert_eq!(
        idx.search(
            &parse_query("tajny_soubor"),
            &SearchOptions {
                include_hidden: false,
                limit: 10,
            },
            &tok,
        )
        .unwrap()
        .total_matches,
        0
    );
    assert_eq!(
        idx.search(
            &parse_query("tajny_soubor"),
            &SearchOptions {
                include_hidden: true,
                limit: 10,
            },
            &tok,
        )
        .unwrap()
        .total_matches,
        1
    );
    assert_eq!(
        idx.search(
            &parse_query("zlutoucky kun"),
            &SearchOptions::default(),
            &tok
        )
        .unwrap()
        .total_matches,
        1
    );
}
#[derive(Clone)]
struct MockEntry {
    name: String,
    attrs: u32,
    size: u64,
    mtime: i64,
    file_id: u64,
    reparse_tag: u32,
}

enum MockNodeKind {
    Dir(Vec<MockEntry>),
    PartialError {
        first_batch: Vec<MockEntry>,
        err: io::ErrorKind,
    },
    FirstReadError(io::ErrorKind),
    Error(io::ErrorKind),
    File,
}

struct MockDirSource {
    nodes: HashMap<String, MockNodeKind>,
    root_info: RootInfo,
    dir_opens: Mutex<HashMap<String, usize>>,
    file_opens: AtomicUsize,
    reparse_opens: AtomicUsize,
    batch_size: usize,
}

impl MockDirSource {
    fn new(root_info: RootInfo, batch_size: usize) -> Self {
        Self {
            nodes: HashMap::new(),
            root_info,
            dir_opens: Mutex::new(HashMap::new()),
            file_opens: AtomicUsize::new(0),
            reparse_opens: AtomicUsize::new(0),
            batch_size: batch_size.max(1),
        }
    }
}

struct MockHandle {
    path: String,
    cursor: Mutex<usize>,
}

impl DirSource for MockDirSource {
    type Handle = MockHandle;
    type Scratch = ();

    fn open_dir(&self, path: &str) -> io::Result<Self::Handle> {
        let norm = path.to_owned();
        match self.nodes.get(&norm) {
            Some(MockNodeKind::File) => {
                self.file_opens.fetch_add(1, Ordering::Relaxed);
                Err(io::Error::other("attempted to open a file"))
            }
            Some(MockNodeKind::Error(kind)) => {
                *self.dir_opens.lock().unwrap().entry(norm).or_insert(0) += 1;
                Err(io::Error::from(*kind))
            }
            Some(
                MockNodeKind::Dir(_)
                | MockNodeKind::PartialError { .. }
                | MockNodeKind::FirstReadError(_),
            ) => {
                *self
                    .dir_opens
                    .lock()
                    .unwrap()
                    .entry(norm.clone())
                    .or_insert(0) += 1;
                Ok(MockHandle {
                    path: norm,
                    cursor: Mutex::new(0),
                })
            }
            None => Err(io::Error::from(io::ErrorKind::NotFound)),
        }
    }

    fn root_info(&self, _dir: &Self::Handle) -> io::Result<RootInfo> {
        Ok(self.root_info)
    }

    fn read_batch(
        &self,
        dir: &Self::Handle,
        restart: bool,
        _scratch: &mut Self::Scratch,
        emit: &mut dyn FnMut(DirEntryRef<'_>),
    ) -> io::Result<bool> {
        let mut cur = dir.cursor.lock().unwrap();
        if restart {
            *cur = 0;
        }
        let entries: &[MockEntry] = match self.nodes.get(&dir.path) {
            Some(MockNodeKind::Dir(entries)) => entries,
            Some(MockNodeKind::FirstReadError(kind)) => return Err(io::Error::from(*kind)),
            Some(MockNodeKind::PartialError { first_batch, err }) => {
                if *cur == 0 {
                    for e in first_batch {
                        emit(DirEntryRef {
                            name: &e.name,
                            attrs: e.attrs,
                            size: e.size,
                            mtime: e.mtime,
                            file_id: e.file_id,
                            reparse_tag: e.reparse_tag,
                        });
                    }
                    *cur = first_batch.len().max(1);
                    return Ok(true);
                }
                return Err(io::Error::from(*err));
            }
            _ => return Err(io::Error::from(io::ErrorKind::NotFound)),
        };
        if *cur >= entries.len() {
            return Ok(false);
        }
        let end = (*cur + self.batch_size).min(entries.len());
        for e in &entries[*cur..end] {
            if (e.attrs & DIRECTORY) != 0
                && (e.attrs & REPARSE_POINT) != 0
                && (e.reparse_tag & 0xFFFF_0FFF) != 0x9000_001A
            {
                let child_path = format!("{}\\{}", dir.path.trim_end_matches('\\'), e.name);
                if self.dir_opens.lock().unwrap().contains_key(&child_path) {
                    self.reparse_opens.fetch_add(1, Ordering::Relaxed);
                }
            }
            emit(DirEntryRef {
                name: &e.name,
                attrs: e.attrs,
                size: e.size,
                mtime: e.mtime,
                file_id: e.file_id,
                reparse_tag: e.reparse_tag,
            });
        }
        *cur = end;
        Ok(true)
    }
}

#[test]
fn acceptance_2_mock_dir_source_10k_one_open_per_dir_zero_file_opens() {
    let mut mock = MockDirSource::new(
        RootInfo {
            volume_serial: 0x1234_5678,
            file_id: 1,
            attrs: DIRECTORY,
            mtime: 100,
        },
        32,
    );

    let mut next_fid = 2u64;
    let mut root_children = Vec::new();
    let mut expected_walked_dirs = HashSet::from(["M:\\".to_owned()]);
    let mut expected_entries = 1usize; // root

    // 100 normal dirs, 10 cloud folders (walked), 10 junctions (NOT walked)
    for d in 0..120usize {
        let (name, attrs, tag, is_walked) = if d < 100 {
            (format!("dir_{d:03}"), DIRECTORY, 0, true)
        } else if d < 110 {
            // Cloud folder family IO_REPARSE_TAG_CLOUD + nibble
            let ctag = 0x9000_001A | (((d - 100) as u32) << 12);
            (
                format!("cloud_{d:03}"),
                DIRECTORY | REPARSE_POINT,
                ctag,
                true,
            )
        } else {
            // Junction -> must be recorded, NEVER opened or descended
            (
                format!("junction_{d:03}"),
                DIRECTORY | REPARSE_POINT,
                0xA000_0003,
                false,
            )
        };
        let d_fid = next_fid;
        next_fid += 1;
        root_children.push(MockEntry {
            name: name.clone(),
            attrs,
            size: 0,
            mtime: 200,
            file_id: d_fid,
            reparse_tag: tag,
        });
        expected_entries += 1;

        let dir_path = format!("M:\\{name}");
        let mut sub_entries = Vec::new();
        for f in 0..90usize {
            let fname = format!("file_{d:03}_{f:03}.txt");
            let f_attrs = if f % 10 == 0 {
                ARCHIVE | OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS
            } else {
                ARCHIVE
            };
            let f_fid = next_fid;
            next_fid += 1;
            sub_entries.push(MockEntry {
                name: fname.clone(),
                attrs: f_attrs,
                size: (f as u64 + 1) * 17,
                mtime: 300 + f as i64,
                file_id: f_fid,
                reparse_tag: 0,
            });
            mock.nodes
                .insert(format!("{dir_path}\\{fname}"), MockNodeKind::File);
            if is_walked {
                expected_entries += 1;
            }
        }
        mock.nodes
            .insert(dir_path.clone(), MockNodeKind::Dir(sub_entries));
        if is_walked {
            expected_walked_dirs.insert(dir_path);
        }
    }

    // Add 1 denied dir and 1 vanished dir under root
    root_children.push(MockEntry {
        name: "denied_dir".to_owned(),
        attrs: DIRECTORY,
        size: 0,
        mtime: 200,
        file_id: next_fid,
        reparse_tag: 0,
    });
    mock.nodes.insert(
        "M:\\denied_dir".to_owned(),
        MockNodeKind::Error(io::ErrorKind::PermissionDenied),
    );
    expected_walked_dirs.insert("M:\\denied_dir".to_owned());
    expected_entries += 1;
    next_fid += 1;

    root_children.push(MockEntry {
        name: "vanished_dir".to_owned(),
        attrs: DIRECTORY,
        size: 0,
        mtime: 200,
        file_id: next_fid,
        reparse_tag: 0,
    });
    mock.nodes.insert(
        "M:\\vanished_dir".to_owned(),
        MockNodeKind::Error(io::ErrorKind::NotFound),
    );
    expected_walked_dirs.insert("M:\\vanished_dir".to_owned());
    expected_entries += 1;

    mock.nodes
        .insert("M:\\".to_owned(), MockNodeKind::Dir(root_children));

    let cancel = AtomicBool::new(false);
    let (idx, stats) = walk_with_source(&mock, "M:\\", WalkOptions::default(), &cancel, |_| {})
        .expect("mock walk succeeds");

    assert!(
        idx.len() >= 10_000,
        "expected >=10k entries, got {}",
        idx.len()
    );
    assert_eq!(idx.len(), expected_entries);
    assert_eq!(stats.dirs + stats.files, idx.len());
    assert_eq!(stats.denied, 1);
    assert_eq!(stats.errors, 1);
    assert_eq!(mock.file_opens.load(Ordering::Relaxed), 0);
    assert_eq!(mock.reparse_opens.load(Ordering::Relaxed), 0);

    let opens = mock.dir_opens.lock().unwrap();
    for j in 110..120 {
        let jp = format!("M:\\junction_{j:03}");
        assert!(
            !opens.contains_key(&jp),
            "junction {jp} must never be opened"
        );
    }
    assert_eq!(opens.len(), expected_walked_dirs.len());
    for (dir_path, &count) in opens.iter() {
        assert!(
            expected_walked_dirs.contains(dir_path),
            "unexpected open of {dir_path}"
        );
        assert_eq!(count, 1, "directory {dir_path} opened {count} times");
    }
    assert!(idx.lookup("M:\\cloud_105\\file_105_001.txt").is_some());
    assert!(idx.lookup("M:\\junction_115").is_some());
    assert_eq!(idx.lookup("M:\\junction_115\\file_115_001.txt"), None);
}

#[test]
fn walker_depth_cap_512_and_cancellation() {
    let mut mock = MockDirSource::new(
        RootInfo {
            volume_serial: 1,
            file_id: 1,
            attrs: DIRECTORY,
            mtime: 1,
        },
        16,
    );
    let mut cur = "D:\\".to_owned();
    for depth in 1..=520u64 {
        let name = format!("d{depth}");
        mock.nodes.insert(
            cur.clone(),
            MockNodeKind::Dir(vec![MockEntry {
                name: name.clone(),
                attrs: DIRECTORY,
                size: 0,
                mtime: 1,
                file_id: depth + 1,
                reparse_tag: 0,
            }]),
        );
        cur = if cur.ends_with('\\') {
            format!("{cur}{name}")
        } else {
            format!("{cur}\\{name}")
        };
    }
    mock.nodes.insert(cur, MockNodeKind::Dir(Vec::new()));

    let cancel = AtomicBool::new(false);
    let (idx, stats) = walk_with_source(&mock, "D:\\", WalkOptions::default(), &cancel, |_| {})
        .expect("walk completes");
    assert_eq!(stats.dirs, 1 + MAX_WALK_DEPTH as usize);
    assert_eq!(idx.len(), 1 + MAX_WALK_DEPTH as usize);
    assert!(idx.by_file_id(1 + MAX_WALK_DEPTH as u64).is_some());
    assert_eq!(idx.by_file_id(2 + MAX_WALK_DEPTH as u64), None);

    let pre_cancelled = AtomicBool::new(true);
    assert!(
        walk_with_source(
            &mock,
            "D:\\",
            WalkOptions::default(),
            &pre_cancelled,
            |_| {}
        )
        .is_none()
    );
}
#[test]
fn acceptance_3_watcher_live_ops_and_2000_file_storm() {
    let tree = TempTree::new("acc3");
    let sub_a = tree.path.join("SubA");
    let sub_b = tree.path.join("SubB");
    let cz_dir = tree.path.join("Žluťoučký");
    fs::create_dir_all(&sub_a).unwrap();
    fs::create_dir_all(&sub_b).unwrap();
    fs::create_dir_all(&cz_dir).unwrap();
    fs::write(sub_a.join("initial.txt"), b"hello").unwrap();
    fs::write(sub_a.join("to_delete.txt"), b"bye").unwrap();
    fs::write(sub_a.join("case_name.txt"), b"case").unwrap();

    let root_str = tree.path_str();
    let cancel = AtomicBool::new(false);
    let (init_idx, _) = walk_root(&root_str, WalkOptions::default(), &cancel, |_| {}).unwrap();
    let live = Arc::new(RwLock::new(init_idx));

    let overflowed = Arc::new(AtomicBool::new(false));
    let overflowed_cb = Arc::clone(&overflowed);
    let watcher = VolumeWatcher::start(
        root_str.clone(),
        Arc::clone(&live),
        WatcherConfig::default(),
        Box::new(|_, _| {}),
        Box::new(move || {
            overflowed_cb.store(true, Ordering::Release);
        }),
    )
    .expect("start watcher");

    // 1. Individual creates, modifies, renames (same-dir, cross-dir, case-only), deletes
    let f_new = sub_a.join("created.txt");
    {
        let mut f = File::create(&f_new).unwrap();
        f.write_all(b"initial payload").unwrap();
        f.sync_all().unwrap();
    }
    fs::write(
        sub_a.join("initial.txt"),
        b"updated payload with longer content",
    )
    .unwrap();
    fs::rename(sub_a.join("case_name.txt"), sub_a.join("CASE_NAME.txt")).unwrap();
    fs::rename(&f_new, sub_b.join("moved_created.txt")).unwrap();
    fs::remove_file(sub_a.join("to_delete.txt")).unwrap();

    let dir_tmp = tree.path.join("TempDir");
    fs::create_dir_all(&dir_tmp).unwrap();
    fs::write(dir_tmp.join("inside.txt"), b"123456").unwrap();
    fs::rename(&dir_tmp, tree.path.join("RenamedDir")).unwrap();

    // 2. 2,000-file storm across subdirectories
    let storm_dir = tree.path.join("Storm");
    fs::create_dir_all(&storm_dir).unwrap();
    for i in 0..2_000usize {
        let p = storm_dir.join(format!("storm_{i:04}.dat"));
        fs::write(&p, format!("payload_{i}")).unwrap();
    }
    for i in 0..200usize {
        fs::write(
            storm_dir.join(format!("storm_{i:04}.dat")),
            b"modified_storm_bytes_123456",
        )
        .unwrap();
    }
    for i in 200..300usize {
        fs::rename(
            storm_dir.join(format!("storm_{i:04}.dat")),
            storm_dir.join(format!("renamed_{i:04}.dat")),
        )
        .unwrap();
    }
    for i in 300..450usize {
        fs::remove_file(storm_dir.join(format!("storm_{i:04}.dat"))).unwrap();
    }

    let storm_end = Instant::now();
    let (fresh_idx, _) = walk_root(&root_str, WalkOptions::default(), &cancel, |_| {}).unwrap();
    let expected = comparable_tree(&fresh_idx, &root_str);

    let mut matched = false;
    let mut last_diff = String::new();
    while storm_end.elapsed() <= Duration::from_millis(500) {
        if overflowed.swap(false, Ordering::AcqRel)
            && let Some((recovered, _)) =
                walk_root(&root_str, WalkOptions::default(), &cancel, |_| {})
        {
            let mut guard = live.write().unwrap();
            fm_index::win::walker::merge_index(&mut guard, recovered);
        }
        let actual = {
            let guard = live.read().unwrap();
            comparable_tree(&guard, &root_str)
        };
        if trees_match_watcher(&expected, &actual) {
            matched = true;
            break;
        }
        last_diff = describe_tree_diff(&expected, &actual);
        thread::sleep(Duration::from_millis(20));
    }

    watcher.stop();
    assert!(
        matched,
        "watcher index did not match fresh walk within 500 ms (elapsed {:?}):\n{last_diff}",
        storm_end.elapsed()
    );
}

fn trees_match_watcher(
    expected: &BTreeMap<String, ComparableEntry>,
    actual: &BTreeMap<String, ComparableEntry>,
) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected.iter().all(|(k, ev)| {
        let Some(av) = actual.get(k) else {
            return false;
        };
        if ev.is_dir != av.is_dir
            || ev.size != av.size
            || ev.parent_rel != av.parent_rel
            || ev.file_id != av.file_id
        {
            return false;
        }
        // On NTFS, FileIdBothDirectoryInfo reads cached $FILE_NAME dir mtimes while
        // ReadDirectoryChangesExW reports $STANDARD_INFORMATION dir mtimes. Files match exactly.
        ev.is_dir || ev.mtime == av.mtime
    })
}

fn describe_tree_diff(
    expected: &BTreeMap<String, ComparableEntry>,
    actual: &BTreeMap<String, ComparableEntry>,
) -> String {
    let mut out = String::new();
    let mut shown = 0usize;
    for (k, v) in expected {
        match actual.get(k) {
            None => {
                if shown < 10 {
                    out.push_str(&format!("  missing in live: {k} ({v:?})\n"));
                    shown += 1;
                }
            }
            Some(av) if av != v && shown < 10 => {
                out.push_str(&format!("  mismatch {k}: expected={v:?}, actual={av:?}\n"));
                shown += 1;
            }
            _ => {}
        }
    }
    for k in actual.keys() {
        if !expected.contains_key(k) && shown < 10 {
            out.push_str(&format!("  extra in live: {k}\n"));
            shown += 1;
        }
    }
    out.push_str(&format!(
        "  counts: expected={}, actual={}",
        expected.len(),
        actual.len()
    ));
    out
}

#[test]
fn acceptance_4_forced_overflow_triggers_rewalk_and_matches_fresh() {
    let tree = TempTree::new("acc4");
    let root_str = tree.path_str();
    let cancel = AtomicBool::new(false);
    let (init_idx, _) = walk_root(&root_str, WalkOptions::default(), &cancel, |_| {}).unwrap();
    let live = Arc::new(RwLock::new(init_idx));

    let overflow_signal = Arc::new((Mutex::new(0usize), Condvar::new()));
    let ov_cb = Arc::clone(&overflow_signal);

    let tiny_cfg = WatcherConfig {
        buffer_bytes: 128,
        coalesce_min: Duration::from_millis(50),
        coalesce_max: Duration::from_millis(100),
        overflow_rewalk_cooldown: Duration::from_millis(50),
    };

    let watcher = VolumeWatcher::start(
        root_str.clone(),
        Arc::clone(&live),
        tiny_cfg,
        Box::new(|_, _| {}),
        Box::new(move || {
            let (mu, cv) = &*ov_cb;
            let mut count = mu.lock().unwrap();
            *count += 1;
            cv.notify_all();
        }),
    )
    .expect("start tiny-buffer watcher");

    // Burst enough mutations to guarantee overflow of the 128-byte buffer
    for i in 0..400usize {
        let p = tree
            .path
            .join(format!("overflow_file_{i:04}_with_long_suffix.txt"));
        fs::write(&p, format!("data_{i}")).unwrap();
    }
    for i in 0..50usize {
        fs::remove_file(
            tree.path
                .join(format!("overflow_file_{i:04}_with_long_suffix.txt")),
        )
        .unwrap();
    }

    // Wait for overflow callback
    {
        let (mu, cv) = &*overflow_signal;
        let mut guard = mu.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while *guard == 0 {
            let rem = deadline.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero(), "expected tiny buffer to trigger overflow");
            let (g, _) = cv.wait_timeout(guard, rem).unwrap();
            guard = g;
        }
    }

    // Wait briefly for filesystem storm notifications to settle, then run the overflow re-walk
    thread::sleep(Duration::from_millis(120));
    let (fresh_idx, _) = walk_root(
        &root_str,
        WalkOptions {
            low_priority: true,
            ..Default::default()
        },
        &cancel,
        |_| {},
    )
    .unwrap();
    let expected = comparable_tree(&fresh_idx, &root_str);
    {
        let mut live_guard = live.write().unwrap();
        merge_index(&mut live_guard, fresh_idx);
        let actual = comparable_tree(&live_guard, &root_str);
        assert_eq!(
            actual, expected,
            "after overflow re-walk live index must equal fresh"
        );
    }
    watcher.stop();

    // Also verify end-to-end Service overflow re-walk with tiny buffer
    let svc_data = TempTree::new("acc4_svc_data");
    let ready_count = Arc::new((Mutex::new(0usize), Condvar::new()));
    let rc_cb = Arc::clone(&ready_count);
    let svc = Service::start_with_watcher_config(
        vec![root_str.clone()],
        svc_data.path.clone(),
        tiny_cfg,
        Box::new(move |ev| {
            if matches!(ev, Event::Ready { .. }) {
                let (mu, cv) = &*rc_cb;
                *mu.lock().unwrap() += 1;
                cv.notify_all();
            }
        }),
    );

    // Wait for initial Ready
    {
        let (mu, cv) = &*ready_count;
        let mut g = mu.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while *g < 1 {
            let rem = deadline.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero(), "initial Ready timeout");
            g = cv.wait_timeout(g, rem).unwrap().0;
        }
    }
    let before_ready = *ready_count.0.lock().unwrap();

    for i in 400..800usize {
        fs::write(
            tree.path.join(format!("svc_overflow_{i:04}_long_name.txt")),
            b"overflow",
        )
        .unwrap();
    }

    let tok = CancelToken::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut hits = Vec::new();
    while Instant::now() < deadline {
        let ready_now = *ready_count.0.lock().unwrap();
        if ready_now > before_ready && !svc.is_scanning() {
            hits = svc
                .search("svc_overflow_0799", None, false, 10, &tok)
                .expect("not cancelled");
            if hits.len() == 1 {
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        hits.len(),
        1,
        "post-overflow Service follow-up walk must index svc_overflow_0799"
    );
    assert_eq!(hits[0].name, "svc_overflow_0799_long_name.txt");
    svc.stop();
}

#[test]
fn acceptance_6_service_search_scope_snapshot_reload_and_corrupt_rebuild() {
    let root_tree = TempTree::new("acc6_root");
    let data_dir = TempTree::new("acc6_data");

    let sub_a = root_tree.path.join("SubA");
    let sub_a_nested = sub_a.join("Nested");
    let sub_a_extra = root_tree.path.join("SubA_Extra");
    let sub_b = root_tree.path.join("SubB");
    let cz_dir = root_tree.path.join("ŽluťoučkáSložka");
    fs::create_dir_all(&sub_a_nested).unwrap();
    fs::create_dir_all(&sub_a_extra).unwrap();
    fs::create_dir_all(&sub_b).unwrap();
    fs::create_dir_all(&cz_dir).unwrap();

    fs::write(sub_a.join("Žluťoučký_faktura.pdf"), b"pdf1").unwrap();
    fs::write(sub_a_nested.join("report_2026.pdf"), b"pdf2").unwrap();
    fs::write(sub_a_extra.join("faktura_outside_scope.pdf"), b"pdf3").unwrap();
    fs::write(sub_b.join("notes.txt"), b"txt1").unwrap();
    fs::write(cz_dir.join("dokument.md"), b"md1").unwrap();
    let hidden_p = root_tree.path.join("skryty_soubor.txt");
    fs::write(&hidden_p, b"secret").unwrap();
    set_hidden(&hidden_p);

    let root_str = root_tree.path_str();
    let wait_ready = |signal: &(Mutex<bool>, Condvar)| {
        let (mu, cv) = signal;
        let mut g = mu.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !*g {
            let rem = deadline.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero(), "timed out waiting for Service Ready");
            g = cv.wait_timeout(g, rem).unwrap().0;
        }
    };

    // 1. First start: scan, search, scope filter, stop (writes snapshot)
    let ready1 = Arc::new((Mutex::new(false), Condvar::new()));
    let r1_cb = Arc::clone(&ready1);
    let svc1 = Service::start(
        vec![root_str.clone()],
        data_dir.path.clone(),
        Box::new(move |ev| {
            if matches!(ev, Event::Ready { .. }) {
                let (mu, cv) = &*r1_cb;
                *mu.lock().unwrap() = true;
                cv.notify_all();
            }
        }),
    );
    wait_ready(&ready1);
    assert!(!svc1.is_scanning());
    assert!(svc1.entries() >= 10);

    let tok = CancelToken::new();
    let folded_hits = svc1
        .search("zlutoucky", None, false, 50, &tok)
        .expect("search");
    assert_eq!(folded_hits.len(), 1);
    assert_eq!(folded_hits[0].name, "Žluťoučký_faktura.pdf");

    let ext_hits = svc1
        .search("ext:pdf", None, false, 50, &tok)
        .expect("ext search");
    assert_eq!(ext_hits.len(), 3);

    // Scope boundary check: SubA must NOT include SubA_Extra or SubA itself
    let sub_a_str = sub_a.to_string_lossy().into_owned();
    let scoped_hits = svc1
        .search("faktura", Some(&sub_a_str), false, 50, &tok)
        .expect("scoped search");
    assert_eq!(scoped_hits.len(), 1);
    assert_eq!(scoped_hits[0].name, "Žluťoučký_faktura.pdf");

    let scoped_all = svc1
        .search("ext:pdf", Some(&sub_a_str), false, 50, &tok)
        .expect("scoped pdf");
    assert_eq!(scoped_all.len(), 2);

    // Unicode case-insensitive scope check
    let cz_scope_lower = cz_dir
        .to_string_lossy()
        .replace("ŽluťoučkáSložka", "žluťoučkásložka");
    let cz_hits = svc1
        .search("dokument", Some(&cz_scope_lower), false, 50, &tok)
        .expect("unicode scope");
    assert_eq!(cz_hits.len(), 1);

    // Hidden toggle
    assert!(
        svc1.search("skryty_soubor", None, false, 10, &tok)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        svc1.search("skryty_soubor", None, true, 10, &tok)
            .unwrap()
            .len(),
        1
    );

    svc1.stop();

    // Locate written snapshot index-<serial>.bin
    let snap_files: Vec<PathBuf> = fs::read_dir(&data_dir.path)
        .unwrap()
        .filter_map(|e| e.ok().map(|x| x.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("index-") && n.ends_with(".bin"))
        })
        .collect();
    assert_eq!(snap_files.len(), 1, "expected 1 snapshot file in data_dir");
    let snap_file = &snap_files[0];
    assert!(fs::metadata(snap_file).unwrap().len() > 48);

    // 2. Second start: loads snapshot synchronously and answers BEFORE re-walk finishes
    let ready2 = Arc::new((Mutex::new(false), Condvar::new()));
    let r2_cb = Arc::clone(&ready2);
    let svc2 = Service::start(
        vec![root_str.clone()],
        data_dir.path.clone(),
        Box::new(move |ev| {
            if matches!(ev, Event::Ready { .. }) {
                let (mu, cv) = &*r2_cb;
                *mu.lock().unwrap() = true;
                cv.notify_all();
            }
        }),
    );
    // Query immediately without waiting for Ready
    let instant_hits = svc2
        .search("zlutoucky", None, false, 50, &tok)
        .expect("instant search from snapshot");
    assert_eq!(instant_hits.len(), 1);
    assert_eq!(instant_hits[0].name, "Žluťoučký_faktura.pdf");

    wait_ready(&ready2);
    svc2.stop();

    // 3. Corrupt snapshot file -> ignored and rebuilt without panic
    let mut bytes = fs::read(snap_file).unwrap();
    let half = bytes.len() / 2;
    bytes[half..].fill(0xAA);
    fs::write(snap_file, &bytes).unwrap();

    let ready3 = Arc::new((Mutex::new(false), Condvar::new()));
    let r3_cb = Arc::clone(&ready3);
    let svc3 = Service::start(
        vec![root_str],
        data_dir.path.clone(),
        Box::new(move |ev| {
            if matches!(ev, Event::Ready { .. }) {
                let (mu, cv) = &*r3_cb;
                *mu.lock().unwrap() = true;
                cv.notify_all();
            }
        }),
    );
    wait_ready(&ready3);
    let rebuilt_hits = svc3
        .search("zlutoucky", None, false, 50, &tok)
        .expect("rebuilt search");
    assert_eq!(rebuilt_hits.len(), 1);
    svc3.stop();

    // Verify rebuilt snapshot loads cleanly
    let mut f = File::open(snap_file).unwrap();
    let reloaded = Index::load(&mut f, None).expect("rebuilt snapshot must be valid");
    assert!(reloaded.len() >= 10);
}

#[test]
fn service_default_roots_returns_fixed_volumes() {
    let roots = default_roots();
    assert!(
        !roots.is_empty(),
        "expected at least one fixed NTFS/ReFS root on Windows"
    );
    for r in &roots {
        assert!(r.ends_with('\\'), "root {r:?} should end with \\");
    }
}
#[test]
fn service_multi_root_ranked_merge_save_cancel_and_rewalk_coalesce() {
    let root_a = TempTree::new("mr_a");
    let root_b = TempTree::new("mr_b");
    let data_dir = TempTree::new("mr_data");

    // Create entries with different match tiers across two roots
    // Exact ("plan") in root_b, Prefix ("plan_prefix.txt") in root_a, WordBoundary ("my_plan.txt") in root_b, Substring ("xplanx.txt") in root_a
    fs::write(root_a.path.join("plan_prefix.txt"), b"p1").unwrap();
    fs::write(root_a.path.join("xplanx.txt"), b"p2").unwrap();
    fs::write(root_b.path.join("plan"), b"p3").unwrap();
    fs::write(root_b.path.join("my_plan.txt"), b"p4").unwrap();

    let ready_count = Arc::new((Mutex::new(0usize), Condvar::new()));
    let rc_cb = Arc::clone(&ready_count);
    let svc = Service::start(
        vec![root_a.path_str(), root_b.path_str()],
        data_dir.path.clone(),
        Box::new(move |ev| {
            if matches!(ev, Event::Ready { .. }) {
                let (mu, cv) = &*rc_cb;
                *mu.lock().unwrap() += 1;
                cv.notify_all();
            }
        }),
    );

    {
        let (mu, cv) = &*ready_count;
        let mut g = mu.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while *g < 2 || svc.is_scanning() {
            let rem = deadline.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero(), "multi-root Ready timeout");
            g = cv.wait_timeout(g, rem).unwrap().0;
        }
    }

    let tok = CancelToken::new();
    let hits = svc
        .search("plan", None, false, 3, &tok)
        .expect("not cancelled");
    let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["plan", "plan_prefix.txt", "my_plan.txt"]);

    // Scope restricted to root_a only returns root_a entries
    let a_str = root_a.path_str();
    let scoped = svc
        .search("plan", Some(&a_str), false, 10, &tok)
        .expect("scoped");
    let scoped_names: Vec<&str> = scoped.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(scoped_names, vec!["plan_prefix.txt", "xplanx.txt"]);

    // Cancelled token returns None
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(svc.search("plan", None, false, 10, &cancelled).is_none());

    // Explicit save + rapid coalesced trigger_rewalk
    svc.save();
    let before = *ready_count.0.lock().unwrap();
    for _ in 0..10 {
        svc.trigger_rewalk();
    }
    {
        let (mu, cv) = &*ready_count;
        let mut g = mu.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while *g < before + 2 || svc.is_scanning() {
            let rem = deadline.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero(), "rewalk Ready timeout");
            g = cv.wait_timeout(g, rem).unwrap().0;
        }
    }
    svc.stop();
}

/// `S:\` with `dirs` subfolders of one file each; every listing takes 8 ms.
struct SlowMockSource {
    dirs: usize,
}
impl DirSource for SlowMockSource {
    type Handle = String;
    type Scratch = ();

    fn open_dir(&self, path: &str) -> io::Result<Self::Handle> {
        Ok(path.to_owned())
    }
    fn root_info(&self, _dir: &Self::Handle) -> io::Result<RootInfo> {
        Ok(RootInfo::default())
    }
    fn read_batch(
        &self,
        dir: &Self::Handle,
        restart: bool,
        _scratch: &mut Self::Scratch,
        emit: &mut dyn FnMut(DirEntryRef<'_>),
    ) -> io::Result<bool> {
        if !restart {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(8));
        if dir == "S:\\" {
            for i in 0..self.dirs {
                let name = format!("d{i:02}");
                emit(DirEntryRef {
                    name: &name,
                    attrs: DIRECTORY,
                    size: 0,
                    mtime: 1,
                    file_id: (i + 2) as u64,
                    reparse_tag: 0,
                });
            }
        } else {
            emit(DirEntryRef {
                name: "item.txt",
                attrs: ARCHIVE,
                size: 10,
                mtime: 1,
                file_id: 0,
                reparse_tag: 0,
            });
        }
        Ok(true)
    }
}

#[test]
fn paced_walk_rests_between_slices_and_lists_the_same() {
    let walk = |duty| {
        let opts = WalkOptions {
            threads: 1,
            duty,
            ..Default::default()
        };
        let t = Instant::now();
        let (idx, _) = walk_with_source(
            &SlowMockSource { dirs: 40 },
            "S:\\",
            opts,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
        (idx.len(), t.elapsed())
    };
    let (full, _) = walk(100);
    let (paced, slow) = walk(40);
    assert_eq!((full, paced), (81, 81));
    // 41 listings of >= 8 ms are >= 328 ms of work. At 40 % duty every 50 ms slice is followed
    // by a rest 1.5x as long, so >= 1.5 x (328 - 50) ms of rests come on top: >= ~745 ms. A lower
    // bound only, so a loaded CPU cannot fail it (a ratio to the flat-out run could).
    assert!(
        slow >= Duration::from_millis(700),
        "paced walk took only {slow:?}"
    );
}

#[test]
fn walker_progress_callback_fires_periodically_during_slow_walk() {
    let mock = SlowMockSource { dirs: 30 };
    let cancel = AtomicBool::new(false);
    let mut progress_samples = Vec::new();
    let opts = WalkOptions {
        threads: 1,
        progress_interval: Duration::from_millis(30),
        ..Default::default()
    };
    let (idx, _) = walk_with_source(&mock, "S:\\", opts, &cancel, |n| {
        progress_samples.push(n);
    })
    .unwrap();
    assert_eq!(idx.len(), 61);
    assert!(
        progress_samples.len() >= 4,
        "expected multiple periodic progress callbacks, got {progress_samples:?}"
    );
    assert_eq!(*progress_samples.last().unwrap(), 61);
}

#[test]
fn walker_failed_root_open_keeps_consistent_stats() {
    let missing = std::env::temp_dir().join(format!(
        "fm_idx_definitely_missing_{}_{}",
        std::process::id(),
        TEST_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let cancel = AtomicBool::new(false);
    let (idx, stats) = walk_root(
        &missing.to_string_lossy(),
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(idx.len(), 0);
    assert_eq!(stats.dirs + stats.files, idx.len());
    assert_eq!(stats.errors, 1);
}

#[test]
fn regression_folder_renamed_during_rewalk_preserves_subtree() {
    let root_info = RootInfo {
        volume_serial: 0x1111_2222,
        file_id: 1,
        attrs: DIRECTORY,
        mtime: 10,
    };

    // 1. Initial walk builds R:\D\P with children c1.txt and c2.txt
    let mut mock_init = MockDirSource::new(root_info, 16);
    mock_init.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "D".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 2,
            reparse_tag: 0,
        }]),
    );
    mock_init.nodes.insert(
        "R:\\D".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "P".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 10,
            reparse_tag: 0,
        }]),
    );
    mock_init.nodes.insert(
        "R:\\D\\P".to_owned(),
        MockNodeKind::Dir(vec![
            MockEntry {
                name: "c1.txt".to_owned(),
                attrs: ARCHIVE,
                size: 11,
                mtime: 20,
                file_id: 11,
                reparse_tag: 0,
            },
            MockEntry {
                name: "c2.txt".to_owned(),
                attrs: ARCHIVE,
                size: 22,
                mtime: 21,
                file_id: 12,
                reparse_tag: 0,
            },
        ]),
    );

    let cancel = AtomicBool::new(false);
    let (mut live, _, _) =
        walk_with_source_report(&mock_init, "R:\\", WalkOptions::default(), &cancel, |_| {})
            .unwrap();
    assert!(live.lookup("R:\\D\\P\\c1.txt").is_some());
    assert!(live.lookup("R:\\D\\P\\c2.txt").is_some());

    // 2. Re-walk: D lists P (fid=10), then P was renamed -> Q so open(R:\D\P) fails NotFound
    let mut mock_rewalk = MockDirSource::new(root_info, 16);
    mock_rewalk.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "D".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 2,
            reparse_tag: 0,
        }]),
    );
    mock_rewalk.nodes.insert(
        "R:\\D".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "P".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 10,
            reparse_tag: 0,
        }]),
    );
    mock_rewalk.nodes.insert(
        "R:\\D\\P".to_owned(),
        MockNodeKind::Error(io::ErrorKind::NotFound),
    );

    let (fresh, _, report) = walk_with_source_report(
        &mock_rewalk,
        "R:\\",
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert!(
        report
            .failed
            .iter()
            .any(|f| f.file_id == 10 && f.rel_path == "D\\P")
    );

    // 3. Merge preserves P's live subtree rather than deleting it for the empty fresh stub
    merge_index_with_report(&mut live, fresh, &report, true);
    assert!(live.lookup("R:\\D\\P\\c1.txt").is_some());
    assert!(live.lookup("R:\\D\\P\\c2.txt").is_some());

    // 4. Replay captured rename P -> Q ends with Q holding all children
    let root_id = live.lookup("R:\\").unwrap();
    let replay = [
        NotifyRecord {
            action: NotifyAction::RenamedOldName,
            rel_path: "D\\P".to_owned(),
            file_id: 10,
            parent_file_id: 2,
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            reparse_tag: 0,
        },
        NotifyRecord {
            action: NotifyAction::RenamedNewName,
            rel_path: "D\\Q".to_owned(),
            file_id: 10,
            parent_file_id: 2,
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            reparse_tag: 0,
        },
    ];
    assert!(apply_records(&mut live, root_id, &replay, &NoopWatcherFs));
    assert_eq!(live.lookup("R:\\D\\P"), None);
    assert!(live.lookup("R:\\D\\Q\\c1.txt").is_some());
    assert!(live.lookup("R:\\D\\Q\\c2.txt").is_some());
}

#[test]
fn regression_root_failure_and_mid_listing_error_keep_live_subtrees() {
    let root_info = RootInfo {
        volume_serial: 0x3333_4444,
        file_id: 1,
        attrs: DIRECTORY,
        mtime: 10,
    };

    let mut mock_init = MockDirSource::new(root_info, 16);
    mock_init.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "M".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 20,
            reparse_tag: 0,
        }]),
    );
    mock_init.nodes.insert(
        "R:\\M".to_owned(),
        MockNodeKind::Dir(vec![
            MockEntry {
                name: "a.txt".to_owned(),
                attrs: ARCHIVE,
                size: 10,
                mtime: 20,
                file_id: 21,
                reparse_tag: 0,
            },
            MockEntry {
                name: "b.txt".to_owned(),
                attrs: ARCHIVE,
                size: 20,
                mtime: 20,
                file_id: 22,
                reparse_tag: 0,
            },
        ]),
    );

    let cancel = AtomicBool::new(false);
    let (mut live, _, _) =
        walk_with_source_report(&mock_init, "R:\\", WalkOptions::default(), &cancel, |_| {})
            .unwrap();
    let expected_len = live.len();

    // 1. Root open failure keeps live unchanged
    let mut mock_root_open_fail = MockDirSource::new(root_info, 16);
    mock_root_open_fail
        .nodes
        .insert("R:\\".to_owned(), MockNodeKind::Error(io::ErrorKind::Other));
    let (fresh1, _, rep1) = walk_with_source_report(
        &mock_root_open_fail,
        "R:\\",
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert!(rep1.root_failed);
    assert!(!merge_index_with_report(&mut live, fresh1, &rep1, true));
    assert_eq!(live.len(), expected_len);
    assert!(live.lookup("R:\\M\\b.txt").is_some());

    // 2. Root first read failure keeps live unchanged
    let mut mock_root_read_fail = MockDirSource::new(root_info, 16);
    mock_root_read_fail.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::FirstReadError(io::ErrorKind::Other),
    );
    let (fresh2, _, rep2) = walk_with_source_report(
        &mock_root_read_fail,
        "R:\\",
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert!(rep2.root_failed);
    assert!(!merge_index_with_report(&mut live, fresh2, &rep2, true));
    assert_eq!(live.len(), expected_len);
    assert!(live.lookup("R:\\M\\b.txt").is_some());

    // 3. Mid-listing error in R:\M preserves M's unseen live children (b.txt)
    let mut mock_mid_err = MockDirSource::new(root_info, 16);
    mock_mid_err.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "M".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 20,
            reparse_tag: 0,
        }]),
    );
    mock_mid_err.nodes.insert(
        "R:\\M".to_owned(),
        MockNodeKind::PartialError {
            first_batch: vec![MockEntry {
                name: "a.txt".to_owned(),
                attrs: ARCHIVE,
                size: 15,
                mtime: 30,
                file_id: 21,
                reparse_tag: 0,
            }],
            err: io::ErrorKind::Other,
        },
    );
    let (fresh3, _, rep3) = walk_with_source_report(
        &mock_mid_err,
        "R:\\",
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert!(!rep3.root_failed);
    assert!(
        rep3.failed
            .iter()
            .any(|f| f.file_id == 20 && f.rel_path == "M")
    );
    merge_index_with_report(&mut live, fresh3, &rep3, true);
    assert_eq!(live.meta(live.lookup("R:\\M\\a.txt").unwrap()).size, 15);
    assert!(
        live.lookup("R:\\M\\b.txt").is_some(),
        "b.txt must survive partial listing failure"
    );
}

#[test]
fn win32_dir_source_rejects_files_and_never_follows_junctions() {
    let mut tree = TempTree::new("win32_dir_only");
    let real_file = tree.path.join("regular.txt");
    let target_dir = tree.path.join("Target");
    fs::write(&real_file, b"payload").unwrap();
    fs::create_dir_all(&target_dir).unwrap();
    fs::write(target_dir.join("inside_target.txt"), b"x").unwrap();

    // Opening a regular file as a directory must fail and never read
    let src = Win32DirSource;
    assert!(src.open_dir(&real_file.to_string_lossy()).is_err());

    // Create a junction and ensure both Win32DirSource and Win32WatcherFs treat it as non-descended
    let junction = tree.path.join("Junc");
    let mklink = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&target_dir)
        .output();
    if let Ok(out) = mklink
        && out.status.success()
    {
        tree.junctions.push(junction.clone());
        let h = src
            .open_dir(&junction.to_string_lossy())
            .expect("reparse skipped handle");
        let mut scratch = <Win32DirSource as DirSource>::Scratch::default();
        let mut count = 0usize;
        let more = src
            .read_batch(&h, true, &mut scratch, &mut |_| {
                count += 1;
            })
            .unwrap();
        assert!(!more);
        assert_eq!(count, 0);

        let children = Win32WatcherFs.list_dir_children(&junction.to_string_lossy());
        assert!(
            children.is_empty(),
            "junction must never be followed by watcher"
        );
    }
}

#[test]
fn service_ready_always_emits_cooldown_cleans_temps_and_missing_root_errors() {
    assert_eq!(
        WatcherConfig::default().overflow_rewalk_cooldown,
        Duration::from_secs(60)
    );

    // 1. Stale temp cleanup + Error event on missing root + fast stop
    let data_dir = TempTree::new("svc_hygiene_data");
    let stale_tmp = data_dir.path.join("index-12345678.bin.tmp.999.1");
    let real_bin = data_dir.path.join("index-12345678.bin");
    fs::write(&stale_tmp, b"leftover").unwrap();
    fs::write(&real_bin, b"corrupt_or_other").unwrap();

    let missing_root = std::env::temp_dir().join(format!(
        "fm_missing_svc_root_{}_{}",
        std::process::id(),
        TEST_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let ev_cb = Arc::clone(&events);
    let svc_missing = Service::start(
        vec![missing_root.to_string_lossy().into_owned()],
        data_dir.path.clone(),
        Box::new(move |ev| {
            ev_cb.lock().unwrap().push(ev);
        }),
    );

    assert!(
        !stale_tmp.exists(),
        "stale .bin.tmp.* must be removed at start"
    );
    assert!(real_bin.exists(), "non-tmp files must remain");

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut got_error = false;
    let mut got_ready = false;
    while Instant::now() < deadline {
        {
            let guard = events.lock().unwrap();
            got_error = guard.iter().any(|e| matches!(e, Event::Error { .. }));
            got_ready = guard.iter().any(|e| matches!(e, Event::Ready { .. }));
        }
        if got_error && got_ready {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(got_error, "expected Event::Error for missing watched root");
    assert!(got_ready, "expected Event::Ready even on failed walk");
    let t0 = Instant::now();
    svc_missing.stop();
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "stop must wake retrying watcher quickly"
    );

    // 2. Overflow cooldown keeps is_scanning() == false between walks and manual trigger bypasses it
    let tree = TempTree::new("svc_cooldown_root");
    let data2 = TempTree::new("svc_cooldown_data");
    let ready_cnt = Arc::new((Mutex::new(0usize), Condvar::new()));
    let rc_cb = Arc::clone(&ready_cnt);
    let cfg = WatcherConfig {
        buffer_bytes: 128,
        coalesce_min: Duration::from_millis(20),
        coalesce_max: Duration::from_millis(40),
        overflow_rewalk_cooldown: Duration::from_secs(30),
    };
    let svc = Service::start_with_watcher_config(
        vec![tree.path_str()],
        data2.path.clone(),
        cfg,
        Box::new(move |ev| {
            if matches!(ev, Event::Ready { .. }) {
                let (mu, cv) = &*rc_cb;
                *mu.lock().unwrap() += 1;
                cv.notify_all();
            }
        }),
    );

    {
        let (mu, cv) = &*ready_cnt;
        let mut g = mu.lock().unwrap();
        let d = Instant::now() + Duration::from_secs(5);
        while *g < 1 {
            let rem = d.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero());
            g = cv.wait_timeout(g, rem).unwrap().0;
        }
    }
    assert!(!svc.is_scanning());

    // Force an overflow: cooldown is 30 s so no immediate overflow re-walk starts and is_scanning stays false
    for i in 0..300usize {
        fs::write(tree.path.join(format!("ov_{i:04}_long_name.txt")), b"x").unwrap();
    }
    thread::sleep(Duration::from_millis(250));
    assert!(
        !svc.is_scanning(),
        "is_scanning must be false during overflow cooldown"
    );
    assert_eq!(*ready_cnt.0.lock().unwrap(), 1);

    // Manual trigger_rewalk must bypass the 30 s overflow cooldown immediately
    svc.trigger_rewalk();
    {
        let (mu, cv) = &*ready_cnt;
        let mut g = mu.lock().unwrap();
        let d = Instant::now() + Duration::from_secs(5);
        while *g < 2 {
            let rem = d.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero(), "manual trigger_rewalk must bypass cooldown");
            g = cv.wait_timeout(g, rem).unwrap().0;
        }
    }
    svc.stop();
}

#[test]
fn service_hidden_scope_finds_visible_child_and_hides_nested_hidden() {
    let root = TempTree::new("hidden_scope_root");
    let data = TempTree::new("hidden_scope_data");
    let hidden_folder = root.path.join("SecretFolder");
    let sub_hidden = hidden_folder.join("NestedHidden");
    fs::create_dir_all(&sub_hidden).unwrap();
    set_hidden(&hidden_folder);
    set_hidden(&sub_hidden);

    fs::write(hidden_folder.join("visible_in_hidden.txt"), b"ok").unwrap();
    fs::write(sub_hidden.join("should_stay_hidden.txt"), b"no").unwrap();

    let ready = Arc::new((Mutex::new(false), Condvar::new()));
    let r_cb = Arc::clone(&ready);
    let svc = Service::start(
        vec![root.path_str()],
        data.path.clone(),
        Box::new(move |ev| {
            if matches!(ev, Event::Ready { .. }) {
                let (mu, cv) = &*r_cb;
                *mu.lock().unwrap() = true;
                cv.notify_all();
            }
        }),
    );
    {
        let (mu, cv) = &*ready;
        let mut g = mu.lock().unwrap();
        let d = Instant::now() + Duration::from_secs(5);
        while !*g {
            let rem = d.saturating_duration_since(Instant::now());
            assert!(!rem.is_zero());
            g = cv.wait_timeout(g, rem).unwrap().0;
        }
    }

    let tok = CancelToken::new();
    let global = svc
        .search("visible_in_hidden", None, false, 10, &tok)
        .unwrap();
    assert!(global.is_empty());

    let scope_str = hidden_folder.to_string_lossy().into_owned();
    let scoped = svc
        .search("txt", Some(&scope_str), false, 10, &tok)
        .unwrap();
    let names: Vec<&str> = scoped.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["visible_in_hidden.txt"]);
    svc.stop();
}

#[test]
fn watcher_panic_cancels_pending_io_and_recovers_poisoned_lock() {
    let tree = TempTree::new("panic_recover");
    let root_str = tree.path_str();
    let cancel = AtomicBool::new(false);
    let (init_idx, _) = walk_root(&root_str, WalkOptions::default(), &cancel, |_| {}).unwrap();
    let shared_idx = Arc::new(RwLock::new(init_idx));

    // 1. Trigger a real panic inside VolumeWatcher::on_applied while the second ReadDirectoryChangesExW
    // buffer IS already armed in the kernel; ArmedWatchState::drop must CancelIoEx + await completion
    // safely before freeing buffers/OVERLAPPED!
    let panicked_flag = Arc::new(AtomicBool::new(false));
    let pf_cb = Arc::clone(&panicked_flag);
    let panic_watcher = VolumeWatcher::start(
        root_str.clone(),
        Arc::clone(&shared_idx),
        WatcherConfig::default(),
        Box::new(move |_recs, _ch| {
            pf_cb.store(true, Ordering::Release);
            panic!("intentional watcher callback panic during armed I/O");
        }),
        Box::new(|| {}),
    )
    .expect("start panic watcher");
    fs::write(tree.path.join("trigger_watcher_panic.txt"), b"p").unwrap();
    let p_deadline = Instant::now() + Duration::from_secs(3);
    while !panicked_flag.load(Ordering::Acquire) && Instant::now() < p_deadline {
        thread::sleep(Duration::from_millis(15));
    }
    assert!(panicked_flag.load(Ordering::Acquire));
    panic_watcher.stop();

    // 2. Intentionally poison the shared RwLock<Index> and verify VolumeWatcher recovers cleanly
    let idx_poison = Arc::clone(&shared_idx);
    let _ = thread::spawn(move || {
        let _g = idx_poison.write().unwrap();
        panic!("intentional lock poison test");
    })
    .join();
    assert!(shared_idx.read().is_err(), "lock should now be poisoned");

    // Start a watcher whose index RwLock IS poisoned and trigger a real file write; must not panic!
    let got_applied = Arc::new((Mutex::new(false), Condvar::new()));
    let ga_cb = Arc::clone(&got_applied);
    let watcher = VolumeWatcher::start(
        root_str.clone(),
        Arc::clone(&shared_idx),
        WatcherConfig::default(),
        Box::new(move |_recs, ch| {
            if ch {
                let (mu, cv) = &*ga_cb;
                *mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                cv.notify_all();
            }
        }),
        Box::new(|| {}),
    )
    .expect("watcher starts against poisoned lock");

    fs::write(tree.path.join("recovered_after_poison.txt"), b"ok").unwrap();
    {
        let (mu, cv) = &*got_applied;
        let mut g = mu.lock().unwrap();
        let d = Instant::now() + Duration::from_secs(3);
        while !*g {
            let rem = d.saturating_duration_since(Instant::now());
            assert!(
                !rem.is_zero(),
                "watcher did not recover poisoned lock in time"
            );
            g = cv.wait_timeout(g, rem).unwrap().0;
        }
    }
    watcher.stop();

    let recovered = shared_idx
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        recovered
            .lookup(&format!("{root_str}\\recovered_after_poison.txt"))
            .is_some()
    );
}
#[test]
fn regression_partial_error_rename_and_reused_old_path() {
    let root_info = RootInfo {
        volume_serial: 0x5555_6666,
        file_id: 1,
        attrs: DIRECTORY,
        mtime: 10,
    };
    let cancel = AtomicBool::new(false);

    // Case A: P (fid=10) renamed to Q (fid=10) before walk reaches it, and listing Q hits PartialError
    // yielding only a.txt while b.txt is unseen. Merge keeps one Q with updated a.txt and preserved b.txt.
    let mut mock_init_a = MockDirSource::new(root_info, 16);
    mock_init_a.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "D".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 2,
            reparse_tag: 0,
        }]),
    );
    mock_init_a.nodes.insert(
        "R:\\D".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "P".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 10,
            reparse_tag: 0,
        }]),
    );
    mock_init_a.nodes.insert(
        "R:\\D\\P".to_owned(),
        MockNodeKind::Dir(vec![
            MockEntry {
                name: "a.txt".to_owned(),
                attrs: ARCHIVE,
                size: 10,
                mtime: 20,
                file_id: 11,
                reparse_tag: 0,
            },
            MockEntry {
                name: "b.txt".to_owned(),
                attrs: ARCHIVE,
                size: 20,
                mtime: 21,
                file_id: 12,
                reparse_tag: 0,
            },
        ]),
    );
    let (mut live_a, _, _) = walk_with_source_report(
        &mock_init_a,
        "R:\\",
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(live_a.len(), 5);

    let mut mock_rewalk_a = MockDirSource::new(root_info, 16);
    mock_rewalk_a.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "D".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 2,
            reparse_tag: 0,
        }]),
    );
    mock_rewalk_a.nodes.insert(
        "R:\\D".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "Q".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 11,
            file_id: 10,
            reparse_tag: 0,
        }]),
    );
    mock_rewalk_a.nodes.insert(
        "R:\\D\\Q".to_owned(),
        MockNodeKind::PartialError {
            first_batch: vec![MockEntry {
                name: "a.txt".to_owned(),
                attrs: ARCHIVE,
                size: 15,
                mtime: 30,
                file_id: 11,
                reparse_tag: 0,
            }],
            err: io::ErrorKind::Other,
        },
    );
    let (fresh_a, _, rep_a) = walk_with_source_report(
        &mock_rewalk_a,
        "R:\\",
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert!(
        rep_a
            .failed
            .iter()
            .any(|f| f.file_id == 10 && f.rel_path == "D\\Q")
    );
    merge_index_with_report(&mut live_a, fresh_a, &rep_a, true);
    assert_eq!(live_a.lookup("R:\\D\\P"), None);
    let q_id = live_a.lookup("R:\\D\\Q").expect("Q present");
    assert_eq!(live_a.by_file_id(10), Some(q_id));
    assert_eq!(
        live_a.meta(live_a.lookup("R:\\D\\Q\\a.txt").unwrap()).size,
        15
    );
    assert!(live_a.lookup("R:\\D\\Q\\b.txt").is_some());
    assert_eq!(live_a.len(), 5, "zero duplicate Q or children");

    // Case B1: A (fid=10) -> B (fid=10, PartialError) while a brand-new dir A (fid=99) occupies "D\A".
    // Case B2: P (fid=100) was renamed to Q2 (fid=100, open fails NotFound) while a new P (fid=199) occupies "D\P".
    let mut mock_init_b = MockDirSource::new(root_info, 16);
    mock_init_b.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "D".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 2,
            reparse_tag: 0,
        }]),
    );
    mock_init_b.nodes.insert(
        "R:\\D".to_owned(),
        MockNodeKind::Dir(vec![
            MockEntry {
                name: "A".to_owned(),
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 10,
                reparse_tag: 0,
            },
            MockEntry {
                name: "P".to_owned(),
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 100,
                reparse_tag: 0,
            },
        ]),
    );
    mock_init_b.nodes.insert(
        "R:\\D\\A".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "secret_a.txt".to_owned(),
            attrs: ARCHIVE,
            size: 77,
            mtime: 20,
            file_id: 13,
            reparse_tag: 0,
        }]),
    );
    mock_init_b.nodes.insert(
        "R:\\D\\P".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "secret_p.txt".to_owned(),
            attrs: ARCHIVE,
            size: 88,
            mtime: 20,
            file_id: 101,
            reparse_tag: 0,
        }]),
    );
    let (mut live_b, _, _) = walk_with_source_report(
        &mock_init_b,
        "R:\\",
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();

    let mut mock_rewalk_b = MockDirSource::new(root_info, 16);
    mock_rewalk_b.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "D".to_owned(),
            attrs: DIRECTORY,
            size: 0,
            mtime: 10,
            file_id: 2,
            reparse_tag: 0,
        }]),
    );
    mock_rewalk_b.nodes.insert(
        "R:\\D".to_owned(),
        MockNodeKind::Dir(vec![
            MockEntry {
                name: "A".to_owned(),
                attrs: DIRECTORY,
                size: 0,
                mtime: 30,
                file_id: 99,
                reparse_tag: 0,
            },
            MockEntry {
                name: "B".to_owned(),
                attrs: DIRECTORY,
                size: 0,
                mtime: 30,
                file_id: 10,
                reparse_tag: 0,
            },
            MockEntry {
                name: "P".to_owned(),
                attrs: DIRECTORY,
                size: 0,
                mtime: 30,
                file_id: 199,
                reparse_tag: 0,
            },
            MockEntry {
                name: "Q2".to_owned(),
                attrs: DIRECTORY,
                size: 0,
                mtime: 30,
                file_id: 100,
                reparse_tag: 0,
            },
        ]),
    );
    mock_rewalk_b.nodes.insert(
        "R:\\D\\A".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "new_a.txt".to_owned(),
            attrs: ARCHIVE,
            size: 5,
            mtime: 30,
            file_id: 98,
            reparse_tag: 0,
        }]),
    );
    mock_rewalk_b.nodes.insert(
        "R:\\D\\B".to_owned(),
        MockNodeKind::PartialError {
            first_batch: Vec::new(),
            err: io::ErrorKind::Other,
        },
    );
    mock_rewalk_b.nodes.insert(
        "R:\\D\\P".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "new_p.txt".to_owned(),
            attrs: ARCHIVE,
            size: 6,
            mtime: 30,
            file_id: 198,
            reparse_tag: 0,
        }]),
    );
    mock_rewalk_b.nodes.insert(
        "R:\\D\\Q2".to_owned(),
        MockNodeKind::Error(io::ErrorKind::NotFound),
    );

    let (fresh_b, _, rep_b) = walk_with_source_report(
        &mock_rewalk_b,
        "R:\\",
        WalkOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap();
    merge_index_with_report(&mut live_b, fresh_b, &rep_b, true);

    assert_eq!(live_b.by_file_id(99), live_b.lookup("R:\\D\\A"));
    assert!(live_b.lookup("R:\\D\\A\\new_a.txt").is_some());
    assert_eq!(live_b.lookup("R:\\D\\A\\secret_a.txt"), None);
    assert_eq!(live_b.by_file_id(10), live_b.lookup("R:\\D\\B"));
    assert!(live_b.lookup("R:\\D\\B\\secret_a.txt").is_some());

    assert_eq!(live_b.by_file_id(199), live_b.lookup("R:\\D\\P"));
    assert!(live_b.lookup("R:\\D\\P\\new_p.txt").is_some());
    assert_eq!(live_b.lookup("R:\\D\\P\\secret_p.txt"), None);
    assert!(live_b.by_file_id(100).is_some());

    let root_b = live_b.lookup("R:\\").unwrap();
    let replay_b = [
        NotifyRecord {
            action: NotifyAction::RenamedOldName,
            rel_path: "D\\Q2".to_owned(),
            file_id: 100,
            parent_file_id: 2,
            attrs: DIRECTORY,
            size: 0,
            mtime: 30,
            reparse_tag: 0,
        },
        NotifyRecord {
            action: NotifyAction::RenamedNewName,
            rel_path: "D\\Q3".to_owned(),
            file_id: 100,
            parent_file_id: 2,
            attrs: DIRECTORY,
            size: 0,
            mtime: 30,
            reparse_tag: 0,
        },
    ];
    assert!(apply_records(
        &mut live_b,
        root_b,
        &replay_b,
        &NoopWatcherFs
    ));
    assert!(live_b.lookup("R:\\D\\Q3\\secret_p.txt").is_some());
    assert_eq!(live_b.by_file_id(100), live_b.lookup("R:\\D\\Q3"));
}

#[test]
fn walker_worker_panic_fails_the_whole_walk() {
    // Fails on 4056029: the panicked worker's output was dropped by `filter_map(join().ok())`,
    // so the walk returned Some(index without R:\D's subtree) and the merge deleted it.
    struct PanickingSource(MockDirSource);
    impl DirSource for PanickingSource {
        type Handle = MockHandle;
        type Scratch = ();
        fn open_dir(&self, path: &str) -> io::Result<Self::Handle> {
            self.0.open_dir(path)
        }
        fn root_info(&self, dir: &Self::Handle) -> io::Result<RootInfo> {
            self.0.root_info(dir)
        }
        fn read_batch(
            &self,
            dir: &Self::Handle,
            restart: bool,
            scratch: &mut Self::Scratch,
            emit: &mut dyn FnMut(DirEntryRef<'_>),
        ) -> io::Result<bool> {
            assert!(dir.path != "R:\\D", "simulated walker worker panic");
            self.0.read_batch(dir, restart, scratch, emit)
        }
    }

    let root_info = RootInfo {
        volume_serial: 0x7777_8888,
        file_id: 1,
        attrs: DIRECTORY,
        mtime: 10,
    };
    let mut mock = MockDirSource::new(root_info, 16);
    mock.nodes.insert(
        "R:\\".to_owned(),
        MockNodeKind::Dir(vec![
            MockEntry {
                name: "D".to_owned(),
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 2,
                reparse_tag: 0,
            },
            MockEntry {
                name: "top.txt".to_owned(),
                attrs: ARCHIVE,
                size: 1,
                mtime: 10,
                file_id: 3,
                reparse_tag: 0,
            },
        ]),
    );
    mock.nodes.insert(
        "R:\\D".to_owned(),
        MockNodeKind::Dir(vec![MockEntry {
            name: "kid.txt".to_owned(),
            attrs: ARCHIVE,
            size: 1,
            mtime: 10,
            file_id: 4,
            reparse_tag: 0,
        }]),
    );
    let cancel = AtomicBool::new(false);
    let opts = WalkOptions {
        threads: 2,
        ..WalkOptions::default()
    };
    let res = walk_with_source_report(&PanickingSource(mock), "R:\\", opts, &cancel, |_| {});
    assert!(res.is_none(), "a panicked worker must fail the walk");
}

#[test]
fn streaming_walk_sends_every_entry_once_and_each_folder_after_its_parent() {
    // 8 x 8 folders with files on every level, a few empty ones, 8 workers racing.
    let tree = TempTree::new("stream");
    for a in 0..8 {
        for b in 0..8 {
            let d = tree.path.join(format!("a{a}")).join(format!("b{b}"));
            fs::create_dir_all(&d).unwrap();
            if b % 3 != 0 {
                for f in 0..4 {
                    File::create(d.join(format!("f{f}.txt"))).unwrap();
                }
            }
        }
        File::create(tree.path.join(format!("a{a}")).join("mid.txt")).unwrap();
    }
    File::create(tree.path.join("top.txt")).unwrap();
    let root = tree.path_str();
    let opts = WalkOptions {
        threads: 8,
        ..WalkOptions::default()
    };
    let sent = Mutex::new(Vec::new());
    let sink = |b: WalkBatch| sent.lock().unwrap().push(b);
    let (idx, _, _) = walk_with_source_streaming(
        &Win32DirSource,
        &root,
        opts,
        &AtomicBool::new(false),
        |_| {},
        Some(&sink),
    )
    .expect("walk succeeds");

    let mut announced = HashSet::new();
    let mut batched = HashSet::new();
    let mut entries = HashSet::new();
    let mut sent_count = 0;
    for b in sent.into_inner().unwrap() {
        assert!(
            b.rel_path.is_empty() || announced.contains(&b.rel_path),
            "batch of {} reached the sink before its parent's",
            b.rel_path
        );
        assert!(
            batched.insert(b.rel_path.clone()),
            "two batches of {}",
            b.rel_path
        );
        for (name, _, descend) in &b.children {
            let rel = if b.rel_path.is_empty() {
                name.clone()
            } else {
                format!("{}\\{name}", b.rel_path)
            };
            if *descend {
                announced.insert(rel.clone());
            }
            entries.insert(rel);
            sent_count += 1;
        }
    }
    assert_eq!(
        batched.len(),
        1 + announced.len(),
        "every descended folder sends a batch"
    );
    assert_eq!(sent_count, entries.len(), "no entry is sent twice");
    let indexed: HashSet<String> = index_rel_triples(&idx, &root)
        .into_iter()
        .map(|(rel, _, _)| rel)
        .collect();
    assert_eq!(entries, indexed);
    assert_eq!(entries.len(), 1 + 8 * (2 + 8) + 8 * 5 * 4);
}

#[test]
fn walk_on_one_or_eight_threads_builds_the_same_index() {
    // Workers insert every folder's listing as they go; three levels so children of folders
    // listed by other workers land under the right parent.
    let tree = TempTree::new("threads");
    for a in 0..5 {
        for b in 0..5 {
            for c in 0..3 {
                let d = tree
                    .path
                    .join(format!("a{a}"))
                    .join(format!("b{b}"))
                    .join(format!("c{c}"));
                fs::create_dir_all(&d).unwrap();
                fs::write(d.join(format!("f{a}{b}{c}.txt")), vec![0u8; a + b + c]).unwrap();
            }
            File::create(
                tree.path
                    .join(format!("a{a}"))
                    .join(format!("b{b}"))
                    .join("mid.txt"),
            )
            .unwrap();
        }
    }
    let root = tree.path_str();
    let walk = |threads| {
        let opts = WalkOptions {
            threads,
            ..WalkOptions::default()
        };
        walk_root(&root, opts, &AtomicBool::new(false), |_| {})
            .expect("walk succeeds")
            .0
    };
    let (one, eight) = (walk(1), walk(8));
    assert_eq!(one.len(), 1 + 5 * (1 + 5 * (1 + 1 + 3 * 2)));
    assert_eq!(eight.len(), one.len());
    assert_eq!(
        index_rel_triples(&eight, &root),
        index_rel_triples(&one, &root)
    );
    let deep = format!(r"{root}\a4\b3\c2\f432.txt");
    assert_eq!(eight.meta(eight.lookup(&deep).expect("deep file")).size, 9);
}

#[test]
fn walk_lists_skipped_folders_but_never_enters_them_and_a_merge_drops_old_contents() {
    let entry = |name: &str, attrs: u32, file_id: u64| MockEntry {
        name: name.to_owned(),
        attrs,
        size: 1,
        mtime: 5,
        file_id,
        reparse_tag: 0,
    };
    let dir = |name: &str, file_id| entry(name, DIRECTORY, file_id);
    let mut mock = MockDirSource::new(
        RootInfo {
            volume_serial: 7,
            file_id: 1,
            attrs: DIRECTORY,
            mtime: 1,
        },
        32,
    );
    let tree: &[(&str, Vec<MockEntry>)] = &[
        (
            r"M:\",
            vec![dir("Windows", 2), dir("proj", 3), dir("Users", 4)],
        ),
        (r"M:\Windows", vec![dir("winsxs", 5), dir("System32", 6)]),
        (
            r"M:\Windows\System32",
            vec![entry("kernel32.dll", ARCHIVE, 7)],
        ),
        (r"M:\Windows\winsxs", vec![entry("a.dll", ARCHIVE, 8)]),
        (
            r"M:\proj",
            vec![dir("Node_Modules", 9), entry("app.js", ARCHIVE, 10)],
        ),
        (
            r"M:\proj\Node_Modules",
            vec![entry("react.js", ARCHIVE, 11)],
        ),
        (r"M:\Users", vec![dir("a", 12)]),
        (r"M:\Users\a", vec![dir("AppData", 13), dir("Temp", 14)]),
        (r"M:\Users\a\Temp", vec![entry("mine.txt", ARCHIVE, 15)]),
        (r"M:\Users\a\AppData", vec![dir("Local", 16)]),
        (r"M:\Users\a\AppData\Local", vec![dir("Temp", 17)]),
        (
            r"M:\Users\a\AppData\Local\Temp",
            vec![entry("x.tmp", ARCHIVE, 18)],
        ),
    ];
    for (path, children) in tree {
        mock.nodes
            .insert((*path).to_owned(), MockNodeKind::Dir(children.clone()));
    }

    let (fresh, stats, report) = walk_with_source_report(
        &mock,
        r"M:\",
        WalkOptions::default(),
        &AtomicBool::new(false),
        |_| {},
    )
    .expect("walk succeeds");
    let has = |idx: &Index, p: &str| idx.lookup(p).is_some();
    let opened: Vec<String> = mock.dir_opens.lock().unwrap().keys().cloned().collect();
    for skipped in [
        r"M:\Windows\WinSxS",
        r"M:\proj\node_modules",
        r"M:\Users\a\AppData\Local\Temp",
    ] {
        assert!(has(&fresh, skipped), "{skipped} itself is listed");
        assert!(
            !opened.iter().any(|p| p.eq_ignore_ascii_case(skipped)),
            "{skipped} is never opened"
        );
    }
    assert!(!has(&fresh, r"M:\Windows\winsxs\a.dll"));
    assert!(!has(&fresh, r"M:\proj\node_modules\react.js"));
    assert!(!has(&fresh, r"M:\Users\a\AppData\Local\Temp\x.tmp"));
    assert!(has(&fresh, r"M:\Windows\System32\kernel32.dll"));
    assert!(has(&fresh, r"M:\proj\app.js"));
    assert!(
        has(&fresh, r"M:\Users\a\Temp\mine.txt"),
        "a Temp of one's own stays"
    );
    assert_eq!(report, Default::default());
    assert_eq!((stats.denied, stats.errors), (0, 0));

    // An index from before the exclusion loses the folders' contents, keeps the folders.
    let mut live = Index::new();
    let root = live.add_root(r"M:\");
    let meta = |attrs, file_id| fm_index::Meta {
        attrs,
        size: 1,
        mtime: 5,
        file_id,
    };
    let win = live.insert(root, "Windows", meta(DIRECTORY, 2));
    let sxs = live.insert(win, "winsxs", meta(DIRECTORY, 5));
    live.insert(sxs, "a.dll", meta(ARCHIVE, 8));
    let proj = live.insert(root, "proj", meta(DIRECTORY, 3));
    let nm = live.insert(proj, "Node_Modules", meta(DIRECTORY, 9));
    live.insert(nm, "react.js", meta(ARCHIVE, 11));
    merge_index_with_report(&mut live, fresh, &report, true);
    assert!(has(&live, r"M:\Windows\winsxs"));
    assert!(has(&live, r"M:\proj\node_modules"));
    assert!(!has(&live, r"M:\Windows\winsxs\a.dll"));
    assert!(!has(&live, r"M:\proj\node_modules\react.js"));
    assert!(has(&live, r"M:\Users\a\Temp\mine.txt"));
}
