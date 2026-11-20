//! Parallel Win32 directory walker using `FileIdBothDirectoryInfo` and a `DirSource` abstraction.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use fm_fs::attrs::{DIRECTORY, REPARSE_POINT};
use fm_fs::fold_into;
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NO_MORE_FILES};
use windows::Win32::Storage::FileSystem::{
    FILE_ID_BOTH_DIR_INFO, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
    GetFileInformationByHandleEx,
};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
};

use crate::arena::{EntryId, Index, Meta, NONE};
use crate::win::{
    DirOpen, decode_utf16_le_into, is_cloud_reparse_tag, join_child_path, lock_or_recover,
    normalize_display_path, open_dir_checked, query_dir_info_by_handle, wait_or_recover,
    wait_timeout_or_recover, win32_error_code, win32_to_io,
};

/// Maximum recursion depth below the walk root (`0` = root).
pub const MAX_WALK_DEPTH: u16 = 512;

/// Folders (by name, any case) whose contents stay out of the index: a walk lists the folder but
/// never enters it, and the watcher drops changes inside it (also for one created later).
/// The user's call (2026-11-20): on C: they held 70 % of all entries (node_modules alone 53 %).
pub const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "WinSxS",
    "npm-cache",
    "site-packages",
    "Code Cache",
];

/// [`SKIPPED_DIRS`] that count only inside an `AppData` folder (the user's Temp, app caches), so
/// a folder of one's own called "Temp" or "Cache" stays searchable.
pub const SKIPPED_IN_APPDATA: &[&str] = &["Temp", "Cache"];

/// `true` if a folder named `name` is skipped; `in_appdata` says whether an ancestor is `AppData`.
pub(crate) fn is_skipped_name(name: &str, in_appdata: bool) -> bool {
    let appdata: &[&str] = if in_appdata { SKIPPED_IN_APPDATA } else { &[] };
    SKIPPED_DIRS
        .iter()
        .chain(appdata)
        .any(|s| name.eq_ignore_ascii_case(s))
}

/// `true` if the folder at `path` (relative to the walk root) is skipped or lies inside a skipped
/// folder.
pub(crate) fn under_skipped_dir(path: &str) -> bool {
    skip_context(path).0
}

/// `(skipped, inside AppData)` for the folder at `path` (relative to the walk root). A folder
/// moved between two paths with the same context keeps the same parts of its subtree indexed.
pub(crate) fn skip_context(path: &str) -> (bool, bool) {
    let mut in_appdata = false;
    for seg in path.split('\\') {
        if is_skipped_name(seg, in_appdata) {
            return (true, false);
        }
        in_appdata |= seg.eq_ignore_ascii_case("AppData");
    }
    (false, in_appdata)
}

/// Removes what an index from before [`SKIPPED_DIRS`] holds inside skipped folders (the folders
/// stay), so a snapshot saved by an older fm does not show it until the first re-walk. Returns
/// `true` if anything was removed.
pub(crate) fn drop_skipped_contents(idx: &mut Index) -> bool {
    let mut doomed = vec![false; idx.parent.len()];
    let mut any = false;
    let mut stack: Vec<(EntryId, bool)> = idx.roots.iter().map(|&r| (r, false)).collect();
    while let Some((dir, in_appdata)) = stack.pop() {
        let mut c = idx.first_child[dir as usize];
        while c != NONE {
            if idx.is_dir(c) {
                let name = idx.display_str_unchecked(c);
                if is_skipped_name(name, in_appdata) {
                    let mut k = idx.first_child[c as usize];
                    while k != NONE {
                        doomed[k as usize] = true;
                        any = true;
                        k = idx.next_sibling[k as usize];
                    }
                } else {
                    stack.push((c, in_appdata || name.eq_ignore_ascii_case("AppData")));
                }
            }
            c = idx.next_sibling[c as usize];
        }
    }
    if any {
        idx.remove_flagged(&doomed);
        idx.compact();
    }
    any
}

/// Default 64 KiB enumeration buffer size for `GetFileInformationByHandleEx`.
pub const DIR_ENUM_BUFFER_BYTES: usize = 64 * 1024;

/// Folder identifier (`file_id` + relative path) captured when a walk failed or was denied access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkFolderRef {
    /// 64-bit NTFS/ReFS FileId (`0` if unknown).
    pub file_id: u64,
    /// Relative path beneath the walk root (`""` for the walk root itself).
    pub rel_path: String,
}

/// Failed and access-denied folders encountered during a walk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WalkFoldersReport {
    /// `true` when opening the walk root or reading its very first batch failed/denied.
    pub root_failed: bool,
    /// Folders whose open or enumeration failed (excluding access denied).
    pub failed: Vec<WalkFolderRef>,
    /// Folders that returned `ERROR_ACCESS_DENIED` / `PermissionDenied`.
    pub denied: Vec<WalkFolderRef>,
}

/// Summary statistics from a completed directory walk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalkStats {
    /// Total directory entries recorded (including the root and unwalked reparse dirs).
    pub dirs: usize,
    /// Total non-directory file entries recorded.
    pub files: usize,
    /// Directories skipped due to access denied (`ERROR_ACCESS_DENIED`).
    pub denied: usize,
    /// Directories skipped due to sharing violations, vanished dirs, or other I/O errors.
    pub errors: usize,
    /// Wall-clock duration in milliseconds.
    pub millis: u64,
}

/// One directory's listing, handed to the sink of [`walk_with_source_streaming`] as soon as the
/// directory is read.
#[derive(Debug, Clone)]
pub struct WalkBatch {
    /// Directory path relative to the walk root (`""` for the root itself).
    pub rel_path: String,
    /// The directory's own metadata as its parent listed it (root: `DIRECTORY` + its root info).
    pub meta: Meta,
    /// Listing failed with an error other than access denied (see [`WalkFoldersReport::failed`]).
    pub failed: bool,
    /// Listing was refused with access denied (see [`WalkFoldersReport::denied`]).
    pub denied: bool,
    /// Children as listed: name, metadata, and whether the walk descends into it (its own batch
    /// then reaches the sink after this one).
    pub children: Vec<(String, Meta, bool)>,
}

/// Configuration for [`walk_root`] and [`walk_with_source`].
#[derive(Debug, Clone, Copy)]
pub struct WalkOptions {
    /// Worker thread count (`0` picks `min(8, available_parallelism)`).
    pub threads: usize,
    /// Approximate cadence of progress callbacks.
    pub progress_interval: Duration,
    /// Whether worker threads enter `THREAD_MODE_BACKGROUND_BEGIN` (for background re-walks).
    pub low_priority: bool,
    /// Maximum directory recursion depth (default [`MAX_WALK_DEPTH`] = 512).
    pub max_depth: u16,
    /// Percent of the time a worker may spend walking (1–100; default 100, never pausing). Below
    /// 100 it rests after every [`PACE_SLICE`] of work, so a background re-walk of a few million
    /// entries leaves the disk mostly idle instead of busy for minutes.
    pub duty: u8,
}

/// The work between two rests of a paced walk ([`WalkOptions::duty`]).
pub const PACE_SLICE: Duration = Duration::from_millis(50);

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            threads: 0,
            progress_interval: Duration::from_millis(100),
            low_priority: false,
            max_depth: MAX_WALK_DEPTH,
            duty: 100,
        }
    }
}

/// Metadata for the root directory of a walk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RootInfo {
    /// Volume serial number (`0` if unknown).
    pub volume_serial: u32,
    /// 64-bit NTFS/ReFS FileId of the root directory itself (`0` if unknown).
    pub file_id: u64,
    /// Win32 `FILE_ATTRIBUTE_*` bitmask of the root directory.
    pub attrs: u32,
    /// Last modification time in Windows `FILETIME` ticks.
    pub mtime: i64,
}

/// Borrowed directory child record yielded by [`DirSource::read_batch`].
#[derive(Debug, Clone, Copy)]
pub struct DirEntryRef<'a> {
    /// Child filename (never `.` or `..`).
    pub name: &'a str,
    /// Win32 `FILE_ATTRIBUTE_*` bitmask.
    pub attrs: u32,
    /// File size in bytes (`0` for directories).
    pub size: u64,
    /// Last modification time in Windows `FILETIME` ticks.
    pub mtime: i64,
    /// 64-bit NTFS/ReFS FileId (`0` if unknown).
    pub file_id: u64,
    /// Reparse tag (`EaSize`) when `REPARSE_POINT` is set, else `0`.
    pub reparse_tag: u32,
}

/// Abstraction over directory enumeration (implemented by [`Win32DirSource`] and test mocks).
pub trait DirSource: Send + Sync {
    /// Opaque handle representing an opened directory.
    type Handle;
    /// Reusable per-worker scratch buffer for batch directory enumeration.
    type Scratch: Default;

    /// Opens an existing directory for listing only (never opens files).
    fn open_dir(&self, path: &str) -> io::Result<Self::Handle>;

    /// Queries volume serial and FileId metadata for an open root directory handle.
    fn root_info(&self, dir: &Self::Handle) -> io::Result<RootInfo>;

    /// Reads the next batch of directory entries, invoking `emit` for each child.
    ///
    /// Returns `Ok(true)` if a batch was read, or `Ok(false)` when enumeration is complete.
    fn read_batch(
        &self,
        dir: &Self::Handle,
        restart: bool,
        scratch: &mut Self::Scratch,
        emit: &mut dyn FnMut(DirEntryRef<'_>),
    ) -> io::Result<bool>;
}

/// Real Win32 [`DirSource`] backed by `CreateFileW` + `GetFileInformationByHandleEx`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32DirSource;

/// Reusable 8-byte-aligned 64 KiB buffer and UTF-8 scratch string for [`Win32DirSource`].
pub struct Win32Scratch {
    buf: Vec<u64>,
    name_buf: String,
}

impl Default for Win32Scratch {
    fn default() -> Self {
        Self {
            buf: vec![0u64; DIR_ENUM_BUFFER_BYTES / 8],
            name_buf: String::with_capacity(260),
        }
    }
}

/// Open Win32 directory handle wrapper (or closed marker when a junction/reparse dir is skipped).
pub struct Win32DirHandle(DirOpen);

impl DirSource for Win32DirSource {
    type Handle = Win32DirHandle;
    type Scratch = Win32Scratch;

    fn open_dir(&self, path: &str) -> io::Result<Self::Handle> {
        let mut wbuf = Vec::new();
        open_dir_checked(path, false, &mut wbuf).map(Win32DirHandle)
    }

    fn root_info(&self, dir: &Self::Handle) -> io::Result<RootInfo> {
        match &dir.0 {
            DirOpen::Dir(h) => {
                let (volume_serial, file_id, attrs, mtime) = query_dir_info_by_handle(h.0)?;
                Ok(RootInfo {
                    volume_serial,
                    file_id,
                    attrs,
                    mtime,
                })
            }
            DirOpen::ReparseSkipped => Ok(RootInfo {
                attrs: DIRECTORY | REPARSE_POINT,
                ..Default::default()
            }),
        }
    }

    fn read_batch(
        &self,
        dir: &Self::Handle,
        restart: bool,
        scratch: &mut Self::Scratch,
        emit: &mut dyn FnMut(DirEntryRef<'_>),
    ) -> io::Result<bool> {
        let DirOpen::Dir(handle) = &dir.0 else {
            return Ok(false);
        };
        let class = if restart {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        let byte_cap = (scratch.buf.len() * 8) as u32;
        let res = unsafe {
            GetFileInformationByHandleEx(handle.0, class, scratch.buf.as_mut_ptr().cast(), byte_cap)
        };
        if let Err(err) = res {
            if win32_error_code(&err) == Some(ERROR_NO_MORE_FILES.0) {
                return Ok(false);
            }
            return Err(win32_to_io(err));
        }

        let buf_bytes = unsafe {
            std::slice::from_raw_parts(scratch.buf.as_ptr().cast::<u8>(), scratch.buf.len() * 8)
        };
        let name_field_off = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let struct_size = std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>();
        let mut offset = 0usize;

        while offset + name_field_off <= buf_bytes.len() {
            let avail = buf_bytes.len() - offset;
            let mut raw_info = std::mem::MaybeUninit::<FILE_ID_BOTH_DIR_INFO>::zeroed();
            let copy_len = avail.min(struct_size);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    buf_bytes.as_ptr().add(offset),
                    raw_info.as_mut_ptr().cast::<u8>(),
                    copy_len,
                );
            }
            let info = unsafe { raw_info.assume_init() };
            let name_bytes_len = info.FileNameLength as usize;
            let name_start = offset + name_field_off;
            if !name_bytes_len.is_multiple_of(2) || name_start + name_bytes_len > buf_bytes.len() {
                break;
            }
            let name_bytes = &buf_bytes[name_start..name_start + name_bytes_len];
            let is_dot = name_bytes == [b'.', 0] || name_bytes == [b'.', 0, b'.', 0];
            if !is_dot && !name_bytes.is_empty() {
                decode_utf16_le_into(name_bytes, &mut scratch.name_buf);
                let attrs = info.FileAttributes;
                let is_dir = (attrs & DIRECTORY) != 0;
                let size = if is_dir {
                    0
                } else {
                    info.EndOfFile.max(0) as u64
                };
                let file_id = if info.FileId == -1 {
                    0
                } else {
                    info.FileId as u64
                };
                let reparse_tag = if (attrs & REPARSE_POINT) != 0 {
                    info.EaSize
                } else {
                    0
                };
                emit(DirEntryRef {
                    name: &scratch.name_buf,
                    attrs,
                    size,
                    mtime: info.LastWriteTime,
                    file_id,
                    reparse_tag,
                });
            }

            if info.NextEntryOffset == 0 {
                break;
            }
            let next = offset.saturating_add(info.NextEntryOffset as usize);
            if next <= offset || next >= buf_bytes.len() {
                break;
            }
            offset = next;
        }
        Ok(true)
    }
}

/// Walks `root` with the default Win32 [`Win32DirSource`], returning `None` if cancelled.
pub fn walk_root(
    root: &str,
    opts: WalkOptions,
    cancel: &AtomicBool,
    on_progress: impl FnMut(usize),
) -> Option<(Index, WalkStats)> {
    walk_root_report(root, opts, cancel, on_progress).map(|(idx, stats, _)| (idx, stats))
}

/// Walks `root` with [`Win32DirSource`], also reporting failed and denied directories.
pub fn walk_root_report(
    root: &str,
    opts: WalkOptions,
    cancel: &AtomicBool,
    on_progress: impl FnMut(usize),
) -> Option<(Index, WalkStats, WalkFoldersReport)> {
    walk_with_source_report(&Win32DirSource, root, opts, cancel, on_progress)
}

/// Walks `root` using an abstract [`DirSource`], returning `None` if cancelled.
pub fn walk_with_source<S: DirSource>(
    source: &S,
    root: &str,
    opts: WalkOptions,
    cancel: &AtomicBool,
    on_progress: impl FnMut(usize),
) -> Option<(Index, WalkStats)>
where
    S::Handle: Send,
{
    walk_with_source_report(source, root, opts, cancel, on_progress)
        .map(|(idx, stats, _)| (idx, stats))
}

/// Walks `root` using an abstract [`DirSource`], also returning failed and denied directories.
pub fn walk_with_source_report<S: DirSource>(
    source: &S,
    root: &str,
    opts: WalkOptions,
    cancel: &AtomicBool,
    on_progress: impl FnMut(usize),
) -> Option<(Index, WalkStats, WalkFoldersReport)>
where
    S::Handle: Send,
{
    walk_with_source_streaming(source, root, opts, cancel, on_progress, None)
}

/// [`walk_with_source_report`] that also hands every listed directory to `sink`, from the worker
/// threads. A directory's batch reaches `sink` before any of its subdirectories is queued, so it
/// always arrives before the batches of its subdirectories.
pub fn walk_with_source_streaming<S: DirSource>(
    source: &S,
    root: &str,
    opts: WalkOptions,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(usize),
    sink: Option<&(dyn Fn(WalkBatch) + Sync)>,
) -> Option<(Index, WalkStats, WalkFoldersReport)>
where
    S::Handle: Send,
{
    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let started = Instant::now();
    let root_display = normalize_display_path(root);
    let denied_count = AtomicUsize::new(0);
    let error_count = AtomicUsize::new(0);

    let root_handle = match source.open_dir(&root_display) {
        Ok(h) => h,
        Err(err) => {
            let denied = is_access_denied(&err);
            record_io_error(&err, &denied_count, &error_count);
            let mut report = WalkFoldersReport {
                root_failed: true,
                ..Default::default()
            };
            let fref = WalkFolderRef {
                file_id: 0,
                rel_path: String::new(),
            };
            if denied {
                report.denied.push(fref);
            } else {
                report.failed.push(fref);
            }
            return Some((
                Index::new(),
                WalkStats {
                    dirs: 0,
                    files: 0,
                    denied: denied_count.load(Ordering::Relaxed),
                    errors: error_count.load(Ordering::Relaxed),
                    millis: started.elapsed().as_millis() as u64,
                },
                report,
            ));
        }
    };

    let root_info = match source.root_info(&root_handle) {
        Ok(info) => info,
        Err(err) => {
            record_io_error(&err, &denied_count, &error_count);
            RootInfo::default()
        }
    };

    let threads = if opts.threads > 0 {
        opts.threads
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(4)
            .max(1)
    };

    struct DirTask<H> {
        dir_id: u32,
        path: String,
        rel_path: String,
        meta: Meta,
        depth: u16,
        preopened: Option<H>,
    }

    struct QueueState<H> {
        queue: VecDeque<DirTask<H>>,
        active_workers: usize,
        done: bool,
    }

    struct Shared<H> {
        mu: Mutex<QueueState<H>>,
        cv: Condvar,
        done_mu: Mutex<bool>,
        done_cv: Condvar,
    }

    struct ActiveTaskGuard<'a, H> {
        shared: &'a Shared<H>,
        active: bool,
    }

    impl<H> Drop for ActiveTaskGuard<'_, H> {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            let finished = {
                let mut guard = lock_or_recover(&self.shared.mu);
                guard.active_workers = guard.active_workers.saturating_sub(1);
                let fin = guard.queue.is_empty() && guard.active_workers == 0;
                if fin {
                    guard.done = true;
                }
                self.shared.cv.notify_all();
                fin
            };
            if finished {
                *lock_or_recover(&self.shared.done_mu) = true;
                self.shared.done_cv.notify_all();
            }
        }
    }

    struct ScannedNode {
        name_off: u32,
        name_len: u16,
        folded_off: u32,
        folded_len: u16,
        meta: Meta,
    }

    struct WorkerOutput {
        root_failed: bool,
        failed: Vec<WalkFolderRef>,
        denied: Vec<WalkFolderRef>,
    }

    // Workers insert each listed directory straight into the index (a task carries its folder's
    // EntryId), so a walk never holds a second copy of everything it listed: a re-walk of C:
    // (4 M entries) kept ~300 MB of per-worker listings beside the live and the fresh index.
    let mut idx = Index::new();
    idx.set_volume_serial(root_info.volume_serial);
    let root_id = idx.add_root(&root_display);
    // Drive roots on Windows always carry HIDDEN|SYSTEM; keep DIRECTORY-only on root so
    // non-hidden descendants are not hidden by ancestor inheritance.
    if root_info.file_id != 0 {
        idx.set_file_id(root_id, root_info.file_id);
    }
    idx.mtime[root_id as usize] = root_info.mtime;
    let built = Mutex::new(idx);

    let mut init_queue = VecDeque::new();
    init_queue.push_back(DirTask {
        dir_id: root_id,
        path: root_display.clone(),
        rel_path: String::new(),
        meta: Meta {
            attrs: DIRECTORY,
            size: 0,
            mtime: root_info.mtime,
            file_id: root_info.file_id,
        },
        depth: 0,
        preopened: Some(root_handle),
    });

    let shared = Shared {
        mu: Mutex::new(QueueState {
            queue: init_queue,
            active_workers: 0,
            done: false,
        }),
        cv: Condvar::new(),
        done_mu: Mutex::new(false),
        done_cv: Condvar::new(),
    };

    let entries_seen = AtomicUsize::new(1);
    let dirs_count = AtomicUsize::new(1);
    let files_count = AtomicUsize::new(0);

    let worker_outputs: Option<Vec<WorkerOutput>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            handles.push(scope.spawn(|| {
                let _bg = if opts.low_priority {
                    Some(BackgroundPriorityGuard::enter())
                } else {
                    None
                };

                let mut scratch = S::Scratch::default();
                // One directory's listing at a time.
                let mut names = String::new();
                let mut folded = String::new();
                let mut nodes = Vec::<ScannedNode>::new();
                let mut eids = Vec::<EntryId>::new();
                let mut root_failed = false;
                let mut failed = Vec::<WalkFolderRef>::new();
                let mut denied = Vec::<WalkFolderRef>::new();
                let mut pending_tasks = Vec::<DirTask<S::Handle>>::new();
                let duty = u32::from(opts.duty.clamp(1, 100));
                let mut working_since = Instant::now();

                loop {
                    if duty < 100 && working_since.elapsed() >= PACE_SLICE {
                        // Rest outside the queue lock, in short steps so a cancel stays prompt.
                        let rest_until =
                            Instant::now() + working_since.elapsed() * (100 - duty) / duty;
                        while !cancel.load(Ordering::Relaxed) && Instant::now() < rest_until {
                            std::thread::sleep(
                                rest_until
                                    .saturating_duration_since(Instant::now())
                                    .min(Duration::from_millis(50)),
                            );
                        }
                        working_since = Instant::now();
                    }
                    let task = {
                        let mut guard = lock_or_recover(&shared.mu);
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                guard.done = true;
                                shared.cv.notify_all();
                                drop(guard);
                                *lock_or_recover(&shared.done_mu) = true;
                                shared.done_cv.notify_all();
                                return WorkerOutput {
                                    root_failed,
                                    failed,
                                    denied,
                                };
                            }
                            if let Some(t) = guard.queue.pop_front() {
                                guard.active_workers += 1;
                                break t;
                            }
                            if guard.active_workers == 0 {
                                guard.done = true;
                                shared.cv.notify_all();
                                drop(guard);
                                *lock_or_recover(&shared.done_mu) = true;
                                shared.done_cv.notify_all();
                                return WorkerOutput {
                                    root_failed,
                                    failed,
                                    denied,
                                };
                            }
                            guard = wait_or_recover(&shared.cv, guard);
                        }
                    };

                    let mut active_guard = ActiveTaskGuard {
                        shared: &shared,
                        active: true,
                    };

                    let dir_handle = match task.preopened {
                        Some(h) => Ok(h),
                        None => source.open_dir(&task.path),
                    };

                    names.clear();
                    folded.clear();
                    nodes.clear();
                    let in_appdata = task
                        .rel_path
                        .split('\\')
                        .any(|seg| seg.eq_ignore_ascii_case("AppData"));
                    let mut local_dirs = 0usize;
                    let mut local_files = 0usize;
                    let (failed_before, denied_before) = (failed.len(), denied.len());
                    let mut listed = Vec::new();
                    pending_tasks.clear();

                    match dir_handle {
                        Ok(handle) => {
                            let mut restart = true;
                            loop {
                                if cancel.load(Ordering::Relaxed) {
                                    break;
                                }
                                let res = source.read_batch(
                                    &handle,
                                    restart,
                                    &mut scratch,
                                    &mut |entry| {
                                        let is_dir = (entry.attrs & DIRECTORY) != 0;
                                        let is_reparse = (entry.attrs & REPARSE_POINT) != 0;
                                        let Ok(name_off) = u32::try_from(names.len()) else {
                                            return;
                                        };
                                        let Ok(name_len) = u16::try_from(entry.name.len()) else {
                                            return;
                                        };
                                        names.push_str(entry.name);
                                        // Folded here, so the index lock covers only the insert.
                                        let folded_off = folded.len();
                                        fold_into(entry.name, &mut folded);
                                        let Ok(folded_len) =
                                            u16::try_from(folded.len() - folded_off)
                                        else {
                                            names.truncate(name_off as usize);
                                            folded.truncate(folded_off);
                                            return;
                                        };

                                        let should_descend = is_dir
                                            && !is_skipped_name(entry.name, in_appdata)
                                            && (!is_reparse
                                                || is_cloud_reparse_tag(entry.reparse_tag))
                                            && task.depth + 1 < opts.max_depth;
                                        let meta = Meta {
                                            attrs: entry.attrs,
                                            size: if is_dir { 0 } else { entry.size },
                                            mtime: entry.mtime,
                                            file_id: entry.file_id,
                                        };

                                        if should_descend {
                                            let child_rel = if task.rel_path.is_empty() {
                                                entry.name.to_owned()
                                            } else {
                                                join_child_path(&task.rel_path, entry.name)
                                            };
                                            pending_tasks.push(DirTask {
                                                // Its position here until it has an EntryId.
                                                dir_id: nodes.len() as u32,
                                                path: join_child_path(&task.path, entry.name),
                                                rel_path: child_rel,
                                                meta,
                                                depth: task.depth + 1,
                                                preopened: None,
                                            });
                                        }

                                        if is_dir {
                                            local_dirs += 1;
                                        } else {
                                            local_files += 1;
                                        }

                                        if sink.is_some() {
                                            listed.push((
                                                entry.name.to_owned(),
                                                meta,
                                                should_descend,
                                            ));
                                        }
                                        nodes.push(ScannedNode {
                                            name_off,
                                            name_len,
                                            folded_off: folded_off as u32,
                                            folded_len,
                                            meta,
                                        });
                                    },
                                );
                                match res {
                                    Ok(true) => {
                                        restart = false;
                                    }
                                    Ok(false) => break,
                                    Err(err) => {
                                        record_io_error(&err, &denied_count, &error_count);
                                        let fref = WalkFolderRef {
                                            file_id: task.meta.file_id,
                                            rel_path: task.rel_path.clone(),
                                        };
                                        if is_access_denied(&err) {
                                            if task.dir_id == root_id && restart {
                                                root_failed = true;
                                            }
                                            denied.push(fref);
                                        } else {
                                            if task.dir_id == root_id && restart {
                                                root_failed = true;
                                            }
                                            failed.push(fref);
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            record_io_error(&err, &denied_count, &error_count);
                            let fref = WalkFolderRef {
                                file_id: task.meta.file_id,
                                rel_path: task.rel_path.clone(),
                            };
                            if is_access_denied(&err) {
                                if task.dir_id == root_id {
                                    root_failed = true;
                                }
                                denied.push(fref);
                            } else {
                                if task.dir_id == root_id {
                                    root_failed = true;
                                }
                                failed.push(fref);
                            }
                        }
                    }

                    let added = nodes.len();
                    if added > 0 {
                        eids.clear();
                        let mut idx = lock_or_recover(&built);
                        for node in &nodes {
                            let off = node.name_off as usize;
                            let name = &names[off..off + node.name_len as usize];
                            let f_off = node.folded_off as usize;
                            let f_name = &folded[f_off..f_off + node.folded_len as usize];
                            eids.push(idx.insert_with_folded(task.dir_id, name, f_name, node.meta));
                        }
                        drop(idx);
                        for pt in &mut pending_tasks {
                            pt.dir_id = eids[pt.dir_id as usize];
                        }
                        entries_seen.fetch_add(added, Ordering::Relaxed);
                        if local_dirs > 0 {
                            dirs_count.fetch_add(local_dirs, Ordering::Relaxed);
                        }
                        if local_files > 0 {
                            files_count.fetch_add(local_files, Ordering::Relaxed);
                        }
                    }

                    // Before `pending_tasks` is queued: no subdirectory batch can overtake this one.
                    if let Some(sink) = sink {
                        sink(WalkBatch {
                            rel_path: task.rel_path,
                            meta: task.meta,
                            failed: failed.len() > failed_before,
                            denied: denied.len() > denied_before,
                            children: listed,
                        });
                    }

                    active_guard.active = false;
                    let finished_all = {
                        let mut guard = lock_or_recover(&shared.mu);
                        if !cancel.load(Ordering::Relaxed) {
                            for pt in pending_tasks.drain(..) {
                                guard.queue.push_back(pt);
                            }
                        }
                        guard.active_workers = guard.active_workers.saturating_sub(1);
                        let finished = guard.queue.is_empty() && guard.active_workers == 0;
                        if finished {
                            guard.done = true;
                        }
                        shared.cv.notify_all();
                        finished
                    };
                    if finished_all {
                        *lock_or_recover(&shared.done_mu) = true;
                        shared.done_cv.notify_all();
                    }
                }
            }));
        }

        // Caller thread drives ~100 ms progress callbacks outside locks until workers finish or cancel.
        {
            let interval = opts.progress_interval.max(Duration::from_millis(1));
            let mut next_progress = Instant::now() + interval;
            loop {
                let wait_for = next_progress.saturating_duration_since(Instant::now());
                let is_done = {
                    let done_guard = lock_or_recover(&shared.done_mu);
                    if *done_guard || cancel.load(Ordering::Relaxed) {
                        true
                    } else {
                        let (g, _) = wait_timeout_or_recover(&shared.done_cv, done_guard, wait_for);
                        *g || cancel.load(Ordering::Relaxed)
                    }
                };
                if is_done {
                    break;
                }
                let now = Instant::now();
                if now >= next_progress {
                    on_progress(entries_seen.load(Ordering::Relaxed));
                    while next_progress <= now {
                        next_progress += interval;
                    }
                }
            }
            if cancel.load(Ordering::Relaxed) {
                let mut guard = lock_or_recover(&shared.mu);
                guard.done = true;
                shared.cv.notify_all();
            }
        }

        // Join every worker (an unjoined panicked one would re-panic the scope), then fail the
        // whole walk if any panicked: its listings and queued folders are lost, and merging the
        // partial result would delete those live subtrees.
        let joined: Vec<Option<WorkerOutput>> =
            handles.into_iter().map(|h| h.join().ok()).collect();
        joined.into_iter().collect::<Option<Vec<_>>>()
    });

    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let worker_outputs = worker_outputs?;

    let mut report = WalkFoldersReport::default();
    for out in &worker_outputs {
        report.root_failed |= out.root_failed;
        report.failed.extend(out.failed.iter().cloned());
        report.denied.extend(out.denied.iter().cloned());
    }

    let idx = built.into_inner().unwrap_or_else(PoisonError::into_inner);
    on_progress(idx.len());
    Some((
        idx,
        WalkStats {
            dirs: dirs_count.load(Ordering::Relaxed),
            files: files_count.load(Ordering::Relaxed),
            denied: denied_count.load(Ordering::Relaxed),
            errors: error_count.load(Ordering::Relaxed),
            millis: started.elapsed().as_millis() as u64,
        },
        report,
    ))
}

fn resolve_folder_in_index(
    idx: &Index,
    root_id: EntryId,
    root_disp: &str,
    fref: &WalkFolderRef,
    use_file_id: bool,
) -> Option<EntryId> {
    if fref.rel_path.is_empty() {
        return Some(root_id);
    }
    if use_file_id
        && fref.file_id != 0
        && let Some(eid) = idx.by_file_id(fref.file_id)
        && idx.is_dir(eid)
    {
        let mut cur = eid;
        while cur != NONE {
            if cur == root_id {
                return Some(eid);
            }
            cur = idx.parent[cur as usize];
        }
    }
    let full = join_child_path(root_disp, &fref.rel_path);
    idx.lookup(&full).filter(|&eid| idx.is_dir(eid))
}

fn mark_subtree_seen(idx: &Index, root: EntryId, initial_slots: usize, seen: &mut [bool]) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let ni = node as usize;
        if ni < initial_slots {
            if seen[ni] {
                continue;
            }
            seen[ni] = true;
        }
        let mut child = idx.first_child[ni];
        while child != NONE {
            stack.push(child);
            child = idx.next_sibling[child as usize];
        }
    }
}

// shortcut: a sibling folder deleted before the walk and missed by the watcher survives until
// the next walk; upgrade if path-only volumes ever get a stable identity to match on.
#[allow(clippy::too_many_arguments)]
fn keep_unlisted_sibling_dirs(
    live: &Index,
    fresh: &Index,
    live_root: EntryId,
    fresh_root: EntryId,
    root_disp: &str,
    failed_rel: &str,
    initial_slots: usize,
    keep_alive: &mut [bool],
) {
    let parent_rel = failed_rel.rsplit_once('\\').map_or("", |(p, _)| p);
    let (lp, fp) = if parent_rel.is_empty() {
        (Some(live_root), Some(fresh_root))
    } else {
        let full = join_child_path(root_disp, parent_rel);
        (live.lookup(&full), fresh.lookup(&full))
    };
    let (Some(lp), Some(fp)) = (lp, fp) else {
        return;
    };
    let mut child = live.first_child[lp as usize];
    while child != NONE {
        if live.is_dir(child)
            && fresh
                .find_child(
                    fp,
                    live.folded_bytes(child),
                    live.display_str_unchecked(child),
                )
                .is_none()
        {
            mark_subtree_seen(live, child, initial_slots, keep_alive);
        }
        child = live.next_sibling[child as usize];
    }
}

fn is_ancestor_or_same(idx: &Index, anc: EntryId, mut node: EntryId) -> bool {
    while node != NONE {
        if node == anc {
            return true;
        }
        node = idx.parent[node as usize];
    }
    false
}

/// Merges `fresh` into `live` (adding new entries, updating changed ones, removing missing ones).
///
/// Returns `true` if `live` was modified.
pub fn merge_index(live: &mut Index, fresh: Index) -> bool {
    merge_index_with_report(live, fresh, &WalkFoldersReport::default(), true)
}

/// Merges `fresh` into `live`, preserving subtrees of folders in `report.failed` and keeping
/// `live` untouched when `report.root_failed` is `true`.
pub fn merge_index_with_report(
    live: &mut Index,
    mut fresh: Index,
    report: &WalkFoldersReport,
    use_file_id: bool,
) -> bool {
    if report.root_failed || fresh.is_empty() || fresh.roots.is_empty() {
        return false;
    }

    let fresh_root = fresh.roots[0];
    let root_disp = fresh.display_str_unchecked(fresh_root).to_owned();

    if live.is_empty() {
        for fref in &report.failed {
            if !fref.rel_path.is_empty()
                && let Some(fe) =
                    resolve_folder_in_index(&fresh, fresh_root, &root_disp, fref, use_file_id)
                && fresh.first_child[fe as usize] == NONE
            {
                fresh.remove(fe);
            }
        }
        let changed = !fresh.is_empty();
        *live = fresh;
        return changed;
    }

    let mut changed = false;
    if fresh.volume_serial() != 0 && live.volume_serial() != fresh.volume_serial() {
        live.set_volume_serial(fresh.volume_serial());
    }

    let initial_slots = live.parent.len();
    let mut keep_alive = vec![false; initial_slots];
    let mut matched = vec![false; initial_slots];

    let live_root = live.add_root(&root_disp);
    if (live_root as usize) < initial_slots {
        keep_alive[live_root as usize] = true;
        matched[live_root as usize] = true;
    }
    let fresh_root_fid = fresh.file_id(fresh_root);
    if live.file_id(live_root) != fresh_root_fid {
        live.set_file_id(live_root, fresh_root_fid);
        changed = true;
    }
    let fresh_root_mtime = fresh.mtime[fresh_root as usize];
    if live.mtime[live_root as usize] != fresh_root_mtime {
        live.mtime[live_root as usize] = fresh_root_mtime;
        changed = true;
    }

    // Separate `keep_alive` (do not delete) from `matched` (claimed by a fresh node) so failed
    // folders keep unseen subtrees while still letting Pass 1 (and Pass 2 for empty failed stubs)
    // move/rename them to their observed paths even if a new occupant sits at the old path.
    let mut failed_empty_stubs = vec![false; fresh.parent.len()];
    for fref in &report.failed {
        if let Some(le) = resolve_folder_in_index(live, live_root, &root_disp, fref, use_file_id) {
            mark_subtree_seen(live, le, initial_slots, &mut keep_alive);
        } else if !use_file_id && !fref.rel_path.is_empty() {
            // Path-only volume: the failed folder vanished from its listed path (typically the
            // watcher already renamed it in live, P -> Q). Without FileIds the renamed folder
            // cannot be identified, so keep every live sibling folder the parent listing lacks;
            // the replayed rename/remove records settle them.
            keep_unlisted_sibling_dirs(
                live,
                &fresh,
                live_root,
                fresh_root,
                &root_disp,
                &fref.rel_path,
                initial_slots,
                &mut keep_alive,
            );
        }
        if !fref.rel_path.is_empty()
            && let Some(fe) =
                resolve_folder_in_index(&fresh, fresh_root, &root_disp, fref, use_file_id)
            && fresh.first_child[fe as usize] == NONE
        {
            failed_empty_stubs[fe as usize] = true;
        }
    }

    let mut queue = VecDeque::new();
    let mut deferred_empty_stubs = Vec::new();
    let mut parked = Vec::new();
    queue.push_back((live_root, fresh_root));

    while let Some((lp, fp)) = queue.pop_front() {
        let mut fc = fresh.first_child[fp as usize];
        while fc != NONE {
            let next_fc = fresh.next_sibling[fc as usize];
            if failed_empty_stubs[fc as usize] {
                deferred_empty_stubs.push((lp, fc));
                fc = next_fc;
                continue;
            }
            if let Some(lc) = merge_fresh_entry(
                live,
                &fresh,
                lp,
                fp,
                fc,
                initial_slots,
                &mut matched,
                &mut keep_alive,
                use_file_id,
                true,
                &mut changed,
                &mut parked,
            ) && fresh.first_child[fc as usize] != NONE
            {
                queue.push_back((lc, fc));
            }
            fc = next_fc;
        }
    }

    // Pass 2: reconcile empty failed stubs whose FileId was not already claimed by a healthy
    // fresh directory in Pass 1 (e.g., cross-parent move during walk), renaming/moving parked
    // live subtrees to the observed stub path without inserting new phantom stubs.
    for (lp, fc) in deferred_empty_stubs {
        let f_fid = fresh.file_id(fc);
        if use_file_id
            && f_fid != 0
            && let Some(existing) = live.by_file_id(f_fid)
            && live.is_dir(existing)
            && ((existing as usize) >= initial_slots || matched[existing as usize])
        {
            continue;
        }
        let fp = fresh.parent[fc as usize];
        let _ = merge_fresh_entry(
            live,
            &fresh,
            lp,
            fp,
            fc,
            initial_slots,
            &mut matched,
            &mut keep_alive,
            use_file_id,
            false,
            &mut changed,
            &mut parked,
        );
    }

    // A parked sibling still carrying its park name was never claimed by any fresh entry: it is
    // the stale occupant of a name now held by a different FileId, so drop it with its subtree.
    for p in parked {
        if live.is_alive(p) && live.display_str_unchecked(p) == park_name(live, p) {
            if (p as usize) < initial_slots {
                keep_alive[p as usize] = false;
            } else {
                live.remove(p);
                changed = true;
            }
        }
    }

    let doomed: Vec<bool> = keep_alive.iter().map(|&keep| !keep).collect();
    changed |= live.remove_flagged(&doomed);
    // Compacting while `fresh` is still held would keep three full indexes at once.
    drop(fresh);

    if live.parent.len() > 1024
        && live.len().saturating_mul(4) < live.parent.len().saturating_mul(3)
    {
        live.compact();
    }
    changed
}

#[allow(clippy::too_many_arguments)]
fn merge_fresh_entry(
    live: &mut Index,
    fresh: &Index,
    lp: EntryId,
    fp: EntryId,
    fc: EntryId,
    initial_slots: usize,
    matched: &mut [bool],
    keep_alive: &mut [bool],
    use_file_id: bool,
    insert_if_missing: bool,
    changed: &mut bool,
    parked: &mut Vec<EntryId>,
) -> Option<EntryId> {
    let f_name = fresh.display_str_unchecked(fc);
    let f_folded = fresh.folded_str(fc);
    let f_meta = fresh.meta(fc);
    let f_is_dir = (f_meta.attrs & DIRECTORY) != 0;

    let matched_lc = find_merge_target(
        live,
        fresh,
        lp,
        fp,
        f_name,
        f_folded,
        f_meta.file_id,
        f_is_dir,
        initial_slots,
        matched,
        use_file_id,
    );

    if let Some(lc) = matched_lc {
        if (lc as usize) < initial_slots {
            keep_alive[lc as usize] = true;
            matched[lc as usize] = true;
        }
        if live.parent[lc as usize] != lp {
            if let Some(blocker) = live.find_child(lp, f_folded.as_bytes(), f_name)
                && blocker != lc
                && use_file_id
                && should_preserve_colliding(
                    live,
                    fresh,
                    blocker,
                    f_meta.file_id,
                    initial_slots,
                    matched,
                    keep_alive,
                )
            {
                park_colliding_sibling(live, blocker, parked);
            }
            live.move_to(lc, lp, f_name);
            *changed = true;
        } else if live.display_str_unchecked(lc) != f_name {
            if let Some(blocker) = live.find_child(lp, f_folded.as_bytes(), f_name)
                && blocker != lc
                && use_file_id
                && should_preserve_colliding(
                    live,
                    fresh,
                    blocker,
                    f_meta.file_id,
                    initial_slots,
                    matched,
                    keep_alive,
                )
            {
                park_colliding_sibling(live, blocker, parked);
            }
            live.rename(lc, f_name);
            *changed = true;
        }
        if live.meta(lc) != f_meta {
            live.update_meta(lc, f_meta);
            *changed = true;
        }
        return Some(lc);
    }

    if !insert_if_missing {
        return None;
    }

    if let Some(blocker) = live.find_child(lp, f_folded.as_bytes(), f_name) {
        if use_file_id
            && f_meta.file_id != 0
            && should_preserve_colliding(
                live,
                fresh,
                blocker,
                f_meta.file_id,
                initial_slots,
                matched,
                keep_alive,
            )
        {
            park_colliding_sibling(live, blocker, parked);
            let new_id = live.insert_with_folded(lp, f_name, f_folded, f_meta);
            *changed = true;
            return Some(new_id);
        }
        if live.is_dir(blocker) && !f_is_dir {
            live.remove(blocker);
        }
    }
    let new_id = live.insert_with_folded(lp, f_name, f_folded, f_meta);
    // A same-name blocker (e.g. a folder recreated with a new FileId) is updated in place and
    // keeps its old id: claim it, or the final sweep deletes it with the fresh children under it.
    if (new_id as usize) < initial_slots {
        keep_alive[new_id as usize] = true;
        matched[new_id as usize] = true;
    }
    *changed = true;
    Some(new_id)
}

fn should_preserve_colliding(
    live: &Index,
    fresh: &Index,
    blocker: EntryId,
    incoming_fid: u64,
    initial_slots: usize,
    matched: &[bool],
    keep_alive: &[bool],
) -> bool {
    let bi = blocker as usize;
    let b_fid = live.file_id[bi];
    if b_fid == 0 || b_fid == incoming_fid {
        return false;
    }
    if bi < initial_slots && (matched[bi] || keep_alive[bi]) {
        return true;
    }
    fresh.by_file_id(b_fid).is_some()
}

fn park_name(live: &Index, blocker: EntryId) -> String {
    format!(
        "__fm_merge_park_{blocker}_{}",
        live.file_id[blocker as usize]
    )
}

fn park_colliding_sibling(live: &mut Index, blocker: EntryId, parked: &mut Vec<EntryId>) {
    let tmp = park_name(live, blocker);
    live.rename(blocker, &tmp);
    parked.push(blocker);
}

#[allow(clippy::too_many_arguments)]
fn find_merge_target(
    live: &Index,
    fresh: &Index,
    lp: EntryId,
    fp: EntryId,
    f_name: &str,
    f_folded: &str,
    f_fid: u64,
    f_is_dir: bool,
    initial_slots: usize,
    matched: &[bool],
    use_file_id: bool,
) -> Option<EntryId> {
    let by_name = live
        .find_child(lp, f_folded.as_bytes(), f_name)
        .filter(|&c| (c as usize) >= initial_slots || !matched[c as usize]);

    if use_file_id && f_fid != 0 {
        // 1. Check in-parent FileId match first (prefer exact name sibling if multiple hard links share FileId).
        if let Some(c) = by_name {
            let c_fid = live.file_id(c);
            if c_fid == f_fid || c_fid == 0 {
                return Some(c);
            }
        }

        // Candidates come from the FileId index (plus hard-link dups), never a sibling scan:
        // scanning made a 40k-child folder merge quadratic under the index write lock.
        let dups = live.fid_dups.get(&f_fid).map_or(&[][..], Vec::as_slice);
        for child in live
            .by_file_id(f_fid)
            .into_iter()
            .chain(dups.iter().copied())
        {
            if live.is_alive(child)
                && live.file_id[child as usize] == f_fid
                && live.parent[child as usize] == lp
                && ((child as usize) >= initial_slots || !matched[child as usize])
                && fresh
                    .find_child(
                        fp,
                        live.folded_bytes(child),
                        live.display_str_unchecked(child),
                    )
                    .is_none_or(|fc_same| fresh.file_id(fc_same) != f_fid)
            {
                return Some(child);
            }
        }

        // 2. Directories never have hard links: match across parents by FileId even if old path
        // is now occupied by a different FileId.
        if f_is_dir
            && let Some(global_lc) = live.by_file_id(f_fid)
            && ((global_lc as usize) >= initial_slots || !matched[global_lc as usize])
            && live.is_dir(global_lc)
            && live.parent[global_lc as usize] != NONE
            && !is_ancestor_or_same(live, global_lc, lp)
            && fresh
                .lookup(&live.path(global_lc))
                .is_none_or(|old_at_path| fresh.file_id(old_at_path) != f_fid)
        {
            return Some(global_lc);
        }

        // 3. Name fallback when FileIds differ: refuse to hijack an entry whose FileId still exists
        // elsewhere in fresh or belongs to a directory (caller will park/insert separately).
        if let Some(c) = by_name {
            let c_fid = live.file_id(c);
            if c_fid != 0 && (live.is_dir(c) || fresh.by_file_id(c_fid).is_some()) {
                return None;
            }
        }
    }

    by_name
}

#[inline]
fn is_access_denied(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::PermissionDenied
        || err.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32)
}

fn record_io_error(err: &io::Error, denied: &AtomicUsize, errors: &AtomicUsize) {
    if is_access_denied(err) {
        denied.fetch_add(1, Ordering::Relaxed);
    } else {
        errors.fetch_add(1, Ordering::Relaxed);
    }
}
struct BackgroundPriorityGuard;

impl BackgroundPriorityGuard {
    fn enter() -> Self {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
        }
        Self
    }
}

impl Drop for BackgroundPriorityGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_END);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_fs::attrs::ARCHIVE;

    #[test]
    fn cloud_reparse_tags_match_only_cloud_family() {
        for nibble in 0u32..=0xF {
            let tag = 0x9000_001A | (nibble << 12);
            assert!(is_cloud_reparse_tag(tag), "tag {tag:#x} should be cloud");
        }
        assert!(!is_cloud_reparse_tag(0xA000_0003)); // IO_REPARSE_TAG_MOUNT_POINT
        assert!(!is_cloud_reparse_tag(0xA000_000C)); // IO_REPARSE_TAG_SYMLINK
        assert!(!is_cloud_reparse_tag(0x8000_0013)); // IO_REPARSE_TAG_DEDUP
        assert!(!is_cloud_reparse_tag(0x8000_001B)); // IO_REPARSE_TAG_APPEXECLINK
    }

    #[test]
    fn merge_index_adds_updates_removes_and_preserves_file_ids() {
        let mut live = Index::new();
        let lr = live.add_root("C:\\");
        live.set_file_id(lr, 1);
        let ld = live.insert(
            lr,
            "Docs",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 2,
            },
        );
        let _keep = live.insert(
            ld,
            "keep.txt",
            Meta {
                attrs: ARCHIVE,
                size: 10,
                mtime: 100,
                file_id: 3,
            },
        );
        let _gone = live.insert(
            ld,
            "gone.txt",
            Meta {
                attrs: ARCHIVE,
                size: 20,
                mtime: 100,
                file_id: 4,
            },
        );

        let mut fresh = Index::new();
        let fr = fresh.add_root("C:\\");
        fresh.set_file_id(fr, 1);
        let fd = fresh.insert(
            fr,
            "Docs",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 2,
            },
        );
        fresh.insert(
            fd,
            "Keep.TXT",
            Meta {
                attrs: ARCHIVE,
                size: 99,
                mtime: 200,
                file_id: 3,
            },
        );
        fresh.insert(
            fd,
            "added.txt",
            Meta {
                attrs: ARCHIVE,
                size: 55,
                mtime: 300,
                file_id: 5,
            },
        );

        assert!(merge_index(&mut live, fresh));
        assert_eq!(live.len(), 4);
        assert_eq!(live.lookup("C:\\Docs\\gone.txt"), None);
        assert_eq!(live.by_file_id(4), None);

        let keep_id = live.lookup("C:\\Docs\\keep.txt").unwrap();
        assert_eq!(live.name(keep_id), "Keep.TXT");
        assert_eq!(live.meta(keep_id).size, 99);
        assert_eq!(live.meta(keep_id).mtime, 200);
        assert_eq!(live.by_file_id(3), Some(keep_id));

        let add_id = live.lookup("C:\\Docs\\added.txt").unwrap();
        assert_eq!(live.by_file_id(5), Some(add_id));

        // Empty fresh (e.g. transient root open_dir failure) must preserve live index intact
        let live_len = live.len();
        assert!(!merge_index(&mut live, Index::new()));
        assert_eq!(live.len(), live_len);
        assert_eq!(live.lookup("C:\\Docs\\keep.txt"), Some(keep_id));
    }

    #[test]
    fn merge_keeps_folder_recreated_with_same_name_and_new_file_id() {
        // Fails on 4056029 and before this fix: `rm -rf dist && mkdir dist` (old FileId gone).
        // The in-place update of `dist` kept its old id, never marked alive, so the final sweep
        // deleted `dist` together with the fresh `new.js` inserted under it.
        let m = |attrs, fid| Meta {
            attrs,
            size: 1,
            mtime: 1,
            file_id: fid,
        };
        for use_file_id in [true, false] {
            let mut live = Index::new();
            let r = live.add_root("R:\\");
            live.set_file_id(r, 1);
            let d = live.insert(r, "dist", m(DIRECTORY, 10));
            live.insert(d, "old.js", m(ARCHIVE, 11));
            let mut fresh = Index::new();
            let fr = fresh.add_root("R:\\");
            fresh.set_file_id(fr, 1);
            let fd = fresh.insert(fr, "dist", m(DIRECTORY, 20));
            fresh.insert(fd, "new.js", m(ARCHIVE, 21));
            merge_index_with_report(&mut live, fresh, &WalkFoldersReport::default(), use_file_id);
            assert!(live.lookup("R:\\dist\\new.js").is_some(), "{use_file_id}");
            assert_eq!(live.lookup("R:\\dist\\old.js"), None, "{use_file_id}");
            assert_eq!(live.file_id(live.lookup("R:\\dist").unwrap()), 20);
            assert_eq!(live.len(), 3, "{use_file_id}");
        }
    }

    #[test]
    fn merge_drops_unclaimed_parked_sibling() {
        // Fails on 4056029: inside a partially listed (failed) folder, the replaced `Sub`
        // (FileId 5) stayed searchable as `__fm_merge_park_2_5` with its stale subtree.
        let m = |attrs, fid| Meta {
            attrs,
            size: 1,
            mtime: 1,
            file_id: fid,
        };
        let mut live = Index::new();
        let r = live.add_root("R:\\");
        live.set_file_id(r, 1);
        let f = live.insert(r, "F", m(DIRECTORY, 2));
        let sub = live.insert(f, "Sub", m(DIRECTORY, 5));
        live.insert(sub, "old.txt", m(ARCHIVE, 50));
        let mut fresh = Index::new();
        let fr = fresh.add_root("R:\\");
        fresh.set_file_id(fr, 1);
        let ff = fresh.insert(fr, "F", m(DIRECTORY, 2));
        fresh.insert(ff, "Sub", m(DIRECTORY, 6));
        let report = WalkFoldersReport {
            root_failed: false,
            failed: vec![WalkFolderRef {
                file_id: 2,
                rel_path: "F".into(),
            }],
            denied: Vec::new(),
        };
        merge_index_with_report(&mut live, fresh, &report, true);
        let f = live.lookup("R:\\F").unwrap();
        let names: Vec<&str> = live
            .iter_ids()
            .filter(|&id| live.parent(id) == Some(f))
            .map(|id| live.name(id))
            .collect();
        assert_eq!(names, ["Sub"]);
        assert_eq!(live.by_file_id(6), live.lookup("R:\\F\\Sub"));
        assert_eq!(live.by_file_id(50), None);
        assert_eq!(live.len(), 3);
    }

    #[test]
    fn merge_bulk_removal_keeps_sibling_lists_consistent() {
        let m = |attrs, fid| Meta {
            attrs,
            size: 1,
            mtime: 1,
            file_id: fid,
        };
        let mut live = Index::new();
        let r = live.add_root("R:\\");
        live.set_file_id(r, 1);
        let d = live.insert(r, "D", m(DIRECTORY, 2));
        for i in 0..50u64 {
            let c = live.insert(d, &format!("c{i}"), m(DIRECTORY, 100 + i));
            live.insert(c, "inner.txt", m(ARCHIVE, 1000 + i));
        }
        let mut fresh = Index::new();
        let fr = fresh.add_root("R:\\");
        fresh.set_file_id(fr, 1);
        let fd = fresh.insert(fr, "D", m(DIRECTORY, 2));
        for i in (0..50u64).filter(|i| i % 3 == 0) {
            let c = fresh.insert(fd, &format!("c{i}"), m(DIRECTORY, 100 + i));
            fresh.insert(c, "inner.txt", m(ARCHIVE, 1000 + i));
        }
        assert!(merge_index(&mut live, fresh));
        let d = live.lookup("R:\\D").unwrap();
        let mut seen = 0;
        let mut c = live.first_child[d as usize];
        while c != NONE {
            assert!(live.is_alive(c));
            seen += 1;
            c = live.next_sibling[c as usize];
        }
        assert_eq!(seen, 17);
        assert_eq!(live.len(), 2 + 17 * 2);
        assert!(live.lookup("R:\\D\\c3\\inner.txt").is_some());
        assert_eq!(live.lookup("R:\\D\\c4"), None);
    }

    #[test]
    fn snapshot_from_an_older_fm_loses_skipped_contents_but_keeps_the_folders() {
        let m = |attrs| Meta {
            attrs,
            size: 1,
            mtime: 1,
            file_id: 0,
        };
        let mut idx = Index::new();
        let root = idx.add_root(r"R:\");
        let proj = idx.insert(root, "proj", m(DIRECTORY));
        idx.insert(proj, "app.js", m(ARCHIVE));
        let nm = idx.insert(proj, "Node_Modules", m(DIRECTORY));
        let react = idx.insert(nm, "react", m(DIRECTORY));
        idx.insert(react, "index.js", m(ARCHIVE));
        let users = idx.insert(root, "Users", m(DIRECTORY));
        let a = idx.insert(users, "a", m(DIRECTORY));
        let own_temp = idx.insert(a, "Temp", m(DIRECTORY));
        idx.insert(own_temp, "mine.txt", m(ARCHIVE));
        let appdata = idx.insert(a, "AppData", m(DIRECTORY));
        let local = idx.insert(appdata, "Local", m(DIRECTORY));
        let temp = idx.insert(local, "Temp", m(DIRECTORY));
        idx.insert(temp, "x.tmp", m(ARCHIVE));

        assert!(drop_skipped_contents(&mut idx));
        let has = |p: &str| idx.lookup(p).is_some();
        assert!(has(r"R:\proj\node_modules"));
        assert!(!has(r"R:\proj\node_modules\react"));
        assert!(!has(r"R:\proj\node_modules\react\index.js"));
        assert!(has(r"R:\Users\a\AppData\Local\Temp"));
        assert!(!has(r"R:\Users\a\AppData\Local\Temp\x.tmp"));
        assert!(has(r"R:\proj\app.js"));
        assert!(has(r"R:\Users\a\Temp\mine.txt"));
        assert_eq!(idx.len(), 11);
        assert!(!drop_skipped_contents(&mut idx), "nothing left to drop");
    }
}
