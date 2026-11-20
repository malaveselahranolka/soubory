//! Live `ReadDirectoryChangesExW(ReadDirectoryNotifyExtendedInformation)` watcher with coalescing.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fm_fs::attrs::{DIRECTORY, REPARSE_POINT};
use fm_fs::fold_into;
use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
    FILE_ACTION_RENAMED_OLD_NAME, FILE_NOTIFY_CHANGE, FILE_NOTIFY_CHANGE_ATTRIBUTES,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_EXTENDED_INFORMATION,
    GetLongPathNameW, ReadDirectoryChangesExW, ReadDirectoryNotifyExtendedInformation,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects,
};
use windows::core::PCWSTR;

use crate::arena::{EntryId, Index, Meta, NONE};
use crate::win::walker::{DirSource, MAX_WALK_DEPTH, Win32DirSource};
use crate::win::{
    WinHandle, decode_utf16_le_into, is_cloud_reparse_tag, join_child_path, normalize_display_path,
    open_dir_handle, to_extended_path, wide_null, win32_error_code, win32_to_io,
};

/// Default 64 KiB `ReadDirectoryChangesExW` notification buffer size.
pub const DEFAULT_WATCHER_BUFFER_BYTES: usize = 64 * 1024;

const NOTIFY_FILTER: FILE_NOTIFY_CHANGE = FILE_NOTIFY_CHANGE(
    FILE_NOTIFY_CHANGE_FILE_NAME.0
        | FILE_NOTIFY_CHANGE_DIR_NAME.0
        | FILE_NOTIFY_CHANGE_SIZE.0
        | FILE_NOTIFY_CHANGE_LAST_WRITE.0
        | FILE_NOTIFY_CHANGE_ATTRIBUTES.0
        | FILE_NOTIFY_CHANGE_CREATION.0,
);

/// Action reported by `ReadDirectoryChangesExW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyAction {
    /// Item was created or moved into the watched tree (`FILE_ACTION_ADDED`).
    Added,
    /// Item was deleted or moved out of the watched tree (`FILE_ACTION_REMOVED`).
    Removed,
    /// Item metadata or contents changed (`FILE_ACTION_MODIFIED`).
    Modified,
    /// Previous path in a rename/move pair (`FILE_ACTION_RENAMED_OLD_NAME`).
    RenamedOldName,
    /// New path in a rename/move pair (`FILE_ACTION_RENAMED_NEW_NAME`).
    RenamedNewName,
}

/// Decoded change record from `FILE_NOTIFY_EXTENDED_INFORMATION`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyRecord {
    /// Change action.
    pub action: NotifyAction,
    /// Path relative to the watched root (e.g. `"sub\\file.txt"`).
    pub rel_path: String,
    /// 64-bit NTFS/ReFS FileId (`0` if unknown).
    pub file_id: u64,
    /// 64-bit NTFS/ReFS FileId of the parent directory (`0` if unknown).
    pub parent_file_id: u64,
    /// Win32 `FILE_ATTRIBUTE_*` bitmask.
    pub attrs: u32,
    /// Logical file size in bytes (`0` for directories).
    pub size: u64,
    /// Last modification time in Windows `FILETIME` ticks.
    pub mtime: i64,
    /// Reparse point tag when `REPARSE_POINT` is set, else `0`.
    pub reparse_tag: u32,
}

/// Configuration for [`VolumeWatcher`].
#[derive(Debug, Clone, Copy)]
pub struct WatcherConfig {
    /// Kernel notification buffer size in bytes (default 64 KiB; small values force overflow).
    pub buffer_bytes: usize,
    /// Minimum coalescing wait after a change batch arrives (default 50 ms).
    pub coalesce_min: Duration,
    /// Maximum coalescing delay from the first un-flushed event (default 100 ms).
    pub coalesce_max: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            buffer_bytes: DEFAULT_WATCHER_BUFFER_BYTES,
            coalesce_min: Duration::from_millis(50),
            coalesce_max: Duration::from_millis(100),
        }
    }
}

/// Child entry yielded when enumerating a newly added directory on disk.
#[derive(Debug, Clone)]
pub struct AddedChild {
    /// Child filename.
    pub name: String,
    /// Entry metadata.
    pub meta: Meta,
    /// Reparse tag when `REPARSE_POINT` is set.
    pub reparse_tag: u32,
}

/// Filesystem helper used when applying watcher records (mockable for pure unit tests).
pub trait WatcherFs: Send + Sync {
    /// Resolves an 8.3 short path via `GetLongPathNameW`, returning only the final component.
    fn resolve_long_leaf(&self, full_path: &str) -> Option<String>;

    /// Lists direct children of a newly added directory without opening any files.
    fn list_dir_children(&self, full_dir_path: &str) -> Vec<AddedChild>;
}

/// Real Win32 [`WatcherFs`] implementation using `GetLongPathNameW` and [`Win32DirSource`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32WatcherFs;

impl WatcherFs for Win32WatcherFs {
    fn resolve_long_leaf(&self, full_path: &str) -> Option<String> {
        let ext = to_extended_path(full_path);
        let w = wide_null(&ext);
        let mut out = vec![0u16; w.len().max(512)];
        let n = unsafe { GetLongPathNameW(PCWSTR(w.as_ptr()), Some(&mut out)) };
        if n == 0 {
            return None;
        }
        let len = if (n as usize) >= out.len() {
            out.resize(n as usize + 1, 0);
            let n2 = unsafe { GetLongPathNameW(PCWSTR(w.as_ptr()), Some(&mut out)) };
            if n2 == 0 || (n2 as usize) >= out.len() {
                return None;
            }
            n2 as usize
        } else {
            n as usize
        };
        let long_full = String::from_utf16_lossy(&out[..len]);
        long_full
            .trim_end_matches('\\')
            .rsplit('\\')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    fn list_dir_children(&self, full_dir_path: &str) -> Vec<AddedChild> {
        let source = Win32DirSource;
        let Ok(handle) = source.open_dir(full_dir_path) else {
            return Vec::new();
        };
        let mut scratch = <Win32DirSource as DirSource>::Scratch::default();
        let mut children = Vec::new();
        let mut restart = true;
        while let Ok(true) = source.read_batch(&handle, restart, &mut scratch, &mut |entry| {
            let is_dir = (entry.attrs & DIRECTORY) != 0;
            children.push(AddedChild {
                name: entry.name.to_owned(),
                meta: Meta {
                    attrs: entry.attrs,
                    size: if is_dir { 0 } else { entry.size },
                    mtime: entry.mtime,
                    file_id: entry.file_id,
                },
                reparse_tag: entry.reparse_tag,
            });
        }) {
            restart = false;
        }
        children
    }
}

/// No-op [`WatcherFs`] for pure synthetic unit tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWatcherFs;

impl WatcherFs for NoopWatcherFs {
    fn resolve_long_leaf(&self, _full_path: &str) -> Option<String> {
        None
    }

    fn list_dir_children(&self, _full_dir_path: &str) -> Vec<AddedChild> {
        Vec::new()
    }
}
/// Coalesces redundant `Modified` events within a batch while preserving relative ordering.
#[must_use]
pub fn coalesce_records(records: Vec<NotifyRecord>) -> Vec<NotifyRecord> {
    use std::collections::HashMap;

    let mut out = Vec::with_capacity(records.len());
    let mut last_mod_target: HashMap<(u64, u64), usize> = HashMap::new();

    for rec in records {
        match rec.action {
            NotifyAction::Added | NotifyAction::RenamedNewName => {
                let key = (rec.parent_file_id, rec.file_id);
                let pos = out.len();
                out.push(rec);
                if key.1 != 0 {
                    last_mod_target.insert(key, pos);
                }
            }
            NotifyAction::Modified => {
                let key = (rec.parent_file_id, rec.file_id);
                if key.1 != 0
                    && let Some(&idx) = last_mod_target.get(&key)
                {
                    let prev = &mut out[idx];
                    prev.attrs = rec.attrs;
                    prev.size = rec.size;
                    prev.mtime = rec.mtime;
                    prev.reparse_tag = rec.reparse_tag;
                    continue;
                }
                let pos = out.len();
                out.push(rec);
                if key.1 != 0 {
                    last_mod_target.insert(key, pos);
                }
            }
            NotifyAction::Removed | NotifyAction::RenamedOldName => {
                if rec.file_id != 0 {
                    last_mod_target.remove(&(rec.parent_file_id, rec.file_id));
                }
                out.push(rec);
            }
        }
    }
    out
}

/// Applies a slice of [`NotifyRecord`]s to `idx` under `root_id`, returning `true` if `idx` changed.
fn maybe_compact(idx: &mut Index) {
    if idx.parent.len() > 1024 && idx.len().saturating_mul(4) < idx.parent.len().saturating_mul(3) {
        idx.compact();
    }
}

/// Applies a slice of [`NotifyRecord`]s to `idx` under `root_id`, returning `true` if `idx` changed.
pub fn apply_records(
    idx: &mut Index,
    root_id: EntryId,
    records: &[NotifyRecord],
    fs: &dyn WatcherFs,
) -> bool {
    let mut pending_old = None;
    let mut changed = apply_records_with_pending(idx, root_id, records, fs, &mut pending_old);
    if let Some(orphan) = pending_old.take() {
        changed |= apply_removed_record(idx, root_id, &orphan);
    }
    maybe_compact(idx);
    changed
}

pub(crate) fn apply_records_with_pending(
    idx: &mut Index,
    root_id: EntryId,
    records: &[NotifyRecord],
    fs: &dyn WatcherFs,
    pending_old: &mut Option<NotifyRecord>,
) -> bool {
    if !idx.is_alive(root_id) {
        return false;
    }
    let mut changed = false;

    for rec in records {
        match rec.action {
            NotifyAction::RenamedOldName => {
                if let Some(prev) = pending_old.replace(rec.clone()) {
                    changed |= apply_removed_record(idx, root_id, &prev);
                }
            }
            NotifyAction::RenamedNewName => {
                if let Some(old) = pending_old.take() {
                    changed |= apply_rename_pair(idx, root_id, &old, rec, fs);
                } else {
                    changed |= apply_added_record(idx, root_id, rec, fs);
                }
            }
            NotifyAction::Added => {
                if let Some(prev) = pending_old.take() {
                    changed |= apply_removed_record(idx, root_id, &prev);
                }
                changed |= apply_added_record(idx, root_id, rec, fs);
            }
            NotifyAction::Removed => {
                if let Some(prev) = pending_old.take() {
                    changed |= apply_removed_record(idx, root_id, &prev);
                }
                changed |= apply_removed_record(idx, root_id, rec);
            }
            NotifyAction::Modified => {
                if let Some(prev) = pending_old.take() {
                    changed |= apply_removed_record(idx, root_id, &prev);
                }
                changed |= apply_modified_record(idx, root_id, rec);
            }
        }
    }

    changed
}

fn leaf_of(rel_path: &str) -> &str {
    rel_path
        .rsplit_once('\\')
        .map_or(rel_path, |(_, leaf)| leaf)
}

fn parent_rel_of(rel_path: &str) -> Option<&str> {
    rel_path.rsplit_once('\\').map(|(parent, _)| parent)
}

fn resolve_parent_entry(
    idx: &Index,
    root_id: EntryId,
    parent_file_id: u64,
    rel_path: &str,
) -> Option<EntryId> {
    if parent_file_id != 0
        && let Some(pid) = idx.by_file_id(parent_file_id)
    {
        return Some(pid);
    }
    let pid = match parent_rel_of(rel_path) {
        None | Some("") => root_id,
        Some(rel_parent) => {
            let root_path = idx.path(root_id);
            let full = join_child_path(&root_path, rel_parent);
            idx.lookup(&full)?
        }
    };
    if parent_file_id != 0 {
        let pid_fid = idx.file_id(pid);
        if pid_fid != 0 && pid_fid != parent_file_id {
            return None;
        }
    }
    Some(pid)
}

fn resolve_target_in_parent(
    idx: &Index,
    parent_id: EntryId,
    file_id: u64,
    leaf: &str,
) -> Option<EntryId> {
    if file_id != 0
        && let Some(id) = idx.find_child_by_file_id(parent_id, file_id)
    {
        return Some(id);
    }
    let mut folded = String::with_capacity(leaf.len());
    fold_into(leaf, &mut folded);
    let candidate = idx.find_child(parent_id, folded.as_bytes(), leaf)?;
    if file_id != 0 {
        let cand_fid = idx.file_id(candidate);
        if cand_fid != 0 && cand_fid != file_id {
            return None;
        }
    }
    Some(candidate)
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

fn resolve_leaf_if_short(
    idx: &Index,
    parent_id: EntryId,
    raw_leaf: &str,
    fs: &dyn WatcherFs,
) -> String {
    if raw_leaf.contains('~') {
        let full = join_child_path(&idx.path(parent_id), raw_leaf);
        if let Some(long_leaf) = fs.resolve_long_leaf(&full) {
            return long_leaf;
        }
    }
    raw_leaf.to_owned()
}

fn entry_depth(idx: &Index, mut id: EntryId) -> u16 {
    let mut depth = 0u16;
    while id != NONE {
        let p = idx.parent[id as usize];
        if p == NONE {
            break;
        }
        depth = depth.saturating_add(1);
        id = p;
    }
    depth
}

fn apply_added_record(
    idx: &mut Index,
    root_id: EntryId,
    rec: &NotifyRecord,
    fs: &dyn WatcherFs,
) -> bool {
    let Some(parent_id) = resolve_parent_entry(idx, root_id, rec.parent_file_id, &rec.rel_path)
    else {
        return false;
    };
    let raw_leaf = leaf_of(&rec.rel_path);
    if raw_leaf.is_empty() {
        return false;
    }
    let leaf = resolve_leaf_if_short(idx, parent_id, raw_leaf, fs);
    let is_dir = (rec.attrs & DIRECTORY) != 0;
    let meta = Meta {
        attrs: rec.attrs,
        size: if is_dir { 0 } else { rec.size },
        mtime: rec.mtime,
        file_id: rec.file_id,
    };
    let eid = idx.insert(parent_id, &leaf, meta);

    let should_descend =
        is_dir && ((rec.attrs & REPARSE_POINT) == 0 || is_cloud_reparse_tag(rec.reparse_tag));
    if should_descend && idx.first_child[eid as usize] == NONE {
        let start_depth = entry_depth(idx, eid);
        if start_depth < MAX_WALK_DEPTH {
            let mut queue = VecDeque::new();
            queue.push_back((eid, idx.path(eid), start_depth));
            while let Some((dir_id, dir_path, depth)) = queue.pop_front() {
                for child in fs.list_dir_children(&dir_path) {
                    let c_is_dir = (child.meta.attrs & DIRECTORY) != 0;
                    let c_id = idx.insert(dir_id, &child.name, child.meta);
                    let c_descend = c_is_dir
                        && ((child.meta.attrs & REPARSE_POINT) == 0
                            || is_cloud_reparse_tag(child.reparse_tag));
                    if c_descend && depth + 1 < MAX_WALK_DEPTH {
                        queue.push_back((c_id, join_child_path(&dir_path, &child.name), depth + 1));
                    }
                }
            }
        }
    }
    true
}

fn apply_removed_record(idx: &mut Index, root_id: EntryId, rec: &NotifyRecord) -> bool {
    let target = if let Some(parent_id) =
        resolve_parent_entry(idx, root_id, rec.parent_file_id, &rec.rel_path)
    {
        resolve_target_in_parent(idx, parent_id, rec.file_id, leaf_of(&rec.rel_path))
    } else {
        None
    };
    let Some(id) = target else {
        return false;
    };
    idx.remove(id);
    true
}

fn apply_modified_record(idx: &mut Index, root_id: EntryId, rec: &NotifyRecord) -> bool {
    let Some(parent_id) = resolve_parent_entry(idx, root_id, rec.parent_file_id, &rec.rel_path)
    else {
        return false;
    };
    let Some(target) =
        resolve_target_in_parent(idx, parent_id, rec.file_id, leaf_of(&rec.rel_path))
    else {
        return false;
    };
    let is_dir = (rec.attrs & DIRECTORY) != 0;
    let fid = if rec.file_id != 0 {
        rec.file_id
    } else {
        idx.file_id(target)
    };
    let new_meta = Meta {
        attrs: rec.attrs,
        size: if is_dir { 0 } else { rec.size },
        mtime: rec.mtime,
        file_id: fid,
    };
    if idx.meta(target) == new_meta {
        return false;
    }
    idx.update_meta(target, new_meta);
    true
}

fn apply_rename_pair(
    idx: &mut Index,
    root_id: EntryId,
    old: &NotifyRecord,
    new: &NotifyRecord,
    fs: &dyn WatcherFs,
) -> bool {
    let old_target = resolve_parent_entry(idx, root_id, old.parent_file_id, &old.rel_path)
        .and_then(|op| resolve_target_in_parent(idx, op, old.file_id, leaf_of(&old.rel_path)))
        .or_else(|| {
            (old.file_id != 0)
                .then(|| idx.by_file_id(old.file_id))
                .flatten()
        });

    let Some(target) = old_target else {
        return apply_added_record(idx, root_id, new, fs);
    };
    let Some(new_parent) = resolve_parent_entry(idx, root_id, new.parent_file_id, &new.rel_path)
    else {
        idx.remove(target);
        return true;
    };

    if idx.parent[target as usize] == NONE || is_ancestor_or_same(idx, target, new_parent) {
        return false;
    }
    let raw_new_leaf = leaf_of(&new.rel_path);
    if raw_new_leaf.is_empty() {
        return false;
    }
    let new_leaf = resolve_leaf_if_short(idx, new_parent, raw_new_leaf, fs);
    idx.move_to(target, new_parent, &new_leaf);

    let is_dir = (new.attrs & DIRECTORY) != 0;
    let fid = if new.file_id != 0 {
        new.file_id
    } else {
        idx.file_id(target)
    };
    let new_meta = Meta {
        attrs: new.attrs,
        size: if is_dir { 0 } else { new.size },
        mtime: new.mtime,
        file_id: fid,
    };
    if idx.meta(target) != new_meta {
        idx.update_meta(target, new_meta);
    }
    true
}

/// Parses a `ReadDirectoryNotifyExtendedInformation` byte buffer into [`NotifyRecord`]s.
pub(crate) fn parse_notify_buffer(buf: &[u8]) -> Vec<NotifyRecord> {
    let mut records = Vec::new();
    let name_field_off = std::mem::offset_of!(FILE_NOTIFY_EXTENDED_INFORMATION, FileName);
    let struct_size = std::mem::size_of::<FILE_NOTIFY_EXTENDED_INFORMATION>();
    let mut offset = 0usize;

    while offset + name_field_off <= buf.len() {
        let avail = buf.len() - offset;
        let mut raw_info = std::mem::MaybeUninit::<FILE_NOTIFY_EXTENDED_INFORMATION>::zeroed();
        let copy_len = avail.min(struct_size);
        unsafe {
            std::ptr::copy_nonoverlapping(
                buf.as_ptr().add(offset),
                raw_info.as_mut_ptr().cast::<u8>(),
                copy_len,
            );
        }
        let info = unsafe { raw_info.assume_init() };
        let name_bytes_len = info.FileNameLength as usize;
        let name_start = offset + name_field_off;
        if !name_bytes_len.is_multiple_of(2) || name_start + name_bytes_len > buf.len() {
            break;
        }
        let mut rel_path = String::with_capacity(name_bytes_len / 2);
        decode_utf16_le_into(&buf[name_start..name_start + name_bytes_len], &mut rel_path);

        let action = match info.Action {
            FILE_ACTION_ADDED => Some(NotifyAction::Added),
            FILE_ACTION_REMOVED => Some(NotifyAction::Removed),
            FILE_ACTION_MODIFIED => Some(NotifyAction::Modified),
            FILE_ACTION_RENAMED_OLD_NAME => Some(NotifyAction::RenamedOldName),
            FILE_ACTION_RENAMED_NEW_NAME => Some(NotifyAction::RenamedNewName),
            _ => None,
        };

        if let Some(action) = action {
            let attrs = info.FileAttributes;
            let is_dir = (attrs & DIRECTORY) != 0;
            let file_id = if info.FileId == -1 {
                0
            } else {
                info.FileId as u64
            };
            let parent_file_id = if info.ParentFileId == -1 {
                0
            } else {
                info.ParentFileId as u64
            };
            let reparse_tag = if (attrs & REPARSE_POINT) != 0 {
                unsafe { info.Anonymous.ReparsePointTag }
            } else {
                0
            };
            records.push(NotifyRecord {
                action,
                rel_path,
                file_id,
                parent_file_id,
                attrs,
                size: if is_dir {
                    0
                } else {
                    info.FileSize.max(0) as u64
                },
                mtime: info.LastModificationTime,
                reparse_tag,
            });
        }

        if info.NextEntryOffset == 0 {
            break;
        }
        let next = offset.saturating_add(info.NextEntryOffset as usize);
        if next <= offset || next >= buf.len() {
            break;
        }
        offset = next;
    }

    records
}

/// Callback invoked after a coalesced batch of notify records is applied to the index.
pub type OnAppliedCallback = Box<dyn Fn(&[NotifyRecord], bool) + Send + Sync>;

/// Callback invoked when the watcher buffer overflows and requires a re-walk.
pub type OnOverflowCallback = Box<dyn Fn() + Send + Sync>;

/// Internal hook invoked while the index write lock is still held after applying records.
pub(crate) type OnLockedCallback = Box<dyn Fn(&[NotifyRecord]) + Send + Sync>;

/// Live overlapped Win32 `ReadDirectoryChangesExW` watcher for one volume/directory root.
pub struct VolumeWatcher {
    stop_flag: Arc<AtomicBool>,
    stop_event: Arc<WinHandle>,
    thread: Option<JoinHandle<()>>,
}

impl VolumeWatcher {
    /// Starts an overlapped `ReadDirectoryChangesExW` watcher on `root`.
    ///
    /// Waits until the initial `ReadDirectoryChangesExW` request is armed in the kernel before
    /// returning so immediate filesystem mutations are observed.
    pub fn start(
        root: String,
        index: Arc<RwLock<Index>>,
        config: WatcherConfig,
        on_applied: OnAppliedCallback,
        on_overflow: OnOverflowCallback,
    ) -> io::Result<Self> {
        Self::start_with_locked_hook(
            root,
            index,
            config,
            Box::new(|_| {}),
            on_applied,
            on_overflow,
        )
    }

    pub(crate) fn start_with_locked_hook(
        root: String,
        index: Arc<RwLock<Index>>,
        config: WatcherConfig,
        on_locked: OnLockedCallback,
        on_applied: OnAppliedCallback,
        on_overflow: OnOverflowCallback,
    ) -> io::Result<Self> {
        let root_display = normalize_display_path(&root);
        let mut wbuf = Vec::new();
        let dir_handle = open_dir_handle(&root_display, true, &mut wbuf)?;
        let io_event =
            WinHandle(unsafe { CreateEventW(None, true, false, None) }.map_err(win32_to_io)?);
        let stop_event = Arc::new(WinHandle(
            unsafe { CreateEventW(None, true, false, None) }.map_err(win32_to_io)?,
        ));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let (armed_tx, armed_rx) = std::sync::mpsc::sync_channel::<io::Result<()>>(1);
        let stop_flag_w = Arc::clone(&stop_flag);
        let stop_event_w = Arc::clone(&stop_event);

        let thread = thread::Builder::new()
            .name(format!("fm-watch:{root_display}"))
            .spawn(move || {
                watcher_loop(
                    &root_display,
                    dir_handle,
                    io_event,
                    stop_event_w,
                    stop_flag_w,
                    index,
                    config,
                    armed_tx,
                    on_locked,
                    on_applied,
                    on_overflow,
                );
            })?;

        match armed_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_flag,
                stop_event,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                stop_flag.store(true, Ordering::Release);
                unsafe {
                    let _ = SetEvent(stop_event.0);
                }
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop_flag.store(true, Ordering::Release);
                unsafe {
                    let _ = SetEvent(stop_event.0);
                }
                let _ = thread.join();
                Err(io::Error::other("watcher thread exited during startup"))
            }
        }
    }

    /// Signals the watcher thread to stop and joins it cleanly.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        unsafe {
            let _ = SetEvent(self.stop_event.0);
        }
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for VolumeWatcher {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[allow(clippy::too_many_arguments)]
fn watcher_loop(
    root_display: &str,
    dir_handle: WinHandle,
    io_event: WinHandle,
    stop_event: Arc<WinHandle>,
    stop_flag: Arc<AtomicBool>,
    index: Arc<RwLock<Index>>,
    config: WatcherConfig,
    armed_tx: std::sync::mpsc::SyncSender<io::Result<()>>,
    on_locked: OnLockedCallback,
    on_applied: OnAppliedCallback,
    on_overflow: OnOverflowCallback,
) {
    let aligned_bytes = config.buffer_bytes.max(128).div_ceil(8) * 8;
    let mut buf = vec![0u64; aligned_bytes / 8];
    let byte_cap = aligned_bytes as u32;
    let mut overlapped = OVERLAPPED {
        hEvent: io_event.0,
        ..Default::default()
    };

    let fs = Win32WatcherFs;
    let mut pending_batch: Vec<NotifyRecord> = Vec::new();
    let mut pending_old: Option<NotifyRecord> = None;
    let mut batch_start: Option<Instant> = None;
    let mut coalesce_deadline: Option<Instant> = None;

    let arm = |overlapped: &mut OVERLAPPED, buf: &mut [u64]| -> Result<bool, io::Error> {
        unsafe {
            let _ = ResetEvent(io_event.0);
        }
        overlapped.Internal = 0;
        overlapped.InternalHigh = 0;
        let res = unsafe {
            ReadDirectoryChangesExW(
                dir_handle.0,
                buf.as_mut_ptr().cast(),
                byte_cap,
                true,
                NOTIFY_FILTER,
                None,
                Some(overlapped as *mut OVERLAPPED),
                None,
                ReadDirectoryNotifyExtendedInformation,
            )
        };
        match res {
            Ok(()) => Ok(true),
            Err(e) => match win32_error_code(&e) {
                Some(c) if c == ERROR_IO_PENDING.0 => Ok(true),
                Some(c) if c == ERROR_NOTIFY_ENUM_DIR.0 => Ok(false),
                Some(c) if c == ERROR_OPERATION_ABORTED.0 => {
                    Err(io::Error::from_raw_os_error(c as i32))
                }
                _ => Err(win32_to_io(e)),
            },
        }
    };

    let mut is_armed = match arm(&mut overlapped, &mut buf) {
        Ok(true) => {
            let _ = armed_tx.send(Ok(()));
            true
        }
        Ok(false) => {
            let _ = armed_tx.send(Ok(()));
            on_overflow();
            false
        }
        Err(e) => {
            let _ = armed_tx.send(Err(e));
            return;
        }
    };

    let handles = [io_event.0, stop_event.0];

    let mut pending_overflow = false;
    let mut consecutive_arm_overflows = 0u32;

    while !stop_flag.load(Ordering::Acquire) {
        if !is_armed {
            match arm(&mut overlapped, &mut buf) {
                Ok(true) => {
                    is_armed = true;
                    consecutive_arm_overflows = 0;
                    if pending_overflow {
                        pending_overflow = false;
                        on_overflow();
                    }
                }
                Ok(false) => {
                    pending_overflow = true;
                    consecutive_arm_overflows += 1;
                    if consecutive_arm_overflows >= 3 {
                        pending_overflow = false;
                        on_overflow();
                        thread::sleep(Duration::from_millis(10));
                    }
                    continue;
                }
                Err(_) => break,
            }
        }

        let timeout_ms = match coalesce_deadline {
            Some(dl) => dl.saturating_duration_since(Instant::now()).as_millis() as u32,
            None => INFINITE,
        };

        let wait_res = unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) };
        if wait_res == WAIT_OBJECT_0 {
            let mut bytes_returned = 0u32;
            let ov_res = unsafe {
                GetOverlappedResult(dir_handle.0, &overlapped, &mut bytes_returned, false)
            };
            is_armed = false;

            match ov_res {
                Ok(()) => {
                    if bytes_returned == 0 {
                        flush_pending(
                            root_display,
                            &index,
                            &fs,
                            &mut pending_batch,
                            &mut pending_old,
                            &*on_locked,
                            &*on_applied,
                        );
                        batch_start = None;
                        coalesce_deadline = None;
                        pending_overflow = true;
                    } else {
                        let byte_slice = unsafe {
                            std::slice::from_raw_parts(
                                buf.as_ptr().cast::<u8>(),
                                (bytes_returned as usize).min(aligned_bytes),
                            )
                        };
                        let parsed = parse_notify_buffer(byte_slice);
                        if !parsed.is_empty() {
                            pending_batch.extend(parsed);
                            let now = Instant::now();
                            let start = *batch_start.get_or_insert(now);
                            let max_deadline = start + config.coalesce_max;
                            if now >= max_deadline || pending_batch.len() >= 4096 {
                                flush_pending(
                                    root_display,
                                    &index,
                                    &fs,
                                    &mut pending_batch,
                                    &mut pending_old,
                                    &*on_locked,
                                    &*on_applied,
                                );
                                batch_start = None;
                                coalesce_deadline = None;
                            } else {
                                coalesce_deadline =
                                    Some((now + config.coalesce_min).min(max_deadline));
                            }
                        }
                    }
                }
                Err(e) => {
                    if win32_error_code(&e) == Some(ERROR_NOTIFY_ENUM_DIR.0) {
                        flush_pending(
                            root_display,
                            &index,
                            &fs,
                            &mut pending_batch,
                            &mut pending_old,
                            &*on_locked,
                            &*on_applied,
                        );
                        batch_start = None;
                        coalesce_deadline = None;
                        pending_overflow = true;
                    } else if win32_error_code(&e) == Some(ERROR_OPERATION_ABORTED.0)
                        || stop_flag.load(Ordering::Acquire)
                    {
                        break;
                    } else {
                        on_overflow();
                        thread::sleep(Duration::from_millis(25));
                    }
                }
            }
        } else if wait_res == windows::Win32::Foundation::WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
            break;
        } else if wait_res == WAIT_TIMEOUT {
            flush_pending(
                root_display,
                &index,
                &fs,
                &mut pending_batch,
                &mut pending_old,
                &*on_locked,
                &*on_applied,
            );
            batch_start = None;
            coalesce_deadline = None;
        } else {
            break;
        }
    }

    if is_armed {
        unsafe {
            let _ = CancelIoEx(dir_handle.0, Some(&overlapped));
            let mut dummy = 0u32;
            if GetOverlappedResult(dir_handle.0, &overlapped, &mut dummy, true).is_ok() && dummy > 0
            {
                let byte_slice = std::slice::from_raw_parts(
                    buf.as_ptr().cast::<u8>(),
                    (dummy as usize).min(aligned_bytes),
                );
                pending_batch.extend(parse_notify_buffer(byte_slice));
            }
        }
    }
    flush_pending(
        root_display,
        &index,
        &fs,
        &mut pending_batch,
        &mut pending_old,
        &*on_locked,
        &*on_applied,
    );
    if let Some(orphan) = pending_old.take() {
        let mut guard = index.write().expect("index rwlock poisoned");
        let changed = if let Some(root_id) = guard.lookup(root_display) {
            let c = apply_removed_record(&mut guard, root_id, &orphan);
            maybe_compact(&mut guard);
            c
        } else {
            false
        };
        let slice = std::slice::from_ref(&orphan);
        on_locked(slice);
        drop(guard);
        on_applied(slice, changed);
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_pending(
    root_display: &str,
    index: &Arc<RwLock<Index>>,
    fs: &dyn WatcherFs,
    pending_batch: &mut Vec<NotifyRecord>,
    pending_old: &mut Option<NotifyRecord>,
    on_locked: &dyn Fn(&[NotifyRecord]),
    on_applied: &dyn Fn(&[NotifyRecord], bool),
) {
    if pending_batch.is_empty() {
        return;
    }
    let coalesced = coalesce_records(std::mem::take(pending_batch));
    let mut guard = index.write().expect("index rwlock poisoned");
    let changed = if let Some(root_id) = guard.lookup(root_display) {
        let c = apply_records_with_pending(&mut guard, root_id, &coalesced, fs, pending_old);
        maybe_compact(&mut guard);
        c
    } else {
        false
    };
    on_locked(&coalesced);
    drop(guard);
    on_applied(&coalesced, changed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_fs::attrs::ARCHIVE;

    #[test]
    fn remove_and_rename_match_by_file_id_with_short_83_names() {
        let mut idx = Index::new();
        let root = idx.add_root("C:\\Temp");
        idx.set_file_id(root, 10);

        let sub = idx.insert(
            root,
            "LongDirectoryName",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 100,
                file_id: 20,
            },
        );
        let f1 = idx.insert(
            sub,
            "VeryLongDocumentName.pdf",
            Meta {
                attrs: ARCHIVE,
                size: 1234,
                mtime: 500,
                file_id: 30,
            },
        );
        let f2 = idx.insert(
            sub,
            "AnotherLongReport2026.txt",
            Meta {
                attrs: ARCHIVE,
                size: 42,
                mtime: 600,
                file_id: 31,
            },
        );

        let records = vec![
            // Rename f1 using 8.3 short name in RENAMED_OLD_NAME
            NotifyRecord {
                action: NotifyAction::RenamedOldName,
                rel_path: "LONGDI~1\\VERYLO~1.PDF".to_owned(),
                file_id: 30,
                parent_file_id: 20,
                attrs: ARCHIVE,
                size: 1234,
                mtime: 500,
                reparse_tag: 0,
            },
            NotifyRecord {
                action: NotifyAction::RenamedNewName,
                rel_path: "LONGDI~1\\RenamedFinal.pdf".to_owned(),
                file_id: 30,
                parent_file_id: 20,
                attrs: ARCHIVE,
                size: 1234,
                mtime: 550,
                reparse_tag: 0,
            },
            // Remove f2 using 8.3 short name
            NotifyRecord {
                action: NotifyAction::Removed,
                rel_path: "LONGDI~1\\ANOTHE~1.TXT".to_owned(),
                file_id: 31,
                parent_file_id: 20,
                attrs: ARCHIVE,
                size: 42,
                mtime: 600,
                reparse_tag: 0,
            },
        ];

        let changed = apply_records(&mut idx, root, &records, &NoopWatcherFs);
        assert!(changed);

        assert_eq!(idx.by_file_id(30), Some(f1));
        assert_eq!(idx.name(f1), "RenamedFinal.pdf");
        assert_eq!(idx.meta(f1).mtime, 550);
        assert_eq!(
            idx.lookup("C:\\Temp\\LongDirectoryName\\RenamedFinal.pdf"),
            Some(f1)
        );
        assert_eq!(
            idx.lookup("C:\\Temp\\LongDirectoryName\\VeryLongDocumentName.pdf"),
            None
        );

        assert!(!idx.is_alive(f2));
        assert_eq!(idx.by_file_id(31), None);
        assert_eq!(
            idx.lookup("C:\\Temp\\LongDirectoryName\\AnotherLongReport2026.txt"),
            None
        );
    }

    #[test]
    fn added_resolves_tilde_83_name_via_resolver() {
        struct MockShortResolver;
        impl WatcherFs for MockShortResolver {
            fn resolve_long_leaf(&self, full_path: &str) -> Option<String> {
                if full_path.ends_with("\\SHORTN~1.TXT") {
                    Some("Žluťoučký dlouhý název.txt".to_owned())
                } else {
                    None
                }
            }
            fn list_dir_children(&self, _full_dir_path: &str) -> Vec<AddedChild> {
                Vec::new()
            }
        }

        let mut idx = Index::new();
        let root = idx.add_root("C:\\Temp");
        idx.set_file_id(root, 1);

        let rec = NotifyRecord {
            action: NotifyAction::Added,
            rel_path: "SHORTN~1.TXT".to_owned(),
            file_id: 99,
            parent_file_id: 1,
            attrs: ARCHIVE,
            size: 77,
            mtime: 888,
            reparse_tag: 0,
        };
        assert!(apply_records(&mut idx, root, &[rec], &MockShortResolver));
        let eid = idx.by_file_id(99).expect("inserted");
        assert_eq!(idx.name(eid), "Žluťoučký dlouhý název.txt");
    }

    #[test]
    fn replay_skips_cyclic_rename_file_id_mismatch_and_handles_trailing_orphan_after_compact() {
        let mut idx = Index::new();
        let sentinel_root = idx.add_root("Z:\\Unused");
        idx.set_file_id(sentinel_root, 9_000_000);
        // Add >=1030 children under sentinel_root then remove >25% so maybe_compact actually triggers
        let mut victims = Vec::new();
        for i in 0..1100u64 {
            let id = idx.insert(
                sentinel_root,
                &format!("pad_{i}.txt"),
                Meta {
                    attrs: ARCHIVE,
                    size: 1,
                    mtime: 1,
                    file_id: 100_000 + i,
                },
            );
            if i < 400 {
                victims.push(id);
            }
        }
        for vid in victims {
            idx.remove(vid);
        }

        let root = idx.add_root("C:\\Temp");
        idx.set_file_id(root, 1);
        let p = idx.insert(
            root,
            "P",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 2,
            },
        );
        let q = idx.insert(
            p,
            "Q",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 3,
            },
        );
        let _newer = idx.insert(
            q,
            "same.txt",
            Meta {
                attrs: ARCHIVE,
                size: 55,
                mtime: 99,
                file_id: 200,
            },
        );
        let _orphan_target = idx.insert(
            root,
            "orphan.txt",
            Meta {
                attrs: ARCHIVE,
                size: 10,
                mtime: 10,
                file_id: 300,
            },
        );

        let records = vec![
            // Stale cyclic move: Q is currently inside P, trying to move P under Q must not panic
            NotifyRecord {
                action: NotifyAction::RenamedOldName,
                rel_path: "P".to_owned(),
                file_id: 2,
                parent_file_id: 1,
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                reparse_tag: 0,
            },
            NotifyRecord {
                action: NotifyAction::RenamedNewName,
                rel_path: "Q\\P".to_owned(),
                file_id: 2,
                parent_file_id: 3,
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                reparse_tag: 0,
            },
            // Stale delete for old same.txt (fid 199) must NOT delete newer same.txt (fid 200)
            NotifyRecord {
                action: NotifyAction::Removed,
                rel_path: "P\\Q\\same.txt".to_owned(),
                file_id: 199,
                parent_file_id: 3,
                attrs: ARCHIVE,
                size: 12,
                mtime: 50,
                reparse_tag: 0,
            },
            // Trailing orphan RenamedOldName removed safely even when sentinel_root causes root remap
            NotifyRecord {
                action: NotifyAction::RenamedOldName,
                rel_path: "orphan.txt".to_owned(),
                file_id: 300,
                parent_file_id: 1,
                attrs: ARCHIVE,
                size: 10,
                mtime: 10,
                reparse_tag: 0,
            },
        ];

        assert!(apply_records(&mut idx, root, &records, &NoopWatcherFs));
        let newer_after = idx.by_file_id(200).expect("newer entry must survive");
        assert!(idx.is_alive(newer_after));
        assert_eq!(idx.meta(newer_after).size, 55);
        assert_eq!(idx.lookup("C:\\Temp\\P\\Q\\same.txt"), Some(newer_after));
        assert_eq!(idx.by_file_id(300), None);
        assert_eq!(idx.lookup("C:\\Temp\\orphan.txt"), None);
    }

    #[test]
    fn flush_pending_invokes_on_locked_even_on_cold_empty_index() {
        let index = Arc::new(RwLock::new(Index::new()));
        let locked_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let locked_clone = Arc::clone(&locked_calls);
        let mut batch = vec![NotifyRecord {
            action: NotifyAction::Added,
            rel_path: "cold_file.txt".to_owned(),
            file_id: 42,
            parent_file_id: 1,
            attrs: ARCHIVE,
            size: 5,
            mtime: 10,
            reparse_tag: 0,
        }];
        let mut pending_old = None;
        let applied_changed = Arc::new(AtomicBool::new(true));
        let applied_clone = Arc::clone(&applied_changed);
        flush_pending(
            "C:\\ColdMissingRoot",
            &index,
            &NoopWatcherFs,
            &mut batch,
            &mut pending_old,
            &|recs| {
                locked_clone.fetch_add(recs.len(), Ordering::SeqCst);
            },
            &|_recs, ch| {
                applied_clone.store(ch, Ordering::SeqCst);
            },
        );
        assert_eq!(locked_calls.load(Ordering::SeqCst), 1);
        assert!(!applied_changed.load(Ordering::SeqCst));
        assert!(index.read().unwrap().is_empty());
    }
}
