//! Structure-of-arrays (SoA) arena index for filenames, metadata, and tree relationships.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_instance_id() -> u64 {
    NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed)
}

use fm_fs::attrs::{DIRECTORY, HIDDEN};
use fm_fs::fold_into;

/// Compact 32-bit identifier for an indexed file or directory entry.
pub type EntryId = u32;

/// Sentinel [`EntryId`] indicating "no entry" (e.g. root parent or empty child list).
pub const NONE: EntryId = u32::MAX;

/// Tombstone sentinel in the open-addressed child lookup table.
pub(crate) const TOMBSTONE: EntryId = u32::MAX - 1;

/// Maximum slot capacity supported (leaving `NONE` and `TOMBSTONE` reserved).
const MAX_SLOTS: usize = (u32::MAX - 2) as usize;

/// Entry is live (not removed).
pub(crate) const FLAG_ALIVE: u8 = 1 << 0;
/// Entry is hidden/system itself or has a hidden/system ancestor.
pub(crate) const FLAG_EFFECTIVE_HIDDEN: u8 = 1 << 1;
pub(crate) const FLAG_HAS_DISPLAY: u8 = 1 << 2;

/// Extension ID meaning "no extension" (or directory).
pub(crate) const EXT_NONE: u16 = 0;
/// Extension ID overflow sentinel when distinct extensions exceed `u16::MAX - 1`.
pub(crate) const EXT_OVERFLOW: u16 = u16::MAX;

/// Metadata stored per indexed entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    /// Win32 `FILE_ATTRIBUTE_*` bitmask (see `fm_fs::attrs`).
    pub attrs: u32,
    /// File size in bytes (typically `0` for directories).
    pub size: u64,
    /// Last modification time in Windows `FILETIME` ticks (100-ns intervals since 1601-01-01).
    pub mtime: i64,
    /// 64-bit NTFS/ReFS file identifier (`0` if unknown).
    pub file_id: u64,
}

/// Structure-of-arrays in-memory filename and metadata index.
#[derive(Debug, Clone)]
pub struct Index {
    pub(crate) folded_arena: Vec<u8>,
    pub(crate) display_arena: Vec<u8>,
    pub(crate) folded_off: Vec<u32>,
    /// Offset into `display_arena`, or `NONE` when display name equals folded name byte-for-byte.
    pub(crate) display_off: Vec<u32>,
    pub(crate) parent: Vec<EntryId>,
    pub(crate) attrs: Vec<u32>,
    pub(crate) size: Vec<u64>,
    pub(crate) mtime: Vec<i64>,
    pub(crate) file_id: Vec<u64>,
    pub(crate) ext_id: Vec<u16>,
    pub(crate) flags: Vec<u8>,
    pub(crate) first_child: Vec<EntryId>,
    pub(crate) next_sibling: Vec<EntryId>,
    /// Open-addressed hash table mapping `(parent, folded_name) -> EntryId`.
    pub(crate) buckets: Vec<EntryId>,
    pub(crate) bucket_occupied: usize,
    /// Open-addressed hash table mapping non-zero `file_id -> EntryId`.
    pub(crate) fid_buckets: Vec<EntryId>,
    pub(crate) fid_occupied: usize,
    pub(crate) fid_live: usize,
    /// Multi-entry tracking only for `file_id`s shared by multiple live entries (hard links).
    pub(crate) fid_dups: HashMap<u64, Vec<EntryId>>,
    pub(crate) roots: Vec<EntryId>,
    pub(crate) exts: Vec<String>,
    pub(crate) ext_map: HashMap<String, u16>,
    pub(crate) ext_overflow: bool,
    pub(crate) live_count: usize,
    pub(crate) dead_arena_bytes: usize,
    pub(crate) arenas_sequential: bool,
    pub(crate) volume_serial: u32,
    pub(crate) instance_id: u64,
    pub(crate) index_gen: u64,
}

impl Default for Index {
    fn default() -> Self {
        Self::new()
    }
}

impl Index {
    /// Creates an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self {
            folded_arena: Vec::new(),
            display_arena: Vec::new(),
            folded_off: Vec::new(),
            display_off: Vec::new(),
            parent: Vec::new(),
            attrs: Vec::new(),
            size: Vec::new(),
            mtime: Vec::new(),
            file_id: Vec::new(),
            ext_id: Vec::new(),
            flags: Vec::new(),
            first_child: Vec::new(),
            next_sibling: Vec::new(),
            buckets: vec![NONE; 64],
            bucket_occupied: 0,
            fid_buckets: Vec::new(),
            fid_occupied: 0,
            fid_live: 0,
            fid_dups: HashMap::new(),
            roots: Vec::new(),
            exts: vec![String::new()],
            ext_map: HashMap::new(),
            ext_overflow: false,
            live_count: 0,
            dead_arena_bytes: 0,
            arenas_sequential: true,
            volume_serial: 0,
            instance_id: next_instance_id(),
            index_gen: 1,
        }
    }

    /// Creates an empty index with preallocated capacity for `entries` entries.
    #[must_use]
    pub fn with_capacity(entries: usize) -> Self {
        let bucket_cap = ((entries.saturating_mul(10)) / 7 + 16).max(64);
        Self {
            folded_arena: Vec::with_capacity(entries.saturating_mul(18)),
            display_arena: Vec::with_capacity(entries.saturating_mul(10)),
            folded_off: Vec::with_capacity(entries),
            display_off: Vec::with_capacity(entries),
            parent: Vec::with_capacity(entries),
            attrs: Vec::with_capacity(entries),
            size: Vec::with_capacity(entries),
            mtime: Vec::with_capacity(entries),
            file_id: Vec::with_capacity(entries),
            ext_id: Vec::with_capacity(entries),
            flags: Vec::with_capacity(entries),
            first_child: Vec::with_capacity(entries),
            next_sibling: Vec::with_capacity(entries),
            buckets: vec![NONE; bucket_cap],
            bucket_occupied: 0,
            fid_buckets: Vec::new(),
            fid_occupied: 0,
            fid_live: 0,
            fid_dups: HashMap::new(),
            roots: Vec::with_capacity(4),
            exts: vec![String::new()],
            ext_map: HashMap::new(),
            ext_overflow: false,
            live_count: 0,
            dead_arena_bytes: 0,
            arenas_sequential: true,
            volume_serial: 0,
            instance_id: next_instance_id(),
            index_gen: 1,
        }
    }

    /// Returns the volume serial associated with this index (used by snapshots).
    #[must_use]
    pub fn volume_serial(&self) -> u32 {
        self.volume_serial
    }

    /// Sets the volume serial associated with this index (stored in snapshots).
    pub fn set_volume_serial(&mut self, serial: u32) {
        self.volume_serial = serial;
    }

    /// Returns the number of live entries in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live_count
    }

    /// Returns `true` if the index contains no live entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live_count == 0
    }

    /// Returns total allocated bytes across internal arenas, column arrays, and lookup tables.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        let exts_bytes: usize = self.exts.iter().map(|s| s.capacity()).sum::<usize>()
            + self.exts.capacity() * std::mem::size_of::<String>();
        self.folded_arena.capacity()
            + self.display_arena.capacity()
            + self.folded_off.capacity() * std::mem::size_of::<u32>()
            + self.display_off.capacity() * std::mem::size_of::<u32>()
            + self.parent.capacity() * std::mem::size_of::<EntryId>()
            + self.attrs.capacity() * std::mem::size_of::<u32>()
            + self.size.capacity() * std::mem::size_of::<u64>()
            + self.mtime.capacity() * std::mem::size_of::<i64>()
            + self.file_id.capacity() * std::mem::size_of::<u64>()
            + self.ext_id.capacity() * std::mem::size_of::<u16>()
            + self.flags.capacity() * std::mem::size_of::<u8>()
            + self.first_child.capacity() * std::mem::size_of::<EntryId>()
            + self.next_sibling.capacity() * std::mem::size_of::<EntryId>()
            + self.buckets.capacity() * std::mem::size_of::<EntryId>()
            + self.fid_buckets.capacity() * std::mem::size_of::<EntryId>()
            + self.fid_dups.capacity()
                * (std::mem::size_of::<u64>() + std::mem::size_of::<Vec<EntryId>>())
            + self
                .fid_dups
                .values()
                .map(|v| v.capacity() * std::mem::size_of::<EntryId>())
                .sum::<usize>()
            + self.roots.capacity() * std::mem::size_of::<EntryId>()
            + exts_bytes
    }

    /// Returns `true` if `id` refers to a valid live entry.
    #[inline]
    #[must_use]
    pub fn is_alive(&self, id: EntryId) -> bool {
        let idx = id as usize;
        idx < self.flags.len() && (self.flags[idx] & FLAG_ALIVE) != 0
    }

    /// Adds a volume or tree root (e.g. `"C:\\"`), returning its `EntryId`.
    ///
    /// If an equivalent root already exists (case-insensitively), returns the existing `EntryId`.
    pub fn add_root(&mut self, root: &str) -> EntryId {
        let (display, folded) = normalize_root(root);
        let disp_trimmed = trim_trailing_root_slash(&display);
        for &rid in &self.roots {
            if self.is_alive(rid)
                && eq_case_insensitive(
                    trim_trailing_root_slash(self.display_str_unchecked(rid)),
                    disp_trimmed,
                )
            {
                return rid;
            }
        }

        assert!(
            self.parent.len() < MAX_SLOTS,
            "index slot capacity exceeded"
        );
        let id = self.parent.len() as EntryId;
        let f_off = push_arena(&mut self.folded_arena, folded.as_bytes());
        let d_off = if display == folded {
            NONE
        } else {
            push_arena(&mut self.display_arena, display.as_bytes())
        };

        self.folded_off.push(f_off);
        self.display_off.push(d_off);
        self.parent.push(NONE);
        self.attrs.push(DIRECTORY);
        self.size.push(0);
        self.mtime.push(0);
        self.file_id.push(0);
        self.ext_id.push(EXT_NONE);
        self.flags.push(FLAG_ALIVE);
        self.first_child.push(NONE);
        self.next_sibling.push(NONE);
        self.roots.push(id);
        self.live_count += 1;
        self.index_gen = self.index_gen.wrapping_add(1);
        id
    }

    /// Inserts or updates a child entry under `parent` by `(parent, folded_name)`.
    pub fn insert(&mut self, parent: EntryId, name: &str, meta: Meta) -> EntryId {
        assert!(
            self.is_alive(parent),
            "insert into invalid or dead parent {parent}"
        );
        let mut folded_buf = String::with_capacity(name.len());
        fold_into(name, &mut folded_buf);
        self.insert_with_folded(parent, name, &folded_buf, meta)
    }

    pub(crate) fn insert_with_folded(
        &mut self,
        parent: EntryId,
        name: &str,
        folded: &str,
        meta: Meta,
    ) -> EntryId {
        if let Some(existing) = self.find_child(parent, folded.as_bytes(), name) {
            self.update_entry_display(existing, name, folded);
            let is_dir = (meta.attrs & DIRECTORY) != 0;
            self.ext_id[existing as usize] = self.compute_ext_id(folded, is_dir);
            self.apply_meta(existing, meta);
            self.index_gen = self.index_gen.wrapping_add(1);
            return existing;
        }

        assert!(
            self.parent.len() < MAX_SLOTS,
            "index slot capacity exceeded"
        );
        let id = self.parent.len() as EntryId;
        let f_off = push_arena(&mut self.folded_arena, folded.as_bytes());
        let d_off = if name == folded {
            NONE
        } else {
            push_arena(&mut self.display_arena, name.as_bytes())
        };
        let is_dir = (meta.attrs & DIRECTORY) != 0;
        let ext = self.compute_ext_id(folded, is_dir);
        let parent_hidden = (self.flags[parent as usize] & FLAG_EFFECTIVE_HIDDEN) != 0;
        let eff_hidden = parent_hidden || is_own_hidden(meta.attrs);
        let flag = FLAG_ALIVE | if eff_hidden { FLAG_EFFECTIVE_HIDDEN } else { 0 };

        self.folded_off.push(f_off);
        self.display_off.push(d_off);
        self.parent.push(parent);
        self.attrs.push(meta.attrs);
        self.size.push(meta.size);
        self.mtime.push(meta.mtime);
        self.file_id.push(meta.file_id);
        self.ext_id.push(ext);
        self.flags.push(flag);
        self.first_child.push(NONE);
        self.next_sibling.push(self.first_child[parent as usize]);
        self.first_child[parent as usize] = id;

        self.bucket_insert(parent, folded.as_bytes(), name, id);
        if meta.file_id != 0 {
            self.fid_insert(meta.file_id, id);
        }
        self.live_count += 1;
        self.index_gen = self.index_gen.wrapping_add(1);
        id
    }
    /// Removes `id` and its entire descendant subtree from the index.
    pub fn remove(&mut self, id: EntryId) {
        if !self.is_alive(id) {
            return;
        }
        let parent = self.parent[id as usize];
        if parent == NONE {
            self.roots.retain(|&r| r != id);
        } else {
            self.unlink_child(parent, id);
        }
        self.kill_subtree(id);
        self.index_gen = self.index_gen.wrapping_add(1);
    }

    /// Removes every live entry `i` with `doomed[i]` (and its subtree), unlinking each affected
    /// parent's child list once instead of once per removed child (O(siblings) each).
    ///
    /// Returns `true` if anything was removed.
    pub(crate) fn remove_flagged(&mut self, doomed: &[bool]) -> bool {
        let is_doomed = |idx: &Self, id: EntryId| {
            (id as usize) < doomed.len() && doomed[id as usize] && idx.is_alive(id)
        };
        let mut tops = Vec::new();
        let mut parents = Vec::new();
        for (i, &d) in doomed.iter().enumerate() {
            let id = i as EntryId;
            if !d || !self.is_alive(id) {
                continue;
            }
            let p = self.parent[i];
            if p != NONE && is_doomed(self, p) {
                continue; // killed with its doomed parent's subtree
            }
            tops.push(id);
            if p == NONE {
                self.roots.retain(|&r| r != id);
            } else {
                parents.push(p);
            }
        }
        if tops.is_empty() {
            return false;
        }
        parents.sort_unstable();
        parents.dedup();
        for p in parents {
            let mut prev = NONE;
            let mut cur = self.first_child[p as usize];
            while cur != NONE {
                let next = self.next_sibling[cur as usize];
                if is_doomed(self, cur) {
                    if prev == NONE {
                        self.first_child[p as usize] = next;
                    } else {
                        self.next_sibling[prev as usize] = next;
                    }
                    self.next_sibling[cur as usize] = NONE;
                } else {
                    prev = cur;
                }
                cur = next;
            }
        }
        for id in tops {
            self.kill_subtree(id);
        }
        self.index_gen = self.index_gen.wrapping_add(1);
        true
    }

    /// Frees `id` and all descendants; `id` must already be unlinked from its parent / roots.
    fn kill_subtree(&mut self, id: EntryId) {
        let mut stack = vec![id];
        while let Some(u) = stack.pop() {
            let ui = u as usize;
            if (self.flags[ui] & FLAG_ALIVE) == 0 {
                continue;
            }
            let mut child = self.first_child[ui];
            while child != NONE {
                let next = self.next_sibling[child as usize];
                stack.push(child);
                child = next;
            }

            let up = self.parent[ui];
            if up != NONE {
                let f_bytes = read_arena_slice(&self.folded_arena, self.folded_off[ui]);
                let hash = hash_key(up, f_bytes);
                bucket_remove_by_id(&mut self.buckets, u, hash);
            }

            self.arenas_sequential = false;
            self.dead_arena_bytes += arena_entry_total_len(&self.folded_arena, self.folded_off[ui]);
            if self.display_off[ui] != NONE {
                self.dead_arena_bytes +=
                    arena_entry_total_len(&self.display_arena, self.display_off[ui]);
            }

            let old_fid = self.file_id[ui];
            if old_fid != 0 {
                self.fid_remove(old_fid, u);
                self.file_id[ui] = 0;
            }

            self.flags[ui] = 0;
            self.parent[ui] = NONE;
            self.first_child[ui] = NONE;
            self.next_sibling[ui] = NONE;
            self.live_count -= 1;
        }
    }

    /// Renames `id` to `new_name` in its current parent directory.
    ///
    /// If another sibling already occupies `(parent, case-insensitive new_name)`, that
    /// colliding sibling subtree is removed first so sibling names remain unique under case folding.
    pub fn rename(&mut self, id: EntryId, new_name: &str) {
        assert!(self.is_alive(id), "rename on invalid or dead entry {id}");
        let parent = self.parent[id as usize];
        if parent == NONE {
            let (display, folded) = normalize_root(new_name);
            let disp_trimmed = trim_trailing_root_slash(&display);
            let victim = self.roots.iter().copied().find(|&rid| {
                rid != id
                    && self.is_alive(rid)
                    && eq_case_insensitive(
                        trim_trailing_root_slash(self.display_str_unchecked(rid)),
                        disp_trimmed,
                    )
            });
            if let Some(v) = victim {
                self.remove(v);
            }
            self.update_entry_folded(id, &folded);
            self.update_entry_display(id, &display, &folded);
            self.index_gen = self.index_gen.wrapping_add(1);
            return;
        }

        let mut folded_buf = String::with_capacity(new_name.len());
        fold_into(new_name, &mut folded_buf);

        if let Some(victim) = self.find_child(parent, folded_buf.as_bytes(), new_name)
            && victim != id
        {
            self.remove(victim);
        }

        let old_folded = self.folded_bytes(id).to_vec();
        if old_folded != folded_buf.as_bytes() {
            let old_hash = hash_key(parent, &old_folded);
            self.bucket_remove_by_id_with_hash(id, old_hash);
            self.update_entry_folded(id, &folded_buf);
            self.bucket_insert(parent, folded_buf.as_bytes(), new_name, id);
        }

        self.update_entry_display(id, new_name, &folded_buf);
        let is_dir = (self.attrs[id as usize] & DIRECTORY) != 0;
        self.ext_id[id as usize] = self.compute_ext_id(&folded_buf, is_dir);
        self.index_gen = self.index_gen.wrapping_add(1);
    }

    /// Moves `id` under `new_parent` with name `new_name`.
    pub fn move_to(&mut self, id: EntryId, new_parent: EntryId, new_name: &str) {
        assert!(self.is_alive(id), "move_to on invalid or dead entry {id}");
        assert!(
            self.is_alive(new_parent),
            "move_to into invalid or dead parent {new_parent}"
        );

        let mut anc = new_parent;
        while anc != NONE {
            assert!(anc != id, "cannot move entry {id} into its own subtree");
            anc = self.parent[anc as usize];
        }

        let old_parent = self.parent[id as usize];
        assert!(old_parent != NONE, "cannot move a root entry {id}");

        if old_parent == new_parent {
            self.rename(id, new_name);
            return;
        }

        let mut folded_buf = String::with_capacity(new_name.len());
        fold_into(new_name, &mut folded_buf);

        // Detach `id` from `old_parent` BEFORE removing any colliding `victim` under `new_parent`,
        // in case `victim` is an ancestor of `id`.
        let old_folded = self.folded_bytes(id).to_vec();
        let old_hash = hash_key(old_parent, &old_folded);
        self.bucket_remove_by_id_with_hash(id, old_hash);
        self.unlink_child(old_parent, id);

        if let Some(victim) = self.find_child(new_parent, folded_buf.as_bytes(), new_name)
            && victim != id
        {
            self.remove(victim);
        }

        if old_folded != folded_buf.as_bytes() {
            self.update_entry_folded(id, &folded_buf);
        }
        self.update_entry_display(id, new_name, &folded_buf);
        let is_dir = (self.attrs[id as usize] & DIRECTORY) != 0;
        self.ext_id[id as usize] = self.compute_ext_id(&folded_buf, is_dir);

        self.parent[id as usize] = new_parent;
        self.next_sibling[id as usize] = self.first_child[new_parent as usize];
        self.first_child[new_parent as usize] = id;
        self.bucket_insert(new_parent, folded_buf.as_bytes(), new_name, id);

        let parent_hidden = (self.flags[new_parent as usize] & FLAG_EFFECTIVE_HIDDEN) != 0;
        let old_eff = (self.flags[id as usize] & FLAG_EFFECTIVE_HIDDEN) != 0;
        let new_eff = parent_hidden || is_own_hidden(self.attrs[id as usize]);
        if new_eff != old_eff {
            self.set_effective_hidden_subtree(id, new_eff);
        }
        self.index_gen = self.index_gen.wrapping_add(1);
    }

    /// Updates metadata (`attrs`, `size`, `mtime`) for `id`.
    pub fn update_meta(&mut self, id: EntryId, meta: Meta) {
        assert!(
            self.is_alive(id),
            "update_meta on invalid or dead entry {id}"
        );
        let old_is_dir = (self.attrs[id as usize] & DIRECTORY) != 0;
        let new_is_dir = (meta.attrs & DIRECTORY) != 0;
        self.apply_meta(id, meta);
        if old_is_dir != new_is_dir {
            let folded = self.folded_str(id).to_owned();
            self.ext_id[id as usize] = self.compute_ext_id(&folded, new_is_dir);
        }
        self.index_gen = self.index_gen.wrapping_add(1);
    }

    /// Looks up an entry by path (case-insensitive, diacritic-exact, accepts `\` and `/`, and trailing slashes).
    #[must_use]
    pub fn lookup(&self, path: &str) -> Option<EntryId> {
        if path.is_empty() {
            return None;
        }
        let mut norm = path.replace('/', "\\");
        while norm.len() > 1 && norm.ends_with('\\') {
            norm.pop();
        }
        if norm.is_empty() {
            return None;
        }

        let mut best_match: Option<(EntryId, usize, usize)> = None;
        for &rid in &self.roots {
            if !self.is_alive(rid) {
                continue;
            }
            let r_disp = trim_trailing_root_slash(self.display_str_unchecked(rid));
            if let Some((seg_count, rest_off)) = match_root_prefix(&norm, r_disp)
                && best_match.is_none_or(|(_, best_segs, _)| seg_count > best_segs)
            {
                best_match = Some((rid, seg_count, rest_off));
            }
        }

        let (mut cur, _, rest_off) = best_match?;
        if rest_off >= norm.len() {
            return Some(cur);
        }
        let mut seg_folded = String::new();
        for seg in norm[rest_off..].split('\\') {
            if seg.is_empty() {
                continue;
            }
            seg_folded.clear();
            fold_into(seg, &mut seg_folded);
            cur = self.find_child(cur, seg_folded.as_bytes(), seg)?;
        }
        Some(cur)
    }

    /// Lazily assembles and returns the full display path of `id`.
    #[must_use]
    pub fn path(&self, id: EntryId) -> String {
        assert!(self.is_alive(id), "path on invalid or dead entry {id}");
        let mut out = String::new();
        self.assemble_path_into(id, false, &mut out);
        out
    }

    /// Returns the display name of `id`.
    #[inline]
    #[must_use]
    pub fn name(&self, id: EntryId) -> &str {
        assert!(self.is_alive(id), "name on invalid or dead entry {id}");
        self.display_str_unchecked(id)
    }

    /// Returns the stored metadata of `id`.
    #[inline]
    #[must_use]
    pub fn meta(&self, id: EntryId) -> Meta {
        assert!(self.is_alive(id), "meta on invalid or dead entry {id}");
        let idx = id as usize;
        Meta {
            attrs: self.attrs[idx],
            size: self.size[idx],
            mtime: self.mtime[idx],
            file_id: self.file_id[idx],
        }
    }

    /// Looks up a live entry by its 64-bit NTFS/ReFS `file_id` (`0` always returns `None`).
    #[must_use]
    pub fn by_file_id(&self, fid: u64) -> Option<EntryId> {
        if fid == 0 || self.fid_buckets.is_empty() {
            return None;
        }
        let len = self.fid_buckets.len();
        let hash = hash_fid(fid);
        let mut slot = fastrange(hash, len);
        loop {
            let entry = self.fid_buckets[slot];
            if entry == NONE {
                return None;
            }
            if entry != TOMBSTONE && self.is_alive(entry) && self.file_id[entry as usize] == fid {
                return Some(entry);
            }
            slot += 1;
            if slot == len {
                slot = 0;
            }
        }
    }

    /// Returns the 64-bit `file_id` stored for `id` (`0` if unknown).
    #[inline]
    #[must_use]
    pub fn file_id(&self, id: EntryId) -> u64 {
        assert!(self.is_alive(id), "file_id on invalid or dead entry {id}");
        self.file_id[id as usize]
    }

    /// Updates the 64-bit `file_id` stored for `id` (`0` clears the mapping).
    pub fn set_file_id(&mut self, id: EntryId, file_id: u64) {
        assert!(
            self.is_alive(id),
            "set_file_id on invalid or dead entry {id}"
        );
        self.set_file_id_inner(id, file_id);
        self.index_gen = self.index_gen.wrapping_add(1);
    }

    pub(crate) fn reserve_fid_map(&mut self, expected_non_zero: usize) {
        if expected_non_zero == 0 {
            return;
        }
        let target = ((expected_non_zero.saturating_mul(10)) / 7 + 16).max(64);
        if self.fid_buckets.is_empty() {
            self.fid_buckets = vec![NONE; target];
        } else if self.fid_buckets.len() < target {
            self.rehash_fid_buckets_to(target);
        }
    }

    pub(crate) fn find_child_by_file_id(&self, parent: EntryId, fid: u64) -> Option<EntryId> {
        if fid == 0 {
            return None;
        }
        if let Some(eid) = self.by_file_id(fid)
            && self.parent[eid as usize] == parent
        {
            return Some(eid);
        }
        if let Some(dups) = self.fid_dups.get(&fid) {
            for &eid in dups {
                if self.is_alive(eid) && self.parent[eid as usize] == parent {
                    return Some(eid);
                }
            }
        }
        None
    }

    /// Returns the parent [`EntryId`] of `id`, or `None` if `id` is a root.
    #[inline]
    #[must_use]
    pub fn parent(&self, id: EntryId) -> Option<EntryId> {
        assert!(self.is_alive(id), "parent on invalid or dead entry {id}");
        let p = self.parent[id as usize];
        if p == NONE { None } else { Some(p) }
    }

    /// Iterates over all live [`EntryId`]s in the index.
    pub fn iter_ids(&self) -> impl Iterator<Item = EntryId> + '_ {
        self.flags
            .iter()
            .enumerate()
            .filter_map(|(i, &f)| ((f & FLAG_ALIVE) != 0).then_some(i as EntryId))
    }

    /// Bytes and count of the files anywhere under directory `id` (0 for a file or a dead id).
    /// Walks only that subtree, iteratively, so deep trees cannot overflow the stack.
    #[must_use]
    pub fn subtree_size(&self, id: EntryId) -> (u64, u64) {
        if !self.is_alive(id) || (self.attrs[id as usize] & DIRECTORY) == 0 {
            return (0, 0);
        }
        let (mut bytes, mut files) = (0u64, 0u64);
        let mut stack = vec![self.first_child[id as usize]];
        while let Some(mut c) = stack.pop() {
            while c != NONE {
                let i = c as usize;
                if (self.attrs[i] & DIRECTORY) != 0 {
                    stack.push(self.first_child[i]);
                } else {
                    bytes = bytes.saturating_add(self.size[i]);
                    files += 1;
                }
                c = self.next_sibling[i];
            }
        }
        (bytes, files)
    }

    /// The index lists something inside directory `id`. A childless directory is empty, or was
    /// never walked (a junction or link, access denied).
    #[must_use]
    pub fn has_children(&self, id: EntryId) -> bool {
        self.is_alive(id) && self.first_child[id as usize] != NONE
    }

    /// Returns `true` if `id` is a directory.
    #[inline]
    #[must_use]
    pub fn is_dir(&self, id: EntryId) -> bool {
        assert!(self.is_alive(id), "is_dir on invalid or dead entry {id}");
        (self.attrs[id as usize] & DIRECTORY) != 0
    }
    /// Drops dead slots and fragmented arena bytes, compacting `EntryId`s densely to `0..len()`.
    ///
    /// Increments the internal generation counter, invalidating previously returned [`crate::Results`]
    /// for [`crate::Index::refine`].
    pub fn compact(&mut self) {
        let _ = self.compact_inner(false);
    }

    /// Drops dead slots and returns the `old_id -> new_id` mapping (`NONE` for removed slots).
    pub fn compact_with_remap(&mut self) -> Vec<EntryId> {
        self.compact_inner(true)
    }

    fn compact_inner(&mut self, build_remap: bool) -> Vec<EntryId> {
        let old_slots = self.parent.len();
        if self.live_count == old_slots && self.dead_arena_bytes == 0 && self.arenas_sequential {
            return if build_remap {
                (0..old_slots as EntryId).collect()
            } else {
                Vec::new()
            };
        }

        let mut remap = vec![NONE; old_slots];
        let mut new_id: EntryId = 0;
        let mut folded_bytes_needed = 0usize;
        let mut display_bytes_needed = 0usize;

        for (old_i, slot) in remap.iter_mut().enumerate() {
            if (self.flags[old_i] & FLAG_ALIVE) != 0 {
                *slot = new_id;
                new_id += 1;
                folded_bytes_needed +=
                    arena_entry_total_len(&self.folded_arena, self.folded_off[old_i]);
                if self.display_off[old_i] != NONE {
                    display_bytes_needed +=
                        arena_entry_total_len(&self.display_arena, self.display_off[old_i]);
                }
            }
        }

        let n = self.live_count;
        let fid_cap = if self.fid_live > 0 {
            ((self.fid_live * 10) / 7 + 16).max(64)
        } else {
            0
        };
        let mut new_idx = Self {
            folded_arena: Vec::with_capacity(folded_bytes_needed),
            display_arena: Vec::with_capacity(display_bytes_needed),
            folded_off: Vec::with_capacity(n),
            display_off: Vec::with_capacity(n),
            parent: Vec::with_capacity(n),
            attrs: Vec::with_capacity(n),
            size: Vec::with_capacity(n),
            mtime: Vec::with_capacity(n),
            file_id: Vec::with_capacity(n),
            ext_id: Vec::with_capacity(n),
            flags: Vec::with_capacity(n),
            first_child: vec![NONE; n],
            next_sibling: vec![NONE; n],
            buckets: vec![NONE; ((n * 10) / 7 + 16).max(64)],
            bucket_occupied: 0,
            fid_buckets: if fid_cap > 0 {
                vec![NONE; fid_cap]
            } else {
                Vec::new()
            },
            fid_occupied: 0,
            fid_live: 0,
            fid_dups: HashMap::new(),
            roots: Vec::with_capacity(self.roots.len()),
            exts: std::mem::take(&mut self.exts),
            ext_map: std::mem::take(&mut self.ext_map),
            ext_overflow: self.ext_overflow,
            live_count: n,
            dead_arena_bytes: 0,
            arenas_sequential: true,
            volume_serial: self.volume_serial,
            instance_id: self.instance_id,
            index_gen: self.index_gen.wrapping_add(1),
        };

        for (old_i, &mapped) in remap.iter().enumerate() {
            if mapped == NONE {
                continue;
            }
            let f_slice = read_arena_slice(&self.folded_arena, self.folded_off[old_i]);
            let f_off = push_arena(&mut new_idx.folded_arena, f_slice);
            let d_off = if self.display_off[old_i] == NONE {
                NONE
            } else {
                let d_slice = read_arena_slice(&self.display_arena, self.display_off[old_i]);
                push_arena(&mut new_idx.display_arena, d_slice)
            };
            let old_p = self.parent[old_i];
            let new_p = if old_p == NONE {
                NONE
            } else {
                remap[old_p as usize]
            };

            let fid = self.file_id[old_i];
            new_idx.folded_off.push(f_off);
            new_idx.display_off.push(d_off);
            new_idx.parent.push(new_p);
            new_idx.attrs.push(self.attrs[old_i]);
            new_idx.size.push(self.size[old_i]);
            new_idx.mtime.push(self.mtime[old_i]);
            new_idx.file_id.push(fid);
            new_idx.ext_id.push(self.ext_id[old_i]);
            new_idx.flags.push(self.flags[old_i]);

            if fid != 0 {
                new_idx.fid_insert(fid, mapped);
            }
            if new_p == NONE {
                new_idx.roots.push(mapped);
            } else {
                new_idx.next_sibling[mapped as usize] = new_idx.first_child[new_p as usize];
                new_idx.first_child[new_p as usize] = mapped;
                new_idx.bucket_insert_unique(new_p, f_slice, mapped);
            }
        }

        *self = new_idx;
        if build_remap { remap } else { Vec::new() }
    }

    #[inline]
    pub(crate) fn folded_bytes(&self, id: EntryId) -> &[u8] {
        read_arena_slice(&self.folded_arena, self.folded_off[id as usize])
    }

    #[inline]
    pub(crate) fn folded_str(&self, id: EntryId) -> &str {
        std::str::from_utf8(self.folded_bytes(id)).expect("folded arena contains valid UTF-8")
    }

    #[inline]
    pub(crate) fn display_str_unchecked(&self, id: EntryId) -> &str {
        let idx = id as usize;
        let off = self.display_off[idx];
        if off == NONE {
            self.folded_str(id)
        } else {
            let bytes = read_arena_slice(&self.display_arena, off);
            std::str::from_utf8(bytes).expect("display arena contains valid UTF-8")
        }
    }

    #[inline]
    pub(crate) fn has_distinct_display(&self, id: EntryId) -> bool {
        self.display_off[id as usize] != NONE
    }

    #[inline]
    pub(crate) fn raw_display_bytes(&self, id: EntryId) -> &[u8] {
        let off = self.display_off[id as usize];
        if off == NONE {
            self.folded_bytes(id)
        } else {
            read_arena_slice(&self.display_arena, off)
        }
    }

    pub(crate) fn assemble_path_into(&self, id: EntryId, folded: bool, out: &mut String) {
        let mut stack_buf = [0u32; 32];
        let mut depth = 0usize;
        let mut overflow = Vec::new();
        let mut cur = id;

        while cur != NONE {
            if depth < stack_buf.len() && overflow.is_empty() {
                stack_buf[depth] = cur;
                depth += 1;
            } else {
                if overflow.is_empty() {
                    overflow.extend_from_slice(&stack_buf[..depth]);
                }
                overflow.push(cur);
            }
            cur = self.parent[cur as usize];
        }

        let chain: &[EntryId] = if overflow.is_empty() {
            &stack_buf[..depth]
        } else {
            &overflow
        };

        out.clear();
        for &node in chain.iter().rev() {
            let seg = if folded {
                self.folded_str(node)
            } else {
                self.display_str_unchecked(node)
            };
            if !out.is_empty() && !out.ends_with('\\') && !out.ends_with('/') {
                out.push('\\');
            }
            out.push_str(seg);
        }
    }

    fn apply_meta(&mut self, id: EntryId, meta: Meta) {
        let idx = id as usize;
        self.attrs[idx] = meta.attrs;
        self.size[idx] = meta.size;
        self.mtime[idx] = meta.mtime;
        self.set_file_id_inner(id, meta.file_id);
        let parent = self.parent[idx];
        let parent_hidden =
            parent != NONE && (self.flags[parent as usize] & FLAG_EFFECTIVE_HIDDEN) != 0;
        let new_eff = parent_hidden || is_own_hidden(meta.attrs);
        let old_eff = (self.flags[idx] & FLAG_EFFECTIVE_HIDDEN) != 0;
        if new_eff != old_eff {
            self.set_effective_hidden_subtree(id, new_eff);
        }
    }

    fn set_effective_hidden_subtree(&mut self, root: EntryId, root_eff: bool) {
        let mut stack = vec![(root, root_eff)];
        while let Some((node, eff)) = stack.pop() {
            let ni = node as usize;
            if (self.flags[ni] & FLAG_ALIVE) == 0 {
                continue;
            }
            if eff {
                self.flags[ni] |= FLAG_EFFECTIVE_HIDDEN;
            } else {
                self.flags[ni] &= !FLAG_EFFECTIVE_HIDDEN;
            }
            let mut child = self.first_child[ni];
            while child != NONE {
                let ci = child as usize;
                let child_eff = eff || is_own_hidden(self.attrs[ci]);
                stack.push((child, child_eff));
                child = self.next_sibling[ci];
            }
        }
    }

    fn update_entry_folded(&mut self, id: EntryId, folded: &str) {
        self.arenas_sequential = false;
        let idx = id as usize;
        let old_off = self.folded_off[idx];
        let old_cap = arena_entry_total_len(&self.folded_arena, old_off);
        let needed = encoded_len(folded.len());
        if needed <= old_cap {
            write_arena_at(&mut self.folded_arena, old_off as usize, folded.as_bytes());
            self.dead_arena_bytes += old_cap - needed;
        } else {
            self.dead_arena_bytes += old_cap;
            self.folded_off[idx] = push_arena(&mut self.folded_arena, folded.as_bytes());
        }
    }

    fn update_entry_display(&mut self, id: EntryId, name: &str, folded: &str) {
        let idx = id as usize;
        let old_off = self.display_off[idx];
        if name == folded {
            if old_off != NONE {
                self.arenas_sequential = false;
                self.dead_arena_bytes += arena_entry_total_len(&self.display_arena, old_off);
                self.display_off[idx] = NONE;
            }
            return;
        }
        if old_off == NONE {
            self.arenas_sequential = false;
            self.display_off[idx] = push_arena(&mut self.display_arena, name.as_bytes());
            return;
        }
        self.arenas_sequential = false;
        let old_cap = arena_entry_total_len(&self.display_arena, old_off);
        let needed = encoded_len(name.len());
        if needed <= old_cap {
            write_arena_at(&mut self.display_arena, old_off as usize, name.as_bytes());
            self.dead_arena_bytes += old_cap - needed;
        } else {
            self.dead_arena_bytes += old_cap;
            self.display_off[idx] = push_arena(&mut self.display_arena, name.as_bytes());
        }
    }
    fn unlink_child(&mut self, parent: EntryId, target: EntryId) {
        let pi = parent as usize;
        let mut prev = NONE;
        let mut cur = self.first_child[pi];
        while cur != NONE {
            if cur == target {
                let next = self.next_sibling[cur as usize];
                if prev == NONE {
                    self.first_child[pi] = next;
                } else {
                    self.next_sibling[prev as usize] = next;
                }
                self.next_sibling[cur as usize] = NONE;
                return;
            }
            prev = cur;
            cur = self.next_sibling[cur as usize];
        }
    }

    fn compute_ext_id(&mut self, folded_name: &str, is_dir: bool) -> u16 {
        if is_dir {
            return EXT_NONE;
        }
        let Some(ext) = extract_ext(folded_name) else {
            return EXT_NONE;
        };
        if let Some(&id) = self.ext_map.get(ext) {
            return id;
        }
        if self.exts.len() >= EXT_OVERFLOW as usize {
            self.ext_overflow = true;
            return EXT_OVERFLOW;
        }
        let id = self.exts.len() as u16;
        let owned = ext.to_owned();
        self.exts.push(owned.clone());
        self.ext_map.insert(owned, id);
        id
    }

    pub(crate) fn find_ext_id(&self, folded_ext: &str) -> Option<u16> {
        if folded_ext.is_empty() {
            return Some(EXT_NONE);
        }
        self.ext_map.get(folded_ext).copied()
    }

    pub(crate) fn find_child(
        &self,
        parent: EntryId,
        folded_name: &[u8],
        display_name: &str,
    ) -> Option<EntryId> {
        let len = self.buckets.len();
        if len == 0 {
            return None;
        }
        let hash = hash_key(parent, folded_name);
        let mut slot = fastrange(hash, len);
        loop {
            let entry = self.buckets[slot];
            if entry == NONE {
                return None;
            }
            if entry != TOMBSTONE
                && self.parent[entry as usize] == parent
                && self.folded_bytes(entry) == folded_name
                && eq_case_insensitive(self.display_str_unchecked(entry), display_name)
            {
                return Some(entry);
            }
            slot += 1;
            if slot == len {
                slot = 0;
            }
        }
    }

    pub(crate) fn bucket_insert(
        &mut self,
        parent: EntryId,
        folded_name: &[u8],
        display_name: &str,
        id: EntryId,
    ) {
        if self.bucket_occupied * 10 >= self.buckets.len() * 7 {
            self.rehash_buckets();
        }
        let len = self.buckets.len();
        let hash = hash_key(parent, folded_name);
        let mut slot = fastrange(hash, len);
        let mut first_tombstone = None;
        loop {
            let entry = self.buckets[slot];
            if entry == NONE {
                let dst = first_tombstone.unwrap_or(slot);
                if first_tombstone.is_none() {
                    self.bucket_occupied += 1;
                }
                self.buckets[dst] = id;
                return;
            }
            if entry == TOMBSTONE {
                if first_tombstone.is_none() {
                    first_tombstone = Some(slot);
                }
            } else if entry == id
                || (self.parent[entry as usize] == parent
                    && self.folded_bytes(entry) == folded_name
                    && eq_case_insensitive(self.display_str_unchecked(entry), display_name))
            {
                self.buckets[slot] = id;
                return;
            }
            slot += 1;
            if slot == len {
                slot = 0;
            }
        }
    }

    fn bucket_insert_unique(&mut self, parent: EntryId, folded_name: &[u8], id: EntryId) {
        if self.bucket_occupied * 10 >= self.buckets.len() * 7 {
            self.rehash_buckets();
        }
        let len = self.buckets.len();
        let hash = hash_key(parent, folded_name);
        let mut slot = fastrange(hash, len);
        while self.buckets[slot] != NONE {
            slot += 1;
            if slot == len {
                slot = 0;
            }
        }
        self.buckets[slot] = id;
        self.bucket_occupied += 1;
    }

    fn bucket_remove_by_id_with_hash(&mut self, id: EntryId, hash: u64) {
        bucket_remove_by_id(&mut self.buckets, id, hash);
    }

    fn rehash_buckets(&mut self) {
        let target_min = ((self.live_count * 10) / 7 + 16).max(64);
        let new_cap = if self.buckets.len() >= target_min {
            self.buckets.len()
        } else {
            (self.buckets.len() + self.buckets.len() / 2).max(target_min)
        };
        let mut next = vec![NONE; new_cap];
        let mut occupied = 0usize;

        for (i, &flags) in self.flags.iter().enumerate() {
            if (flags & FLAG_ALIVE) == 0 {
                continue;
            }
            let p = self.parent[i];
            if p == NONE {
                continue;
            }
            let f_bytes = read_arena_slice(&self.folded_arena, self.folded_off[i]);
            let hash = hash_key(p, f_bytes);
            let mut slot = fastrange(hash, new_cap);
            while next[slot] != NONE {
                slot += 1;
                if slot == new_cap {
                    slot = 0;
                }
            }
            next[slot] = i as EntryId;
            occupied += 1;
        }
        self.buckets = next;
        self.bucket_occupied = occupied;
    }

    pub(crate) fn rebuild_child_links_and_buckets_checked(&mut self) -> Result<(), &'static str> {
        let n = self.parent.len();
        self.first_child.clear();
        self.first_child.resize(n, NONE);
        self.next_sibling.clear();
        self.next_sibling.resize(n, NONE);
        self.roots.clear();

        let cap = ((self.live_count * 10) / 7 + 16).max(64);
        self.buckets.clear();
        self.buckets.resize(cap, NONE);
        self.bucket_occupied = 0;

        let fid_total = self
            .flags
            .iter()
            .zip(&self.file_id)
            .filter(|&(&fl, &fid)| (fl & FLAG_ALIVE) != 0 && fid != 0)
            .count();
        self.fid_buckets.clear();
        if fid_total > 0 {
            let fid_cap = ((fid_total * 10) / 7 + 16).max(64);
            self.fid_buckets.resize(fid_cap, NONE);
        }
        self.fid_occupied = 0;
        self.fid_live = 0;
        self.fid_dups.clear();

        for i in 0..n {
            if (self.flags[i] & FLAG_ALIVE) == 0 {
                continue;
            }
            let id = i as EntryId;
            let fid = self.file_id[i];
            if fid != 0 {
                self.fid_insert(fid, id);
            }
            let p = self.parent[i];
            if p == NONE {
                let disp = trim_trailing_root_slash(self.display_str_unchecked(id));
                if self.roots.iter().any(|&r| {
                    eq_case_insensitive(
                        trim_trailing_root_slash(self.display_str_unchecked(r)),
                        disp,
                    )
                }) {
                    return Err("duplicate root");
                }
                self.roots.push(id);
            } else {
                self.next_sibling[i] = self.first_child[p as usize];
                self.first_child[p as usize] = id;
                let f_bytes = read_arena_slice(&self.folded_arena, self.folded_off[i]);
                let id_disp = self.display_str_unchecked(id);
                let hash = hash_key(p, f_bytes);
                let mut slot = fastrange(hash, cap);
                loop {
                    let existing = self.buckets[slot];
                    if existing == NONE {
                        self.buckets[slot] = id;
                        self.bucket_occupied += 1;
                        break;
                    }
                    if self.parent[existing as usize] == p
                        && read_arena_slice(&self.folded_arena, self.folded_off[existing as usize])
                            == f_bytes
                        && eq_case_insensitive(self.display_str_unchecked(existing), id_disp)
                    {
                        return Err("duplicate sibling name");
                    }
                    slot += 1;
                    if slot == cap {
                        slot = 0;
                    }
                }
            }
        }
        Ok(())
    }

    fn set_file_id_inner(&mut self, id: EntryId, new_fid: u64) {
        let ui = id as usize;
        let old_fid = self.file_id[ui];
        if old_fid == new_fid {
            return;
        }
        if old_fid != 0 {
            self.fid_remove(old_fid, id);
        }
        self.file_id[ui] = new_fid;
        if new_fid != 0 {
            self.fid_insert(new_fid, id);
        }
    }

    fn fid_insert(&mut self, fid: u64, id: EntryId) {
        if fid == 0 {
            return;
        }
        if self.fid_buckets.is_empty() || self.fid_occupied * 10 >= self.fid_buckets.len() * 7 {
            self.rehash_fid_buckets();
        }
        let len = self.fid_buckets.len();
        let hash = hash_fid(fid);
        let mut slot = fastrange(hash, len);
        let mut first_tombstone = None;
        loop {
            let entry = self.fid_buckets[slot];
            if entry == NONE {
                let dst = first_tombstone.unwrap_or(slot);
                if first_tombstone.is_none() {
                    self.fid_occupied += 1;
                }
                self.fid_buckets[dst] = id;
                self.fid_live += 1;
                return;
            }
            if entry == TOMBSTONE {
                if first_tombstone.is_none() {
                    first_tombstone = Some(slot);
                }
            } else if entry == id {
                return;
            } else if self.file_id[entry as usize] == fid {
                let dups = self.fid_dups.entry(fid).or_default();
                if dups.is_empty() {
                    dups.push(entry);
                }
                if !dups.contains(&id) {
                    dups.push(id);
                }
                self.fid_buckets[slot] = id;
                return;
            }
            slot += 1;
            if slot == len {
                slot = 0;
            }
        }
    }

    fn fid_remove(&mut self, fid: u64, id: EntryId) {
        if fid == 0 || self.fid_buckets.is_empty() {
            return;
        }
        let replacement = if let Some(dups) = self.fid_dups.get_mut(&fid) {
            if let Some(pos) = dups.iter().position(|&cur| cur == id) {
                dups.swap_remove(pos);
            }
            if dups.len() <= 1 {
                let rem = dups.first().copied();
                self.fid_dups.remove(&fid);
                rem
            } else {
                dups.first().copied()
            }
        } else {
            None
        };

        let len = self.fid_buckets.len();
        let hash = hash_fid(fid);
        let mut slot = fastrange(hash, len);
        loop {
            let entry = self.fid_buckets[slot];
            if entry == NONE {
                return;
            }
            if entry == id {
                if let Some(rep) = replacement {
                    self.fid_buckets[slot] = rep;
                } else {
                    self.fid_buckets[slot] = TOMBSTONE;
                    self.fid_live = self.fid_live.saturating_sub(1);
                }
                return;
            }
            slot += 1;
            if slot == len {
                slot = 0;
            }
        }
    }

    fn rehash_fid_buckets(&mut self) {
        let target_min = ((self.fid_live * 10) / 7 + 16).max(64);
        let cur_len = self.fid_buckets.len();
        let new_cap = if cur_len >= target_min {
            cur_len
        } else {
            (cur_len + cur_len / 2).max(target_min)
        };
        self.rehash_fid_buckets_to(new_cap);
    }

    fn rehash_fid_buckets_to(&mut self, new_cap: usize) {
        let mut next = vec![NONE; new_cap];
        let mut occupied = 0usize;
        let mut live = 0usize;
        self.fid_dups.clear();

        for (i, &flags) in self.flags.iter().enumerate() {
            if (flags & FLAG_ALIVE) == 0 {
                continue;
            }
            let fid = self.file_id[i];
            if fid == 0 {
                continue;
            }
            let id = i as EntryId;
            let hash = hash_fid(fid);
            let mut slot = fastrange(hash, new_cap);
            let mut replaced = false;
            while next[slot] != NONE {
                let cur = next[slot];
                if self.file_id[cur as usize] == fid {
                    let dups = self.fid_dups.entry(fid).or_default();
                    if dups.is_empty() {
                        dups.push(cur);
                    }
                    dups.push(id);
                    next[slot] = id;
                    replaced = true;
                    break;
                }
                slot += 1;
                if slot == new_cap {
                    slot = 0;
                }
            }
            if !replaced {
                next[slot] = id;
                occupied += 1;
                live += 1;
            }
        }
        self.fid_buckets = next;
        self.fid_occupied = occupied;
        self.fid_live = live;
    }
}

#[inline]
fn hash_fid(fid: u64) -> u64 {
    let mut x = fid.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x
}

#[inline]
pub(crate) fn is_own_hidden(attrs: u32) -> bool {
    (attrs & HIDDEN) != 0
}

#[inline]
pub(crate) fn extract_ext(folded_name: &str) -> Option<&str> {
    let (stem, ext) = folded_name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        None
    } else {
        Some(ext)
    }
}

#[inline]
fn fastrange(hash: u64, len: usize) -> usize {
    (((hash as u32) as u64).wrapping_mul(len as u64) >> 32) as usize
}

#[inline]
fn hash_key(parent: EntryId, folded: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ (parent as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let (chunks, rem) = folded.as_chunks::<8>();
    for &c in chunks {
        let word = u64::from_le_bytes(c);
        h ^= word;
        h = h.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    for &b in rem {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}

#[inline]
fn encoded_len(len: usize) -> usize {
    if len < 255 { 1 + len } else { 3 + len }
}

pub(crate) fn reserve_arena(buf: &mut Vec<u8>, additional: usize) {
    if buf.capacity() - buf.len() >= additional {
        return;
    }
    let cur = buf.capacity();
    let target = if cur >= 256 * 1024 {
        (cur + cur / 4).max(buf.len() + additional)
    } else {
        (cur * 2).max(buf.len() + additional).max(1024)
    };
    buf.reserve_exact(target - buf.len());
}

fn push_arena(arena: &mut Vec<u8>, bytes: &[u8]) -> u32 {
    assert!(bytes.len() <= u16::MAX as usize, "name exceeds 64 KiB");
    let off = u32::try_from(arena.len()).expect("arena exceeds 4 GiB");
    reserve_arena(arena, encoded_len(bytes.len()));
    if bytes.len() < 255 {
        arena.push(bytes.len() as u8);
    } else {
        arena.push(255);
        arena.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    }
    arena.extend_from_slice(bytes);
    off
}

fn write_arena_at(arena: &mut [u8], off: usize, bytes: &[u8]) {
    assert!(bytes.len() <= u16::MAX as usize, "name exceeds 64 KiB");
    if bytes.len() < 255 {
        arena[off] = bytes.len() as u8;
        arena[off + 1..off + 1 + bytes.len()].copy_from_slice(bytes);
    } else {
        arena[off] = 255;
        let len_bytes = (bytes.len() as u16).to_le_bytes();
        arena[off + 1] = len_bytes[0];
        arena[off + 2] = len_bytes[1];
        arena[off + 3..off + 3 + bytes.len()].copy_from_slice(bytes);
    }
}

#[inline]
pub(crate) fn read_arena_slice(arena: &[u8], off: u32) -> &[u8] {
    let i = off as usize;
    let first = arena[i];
    if first < 255 {
        let len = first as usize;
        &arena[i + 1..i + 1 + len]
    } else {
        let len = u16::from_le_bytes([arena[i + 1], arena[i + 2]]) as usize;
        &arena[i + 3..i + 3 + len]
    }
}

#[inline]
pub(crate) fn arena_entry_total_len(arena: &[u8], off: u32) -> usize {
    let i = off as usize;
    let first = arena[i];
    if first < 255 {
        1 + first as usize
    } else {
        3 + u16::from_le_bytes([arena[i + 1], arena[i + 2]]) as usize
    }
}

fn normalize_root(root: &str) -> (String, String) {
    let mut normalized = root.replace('/', "\\");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2..].iter().all(|&b| b == b'\\')
    {
        let drive = bytes[0] as char;
        let display = format!("{drive}:\\");
        let folded = format!("{}:\\", drive.to_ascii_lowercase());
        return (display, folded);
    }
    while normalized.len() > 1 && normalized.ends_with('\\') {
        normalized.pop();
    }
    let folded = fm_fs::fold(&normalized);
    (normalized, folded)
}
#[inline]
pub(crate) fn eq_case_insensitive(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(b);
    }
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

#[inline]
fn trim_trailing_root_slash(s: &str) -> &str {
    if s.len() > 1 {
        s.trim_end_matches('\\')
    } else {
        s
    }
}

fn match_root_prefix(norm: &str, root_trimmed: &str) -> Option<(usize, usize)> {
    if root_trimmed == "\\" {
        return norm.starts_with('\\').then_some((1, 1));
    }
    if let Some(root_unc) = root_trimmed.strip_prefix("\\\\") {
        if !norm.starts_with("\\\\") {
            return None;
        }
        let mut r_segs = root_unc.split('\\').filter(|s| !s.is_empty());
        let mut offset = 2usize;
        let mut seg_count = 0usize;
        for r_seg in &mut r_segs {
            while norm.as_bytes().get(offset) == Some(&b'\\') {
                offset += 1;
            }
            let rest = &norm[offset..];
            let seg = rest.split('\\').next()?;
            if seg.is_empty() || !eq_case_insensitive(seg, r_seg) {
                return None;
            }
            offset += seg.len();
            seg_count += 1;
        }
        if offset < norm.len() && norm.as_bytes()[offset] != b'\\' {
            return None;
        }
        let rest_off = if offset < norm.len() {
            offset + 1
        } else {
            offset
        };
        return Some((seg_count + 1, rest_off));
    }

    if norm.starts_with("\\\\") {
        return None;
    }
    let mut offset = 0usize;
    if root_trimmed.starts_with('\\') {
        if !norm.starts_with('\\') {
            return None;
        }
        offset = 1;
    } else if norm.starts_with('\\') {
        return None;
    }
    let mut seg_count = 0usize;
    for r_seg in root_trimmed.split('\\') {
        if r_seg.is_empty() {
            continue;
        }
        if seg_count > 0 {
            while norm.as_bytes().get(offset) == Some(&b'\\') {
                offset += 1;
            }
        }
        let rest = &norm[offset..];
        let seg = rest.split('\\').next()?;
        if seg.is_empty() || !eq_case_insensitive(seg, r_seg) {
            return None;
        }
        offset += seg.len();
        seg_count += 1;
    }
    if offset < norm.len() && norm.as_bytes()[offset] != b'\\' {
        return None;
    }
    let rest_off = if offset < norm.len() {
        offset + 1
    } else {
        offset
    };
    Some((seg_count, rest_off))
}

fn bucket_remove_by_id(buckets: &mut [EntryId], id: EntryId, hash: u64) {
    let len = buckets.len();
    if len == 0 {
        return;
    }
    let mut slot = fastrange(hash, len);
    loop {
        let entry = buckets[slot];
        if entry == NONE {
            return;
        }
        if entry == id {
            buckets[slot] = TOMBSTONE;
            return;
        }
        slot += 1;
        if slot == len {
            slot = 0;
        }
    }
}
