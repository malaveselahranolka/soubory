//! Parallel Win32 directory walker using `FileIdBothDirectoryInfo` and a `DirSource` abstraction.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
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
    WinHandle, decode_utf16_le_into, is_cloud_reparse_tag, join_child_path, normalize_display_path,
    open_dir_handle, query_dir_info_by_handle, win32_error_code, win32_to_io,
};

/// Maximum recursion depth below the walk root (`0` = root).
pub const MAX_WALK_DEPTH: u16 = 512;

/// Default 64 KiB enumeration buffer size for `GetFileInformationByHandleEx`.
pub const DIR_ENUM_BUFFER_BYTES: usize = 64 * 1024;

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
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            threads: 0,
            progress_interval: Duration::from_millis(100),
            low_priority: false,
            max_depth: MAX_WALK_DEPTH,
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

/// Open Win32 directory handle wrapper.
pub struct Win32DirHandle(WinHandle);

impl DirSource for Win32DirSource {
    type Handle = Win32DirHandle;
    type Scratch = Win32Scratch;

    fn open_dir(&self, path: &str) -> io::Result<Self::Handle> {
        let mut wbuf = Vec::new();
        open_dir_handle(path, false, &mut wbuf).map(Win32DirHandle)
    }

    fn root_info(&self, dir: &Self::Handle) -> io::Result<RootInfo> {
        let (volume_serial, file_id, attrs, mtime) = query_dir_info_by_handle(dir.0.0)?;
        Ok(RootInfo {
            volume_serial,
            file_id,
            attrs,
            mtime,
        })
    }

    fn read_batch(
        &self,
        dir: &Self::Handle,
        restart: bool,
        scratch: &mut Self::Scratch,
        emit: &mut dyn FnMut(DirEntryRef<'_>),
    ) -> io::Result<bool> {
        let class = if restart {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        let byte_cap = (scratch.buf.len() * 8) as u32;
        let res = unsafe {
            GetFileInformationByHandleEx(dir.0.0, class, scratch.buf.as_mut_ptr().cast(), byte_cap)
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
    walk_with_source(&Win32DirSource, root, opts, cancel, on_progress)
}

/// Walks `root` using an abstract [`DirSource`], returning `None` if cancelled.
pub fn walk_with_source<S: DirSource>(
    source: &S,
    root: &str,
    opts: WalkOptions,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(usize),
) -> Option<(Index, WalkStats)>
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
            record_io_error(&err, &denied_count, &error_count);
            return Some((
                Index::new(),
                WalkStats {
                    dirs: 0,
                    files: 0,
                    denied: denied_count.load(Ordering::Relaxed),
                    errors: error_count.load(Ordering::Relaxed),
                    millis: started.elapsed().as_millis() as u64,
                },
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

    struct ScannedNode {
        child_dir_id: u32,
        name_off: u32,
        name_len: u16,
        meta: Meta,
    }

    struct WorkerOutput {
        arena: Vec<u8>,
        nodes: Vec<ScannedNode>,
        batches: Vec<(u32, usize, usize)>,
        non_zero_fids: usize,
    }

    let mut init_queue = VecDeque::new();
    init_queue.push_back(DirTask {
        dir_id: 0,
        path: root_display.clone(),
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
    let next_dir_id = AtomicU32::new(1);

    let worker_outputs: Vec<WorkerOutput> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            handles.push(scope.spawn(|| {
                let _bg = if opts.low_priority {
                    Some(BackgroundPriorityGuard::enter())
                } else {
                    None
                };

                let mut scratch = S::Scratch::default();
                let mut arena = Vec::<u8>::with_capacity(64 * 1024);
                let mut nodes = Vec::<ScannedNode>::with_capacity(4096);
                let mut batches = Vec::<(u32, usize, usize)>::with_capacity(512);
                let mut non_zero_fids = 0usize;
                let mut pending_tasks = Vec::<DirTask<S::Handle>>::new();

                loop {
                    let task = {
                        let mut guard = shared.mu.lock().expect("walker queue lock poisoned");
                        loop {
                            if cancel.load(Ordering::Relaxed) {
                                guard.done = true;
                                shared.cv.notify_all();
                                drop(guard);
                                *shared.done_mu.lock().expect("walker done lock poisoned") = true;
                                shared.done_cv.notify_all();
                                return WorkerOutput {
                                    arena,
                                    nodes,
                                    batches,
                                    non_zero_fids,
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
                                *shared.done_mu.lock().expect("walker done lock poisoned") = true;
                                shared.done_cv.notify_all();
                                return WorkerOutput {
                                    arena,
                                    nodes,
                                    batches,
                                    non_zero_fids,
                                };
                            }
                            guard = shared.cv.wait(guard).expect("walker condvar wait");
                        }
                    };

                    let dir_handle = match task.preopened {
                        Some(h) => Ok(h),
                        None => source.open_dir(&task.path),
                    };

                    let batch_start = nodes.len();
                    let mut local_dirs = 0usize;
                    let mut local_files = 0usize;
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
                                        let name_bytes = entry.name.as_bytes();
                                        let Ok(name_off) = u32::try_from(arena.len()) else {
                                            return;
                                        };
                                        let Ok(name_len) = u16::try_from(name_bytes.len()) else {
                                            return;
                                        };
                                        arena.extend_from_slice(name_bytes);

                                        let should_descend = is_dir
                                            && (!is_reparse
                                                || is_cloud_reparse_tag(entry.reparse_tag))
                                            && task.depth + 1 < opts.max_depth;

                                        let child_dir_id = if should_descend {
                                            let did = next_dir_id.fetch_add(1, Ordering::Relaxed);
                                            pending_tasks.push(DirTask {
                                                dir_id: did,
                                                path: join_child_path(&task.path, entry.name),
                                                depth: task.depth + 1,
                                                preopened: None,
                                            });
                                            did
                                        } else {
                                            NONE
                                        };

                                        if entry.file_id != 0 {
                                            non_zero_fids += 1;
                                        }
                                        if is_dir {
                                            local_dirs += 1;
                                        } else {
                                            local_files += 1;
                                        }

                                        nodes.push(ScannedNode {
                                            child_dir_id,
                                            name_off,
                                            name_len,
                                            meta: Meta {
                                                attrs: entry.attrs,
                                                size: if is_dir { 0 } else { entry.size },
                                                mtime: entry.mtime,
                                                file_id: entry.file_id,
                                            },
                                        });
                                    },
                                );
                                restart = false;
                                match res {
                                    Ok(true) => continue,
                                    Ok(false) => break,
                                    Err(err) => {
                                        record_io_error(&err, &denied_count, &error_count);
                                        break;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            record_io_error(&err, &denied_count, &error_count);
                        }
                    }

                    let added = nodes.len() - batch_start;
                    if added > 0 {
                        batches.push((task.dir_id, batch_start, added));
                        entries_seen.fetch_add(added, Ordering::Relaxed);
                        if local_dirs > 0 {
                            dirs_count.fetch_add(local_dirs, Ordering::Relaxed);
                        }
                        if local_files > 0 {
                            files_count.fetch_add(local_files, Ordering::Relaxed);
                        }
                    }

                    let finished_all = {
                        let mut guard = shared.mu.lock().expect("walker queue lock poisoned");
                        if !cancel.load(Ordering::Relaxed) {
                            for pt in pending_tasks.drain(..) {
                                guard.queue.push_back(pt);
                            }
                        }
                        guard.active_workers -= 1;
                        let finished = guard.queue.is_empty() && guard.active_workers == 0;
                        if finished {
                            guard.done = true;
                        }
                        shared.cv.notify_all();
                        finished
                    };
                    if finished_all {
                        *shared.done_mu.lock().expect("walker done lock poisoned") = true;
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
                    let done_guard = shared.done_mu.lock().expect("walker done lock poisoned");
                    if *done_guard || cancel.load(Ordering::Relaxed) {
                        true
                    } else {
                        let (g, _) = shared
                            .done_cv
                            .wait_timeout(done_guard, wait_for)
                            .expect("walker done condvar wait");
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
                let mut guard = shared.mu.lock().expect("walker queue lock poisoned");
                guard.done = true;
                shared.cv.notify_all();
            }
        }

        handles
            .into_iter()
            .map(|h| h.join().expect("walker worker panicked"))
            .collect()
    });

    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let total_entries = entries_seen.load(Ordering::Relaxed);
    let num_dirs = next_dir_id.load(Ordering::Relaxed) as usize;
    let total_non_zero_fids: usize = worker_outputs
        .iter()
        .map(|w| w.non_zero_fids)
        .sum::<usize>()
        + usize::from(root_info.file_id != 0);

    let mut dir_batches = vec![(0usize, 0usize, 0usize); num_dirs];
    for (w_idx, out) in worker_outputs.iter().enumerate() {
        for &(dir_id, start, len) in &out.batches {
            dir_batches[dir_id as usize] = (w_idx, start, len);
        }
    }

    let mut idx = Index::with_capacity(total_entries);
    idx.reserve_fid_map(total_non_zero_fids);
    idx.set_volume_serial(root_info.volume_serial);
    let root_id = idx.add_root(&root_display);
    // Drive roots on Windows always carry HIDDEN|SYSTEM; keep DIRECTORY-only on root so
    // non-hidden descendants are not hidden by ancestor inheritance.
    if root_info.file_id != 0 {
        idx.set_file_id(root_id, root_info.file_id);
    }
    idx.mtime[root_id as usize] = root_info.mtime;

    let mut build_queue = VecDeque::with_capacity(num_dirs.min(4096));
    build_queue.push_back((0u32, root_id));
    let mut folded_buf = String::with_capacity(128);
    let mut built_steps = 0usize;

    while let Some((dir_id, parent_eid)) = build_queue.pop_front() {
        let (w_idx, start, len) = dir_batches[dir_id as usize];
        if len == 0 {
            continue;
        }
        let w = &worker_outputs[w_idx];
        for node in &w.nodes[start..start + len] {
            built_steps += 1;
            if (built_steps & 0x3FFF) == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let name_bytes =
                &w.arena[node.name_off as usize..node.name_off as usize + node.name_len as usize];
            let name = std::str::from_utf8(name_bytes).expect("decoded UTF-8 name");
            folded_buf.clear();
            fold_into(name, &mut folded_buf);
            let eid = idx.insert_with_folded(parent_eid, name, &folded_buf, node.meta);
            if node.child_dir_id != NONE {
                build_queue.push_back((node.child_dir_id, eid));
            }
        }
    }

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
    ))
}

/// Merges `fresh` into `live` (adding new entries, updating changed ones, removing missing ones).
///
/// Returns `true` if `live` was modified.
pub fn merge_index(live: &mut Index, fresh: Index) -> bool {
    if live.is_empty() {
        let changed = !fresh.is_empty();
        *live = fresh;
        return changed;
    }
    if fresh.is_empty() || fresh.roots.is_empty() {
        return false;
    }

    let mut changed = false;
    if fresh.volume_serial() != 0 && live.volume_serial() != fresh.volume_serial() {
        live.set_volume_serial(fresh.volume_serial());
    }

    let initial_slots = live.parent.len();
    let mut seen = vec![false; initial_slots];

    let fresh_root = fresh.roots[0];
    let live_root = live.add_root(fresh.display_str_unchecked(fresh_root));
    if (live_root as usize) < initial_slots {
        seen[live_root as usize] = true;
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

    let mut queue = VecDeque::new();
    queue.push_back((live_root, fresh_root));

    while let Some((lp, fp)) = queue.pop_front() {
        let mut fc = fresh.first_child[fp as usize];
        while fc != NONE {
            let f_name = fresh.display_str_unchecked(fc);
            let f_folded = fresh.folded_str(fc);
            let f_meta = fresh.meta(fc);

            if let Some(lc) = live.find_child(lp, f_folded.as_bytes(), f_name) {
                if (lc as usize) < initial_slots {
                    seen[lc as usize] = true;
                }
                if live.display_str_unchecked(lc) != f_name {
                    live.rename(lc, f_name);
                    changed = true;
                }
                if live.meta(lc) != f_meta {
                    live.update_meta(lc, f_meta);
                    changed = true;
                }
                if fresh.first_child[fc as usize] != NONE {
                    queue.push_back((lc, fc));
                }
            } else {
                let new_id = live.insert_with_folded(lp, f_name, f_folded, f_meta);
                changed = true;
                if fresh.first_child[fc as usize] != NONE {
                    queue.push_back((new_id, fc));
                }
            }
            fc = fresh.next_sibling[fc as usize];
        }
    }

    for (i, &was_seen) in seen.iter().enumerate() {
        let id = i as EntryId;
        if live.is_alive(id) && !was_seen {
            live.remove(id);
            changed = true;
        }
    }

    if live.parent.len() > 1024
        && live.len().saturating_mul(4) < live.parent.len().saturating_mul(3)
    {
        live.compact();
    }
    changed
}

fn record_io_error(err: &io::Error, denied: &AtomicUsize, errors: &AtomicUsize) {
    if err.kind() == io::ErrorKind::PermissionDenied
        || err.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32)
    {
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
}
