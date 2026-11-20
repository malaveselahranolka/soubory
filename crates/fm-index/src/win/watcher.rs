//! Live `ReadDirectoryChangesExW(ReadDirectoryNotifyExtendedInformation)` watcher with coalescing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use fm_fs::attrs::{DIRECTORY, REPARSE_POINT};
use fm_fs::{fold, fold_into};
use windows::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, WAIT_EVENT, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
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
    CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::PCWSTR;

use crate::arena::{EntryId, Index, Meta, NONE, eq_case_insensitive};
use crate::win::walker::{DirSource, MAX_WALK_DEPTH, WalkFolderRef, Win32DirSource};
use crate::win::{
    WinHandle, decode_utf16_le_into, is_cloud_reparse_tag, join_child_path, lock_or_recover,
    normalize_display_path, open_dir_handle, read_or_recover, to_extended_path, wide_null,
    win32_error_code, win32_to_io, write_or_recover,
};

/// Default 64 KiB `ReadDirectoryChangesExW` notification buffer size.
pub const DEFAULT_WATCHER_BUFFER_BYTES: usize = 64 * 1024;

/// Default minimum delay between a finished walk and an overflow-triggered follow-up walk.
pub const DEFAULT_OVERFLOW_REWALK_COOLDOWN: Duration = Duration::from_secs(60);

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
    /// Minimum delay after a finished walk before an overflow-triggered re-walk starts (default 60 s).
    pub overflow_rewalk_cooldown: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            buffer_bytes: DEFAULT_WATCHER_BUFFER_BYTES,
            coalesce_min: Duration::from_millis(50),
            coalesce_max: Duration::from_millis(100),
            overflow_rewalk_cooldown: DEFAULT_OVERFLOW_REWALK_COOLDOWN,
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

    /// Checked variant returning `Err` on access denied or I/O failure so callers can record
    /// newly encountered denied folders.
    fn list_dir_children_checked(&self, full_dir_path: &str) -> io::Result<Vec<AddedChild>> {
        Ok(self.list_dir_children(full_dir_path))
    }
}

/// Real Win32 [`WatcherFs`] implementation using `GetLongPathNameW` and [`Win32DirSource`].
#[derive(Debug, Default, Clone, Copy)]
pub struct Win32WatcherFs;

impl WatcherFs for Win32WatcherFs {
    fn resolve_long_leaf(&self, full_path: &str) -> Option<String> {
        if !looks_like_short_name(leaf_of(full_path.trim_end_matches('\\'))) {
            return None;
        }
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
        self.list_dir_children_checked(full_dir_path)
            .unwrap_or_default()
    }

    fn list_dir_children_checked(&self, full_dir_path: &str) -> io::Result<Vec<AddedChild>> {
        let source = Win32DirSource;
        let handle = source.open_dir(full_dir_path)?;
        let mut scratch = <Win32DirSource as DirSource>::Scratch::default();
        let mut children = Vec::new();
        let mut restart = true;
        loop {
            match source.read_batch(&handle, restart, &mut scratch, &mut |entry| {
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
                Ok(true) => restart = false,
                Ok(false) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(children)
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

/// Access-denied folder set remembered from walks so watcher updates under them are dropped.
#[derive(Debug, Default, Clone)]
pub(crate) struct DeniedFilter {
    pub(crate) root_denied: bool,
    pub(crate) file_ids: HashSet<u64>,
    pub(crate) rel_prefixes_folded: Vec<String>,
}

impl DeniedFilter {
    pub(crate) fn from_walk(refs: &[WalkFolderRef]) -> Self {
        let mut out = Self::default();
        out.extend_from(refs);
        out
    }

    pub(crate) fn extend_from(&mut self, refs: &[WalkFolderRef]) {
        for r in refs {
            if r.rel_path.is_empty() {
                self.root_denied = true;
            } else {
                let key = case_key(&r.rel_path);
                if !self.rel_prefixes_folded.contains(&key) {
                    self.rel_prefixes_folded.push(key);
                }
            }
            if r.file_id != 0 {
                self.file_ids.insert(r.file_id);
            }
        }
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        !self.root_denied && self.file_ids.is_empty() && self.rel_prefixes_folded.is_empty()
    }

    fn matches_rel_dir(&self, rel_dir: &str) -> bool {
        if self.root_denied {
            return true;
        }
        if rel_dir.is_empty() || self.rel_prefixes_folded.is_empty() {
            return false;
        }
        let key = case_key(rel_dir);
        self.rel_prefixes_folded.iter().any(|prefix| {
            key == *prefix
                || (key.starts_with(prefix.as_str())
                    && key.as_bytes().get(prefix.len()) == Some(&b'\\'))
        })
    }

    pub(crate) fn is_record_denied(
        &self,
        idx: Option<(&Index, EntryId)>,
        rec: &NotifyRecord,
        use_file_id: bool,
    ) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.root_denied {
            return true;
        }
        if use_file_id && rec.parent_file_id != 0 && self.file_ids.contains(&rec.parent_file_id) {
            return true;
        }
        if let Some(rel_parent) = parent_rel_of(&rec.rel_path)
            && self.matches_rel_dir(rel_parent)
        {
            return true;
        }
        if let Some((index, root_id)) = idx
            && use_file_id
            && rec.parent_file_id != 0
            && let Some(pid) = index.by_file_id(rec.parent_file_id)
        {
            let mut cur = pid;
            while cur != NONE && cur != root_id {
                let fid = index.file_id[cur as usize];
                if fid != 0 && self.file_ids.contains(&fid) {
                    return true;
                }
                cur = index.parent[cur as usize];
            }
            if pid != root_id && is_descendant_of(index, pid, root_id) {
                let root_disp = index.path(root_id);
                let full_pid = index.path(pid);
                if let Some(rel) = relative_under_root(&full_pid, &root_disp)
                    && self.matches_rel_dir(rel)
                {
                    return true;
                }
            }
        }
        false
    }
}

fn is_descendant_of(idx: &Index, mut node: EntryId, ancestor: EntryId) -> bool {
    while node != NONE {
        if node == ancestor {
            return true;
        }
        node = idx.parent[node as usize];
    }
    false
}

/// Lowercases a display path without stripping diacritics so distinct non-ASCII folder names never collide.
fn case_key(s: &str) -> String {
    let norm = normalize_display_path(s);
    if norm.is_ascii() {
        norm.to_ascii_lowercase()
    } else {
        norm.chars().flat_map(char::to_lowercase).collect()
    }
}

fn relative_under_root<'a>(full: &'a str, root_disp: &str) -> Option<&'a str> {
    let root_trimmed = root_disp.trim_end_matches('\\');
    if root_trimmed.is_empty() {
        return Some(full.trim_start_matches('\\'));
    }
    if full.is_ascii() && root_trimmed.is_ascii() {
        if full.len() < root_trimmed.len() {
            return None;
        }
        let (prefix, rest) = full.split_at(root_trimmed.len());
        if !prefix.eq_ignore_ascii_case(root_trimmed) {
            return None;
        }
        if rest.is_empty() {
            return Some("");
        }
        return rest.strip_prefix('\\');
    }
    let mut full_ci = full.char_indices();
    let mut byte_end = 0usize;
    for rc in root_trimmed.chars() {
        let (idx, fc) = full_ci.next()?;
        if !fc.to_lowercase().eq(rc.to_lowercase()) {
            return None;
        }
        byte_end = idx + fc.len_utf8();
    }
    let rest = &full[byte_end..];
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('\\')
}
/// Coalesces redundant `Modified` events within a batch while preserving relative ordering.
#[must_use]
pub fn coalesce_records(records: Vec<NotifyRecord>) -> Vec<NotifyRecord> {
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

/// Disk enumeration + short-name resolution results gathered OUTSIDE the index write lock.
#[derive(Default)]
pub(crate) struct PrefetchedBatch {
    long_leaves: HashMap<String, String>,
    dir_children: HashMap<String, Vec<AddedChild>>,
    pub(crate) extra_denied: Vec<WalkFolderRef>,
}

impl WatcherFs for PrefetchedBatch {
    fn resolve_long_leaf(&self, full_path: &str) -> Option<String> {
        let key = fold(&normalize_display_path(full_path));
        self.long_leaves.get(&key).cloned()
    }

    fn list_dir_children(&self, full_dir_path: &str) -> Vec<AddedChild> {
        let key = fold(&normalize_display_path(full_dir_path));
        self.dir_children.get(&key).cloned().unwrap_or_default()
    }
}

/// `true` only for leaves shaped like a generated 8.3 short name (`LONGDI~1`, `VERYLO~1.PDF`):
/// ASCII, base <= 8 chars ending in `~` + digits, extension <= 3 chars. Names like `~$doc.docx`
/// never reach `GetLongPathNameW`, whose no-recall behaviour on cloud placeholders is unverified.
pub(crate) fn looks_like_short_name(leaf: &str) -> bool {
    if !leaf.is_ascii() || leaf.contains(' ') {
        return false;
    }
    let (base, ext) = leaf.split_once('.').unwrap_or((leaf, ""));
    if base.len() > 8 || ext.len() > 3 || ext.contains('.') {
        return false;
    }
    base.rsplit_once('~').is_some_and(|(head, digits)| {
        !head.is_empty() && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

#[inline]
fn record_needs_dir_list(rec: &NotifyRecord) -> bool {
    (rec.action == NotifyAction::Added || rec.action == NotifyAction::RenamedNewName)
        && (rec.attrs & DIRECTORY) != 0
        && ((rec.attrs & REPARSE_POINT) == 0 || is_cloud_reparse_tag(rec.reparse_tag))
}

#[inline]
pub(crate) fn batch_needs_prefetch(records: &[NotifyRecord]) -> bool {
    records.iter().any(|rec| {
        record_needs_dir_list(rec)
            || ((rec.action == NotifyAction::Added || rec.action == NotifyAction::RenamedNewName)
                && looks_like_short_name(leaf_of(&rec.rel_path)))
    })
}

pub(crate) fn collect_parent_hints(
    idx: &Index,
    root_id: EntryId,
    records: &[NotifyRecord],
) -> HashMap<u64, String> {
    let mut hints = HashMap::new();
    for rec in records {
        if rec.parent_file_id == 0 || hints.contains_key(&rec.parent_file_id) {
            continue;
        }
        if (record_needs_dir_list(rec) || looks_like_short_name(leaf_of(&rec.rel_path)))
            && let Some(pid) = idx.by_file_id(rec.parent_file_id)
            && is_descendant_of(idx, pid, root_id)
        {
            hints.insert(rec.parent_file_id, idx.path(pid));
        }
    }
    hints
}

/// Lists added/moved-in directory subtrees and resolves 8.3 leaves without any index lock.
/// Records whose index is in `skip_dir_list` (renames resolving in the index) are not listed;
/// `stop` aborts the listing early so `Service::stop` never waits on a huge subtree.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prefetch_batch_outside_lock(
    root_display: &str,
    records: &[NotifyRecord],
    parent_hints: &HashMap<u64, String>,
    skip_dir_list: &HashSet<usize>,
    denied: &DeniedFilter,
    use_file_id: bool,
    fs: &dyn WatcherFs,
    stop: &AtomicBool,
) -> PrefetchedBatch {
    let mut out = PrefetchedBatch::default();

    for (i, rec) in records.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if rec.action != NotifyAction::Added && rec.action != NotifyAction::RenamedNewName {
            continue;
        }
        let raw_leaf = leaf_of(&rec.rel_path);
        if raw_leaf.is_empty() {
            continue;
        }
        let needs_short = looks_like_short_name(raw_leaf);
        let needs_dir = record_needs_dir_list(rec) && !skip_dir_list.contains(&i);
        if !needs_short && !needs_dir {
            continue;
        }
        if denied.is_record_denied(None, rec, use_file_id) {
            continue;
        }

        let rel_parent = parent_rel_of(&rec.rel_path).unwrap_or("");
        let mut candidate_parents: Vec<(String, String)> = Vec::with_capacity(2);
        let p_by_rel = if rel_parent.is_empty() {
            root_display.to_owned()
        } else {
            join_child_path(root_display, rel_parent)
        };
        candidate_parents.push((p_by_rel, rel_parent.to_owned()));

        if use_file_id
            && rec.parent_file_id != 0
            && let Some(hint_path) = parent_hints.get(&rec.parent_file_id)
            && !eq_case_insensitive(hint_path, &candidate_parents[0].0)
        {
            let hint_rel = relative_under_root(hint_path, root_display)
                .unwrap_or("")
                .to_owned();
            candidate_parents.push((hint_path.clone(), hint_rel));
        }

        for (parent_full, parent_rel) in candidate_parents {
            let short_full = join_child_path(&parent_full, raw_leaf);
            let effective_leaf = if needs_short {
                if let Some(long_leaf) = fs.resolve_long_leaf(&short_full) {
                    out.long_leaves.insert(
                        fold(&normalize_display_path(&short_full)),
                        long_leaf.clone(),
                    );
                    long_leaf
                } else {
                    raw_leaf.to_owned()
                }
            } else {
                raw_leaf.to_owned()
            };

            if needs_dir {
                let start_full = join_child_path(&parent_full, &effective_leaf);
                let start_rel = if parent_rel.is_empty() {
                    effective_leaf
                } else {
                    join_child_path(&parent_rel, &effective_leaf)
                };
                if denied.matches_rel_dir(&start_rel) {
                    continue;
                }
                let start_depth = start_rel.split('\\').filter(|s| !s.is_empty()).count() as u16;
                if start_depth >= MAX_WALK_DEPTH {
                    continue;
                }
                let mut q = VecDeque::new();
                q.push_back((start_full, start_rel, rec.file_id, start_depth));
                while let Some((dir_full, dir_rel, dir_fid, depth)) = q.pop_front() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let key = fold(&normalize_display_path(&dir_full));
                    if out.dir_children.contains_key(&key) {
                        continue;
                    }
                    match fs.list_dir_children_checked(&dir_full) {
                        Ok(children) => {
                            if depth + 1 < MAX_WALK_DEPTH {
                                for c in &children {
                                    let c_is_dir = (c.meta.attrs & DIRECTORY) != 0;
                                    let c_descend = c_is_dir
                                        && ((c.meta.attrs & REPARSE_POINT) == 0
                                            || is_cloud_reparse_tag(c.reparse_tag));
                                    if c_descend {
                                        let c_rel = join_child_path(&dir_rel, &c.name);
                                        if !denied.matches_rel_dir(&c_rel) {
                                            q.push_back((
                                                join_child_path(&dir_full, &c.name),
                                                c_rel,
                                                c.meta.file_id,
                                                depth + 1,
                                            ));
                                        }
                                    }
                                }
                            }
                            out.dir_children.insert(key, children);
                        }
                        Err(err) => {
                            if err.kind() == io::ErrorKind::PermissionDenied {
                                out.extra_denied.push(WalkFolderRef {
                                    file_id: dir_fid,
                                    rel_path: dir_rel,
                                });
                            }
                            out.dir_children.insert(key, Vec::new());
                        }
                    }
                }
            }
        }
    }

    out
}

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
    let empty_denied = DeniedFilter::default();
    apply_records_filtered(idx, root_id, records, fs, &empty_denied, true)
}

/// Applies `records` while filtering denied parents and choosing NTFS (`use_file_id = true`) or
/// path-only resolution (`use_file_id = false`).
pub(crate) fn apply_records_filtered(
    idx: &mut Index,
    root_id: EntryId,
    records: &[NotifyRecord],
    fs: &dyn WatcherFs,
    denied: &DeniedFilter,
    use_file_id: bool,
) -> bool {
    let mut pending_old = None;
    let mut changed = apply_records_with_pending_filtered(
        idx,
        root_id,
        records,
        fs,
        &mut pending_old,
        denied,
        use_file_id,
        false,
    );
    if let Some(orphan) = pending_old.take()
        && !denied.is_record_denied(Some((idx, root_id)), &orphan, use_file_id)
    {
        changed |= apply_removed_record(idx, root_id, &orphan, use_file_id);
    }
    maybe_compact(idx);
    changed
}

/// Replays records captured during a re-walk onto the merged fresh index.
///
/// Unlike the live path, a trailing unpaired `RenamedOldName` is dropped rather than applied as
/// a removal: its `RenamedNewName` may have been lost in a gap, and the fresh walk already shows
/// the item wherever it ended up.
pub(crate) fn replay_records_filtered(
    idx: &mut Index,
    root_id: EntryId,
    records: &[NotifyRecord],
    fs: &dyn WatcherFs,
    denied: &DeniedFilter,
    use_file_id: bool,
) -> bool {
    let mut pending_old = None;
    let changed = apply_records_with_pending_filtered(
        idx,
        root_id,
        records,
        fs,
        &mut pending_old,
        denied,
        use_file_id,
        true,
    );
    maybe_compact(idx);
    changed
}

/// Indices of `RenamedNewName` records whose paired old name resolves in `idx` right now: those
/// renames move the existing entry (subtree included), so their new path need not be listed.
// shortcut: decided before the batch is applied; if an earlier record in the same batch (or a
// merge between this read lock and the apply) makes the old name stop resolving, the rename falls
// back to an add with no listing and the folder stays empty until the next walk. Upgrade by
// re-listing such folders after the apply if this shows up in practice.
pub(crate) fn renames_resolved_in_index(
    idx: &Index,
    root_id: EntryId,
    records: &[NotifyRecord],
    pending_old: Option<&NotifyRecord>,
    denied: &DeniedFilter,
    use_file_id: bool,
) -> HashSet<usize> {
    let mut out = HashSet::new();
    let mut pending = pending_old;
    for (i, rec) in records.iter().enumerate() {
        match rec.action {
            NotifyAction::RenamedOldName => pending = Some(rec),
            NotifyAction::RenamedNewName => {
                if let Some(old) = pending.take()
                    && !splits_into_remove_add(old, rec, use_file_id)
                    && !denied.is_record_denied(Some((idx, root_id)), old, use_file_id)
                    && resolve_rename_source(idx, root_id, old, use_file_id).is_some()
                {
                    out.insert(i);
                }
            }
            _ => pending = None,
        }
    }
    out
}

/// `replay` marks records re-applied onto a merged fresh walk (see [`replay_records_filtered`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_records_with_pending_filtered(
    idx: &mut Index,
    root_id: EntryId,
    records: &[NotifyRecord],
    fs: &dyn WatcherFs,
    pending_old: &mut Option<NotifyRecord>,
    denied: &DeniedFilter,
    use_file_id: bool,
    replay: bool,
) -> bool {
    if !idx.is_alive(root_id) {
        *pending_old = None;
        return false;
    }
    let mut changed = false;

    for rec in records {
        match rec.action {
            NotifyAction::RenamedOldName => {
                if let Some(prev) = pending_old.replace(rec.clone())
                    && !denied.is_record_denied(Some((idx, root_id)), &prev, use_file_id)
                {
                    changed |= apply_removed_record(idx, root_id, &prev, use_file_id);
                }
            }
            NotifyAction::RenamedNewName => {
                if let Some(old) = pending_old.take() {
                    let old_denied =
                        denied.is_record_denied(Some((idx, root_id)), &old, use_file_id);
                    let new_denied =
                        denied.is_record_denied(Some((idx, root_id)), rec, use_file_id);
                    match (old_denied, new_denied) {
                        (false, false) => {
                            changed |=
                                apply_rename_pair(idx, root_id, &old, rec, fs, use_file_id, replay);
                        }
                        (false, true) => {
                            changed |= apply_removed_record(idx, root_id, &old, use_file_id);
                        }
                        (true, false) => {
                            changed |= apply_added_record(idx, root_id, rec, fs, use_file_id);
                        }
                        (true, true) => {}
                    }
                } else if !denied.is_record_denied(Some((idx, root_id)), rec, use_file_id) {
                    changed |= apply_added_record(idx, root_id, rec, fs, use_file_id);
                }
            }
            NotifyAction::Added => {
                if let Some(prev) = pending_old.take()
                    && !denied.is_record_denied(Some((idx, root_id)), &prev, use_file_id)
                {
                    changed |= apply_removed_record(idx, root_id, &prev, use_file_id);
                }
                if !denied.is_record_denied(Some((idx, root_id)), rec, use_file_id) {
                    changed |= apply_added_record(idx, root_id, rec, fs, use_file_id);
                }
            }
            NotifyAction::Removed => {
                if let Some(prev) = pending_old.take()
                    && !denied.is_record_denied(Some((idx, root_id)), &prev, use_file_id)
                {
                    changed |= apply_removed_record(idx, root_id, &prev, use_file_id);
                }
                if !denied.is_record_denied(Some((idx, root_id)), rec, use_file_id) {
                    changed |= apply_removed_record(idx, root_id, rec, use_file_id);
                }
            }
            NotifyAction::Modified => {
                if let Some(prev) = pending_old.take()
                    && !denied.is_record_denied(Some((idx, root_id)), &prev, use_file_id)
                {
                    changed |= apply_removed_record(idx, root_id, &prev, use_file_id);
                }
                if !denied.is_record_denied(Some((idx, root_id)), rec, use_file_id) {
                    changed |= apply_modified_record(idx, root_id, rec, use_file_id);
                }
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

fn parent_agrees_with_rel_path(
    idx: &Index,
    root_id: EntryId,
    pid: EntryId,
    rel_parent: &str,
) -> bool {
    if rel_parent.is_empty() {
        return pid == root_id;
    }
    if pid == root_id {
        return false;
    }

    let mut chain = Vec::new();
    let mut cur = pid;
    while cur != NONE && cur != root_id {
        chain.push(cur);
        cur = idx.parent[cur as usize];
    }
    if cur != root_id {
        return false;
    }

    let rec_segs: Vec<&str> = rel_parent.split('\\').filter(|s| !s.is_empty()).collect();
    if rec_segs.len() != chain.len() {
        return false;
    }

    chain
        .iter()
        .rev()
        .zip(&rec_segs)
        .all(|(&node_id, &rec_seg)| {
            rec_seg.contains('~') || eq_case_insensitive(idx.name(node_id), rec_seg)
        })
}

fn resolve_parent_entry(
    idx: &Index,
    root_id: EntryId,
    parent_file_id: u64,
    rel_path: &str,
    use_file_id: bool,
) -> Option<EntryId> {
    let rel_parent = parent_rel_of(rel_path).unwrap_or("");
    if !use_file_id {
        return if rel_parent.is_empty() {
            Some(root_id)
        } else {
            let root_path = idx.path(root_id);
            let full = join_child_path(&root_path, rel_parent);
            idx.lookup(&full)
        };
    }

    if parent_file_id != 0
        && let Some(pid) = idx.by_file_id(parent_file_id)
        && parent_agrees_with_rel_path(idx, root_id, pid, rel_parent)
    {
        return Some(pid);
    }

    if rel_parent.is_empty() {
        Some(root_id)
    } else {
        let root_path = idx.path(root_id);
        let full = join_child_path(&root_path, rel_parent);
        idx.lookup(&full)
    }
}

fn resolve_target_in_parent(
    idx: &Index,
    parent_id: EntryId,
    file_id: u64,
    leaf: &str,
    use_file_id: bool,
) -> Option<EntryId> {
    let mut folded = String::with_capacity(leaf.len());
    fold_into(leaf, &mut folded);
    let by_name = idx.find_child(parent_id, folded.as_bytes(), leaf);
    if let Some(cand) = by_name {
        if !use_file_id || file_id == 0 {
            return Some(cand);
        }
        let cand_fid = idx.file_id(cand);
        if cand_fid == 0 || cand_fid == file_id {
            return Some(cand);
        }
    }

    if use_file_id && file_id != 0 && (leaf.contains('~') || by_name.is_none()) {
        return idx.find_child_by_file_id(parent_id, file_id);
    }
    None
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
    if looks_like_short_name(raw_leaf) {
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
    use_file_id: bool,
) -> bool {
    let Some(parent_id) =
        resolve_parent_entry(idx, root_id, rec.parent_file_id, &rec.rel_path, use_file_id)
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

fn apply_removed_record(
    idx: &mut Index,
    root_id: EntryId,
    rec: &NotifyRecord,
    use_file_id: bool,
) -> bool {
    let target = if let Some(parent_id) =
        resolve_parent_entry(idx, root_id, rec.parent_file_id, &rec.rel_path, use_file_id)
    {
        resolve_target_in_parent(
            idx,
            parent_id,
            rec.file_id,
            leaf_of(&rec.rel_path),
            use_file_id,
        )
    } else {
        None
    };
    let Some(id) = target else {
        return false;
    };
    idx.remove(id);
    true
}

fn apply_modified_record(
    idx: &mut Index,
    root_id: EntryId,
    rec: &NotifyRecord,
    use_file_id: bool,
) -> bool {
    let Some(parent_id) =
        resolve_parent_entry(idx, root_id, rec.parent_file_id, &rec.rel_path, use_file_id)
    else {
        return false;
    };
    let Some(target) = resolve_target_in_parent(
        idx,
        parent_id,
        rec.file_id,
        leaf_of(&rec.rel_path),
        use_file_id,
    ) else {
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

#[inline]
fn splits_into_remove_add(old: &NotifyRecord, new: &NotifyRecord, use_file_id: bool) -> bool {
    use_file_id && old.file_id != 0 && new.file_id != 0 && old.file_id != new.file_id
}

fn resolve_rename_source(
    idx: &Index,
    root_id: EntryId,
    old: &NotifyRecord,
    use_file_id: bool,
) -> Option<EntryId> {
    resolve_parent_entry(idx, root_id, old.parent_file_id, &old.rel_path, use_file_id)
        .and_then(|op| {
            resolve_target_in_parent(idx, op, old.file_id, leaf_of(&old.rel_path), use_file_id)
        })
        .or_else(|| {
            // Never use global cross-folder FileId fallback for files (hard links share FileIds).
            (use_file_id && old.file_id != 0 && (old.attrs & DIRECTORY) != 0)
                .then(|| idx.by_file_id(old.file_id).filter(|&id| idx.is_dir(id)))
                .flatten()
        })
}

fn apply_rename_pair(
    idx: &mut Index,
    root_id: EntryId,
    old: &NotifyRecord,
    new: &NotifyRecord,
    fs: &dyn WatcherFs,
    use_file_id: bool,
    replay: bool,
) -> bool {
    if splits_into_remove_add(old, new, use_file_id) {
        let mut changed = apply_removed_record(idx, root_id, old, use_file_id);
        changed |= apply_added_record(idx, root_id, new, fs, use_file_id);
        return changed;
    }

    let Some(target) = resolve_rename_source(idx, root_id, old, use_file_id) else {
        return apply_added_record(idx, root_id, new, fs, use_file_id);
    };
    if idx.parent[target as usize] == NONE {
        return false;
    }
    let Some(new_parent) =
        resolve_parent_entry(idx, root_id, new.parent_file_id, &new.rel_path, use_file_id).or_else(
            || {
                // Destination folder known by FileId under a newer path (it was moved or renamed
                // later, e.g. a replay onto a fresh walk that listed the final layout).
                use_file_id
                    .then(|| idx.by_file_id(new.parent_file_id))
                    .flatten()
                    .filter(|&p| idx.is_dir(p) && is_descendant_of(idx, p, root_id))
            },
        )
    else {
        // Moved into a folder the index does not know: drop it instead of leaving a ghost at
        // the old path.
        idx.remove(target);
        return true;
    };

    if is_ancestor_or_same(idx, target, new_parent) {
        return false;
    }
    let raw_new_leaf = leaf_of(&new.rel_path);
    if raw_new_leaf.is_empty() {
        return false;
    }
    let new_leaf = resolve_leaf_if_short(idx, new_parent, raw_new_leaf, fs);
    // Live renames onto an existing folder are impossible on disk, so a live occupant is stale
    // and gets replaced; only a replay may find the rename already applied.
    if replay
        && keep_same_folder_at_rename_target(idx, target, new_parent, &new_leaf, new, use_file_id)
    {
        return true;
    }
    let mut changed = false;
    if idx.parent[target as usize] != new_parent || idx.display_str_unchecked(target) != new_leaf {
        idx.move_to(target, new_parent, &new_leaf);
        changed = true;
    }

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
        changed = true;
    }
    changed
}

/// Replay idempotence: when a folder that is the same item as the renamed one (same FileId, or
/// unknowable on path-only volumes) already sits at the new path, the rename was already applied,
/// e.g. a replayed `Added X` + `X -> Y` onto a fresh walk that listed `Y`. Keeps that folder and
/// its subtree, drops the stale source, and returns `true`; `move_to` would delete the occupant.
fn keep_same_folder_at_rename_target(
    idx: &mut Index,
    target: EntryId,
    new_parent: EntryId,
    new_leaf: &str,
    new: &NotifyRecord,
    use_file_id: bool,
) -> bool {
    let mut folded = String::with_capacity(new_leaf.len());
    fold_into(new_leaf, &mut folded);
    let Some(occ) = idx.find_child(new_parent, folded.as_bytes(), new_leaf) else {
        return false;
    };
    if occ == target || !idx.is_dir(occ) {
        return false;
    }
    let rename_fid = if new.file_id != 0 {
        new.file_id
    } else {
        idx.file_id(target)
    };
    let occ_fid = idx.file_id(occ);
    if use_file_id && rename_fid != 0 && occ_fid != 0 && occ_fid != rename_fid {
        return false;
    }
    idx.remove(target);
    if idx.display_str_unchecked(occ) != new_leaf {
        idx.rename(occ, new_leaf);
    }
    if (new.attrs & DIRECTORY) != 0 {
        let meta = Meta {
            attrs: new.attrs,
            size: 0,
            mtime: new.mtime,
            file_id: if new.file_id != 0 {
                new.file_id
            } else {
                occ_fid
            },
        };
        if idx.meta(occ) != meta {
            idx.update_meta(occ, meta);
        }
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

/// Callback invoked when a watcher recovers after a failure and triggers a reconciling re-walk.
pub type OnRestartedCallback = Box<dyn Fn() + Send + Sync>;

/// Callback invoked when a non-fatal watcher error occurs (watcher retries with backoff).
pub type OnErrorCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Internal hook invoked while the index write lock is still held after applying records.
pub(crate) type OnLockedCallback = Box<dyn Fn(&[NotifyRecord]) + Send + Sync>;

/// Stateful coalescing + orphan-rename tracker driven by the watcher loop and unit tests.
#[derive(Default)]
pub(crate) struct WatcherBatchState {
    pub(crate) pending_batch: Vec<NotifyRecord>,
    pub(crate) pending_old: Option<NotifyRecord>,
    /// Watcher stop flag: aborts subtree listing during a flush.
    pub(crate) stop: Arc<AtomicBool>,
}

impl WatcherBatchState {
    pub(crate) fn clear_pending_old(&mut self) {
        self.pending_old = None;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn on_overflow(
        &mut self,
        root_display: &str,
        index: &Arc<RwLock<Index>>,
        apply_mu: &Arc<Mutex<()>>,
        denied_filter: &Arc<RwLock<DeniedFilter>>,
        use_file_id: bool,
        fs: &dyn WatcherFs,
        on_locked: &dyn Fn(&[NotifyRecord]),
        on_applied: &dyn Fn(&[NotifyRecord], bool),
    ) {
        self.flush(
            root_display,
            index,
            apply_mu,
            denied_filter,
            use_file_id,
            fs,
            on_locked,
            on_applied,
        );
        self.pending_old = None;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn flush(
        &mut self,
        root_display: &str,
        index: &Arc<RwLock<Index>>,
        apply_mu: &Arc<Mutex<()>>,
        denied_filter: &Arc<RwLock<DeniedFilter>>,
        use_file_id: bool,
        fs: &dyn WatcherFs,
        on_locked: &dyn Fn(&[NotifyRecord]),
        on_applied: &dyn Fn(&[NotifyRecord], bool),
    ) {
        if self.pending_batch.is_empty() {
            return;
        }
        let coalesced = coalesce_records(std::mem::take(&mut self.pending_batch));
        let needs_prefetch = batch_needs_prefetch(&coalesced);

        let prefetched = if needs_prefetch {
            let denied_snap = read_or_recover(denied_filter).clone();
            let (hints, skip) = {
                let rguard = read_or_recover(index);
                match rguard.lookup(root_display) {
                    Some(rid) => (
                        if use_file_id {
                            collect_parent_hints(&rguard, rid, &coalesced)
                        } else {
                            HashMap::new()
                        },
                        renames_resolved_in_index(
                            &rguard,
                            rid,
                            &coalesced,
                            self.pending_old.as_ref(),
                            &denied_snap,
                            use_file_id,
                        ),
                    ),
                    None => (HashMap::new(), HashSet::new()),
                }
            };
            Some(prefetch_batch_outside_lock(
                root_display,
                &coalesced,
                &hints,
                &skip,
                &denied_snap,
                use_file_id,
                fs,
                &self.stop,
            ))
        } else {
            None
        };

        let _apply_guard = lock_or_recover(apply_mu);
        if let Some(ref pf) = prefetched
            && !pf.extra_denied.is_empty()
        {
            write_or_recover(denied_filter).extend_from(&pf.extra_denied);
        }

        let mut guard = write_or_recover(index);
        let changed = if let Some(root_id) = guard.lookup(root_display) {
            let df = read_or_recover(denied_filter);
            let fs_ref: &dyn WatcherFs = match &prefetched {
                Some(pf) => pf,
                None => &NoopWatcherFs,
            };
            let c = apply_records_with_pending_filtered(
                &mut guard,
                root_id,
                &coalesced,
                fs_ref,
                &mut self.pending_old,
                &df,
                use_file_id,
                false,
            );
            maybe_compact(&mut guard);
            c
        } else {
            self.pending_old = None;
            false
        };
        on_locked(&coalesced);
        drop(guard);
        drop(_apply_guard);
        on_applied(&coalesced, changed);
    }
}

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
        let use_file_id = crate::win::is_ntfs_volume(&root_display);
        let mut wbuf = Vec::new();
        let initial_dir = open_dir_handle(&root_display, true, &mut wbuf)?;
        let apply_mu = Arc::new(Mutex::new(()));
        let denied_filter = Arc::new(RwLock::new(DeniedFilter::default()));

        Self::spawn_inner(
            root_display,
            Some(initial_dir),
            index,
            apply_mu,
            denied_filter,
            config,
            use_file_id,
            false,
            on_locked,
            on_applied,
            on_overflow,
            Box::new(|| {}),
            Box::new(|_| {}),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_resilient(
        root: String,
        index: Arc<RwLock<Index>>,
        apply_mu: Arc<Mutex<()>>,
        denied_filter: Arc<RwLock<DeniedFilter>>,
        config: WatcherConfig,
        use_file_id: bool,
        on_locked: OnLockedCallback,
        on_applied: OnAppliedCallback,
        on_overflow: OnOverflowCallback,
        on_restarted: OnRestartedCallback,
        on_error: OnErrorCallback,
    ) -> Option<Self> {
        let root_display = normalize_display_path(&root);
        let mut wbuf = Vec::new();
        let initial_dir = open_dir_handle(&root_display, true, &mut wbuf).ok();
        Self::spawn_inner(
            root_display,
            initial_dir,
            index,
            apply_mu,
            denied_filter,
            config,
            use_file_id,
            true,
            on_locked,
            on_applied,
            on_overflow,
            on_restarted,
            on_error,
        )
        .ok()
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_inner(
        root_display: String,
        initial_dir: Option<WinHandle>,
        index: Arc<RwLock<Index>>,
        apply_mu: Arc<Mutex<()>>,
        denied_filter: Arc<RwLock<DeniedFilter>>,
        config: WatcherConfig,
        use_file_id: bool,
        resilient: bool,
        on_locked: OnLockedCallback,
        on_applied: OnAppliedCallback,
        on_overflow: OnOverflowCallback,
        on_restarted: OnRestartedCallback,
        on_error: OnErrorCallback,
    ) -> io::Result<Self> {
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
                    initial_dir,
                    stop_event_w,
                    stop_flag_w,
                    index,
                    apply_mu,
                    denied_filter,
                    config,
                    use_file_id,
                    resilient,
                    armed_tx,
                    on_locked,
                    on_applied,
                    on_overflow,
                    on_restarted,
                    on_error,
                );
            })?;

        match armed_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop_flag,
                stop_event,
                thread: Some(thread),
            }),
            Ok(Err(e)) if !resilient => {
                stop_flag.store(true, Ordering::Release);
                unsafe {
                    let _ = SetEvent(stop_event.0);
                }
                let _ = thread.join();
                Err(e)
            }
            Ok(Err(_)) => Ok(Self {
                stop_flag,
                stop_event,
                thread: Some(thread),
            }),
            Err(_) if !resilient => {
                stop_flag.store(true, Ordering::Release);
                unsafe {
                    let _ = SetEvent(stop_event.0);
                }
                let _ = thread.join();
                Err(io::Error::other("watcher thread exited during startup"))
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

/// Owns the double-buffered overlapped `ReadDirectoryChangesExW` request and guarantees any
/// in-flight kernel I/O is cancelled and awaited before `bufs` / `OVERLAPPED` are freed on drop/panic.
struct ArmedWatchState {
    dir_handle: WinHandle,
    io_event: WinHandle,
    bufs: [Vec<u64>; 2],
    active_buf: usize,
    aligned_bytes: usize,
    overlapped: Box<OVERLAPPED>,
    armed: bool,
}

impl ArmedWatchState {
    fn new(dir_handle: WinHandle, aligned_bytes: usize) -> io::Result<Self> {
        let io_event =
            WinHandle(unsafe { CreateEventW(None, true, false, None) }.map_err(win32_to_io)?);
        let words = aligned_bytes / 8;
        let overlapped = Box::new(OVERLAPPED {
            hEvent: io_event.0,
            ..Default::default()
        });
        Ok(Self {
            dir_handle,
            io_event,
            bufs: [vec![0u64; words], vec![0u64; words]],
            active_buf: 0,
            aligned_bytes,
            overlapped,
            armed: false,
        })
    }

    fn arm(&mut self) -> io::Result<bool> {
        unsafe {
            let _ = ResetEvent(self.io_event.0);
        }
        self.overlapped.Internal = 0;
        self.overlapped.InternalHigh = 0;
        let ov_ptr: *mut OVERLAPPED = &mut *self.overlapped;
        let buf = &mut self.bufs[self.active_buf];
        let res = unsafe {
            ReadDirectoryChangesExW(
                self.dir_handle.0,
                buf.as_mut_ptr().cast(),
                self.aligned_bytes as u32,
                true,
                NOTIFY_FILTER,
                None,
                Some(ov_ptr),
                None,
                ReadDirectoryNotifyExtendedInformation,
            )
        };
        match res {
            Ok(()) => {
                self.armed = true;
                Ok(true)
            }
            Err(e) => match win32_error_code(&e) {
                Some(c) if c == ERROR_IO_PENDING.0 => {
                    self.armed = true;
                    Ok(true)
                }
                Some(c) if c == ERROR_NOTIFY_ENUM_DIR.0 => {
                    self.armed = false;
                    Ok(false)
                }
                Some(c) if c == ERROR_OPERATION_ABORTED.0 => {
                    self.armed = false;
                    Err(io::Error::from_raw_os_error(c as i32))
                }
                _ => {
                    self.armed = false;
                    Err(win32_to_io(e))
                }
            },
        }
    }

    fn cancel_and_collect(&mut self) -> Option<Vec<NotifyRecord>> {
        if !self.armed {
            return None;
        }
        unsafe {
            let _ = CancelIoEx(
                self.dir_handle.0,
                Some(&*self.overlapped as *const OVERLAPPED),
            );
            let mut transferred = 0u32;
            let res = GetOverlappedResult(
                self.dir_handle.0,
                &*self.overlapped as *const OVERLAPPED,
                &mut transferred,
                true,
            );
            self.armed = false;
            if res.is_ok() && transferred > 0 {
                let len = (transferred as usize).min(self.aligned_bytes);
                let bytes =
                    std::slice::from_raw_parts(self.bufs[self.active_buf].as_ptr().cast(), len);
                return Some(parse_notify_buffer(bytes));
            }
        }
        None
    }
}

impl Drop for ArmedWatchState {
    fn drop(&mut self) {
        if self.armed {
            unsafe {
                let _ = CancelIoEx(
                    self.dir_handle.0,
                    Some(&*self.overlapped as *const OVERLAPPED),
                );
                let mut transferred = 0u32;
                let _ = GetOverlappedResult(
                    self.dir_handle.0,
                    &*self.overlapped as *const OVERLAPPED,
                    &mut transferred,
                    true,
                );
            }
            self.armed = false;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn watcher_loop(
    root_display: &str,
    mut initial_dir: Option<WinHandle>,
    stop_event: Arc<WinHandle>,
    stop_flag: Arc<AtomicBool>,
    index: Arc<RwLock<Index>>,
    apply_mu: Arc<Mutex<()>>,
    denied_filter: Arc<RwLock<DeniedFilter>>,
    config: WatcherConfig,
    use_file_id: bool,
    resilient: bool,
    armed_tx: std::sync::mpsc::SyncSender<io::Result<()>>,
    on_locked: OnLockedCallback,
    on_applied: OnAppliedCallback,
    on_overflow: OnOverflowCallback,
    on_restarted: OnRestartedCallback,
    on_error: OnErrorCallback,
) {
    let aligned_bytes = config.buffer_bytes.max(128).div_ceil(8) * 8;
    let fs = Win32WatcherFs;
    let mut batch_state = WatcherBatchState {
        stop: Arc::clone(&stop_flag),
        ..Default::default()
    };
    let mut initial_sent = false;
    let mut need_restart_rewalk = initial_dir.is_none();
    let mut backoff = Duration::from_secs(1);
    let mut wbuf = Vec::new();

    while !stop_flag.load(Ordering::Acquire) {
        let dir_handle = match initial_dir.take() {
            Some(h) => h,
            None => match open_dir_handle(root_display, true, &mut wbuf) {
                Ok(h) => h,
                Err(e) => {
                    on_error(&format!("watcher open failed for {root_display}: {e}"));
                    if !initial_sent {
                        initial_sent = true;
                        let _ = armed_tx.send(Err(e));
                        if !resilient {
                            return;
                        }
                    }
                    let wait_ms = backoff.as_millis().min(u32::MAX as u128) as u32;
                    let _ = unsafe { WaitForSingleObject(stop_event.0, wait_ms) };
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            },
        };

        let mut watch = match ArmedWatchState::new(dir_handle, aligned_bytes) {
            Ok(w) => w,
            Err(e) => {
                on_error(&format!("watcher event creation failed: {e}"));
                if !initial_sent {
                    initial_sent = true;
                    let _ = armed_tx.send(Err(e));
                    if !resilient {
                        return;
                    }
                }
                let wait_ms = backoff.as_millis().min(u32::MAX as u128) as u32;
                let _ = unsafe { WaitForSingleObject(stop_event.0, wait_ms) };
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };

        let mut pending_overflow = false;
        let mut consecutive_arm_overflows = 0u32;

        match watch.arm() {
            Ok(true) => {
                if !initial_sent {
                    initial_sent = true;
                    let _ = armed_tx.send(Ok(()));
                }
                if need_restart_rewalk {
                    on_restarted();
                }
            }
            Ok(false) => {
                if !initial_sent {
                    initial_sent = true;
                    let _ = armed_tx.send(Ok(()));
                }
                if need_restart_rewalk {
                    on_restarted();
                } else {
                    on_overflow();
                }
            }
            Err(e) => {
                on_error(&format!(
                    "watcher initial arm failed for {root_display}: {e}"
                ));
                if !initial_sent {
                    initial_sent = true;
                    let _ = armed_tx.send(Err(e));
                    if !resilient {
                        return;
                    }
                }
                need_restart_rewalk = true;
                batch_state.clear_pending_old();
                let wait_ms = backoff.as_millis().min(u32::MAX as u128) as u32;
                let _ = unsafe { WaitForSingleObject(stop_event.0, wait_ms) };
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        }

        let handles = [watch.io_event.0, stop_event.0];
        let mut batch_start: Option<Instant> = None;
        let mut coalesce_deadline: Option<Instant> = None;
        let mut retry_needed = false;

        while !stop_flag.load(Ordering::Acquire) {
            if !watch.armed {
                match watch.arm() {
                    Ok(true) => {
                        consecutive_arm_overflows = 0;
                        if pending_overflow {
                            pending_overflow = false;
                            on_overflow();
                        }
                    }
                    Ok(false) => {
                        batch_state.clear_pending_old();
                        pending_overflow = true;
                        consecutive_arm_overflows += 1;
                        if consecutive_arm_overflows >= 3 {
                            pending_overflow = false;
                            on_overflow();
                            thread::sleep(Duration::from_millis(10));
                        }
                        continue;
                    }
                    Err(e) => {
                        batch_state.clear_pending_old();
                        if !stop_flag.load(Ordering::Acquire) {
                            on_error(&format!("watcher re-arm error on {root_display}: {e}"));
                            retry_needed = true;
                        }
                        break;
                    }
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
                    GetOverlappedResult(
                        watch.dir_handle.0,
                        &*watch.overlapped as *const OVERLAPPED,
                        &mut bytes_returned,
                        false,
                    )
                };
                watch.armed = false;

                match ov_res {
                    Ok(()) => {
                        // Only a completed read proves the handle works; an arm that fails right
                        // after succeeding must keep backing off.
                        backoff = Duration::from_secs(1);
                        if bytes_returned == 0 {
                            batch_state.on_overflow(
                                root_display,
                                &index,
                                &apply_mu,
                                &denied_filter,
                                use_file_id,
                                &fs,
                                &*on_locked,
                                &*on_applied,
                            );
                            batch_start = None;
                            coalesce_deadline = None;
                            pending_overflow = true;
                        } else {
                            let completed_idx = watch.active_buf;
                            let valid_len = (bytes_returned as usize).min(watch.aligned_bytes);
                            watch.active_buf ^= 1;
                            // Re-arm on the spare buffer BEFORE parsing/applying so the kernel
                            // never stalls on a slow write-lock apply or snapshot save.
                            let arm_res = watch.arm();
                            let byte_slice = unsafe {
                                std::slice::from_raw_parts(
                                    watch.bufs[completed_idx].as_ptr().cast::<u8>(),
                                    valid_len,
                                )
                            };
                            let parsed = parse_notify_buffer(byte_slice);
                            if !parsed.is_empty() {
                                batch_state.pending_batch.extend(parsed);
                                let now = Instant::now();
                                let start = *batch_start.get_or_insert(now);
                                let max_deadline = start + config.coalesce_max;
                                if now >= max_deadline || batch_state.pending_batch.len() >= 4096 {
                                    batch_state.flush(
                                        root_display,
                                        &index,
                                        &apply_mu,
                                        &denied_filter,
                                        use_file_id,
                                        &fs,
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
                            match arm_res {
                                Ok(true) => {}
                                Ok(false) => {
                                    batch_state.on_overflow(
                                        root_display,
                                        &index,
                                        &apply_mu,
                                        &denied_filter,
                                        use_file_id,
                                        &fs,
                                        &*on_locked,
                                        &*on_applied,
                                    );
                                    batch_start = None;
                                    coalesce_deadline = None;
                                    pending_overflow = true;
                                }
                                Err(e) => {
                                    batch_state.clear_pending_old();
                                    if !stop_flag.load(Ordering::Acquire) {
                                        on_error(&format!(
                                            "watcher immediate re-arm error on {root_display}: {e}"
                                        ));
                                        retry_needed = true;
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if win32_error_code(&e) == Some(ERROR_NOTIFY_ENUM_DIR.0) {
                            batch_state.on_overflow(
                                root_display,
                                &index,
                                &apply_mu,
                                &denied_filter,
                                use_file_id,
                                &fs,
                                &*on_locked,
                                &*on_applied,
                            );
                            batch_start = None;
                            coalesce_deadline = None;
                            pending_overflow = true;
                        } else if stop_flag.load(Ordering::Acquire) {
                            batch_state.clear_pending_old();
                            break;
                        } else if win32_error_code(&e) == Some(ERROR_OPERATION_ABORTED.0) {
                            batch_state.clear_pending_old();
                            on_error(&format!(
                                "watcher I/O aborted unexpectedly on {root_display}: {e}"
                            ));
                            retry_needed = true;
                            break;
                        } else {
                            batch_state.on_overflow(
                                root_display,
                                &index,
                                &apply_mu,
                                &denied_filter,
                                use_file_id,
                                &fs,
                                &*on_locked,
                                &*on_applied,
                            );
                            on_error(&format!("watcher I/O error on {root_display}: {e}"));
                            retry_needed = true;
                            break;
                        }
                    }
                }
            } else if wait_res == WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
                break;
            } else if wait_res == WAIT_TIMEOUT {
                batch_state.flush(
                    root_display,
                    &index,
                    &apply_mu,
                    &denied_filter,
                    use_file_id,
                    &fs,
                    &*on_locked,
                    &*on_applied,
                );
                batch_start = None;
                coalesce_deadline = None;
            } else {
                batch_state.clear_pending_old();
                on_error(&format!("watcher wait failed on {root_display}"));
                retry_needed = true;
                break;
            }
        }

        if let Some(final_recs) = watch.cancel_and_collect() {
            batch_state.pending_batch.extend(final_recs);
        }
        batch_state.flush(
            root_display,
            &index,
            &apply_mu,
            &denied_filter,
            use_file_id,
            &fs,
            &*on_locked,
            &*on_applied,
        );
        if let Some(orphan) = batch_state.pending_old.take()
            && !retry_needed
        {
            let _apply_guard = lock_or_recover(&apply_mu);
            let mut guard = write_or_recover(&index);
            let changed = if let Some(root_id) = guard.lookup(root_display) {
                let df = read_or_recover(&denied_filter);
                if !df.is_record_denied(Some((&guard, root_id)), &orphan, use_file_id) {
                    let c = apply_removed_record(&mut guard, root_id, &orphan, use_file_id);
                    maybe_compact(&mut guard);
                    c
                } else {
                    false
                }
            } else {
                false
            };
            let slice = std::slice::from_ref(&orphan);
            on_locked(slice);
            drop(guard);
            drop(_apply_guard);
            on_applied(slice, changed);
        }

        drop(watch);

        if stop_flag.load(Ordering::Acquire) || !retry_needed || !resilient {
            break;
        }
        need_restart_rewalk = true;
        batch_state.clear_pending_old();
        let wait_ms = backoff.as_millis().min(u32::MAX as u128) as u32;
        let _ = unsafe { WaitForSingleObject(stop_event.0, wait_ms) };
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
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
        let apply_mu = Arc::new(Mutex::new(()));
        let denied_filter = Arc::new(RwLock::new(DeniedFilter::default()));
        let locked_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let locked_clone = Arc::clone(&locked_calls);
        let mut state = WatcherBatchState {
            pending_batch: vec![NotifyRecord {
                action: NotifyAction::Added,
                rel_path: "cold_file.txt".to_owned(),
                file_id: 42,
                parent_file_id: 1,
                attrs: ARCHIVE,
                size: 5,
                mtime: 10,
                reparse_tag: 0,
            }],
            pending_old: None,
            ..Default::default()
        };
        let applied_changed = Arc::new(AtomicBool::new(true));
        let applied_clone = Arc::clone(&applied_changed);
        state.flush(
            "C:\\ColdMissingRoot",
            &index,
            &apply_mu,
            &denied_filter,
            true,
            &NoopWatcherFs,
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

    #[test]
    fn half_finished_rename_cleared_on_overflow_and_differing_fid_rename_splits() {
        let idx = Arc::new(RwLock::new(Index::new()));
        let apply_mu = Arc::new(Mutex::new(()));
        let denied = Arc::new(RwLock::new(DeniedFilter::default()));
        {
            let mut g = idx.write().unwrap();
            let root = g.add_root("C:\\Temp");
            g.set_file_id(root, 1);
            g.insert(
                root,
                "old.txt",
                Meta {
                    attrs: ARCHIVE,
                    size: 12,
                    mtime: 10,
                    file_id: 10,
                },
            );
        }

        let mut state = WatcherBatchState::default();
        // 1. Half-finished rename: old-name record arrived before overflow
        state.pending_batch.push(NotifyRecord {
            action: NotifyAction::RenamedOldName,
            rel_path: "old.txt".to_owned(),
            file_id: 10,
            parent_file_id: 1,
            attrs: ARCHIVE,
            size: 12,
            mtime: 10,
            reparse_tag: 0,
        });
        state.flush(
            "C:\\Temp",
            &idx,
            &apply_mu,
            &denied,
            true,
            &NoopWatcherFs,
            &|_| {},
            &|_, _| {},
        );
        assert!(state.pending_old.is_some());

        // 2. Overflow arrives (clears pending_old), and re-walk renames old.txt -> new.txt (fid 10)
        state.on_overflow(
            "C:\\Temp",
            &idx,
            &apply_mu,
            &denied,
            true,
            &NoopWatcherFs,
            &|_| {},
            &|_, _| {},
        );
        assert!(state.pending_old.is_none());
        {
            let mut g = idx.write().unwrap();
            let oid = g.by_file_id(10).unwrap();
            g.rename(oid, "new.txt");
        }

        // 3. Unrelated event after overflow must NOT delete new.txt (fid 10)
        state.pending_batch.push(NotifyRecord {
            action: NotifyAction::Added,
            rel_path: "unrelated.txt".to_owned(),
            file_id: 20,
            parent_file_id: 1,
            attrs: ARCHIVE,
            size: 5,
            mtime: 11,
            reparse_tag: 0,
        });
        state.flush(
            "C:\\Temp",
            &idx,
            &apply_mu,
            &denied,
            true,
            &NoopWatcherFs,
            &|_| {},
            &|_, _| {},
        );
        {
            let g = idx.read().unwrap();
            assert!(
                g.lookup("C:\\Temp\\new.txt").is_some(),
                "renamed item must survive"
            );
            assert!(g.lookup("C:\\Temp\\unrelated.txt").is_some());
        }

        // Differing non-zero FileIds in a rename pair split cleanly into remove(old) + add(new)
        let mut g = idx.write().unwrap();
        let root = g.lookup("C:\\Temp").unwrap();
        let split_recs = [
            NotifyRecord {
                action: NotifyAction::RenamedOldName,
                rel_path: "new.txt".to_owned(),
                file_id: 10,
                parent_file_id: 1,
                attrs: ARCHIVE,
                size: 12,
                mtime: 10,
                reparse_tag: 0,
            },
            NotifyRecord {
                action: NotifyAction::RenamedNewName,
                rel_path: "other_fid.txt".to_owned(),
                file_id: 99,
                parent_file_id: 1,
                attrs: ARCHIVE,
                size: 77,
                mtime: 30,
                reparse_tag: 0,
            },
        ];
        assert!(apply_records(&mut g, root, &split_recs, &NoopWatcherFs));
        assert_eq!(g.lookup("C:\\Temp\\new.txt"), None);
        assert_eq!(g.by_file_id(10), None);
        let new_id = g.by_file_id(99).expect("new fid added");
        assert_eq!(g.lookup("C:\\Temp\\other_fid.txt"), Some(new_id));
    }

    #[test]
    fn hard_links_prefer_name_and_forbid_global_file_fallback() {
        let mut idx = Index::new();
        let root = idx.add_root("C:\\Temp");
        idx.set_file_id(root, 1);
        let d1 = idx.insert(
            root,
            "D1",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 2,
            },
        );
        let d2 = idx.insert(
            root,
            "D2",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 3,
            },
        );
        let a = idx.insert(
            d1,
            "alpha.txt",
            Meta {
                attrs: ARCHIVE,
                size: 10,
                mtime: 10,
                file_id: 50,
            },
        );
        let _b = idx.insert(
            d1,
            "beta.txt",
            Meta {
                attrs: ARCHIVE,
                size: 10,
                mtime: 10,
                file_id: 50,
            },
        );
        let g = idx.insert(
            d2,
            "gamma.txt",
            Meta {
                attrs: ARCHIVE,
                size: 10,
                mtime: 10,
                file_id: 50,
            },
        );

        // Removing beta.txt must remove beta.txt by name, NOT alpha.txt, and keep fid 50 alive
        let rem = NotifyRecord {
            action: NotifyAction::Removed,
            rel_path: "D1\\beta.txt".to_owned(),
            file_id: 50,
            parent_file_id: 2,
            attrs: ARCHIVE,
            size: 10,
            mtime: 10,
            reparse_tag: 0,
        };
        assert!(apply_records(&mut idx, root, &[rem], &NoopWatcherFs));
        assert!(idx.is_alive(a));
        assert!(idx.is_alive(g));
        assert_eq!(idx.lookup("C:\\Temp\\D1\\beta.txt"), None);
        assert!(idx.by_file_id(50).is_some());

        // File rename whose old parent is unknown must NOT steal a different hard link via global FileId
        let rename_unknown_parent = [
            NotifyRecord {
                action: NotifyAction::RenamedOldName,
                rel_path: "MissingDir\\link.txt".to_owned(),
                file_id: 50,
                parent_file_id: 999,
                attrs: ARCHIVE,
                size: 10,
                mtime: 10,
                reparse_tag: 0,
            },
            NotifyRecord {
                action: NotifyAction::RenamedNewName,
                rel_path: "D2\\delta.txt".to_owned(),
                file_id: 50,
                parent_file_id: 3,
                attrs: ARCHIVE,
                size: 10,
                mtime: 20,
                reparse_tag: 0,
            },
        ];
        assert!(apply_records(
            &mut idx,
            root,
            &rename_unknown_parent,
            &NoopWatcherFs
        ));
        assert!(idx.is_alive(a));
        assert!(idx.is_alive(g));
        assert!(idx.lookup("C:\\Temp\\D2\\delta.txt").is_some());
    }

    #[test]
    fn refs_path_only_and_disagreeing_parent_fid_and_denied_filter() {
        let mut idx = Index::new();
        let root = idx.add_root("C:\\Temp");
        idx.set_file_id(root, 1);
        let wrong_dir = idx.insert(
            root,
            "WrongDir",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 77,
            },
        );
        let right_dir = idx.insert(
            root,
            "RightDir",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 88,
            },
        );
        let denied_dir = idx.insert(
            root,
            "SecretDenied",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 99,
            },
        );
        let nested_in_denied = idx.insert(
            denied_dir,
            "Sub",
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime: 10,
                file_id: 100,
            },
        );

        let denied = DeniedFilter::from_walk(&[WalkFolderRef {
            file_id: 99,
            rel_path: "SecretDenied".to_owned(),
        }]);

        let recs = [
            // Colliding parent_file_id (77 = WrongDir) on long path RightDir\hit.txt must follow RightDir
            NotifyRecord {
                action: NotifyAction::Added,
                rel_path: "RightDir\\hit.txt".to_owned(),
                file_id: 500,
                parent_file_id: 77,
                attrs: ARCHIVE,
                size: 1,
                mtime: 1,
                reparse_tag: 0,
            },
            // Denied folder child must be dropped
            NotifyRecord {
                action: NotifyAction::Added,
                rel_path: "SecretDenied\\Sub\\leak.txt".to_owned(),
                file_id: 501,
                parent_file_id: 100,
                attrs: ARCHIVE,
                size: 1,
                mtime: 1,
                reparse_tag: 0,
            },
        ];

        assert!(apply_records_filtered(
            &mut idx,
            root,
            &recs,
            &NoopWatcherFs,
            &denied,
            true,
        ));
        let hit = idx.lookup("C:\\Temp\\RightDir\\hit.txt").unwrap();
        assert_eq!(idx.parent(hit), Some(right_dir));
        assert_eq!(idx.lookup("C:\\Temp\\WrongDir\\hit.txt"), None);
        assert_eq!(idx.parent(wrong_dir), Some(root));
        assert_eq!(idx.parent(nested_in_denied), Some(denied_dir));
        assert_eq!(idx.lookup("C:\\Temp\\SecretDenied\\Sub\\leak.txt"), None);

        // Path-only (ReFS) mode ignores FileIds completely
        let refs_rec = [NotifyRecord {
            action: NotifyAction::Added,
            rel_path: "WrongDir\\refs.txt".to_owned(),
            file_id: 600,
            parent_file_id: 88,
            attrs: ARCHIVE,
            size: 2,
            mtime: 2,
            reparse_tag: 0,
        }];
        assert!(apply_records_filtered(
            &mut idx,
            root,
            &refs_rec,
            &NoopWatcherFs,
            &denied,
            false,
        ));
        assert!(idx.lookup("C:\\Temp\\WrongDir\\refs.txt").is_some());

        // DeniedFilter preserves diacritics ("kůň" denied does not block "kun")
        let mut diacritic_filter = DeniedFilter::default();
        diacritic_filter.extend_from(&[WalkFolderRef {
            file_id: 700,
            rel_path: "kůň".to_owned(),
        }]);
        let kun_ok = NotifyRecord {
            action: NotifyAction::Added,
            rel_path: "kun\\ok.txt".to_owned(),
            file_id: 701,
            parent_file_id: 0,
            attrs: ARCHIVE,
            size: 1,
            mtime: 1,
            reparse_tag: 0,
        };
        let kun_denied = NotifyRecord {
            action: NotifyAction::Added,
            rel_path: "kůň\\blocked.txt".to_owned(),
            file_id: 702,
            parent_file_id: 0,
            attrs: ARCHIVE,
            size: 1,
            mtime: 1,
            reparse_tag: 0,
        };
        assert!(!diacritic_filter.is_record_denied(None, &kun_ok, false));
        assert!(diacritic_filter.is_record_denied(None, &kun_denied, false));

        // relative_under_root handles multi-byte UTF-8 case changes and separator boundaries safely
        assert_eq!(
            relative_under_root("KSub\\file.txt", "kSub"),
            Some("file.txt")
        );
        assert_eq!(relative_under_root("č\\file.txt", "a"), None);
        assert_eq!(relative_under_root("Sub_Extra\\file.txt", "Sub"), None);
        assert_eq!(
            relative_under_root("Sub\\file.txt", "Sub"),
            Some("file.txt")
        );
    }

    fn rec(action: NotifyAction, rel: &str, fid: u64, pfid: u64, attrs: u32) -> NotifyRecord {
        NotifyRecord {
            action,
            rel_path: rel.to_owned(),
            file_id: fid,
            parent_file_id: pfid,
            attrs,
            size: 0,
            mtime: 1,
            reparse_tag: 0,
        }
    }

    fn meta(attrs: u32, fid: u64) -> Meta {
        Meta {
            attrs,
            size: 0,
            mtime: 1,
            file_id: fid,
        }
    }

    /// Counts disk calls; `list_dir_children_checked` returns one file per folder and, when
    /// `stop_after_first` is set, raises the stop flag after the first listing.
    #[derive(Default)]
    struct CountingFs {
        lists: std::sync::atomic::AtomicUsize,
        long_name_calls: std::sync::atomic::AtomicUsize,
        stop: Arc<AtomicBool>,
        stop_after_first: bool,
        nested_dirs: bool,
    }

    impl WatcherFs for CountingFs {
        fn resolve_long_leaf(&self, _full_path: &str) -> Option<String> {
            self.long_name_calls.fetch_add(1, Ordering::SeqCst);
            None
        }
        fn list_dir_children(&self, _full_dir_path: &str) -> Vec<AddedChild> {
            Vec::new()
        }
        fn list_dir_children_checked(&self, _full_dir_path: &str) -> io::Result<Vec<AddedChild>> {
            self.lists.fetch_add(1, Ordering::SeqCst);
            if self.stop_after_first {
                self.stop.store(true, Ordering::SeqCst);
            }
            let mut out = vec![AddedChild {
                name: "listed.txt".to_owned(),
                meta: meta(ARCHIVE, 0),
                reparse_tag: 0,
            }];
            if self.nested_dirs {
                out.push(AddedChild {
                    name: "deeper".to_owned(),
                    meta: meta(DIRECTORY, 0),
                    reparse_tag: 0,
                });
            }
            Ok(out)
        }
    }

    fn photos_index() -> (Index, EntryId) {
        let mut idx = Index::new();
        let root = idx.add_root("R:\\");
        idx.set_file_id(root, 1);
        let p = idx.insert(root, "Photos 2026", meta(DIRECTORY, 7));
        for i in 0..3u64 {
            idx.insert(p, &format!("img{i}.jpg"), meta(ARCHIVE, 100 + i));
        }
        (idx, root)
    }

    fn photo_children(idx: &Index) -> usize {
        (0..3)
            .filter(|i| {
                idx.lookup(&format!("R:\\Photos 2026\\img{i}.jpg"))
                    .is_some()
            })
            .count()
    }

    #[test]
    fn replayed_add_then_rename_keeps_existing_target_subtree() {
        // Fails on 4056029: the replayed `Added Album` creates a second FileId-7 folder and the
        // rename's collision removal deletes the real `Photos 2026` with its 3 children.
        for use_file_id in [true, false] {
            let (mut idx, root) = photos_index();
            let recs = [
                rec(NotifyAction::Added, "Album", 7, 1, DIRECTORY),
                rec(NotifyAction::RenamedOldName, "Album", 7, 1, DIRECTORY),
                rec(NotifyAction::RenamedNewName, "Photos 2026", 7, 1, DIRECTORY),
            ];
            replay_records_filtered(
                &mut idx,
                root,
                &recs,
                &NoopWatcherFs,
                &DeniedFilter::default(),
                use_file_id,
            );
            assert_eq!(photo_children(&idx), 3, "use_file_id={use_file_id}");
            assert_eq!(idx.lookup("R:\\Album"), None, "use_file_id={use_file_id}");
            if use_file_id {
                assert_eq!(idx.by_file_id(7), idx.lookup("R:\\Photos 2026"));
            }
        }

        // Atomic save stays correct: a file renamed over one with a different FileId replaces it.
        let mut idx = Index::new();
        let root = idx.add_root("R:\\");
        idx.set_file_id(root, 1);
        idx.insert(root, "doc.docx", meta(ARCHIVE, 10));
        idx.insert(root, "doc.tmp", meta(ARCHIVE, 20));
        let recs = [
            rec(NotifyAction::RenamedOldName, "doc.tmp", 20, 1, ARCHIVE),
            rec(NotifyAction::RenamedNewName, "doc.docx", 20, 1, ARCHIVE),
        ];
        assert!(apply_records(&mut idx, root, &recs, &NoopWatcherFs));
        assert_eq!(idx.lookup("R:\\doc.tmp"), None);
        assert_eq!(idx.by_file_id(20), idx.lookup("R:\\doc.docx"));
        assert_eq!(idx.by_file_id(10), None);

        // A replayed cross-folder move (Removed + Added) onto a walk that already saw it is a no-op.
        let mut idx = Index::new();
        let root = idx.add_root("R:\\");
        idx.set_file_id(root, 1);
        idx.insert(root, "A", meta(DIRECTORY, 2));
        let z = idx.insert(root, "Z", meta(DIRECTORY, 3));
        let x = idx.insert(z, "X", meta(DIRECTORY, 7));
        idx.insert(x, "kid.txt", meta(ARCHIVE, 8));
        let recs = [
            rec(NotifyAction::Removed, "A\\X", 7, 2, DIRECTORY),
            rec(NotifyAction::Added, "Z\\X", 7, 3, DIRECTORY),
        ];
        replay_records_filtered(
            &mut idx,
            root,
            &recs,
            &NoopWatcherFs,
            &DeniedFilter::default(),
            true,
        );
        assert!(idx.lookup("R:\\Z\\X\\kid.txt").is_some());
    }

    #[test]
    fn rename_into_unknown_folder_removes_the_old_entry() {
        // Fails on 4056029: the rename returned false and left `A` as a ghost at its old path.
        let mut idx = Index::new();
        let root = idx.add_root("R:\\");
        idx.set_file_id(root, 1);
        let a = idx.insert(root, "A", meta(DIRECTORY, 7));
        idx.insert(a, "kid.txt", meta(ARCHIVE, 8));
        let recs = [
            rec(NotifyAction::RenamedOldName, "A", 7, 1, DIRECTORY),
            rec(NotifyAction::RenamedNewName, "Unknown\\A", 7, 99, DIRECTORY),
        ];
        assert!(apply_records(&mut idx, root, &recs, &NoopWatcherFs));
        assert_eq!(idx.lookup("R:\\A"), None);
        assert_eq!(idx.by_file_id(8), None);
    }

    #[test]
    fn folder_rename_resolving_in_index_is_not_listed_from_disk() {
        // Fails on 4056029: every RenamedNewName directory was listed recursively from disk even
        // though the rename just moves the indexed subtree.
        let index = Arc::new(RwLock::new(Index::new()));
        {
            let mut g = index.write().unwrap();
            let root = g.add_root("R:\\");
            g.set_file_id(root, 1);
            let a = g.insert(root, "A", meta(DIRECTORY, 7));
            g.insert(a, "kid.txt", meta(ARCHIVE, 8));
        }
        let apply_mu = Arc::new(Mutex::new(()));
        let denied = Arc::new(RwLock::new(DeniedFilter::default()));
        let fs = CountingFs::default();
        let mut state = WatcherBatchState::default();
        // Old and new halves arrive in separate flushes (pairing via pending_old).
        state
            .pending_batch
            .push(rec(NotifyAction::RenamedOldName, "A", 7, 1, DIRECTORY));
        state.flush(
            "R:\\",
            &index,
            &apply_mu,
            &denied,
            true,
            &fs,
            &|_| {},
            &|_, _| {},
        );
        state
            .pending_batch
            .push(rec(NotifyAction::RenamedNewName, "B", 7, 1, DIRECTORY));
        state.flush(
            "R:\\",
            &index,
            &apply_mu,
            &denied,
            true,
            &fs,
            &|_| {},
            &|_, _| {},
        );
        assert_eq!(fs.lists.load(Ordering::SeqCst), 0);
        let g = index.read().unwrap();
        assert!(g.lookup("R:\\B\\kid.txt").is_some());
        assert_eq!(g.lookup("R:\\B\\listed.txt"), None);

        // An unresolvable rename (old half unknown) is still listed so the folder is not empty.
        drop(g);
        state
            .pending_batch
            .push(rec(NotifyAction::RenamedOldName, "Nope", 70, 1, DIRECTORY));
        state
            .pending_batch
            .push(rec(NotifyAction::RenamedNewName, "C", 70, 1, DIRECTORY));
        state.flush(
            "R:\\",
            &index,
            &apply_mu,
            &denied,
            true,
            &fs,
            &|_| {},
            &|_, _| {},
        );
        assert_eq!(fs.lists.load(Ordering::SeqCst), 1);
        assert!(index.read().unwrap().lookup("R:\\C\\listed.txt").is_some());
    }

    #[test]
    fn prefetch_listing_stops_when_stop_flag_is_raised() {
        // Fails on 4056029 (no stop check): the whole 511-level chain was listed while
        // `Service::stop` waited on the watcher's final flush.
        let fs = CountingFs {
            stop_after_first: true,
            nested_dirs: true,
            ..Default::default()
        };
        let recs = [rec(NotifyAction::Added, "Big", 7, 1, DIRECTORY)];
        let _ = prefetch_batch_outside_lock(
            "R:\\",
            &recs,
            &HashMap::new(),
            &HashSet::new(),
            &DeniedFilter::default(),
            true,
            &fs,
            &fs.stop,
        );
        assert_eq!(fs.lists.load(Ordering::SeqCst), 1);

        let fs_run = CountingFs {
            nested_dirs: true,
            ..Default::default()
        };
        let _ = prefetch_batch_outside_lock(
            "R:\\",
            &recs,
            &HashMap::new(),
            &HashSet::new(),
            &DeniedFilter::default(),
            true,
            &fs_run,
            &AtomicBool::new(false),
        );
        assert_eq!(
            fs_run.lists.load(Ordering::SeqCst),
            usize::from(MAX_WALK_DEPTH - 1)
        );
    }

    #[test]
    fn get_long_path_name_only_for_plausible_short_names() {
        // Fails on 4056029: any leaf containing `~` (e.g. Office lock files `~$doc.docx`) went
        // through GetLongPathNameW.
        for name in [
            "LONGDI~1",
            "VERYLO~1.PDF",
            "SHORTN~1.TXT",
            "ab12f~10.x",
            "PROGRA~1",
        ] {
            assert!(looks_like_short_name(name), "{name}");
        }
        for name in [
            "~$doc.docx",
            "~WRL0001.tmp",
            "file~.txt",
            "a~b.txt",
            "My File~1.txt",
            "LONGNAME~1",
            "abc~1.text",
            "a.b~1",
            "~1",
            "č~1.txt",
        ] {
            assert!(!looks_like_short_name(name), "{name}");
        }

        let fs = CountingFs::default();
        let recs = [
            rec(NotifyAction::Added, "~$doc.docx", 7, 1, ARCHIVE),
            rec(NotifyAction::RenamedNewName, "~WRL0001.tmp", 8, 1, ARCHIVE),
        ];
        assert!(!batch_needs_prefetch(&recs));
        let _ = prefetch_batch_outside_lock(
            "R:\\",
            &recs,
            &HashMap::new(),
            &HashSet::new(),
            &DeniedFilter::default(),
            true,
            &fs,
            &AtomicBool::new(false),
        );
        let mut idx = Index::new();
        let root = idx.add_root("R:\\");
        idx.set_file_id(root, 1);
        apply_records(&mut idx, root, &recs[..1], &fs);
        assert_eq!(fs.long_name_calls.load(Ordering::SeqCst), 0);
        assert!(idx.lookup("R:\\~$doc.docx").is_some());
        assert_eq!(Win32WatcherFs.resolve_long_leaf("R:\\~$doc.docx"), None);
    }

    #[test]
    fn replayed_move_into_folder_moved_or_renamed_later_keeps_subtree() {
        // Fails before this fix: the destination `P` no longer resolves by path (fresh lists it as
        // `Q\\P` or `P2`), so the rename removed `A` (found by FileId) with its subtree.
        for final_parent in ["Q\\P", "P2"] {
            let mut idx = Index::new();
            let root = idx.add_root("R:\\\\");
            idx.set_file_id(root, 1);
            let p = if final_parent == "P2" {
                idx.insert(root, "P2", meta(DIRECTORY, 2))
            } else {
                let q = idx.insert(root, "Q", meta(DIRECTORY, 3));
                idx.insert(q, "P", meta(DIRECTORY, 2))
            };
            let a = idx.insert(p, "A", meta(DIRECTORY, 7));
            idx.insert(a, "kid.txt", meta(ARCHIVE, 8));
            let recs = [
                rec(NotifyAction::RenamedOldName, "A", 7, 1, DIRECTORY),
                rec(NotifyAction::RenamedNewName, "P\\A", 7, 2, DIRECTORY),
            ];
            let changed = replay_records_filtered(
                &mut idx,
                root,
                &recs,
                &NoopWatcherFs,
                &DeniedFilter::default(),
                true,
            );
            assert!(!changed, "{final_parent}");
            assert!(
                idx.lookup(&format!("R:\\{final_parent}\\A\\kid.txt"))
                    .is_some(),
                "{final_parent}"
            );
        }

        // Source still at its old place: it moves into the destination found by FileId.
        let mut idx = Index::new();
        let root = idx.add_root("R:\\\\");
        idx.set_file_id(root, 1);
        let q = idx.insert(root, "Q", meta(DIRECTORY, 3));
        idx.insert(q, "P", meta(DIRECTORY, 2));
        let a = idx.insert(root, "A", meta(DIRECTORY, 7));
        idx.insert(a, "kid.txt", meta(ARCHIVE, 8));
        let recs = [
            rec(NotifyAction::RenamedOldName, "A", 7, 1, DIRECTORY),
            rec(NotifyAction::RenamedNewName, "P\\A", 7, 2, DIRECTORY),
        ];
        assert!(replay_records_filtered(
            &mut idx,
            root,
            &recs,
            &NoopWatcherFs,
            &DeniedFilter::default(),
            true,
        ));
        assert!(idx.lookup("R:\\Q\\P\\A\\kid.txt").is_some());
        assert_eq!(idx.lookup("R:\\A"), None);
    }

    #[test]
    fn live_rename_onto_stale_folder_keeps_the_renamed_subtree() {
        // Fails before this fix on path-only volumes: the replay-only "rename already applied"
        // rule ran live too, kept the stale occupant and deleted the renamed folder's children.
        for use_file_id in [false, true] {
            let mut idx = Index::new();
            let root = idx.add_root("R:\\\\");
            idx.set_file_id(root, 1);
            let d = idx.insert(root, "D", meta(DIRECTORY, 2));
            let p = idx.insert(d, "P", meta(DIRECTORY, 10));
            idx.insert(p, "c1.txt", meta(ARCHIVE, 11));
            idx.insert(p, "c2.txt", meta(ARCHIVE, 12));
            let q = idx.insert(d, "Q", meta(DIRECTORY, 0));
            idx.insert(q, "stale.txt", meta(ARCHIVE, 13));
            let recs = [
                rec(NotifyAction::RenamedOldName, "D\\P", 10, 2, DIRECTORY),
                rec(NotifyAction::RenamedNewName, "D\\Q", 10, 2, DIRECTORY),
            ];
            assert!(apply_records_filtered(
                &mut idx,
                root,
                &recs,
                &NoopWatcherFs,
                &DeniedFilter::default(),
                use_file_id,
            ));
            assert_eq!(idx.lookup("R:\\D\\P"), None, "{use_file_id}");
            assert!(idx.lookup("R:\\D\\Q\\c1.txt").is_some(), "{use_file_id}");
            assert!(idx.lookup("R:\\D\\Q\\c2.txt").is_some(), "{use_file_id}");
            assert_eq!(idx.lookup("R:\\D\\Q\\stale.txt"), None, "{use_file_id}");
        }
    }
}
