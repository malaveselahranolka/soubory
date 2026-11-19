//! Query parsing, parallel `memchr::memmem` search, incremental refinement, and cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fm_fs::attrs::DIRECTORY;
use fm_fs::fold;
use memchr::memmem::Finder;
use rayon::prelude::*;

use crate::arena::{
    EXT_OVERFLOW, EntryId, FLAG_ALIVE, FLAG_EFFECTIVE_HIDDEN, Index, NONE, extract_ext,
};
use crate::rank::{RankEntry, make_filter_only_rank, rank_top_limit, score_entry};

const SCAN_CHUNK_SIZE: usize = 4096;

/// Restricts results to directories (`folder:`) or regular files (`file:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Only directories (`FILE_ATTRIBUTE_DIRECTORY`).
    Dir,
    /// Only non-directory files.
    File,
}

/// Parsed search query produced by [`parse_query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub(crate) name_terms: Vec<String>,
    pub(crate) path_terms: Vec<String>,
    pub(crate) ext_groups: Vec<Vec<String>>,
    pub(crate) kind: Option<EntryKind>,
    pub(crate) impossible: bool,
}

impl Query {
    /// Returns `true` if the query contains no search terms or filters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.impossible
            && self.name_terms.is_empty()
            && self.path_terms.is_empty()
            && self.ext_groups.is_empty()
            && self.kind.is_none()
    }

    /// Returns `true` if every entry matching `self` is guaranteed to also match `prev`.
    pub(crate) fn is_refinement_of(&self, prev: &Query) -> bool {
        if self.is_empty() || prev.is_empty() || self.impossible || prev.impossible {
            return false;
        }
        if let Some(prev_kind) = prev.kind
            && self.kind != Some(prev_kind)
        {
            return false;
        }
        for p_term in &prev.name_terms {
            if !self.name_terms.iter().any(|s_term| s_term.contains(p_term)) {
                return false;
            }
        }
        for p_path in &prev.path_terms {
            if !self.path_terms.iter().any(|s_path| s_path.contains(p_path)) {
                return false;
            }
        }
        for p_exts in &prev.ext_groups {
            let narrowed = self
                .ext_groups
                .iter()
                .any(|s_exts| s_exts.iter().all(|ext| p_exts.iter().any(|pe| pe == ext)));
            if !narrowed {
                return false;
            }
        }
        true
    }
}

/// Options controlling query execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    /// Whether hidden/system entries and entries under hidden ancestors are included.
    pub include_hidden: bool,
    /// Maximum number of ranked `EntryId`s retained in [`Results::ids`].
    pub limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            limit: 1000,
        }
    }
}

/// Ranked results returned by [`Index::search`] and [`Index::refine`].
#[derive(Debug, Clone)]
pub struct Results {
    /// Top-ranked entry IDs up to `SearchOptions::limit`.
    pub ids: Vec<EntryId>,
    /// Exact total number of matching entries before `limit` truncation.
    pub total_matches: usize,
    pub(crate) instance_id: u64,
    pub(crate) generation: u64,
    pub(crate) query: Query,
    pub(crate) include_hidden: bool,
    pub(crate) all_matches: Vec<EntryId>,
}

impl Results {
    fn empty(instance_id: u64, generation: u64, query: Query, include_hidden: bool) -> Self {
        Self {
            ids: Vec::new(),
            total_matches: 0,
            instance_id,
            generation,
            query,
            include_hidden,
            all_matches: Vec::new(),
        }
    }
}

/// Cheaply cloneable cancellation token backed by an atomic generation counter.
///
/// Calling [`CancelToken::next`] issues a new token for a newer query and automatically
/// cancels all previously issued tokens sharing the same counter. Calling [`CancelToken::cancel`]
/// cancels this token (and earlier generations) directly.
#[derive(Debug, Clone)]
pub struct CancelToken {
    current: Arc<AtomicU64>,
    my_gen: u64,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    /// Creates a new uncancelled token with its own generation counter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: Arc::new(AtomicU64::new(0)),
            my_gen: 0,
        }
    }

    /// Advances the shared generation counter and returns a fresh token for a newer query,
    /// cancelling all tokens from earlier generations.
    #[must_use]
    pub fn next(&self) -> Self {
        let next_gen = self.current.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        Self {
            current: Arc::clone(&self.current),
            my_gen: next_gen,
        }
    }

    /// Cancels this token (and any clones with the same or earlier generation).
    pub fn cancel(&self) {
        let target = self.my_gen.wrapping_add(1);
        let _ = self
            .current
            .try_update(Ordering::Release, Ordering::Relaxed, |cur| {
                if cur <= self.my_gen {
                    Some(target)
                } else {
                    None
                }
            });
    }

    /// Returns `true` if this token has been cancelled or superseded by a newer generation.
    #[inline]
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.current.load(Ordering::Relaxed) != self.my_gen
    }
}

/// Parses a search string into a [`Query`]. Never fails; unknown prefixes are plain terms.
#[must_use]
pub fn parse_query(s: &str) -> Query {
    let mut name_terms = Vec::new();
    let mut path_terms = Vec::new();
    let mut ext_groups = Vec::new();
    let mut kind: Option<EntryKind> = None;
    let mut impossible = false;

    for (raw, quoted) in tokenize(s) {
        if raw.is_empty() {
            continue;
        }
        if quoted {
            let folded = fold(&raw);
            if !folded.is_empty() {
                name_terms.push(folded);
            }
            continue;
        }

        if let Some((prefix, rest)) = raw.split_once(':') {
            if prefix.eq_ignore_ascii_case("ext") {
                let mut exts = Vec::new();
                for part in rest.split(';') {
                    let trimmed = part.trim_start_matches('.');
                    let folded = fold(trimmed);
                    if !folded.is_empty() && !exts.contains(&folded) {
                        exts.push(folded);
                    }
                }
                if exts.is_empty() {
                    let folded = fold(&raw);
                    if !folded.is_empty() {
                        name_terms.push(folded);
                    }
                } else {
                    ext_groups.push(exts);
                }
                continue;
            }
            if prefix.eq_ignore_ascii_case("path") {
                if rest.is_empty() {
                    let folded = fold(&raw);
                    if !folded.is_empty() {
                        name_terms.push(folded);
                    }
                } else {
                    let normalized = rest.replace('/', "\\");
                    let folded = fold(&normalized);
                    if !folded.is_empty() {
                        path_terms.push(folded);
                    }
                }
                continue;
            }
            if prefix.eq_ignore_ascii_case("folder") {
                if kind == Some(EntryKind::File) {
                    impossible = true;
                } else {
                    kind = Some(EntryKind::Dir);
                }
                if !rest.is_empty() {
                    let folded = fold(rest);
                    if !folded.is_empty() {
                        name_terms.push(folded);
                    }
                }
                continue;
            }
            if prefix.eq_ignore_ascii_case("file") {
                if kind == Some(EntryKind::Dir) {
                    impossible = true;
                } else {
                    kind = Some(EntryKind::File);
                }
                if !rest.is_empty() {
                    let folded = fold(rest);
                    if !folded.is_empty() {
                        name_terms.push(folded);
                    }
                }
                continue;
            }
        }

        let folded = fold(&raw);
        if !folded.is_empty() {
            name_terms.push(folded);
        }
    }

    Query {
        name_terms,
        path_terms,
        ext_groups,
        kind,
        impossible,
    }
}

fn tokenize(s: &str) -> Vec<(String, bool)> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        if ch == '"' {
            chars.next();
            let mut buf = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                buf.push(c);
            }
            tokens.push((buf, true));
            continue;
        }

        let mut buf = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            if c == '"' && buf.ends_with(':') {
                chars.next();
                for qc in chars.by_ref() {
                    if qc == '"' {
                        break;
                    }
                    buf.push(qc);
                }
                break;
            }
            buf.push(c);
            chars.next();
        }
        tokens.push((buf, false));
    }

    tokens
}
struct PreparedExtGroup<'a> {
    ids: Vec<u16>,
    names: &'a [String],
    check_overflow: bool,
}

enum PreparedPathCheck<'a> {
    NoSlash(Box<Finder<'a>>),
    WithSlash(&'a str),
}

struct WorkerAcc {
    entries: Vec<RankEntry>,
    path_buf: String,
}

impl WorkerAcc {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            path_buf: String::with_capacity(256),
        }
    }
}

impl Index {
    /// Searches the index using `q` and `opts`, returning `None` if cancelled via `cancel`.
    #[must_use]
    pub fn search(&self, q: &Query, opts: &SearchOptions, cancel: &CancelToken) -> Option<Results> {
        if cancel.is_cancelled() {
            return None;
        }
        if q.is_empty() || q.impossible {
            return Some(Results::empty(
                self.instance_id,
                self.index_gen,
                q.clone(),
                opts.include_hidden,
            ));
        }

        let ext_groups = match self.prepare_ext_groups(q) {
            Some(groups) => groups,
            None => {
                return Some(Results::empty(
                    self.instance_id,
                    self.index_gen,
                    q.clone(),
                    opts.include_hidden,
                ));
            }
        };

        let mut ordered_terms: Vec<&str> = q.name_terms.iter().map(String::as_str).collect();
        ordered_terms.sort_by_key(|a| std::cmp::Reverse(a.len()));
        let finders: Vec<Finder<'_>> = ordered_terms
            .iter()
            .map(|t| Finder::new(t.as_bytes()))
            .collect();
        let term_lens: Vec<usize> = ordered_terms.iter().map(|t| t.len()).collect();

        let path_precomp = if q.name_terms.is_empty() && !q.path_terms.is_empty() {
            Some(self.precompute_path_masks(&q.path_terms, cancel)?)
        } else {
            None
        };
        let lazy_path_checks: Vec<PreparedPathCheck<'_>> =
            if !q.name_terms.is_empty() && !q.path_terms.is_empty() {
                q.path_terms
                    .iter()
                    .map(|p| {
                        if p.contains('\\') {
                            PreparedPathCheck::WithSlash(p.as_str())
                        } else {
                            PreparedPathCheck::NoSlash(Box::new(Finder::new(p.as_bytes())))
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };

        let total_slots = self.parent.len();
        let num_chunks = total_slots.div_ceil(SCAN_CHUNK_SIZE);

        let collected = (0..num_chunks)
            .into_par_iter()
            .try_fold(WorkerAcc::new, |mut acc, chunk_idx| {
                if cancel.is_cancelled() {
                    return Err(());
                }
                let start = chunk_idx * SCAN_CHUNK_SIZE;
                let end = (start + SCAN_CHUNK_SIZE).min(total_slots);
                for raw_id in start..end {
                    let id = raw_id as EntryId;
                    let flags = self.flags[raw_id];
                    if (flags & FLAG_ALIVE) == 0 {
                        continue;
                    }
                    if !opts.include_hidden && (flags & FLAG_EFFECTIVE_HIDDEN) != 0 {
                        continue;
                    }
                    if let Some(kind) = q.kind {
                        let is_dir = (self.attrs[raw_id] & DIRECTORY) != 0;
                        if (kind == EntryKind::Dir) != is_dir {
                            continue;
                        }
                    }
                    if !self.matches_ext_groups(id, &ext_groups) {
                        continue;
                    }

                    if !finders.is_empty() {
                        let folded = self.folded_bytes(id);
                        let Some(first_pos) = finders[0].find(folded) else {
                            continue;
                        };
                        if !finders[1..].iter().all(|f| f.find(folded).is_some()) {
                            continue;
                        }
                        if !lazy_path_checks.is_empty()
                            && !self.matches_path_checks(id, &lazy_path_checks, &mut acc.path_buf)
                        {
                            continue;
                        }
                        acc.entries
                            .push(score_entry(self, id, &finders, &term_lens, first_pos));
                    } else {
                        if let Some(masks) = &path_precomp
                            && !self.matches_precomputed_path(id, masks)
                        {
                            continue;
                        }
                        acc.entries.push(make_filter_only_rank(self, id));
                    }
                }
                Ok(acc)
            })
            .try_reduce(WorkerAcc::new, |mut a, mut b| {
                if cancel.is_cancelled() {
                    return Err(());
                }
                if a.entries.len() < b.entries.len() {
                    std::mem::swap(&mut a.entries, &mut b.entries);
                }
                a.entries.extend_from_slice(&b.entries);
                Ok(a)
            });

        let mut ranked = match collected {
            Ok(acc) => acc.entries,
            Err(()) => return None,
        };
        if cancel.is_cancelled() {
            return None;
        }

        let total_matches = ranked.len();
        let all_matches: Vec<EntryId> = ranked.iter().map(|e| e.id).collect();
        let ids = rank_top_limit(self, &mut ranked, opts.limit);
        if cancel.is_cancelled() {
            return None;
        }

        Some(Results {
            ids,
            total_matches,
            instance_id: self.instance_id,
            generation: self.index_gen,
            query: q.clone(),
            include_hidden: opts.include_hidden,
            all_matches,
        })
    }

    /// Refines `prev` when `q` narrows `prev`'s query on an unchanged index generation.
    ///
    /// Falls back to a full [`Index::search`] whenever `prev` is stale or `q` is not a
    /// strict refinement of `prev`'s query. Returns `None` if cancelled via `cancel`.
    #[must_use]
    pub fn refine(
        &self,
        prev: &Results,
        q: &Query,
        opts: &SearchOptions,
        cancel: &CancelToken,
    ) -> Option<Results> {
        if cancel.is_cancelled() {
            return None;
        }
        if q.is_empty() || q.impossible {
            return Some(Results::empty(
                self.instance_id,
                self.index_gen,
                q.clone(),
                opts.include_hidden,
            ));
        }

        let can_narrow = prev.instance_id == self.instance_id
            && prev.generation == self.index_gen
            && (prev.include_hidden || !opts.include_hidden)
            && q.is_refinement_of(&prev.query);

        if !can_narrow {
            return self.search(q, opts, cancel);
        }

        let ext_groups = match self.prepare_ext_groups(q) {
            Some(groups) => groups,
            None => {
                return Some(Results::empty(
                    self.instance_id,
                    self.index_gen,
                    q.clone(),
                    opts.include_hidden,
                ));
            }
        };

        let mut ordered_terms: Vec<&str> = q.name_terms.iter().map(String::as_str).collect();
        ordered_terms.sort_by_key(|a| std::cmp::Reverse(a.len()));
        let finders: Vec<Finder<'_>> = ordered_terms
            .iter()
            .map(|t| Finder::new(t.as_bytes()))
            .collect();
        let term_lens: Vec<usize> = ordered_terms.iter().map(|t| t.len()).collect();
        let lazy_path_checks: Vec<PreparedPathCheck<'_>> = q
            .path_terms
            .iter()
            .map(|p| {
                if p.contains('\\') {
                    PreparedPathCheck::WithSlash(p.as_str())
                } else {
                    PreparedPathCheck::NoSlash(Box::new(Finder::new(p.as_bytes())))
                }
            })
            .collect();
        let filter_hidden = prev.include_hidden && !opts.include_hidden;

        let mut ranked = if prev.all_matches.len() <= 2048 {
            let mut acc = WorkerAcc::new();
            for &id in &prev.all_matches {
                if let Some(entry) = self.eval_refine_candidate(
                    id,
                    filter_hidden,
                    q,
                    &ext_groups,
                    &finders,
                    &term_lens,
                    &lazy_path_checks,
                    &mut acc.path_buf,
                ) {
                    acc.entries.push(entry);
                }
            }
            acc.entries
        } else {
            let collected = prev
                .all_matches
                .par_chunks(2048)
                .try_fold(WorkerAcc::new, |mut acc, chunk| {
                    if cancel.is_cancelled() {
                        return Err(());
                    }
                    for &id in chunk {
                        if let Some(entry) = self.eval_refine_candidate(
                            id,
                            filter_hidden,
                            q,
                            &ext_groups,
                            &finders,
                            &term_lens,
                            &lazy_path_checks,
                            &mut acc.path_buf,
                        ) {
                            acc.entries.push(entry);
                        }
                    }
                    Ok(acc)
                })
                .try_reduce(WorkerAcc::new, |mut a, mut b| {
                    if cancel.is_cancelled() {
                        return Err(());
                    }
                    if a.entries.len() < b.entries.len() {
                        std::mem::swap(&mut a.entries, &mut b.entries);
                    }
                    a.entries.extend_from_slice(&b.entries);
                    Ok(a)
                });
            match collected {
                Ok(acc) => acc.entries,
                Err(()) => return None,
            }
        };

        if cancel.is_cancelled() {
            return None;
        }

        let total_matches = ranked.len();
        let all_matches: Vec<EntryId> = ranked.iter().map(|e| e.id).collect();
        let ids = rank_top_limit(self, &mut ranked, opts.limit);
        if cancel.is_cancelled() {
            return None;
        }

        Some(Results {
            ids,
            total_matches,
            instance_id: self.instance_id,
            generation: self.index_gen,
            query: q.clone(),
            include_hidden: opts.include_hidden,
            all_matches,
        })
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn eval_refine_candidate(
        &self,
        id: EntryId,
        filter_hidden: bool,
        q: &Query,
        ext_groups: &[PreparedExtGroup<'_>],
        finders: &[Finder<'_>],
        term_lens: &[usize],
        path_checks: &[PreparedPathCheck<'_>],
        path_buf: &mut String,
    ) -> Option<RankEntry> {
        let raw_id = id as usize;
        if filter_hidden && (self.flags[raw_id] & FLAG_EFFECTIVE_HIDDEN) != 0 {
            return None;
        }
        if let Some(kind) = q.kind {
            let is_dir = (self.attrs[raw_id] & DIRECTORY) != 0;
            if (kind == EntryKind::Dir) != is_dir {
                return None;
            }
        }
        if !self.matches_ext_groups(id, ext_groups) {
            return None;
        }
        if !finders.is_empty() {
            let folded = self.folded_bytes(id);
            let first_pos = finders[0].find(folded)?;
            if !finders[1..].iter().all(|f| f.find(folded).is_some()) {
                return None;
            }
            if !path_checks.is_empty() && !self.matches_path_checks(id, path_checks, path_buf) {
                return None;
            }
            Some(score_entry(self, id, finders, term_lens, first_pos))
        } else {
            if !path_checks.is_empty() && !self.matches_path_checks(id, path_checks, path_buf) {
                return None;
            }
            Some(make_filter_only_rank(self, id))
        }
    }

    fn prepare_ext_groups<'a>(&self, q: &'a Query) -> Option<Vec<PreparedExtGroup<'a>>> {
        let mut prepared = Vec::with_capacity(q.ext_groups.len());
        for group in &q.ext_groups {
            let mut ids = Vec::with_capacity(group.len());
            for name in group {
                if let Some(eid) = self.find_ext_id(name) {
                    ids.push(eid);
                }
            }
            if ids.is_empty() && !self.ext_overflow {
                return None;
            }
            prepared.push(PreparedExtGroup {
                ids,
                names: group,
                check_overflow: self.ext_overflow,
            });
        }
        Some(prepared)
    }

    #[inline]
    fn matches_ext_groups(&self, id: EntryId, groups: &[PreparedExtGroup<'_>]) -> bool {
        if groups.is_empty() {
            return true;
        }
        let eid = self.ext_id[id as usize];
        for g in groups {
            if eid == EXT_OVERFLOW {
                if !g.check_overflow || (self.attrs[id as usize] & DIRECTORY) != 0 {
                    return false;
                }
                let Some(ext) = extract_ext(self.folded_str(id)) else {
                    return false;
                };
                if !g.names.iter().any(|n| n == ext) {
                    return false;
                }
            } else if !g.ids.contains(&eid) {
                return false;
            }
        }
        true
    }

    #[inline]
    fn matches_path_checks(
        &self,
        id: EntryId,
        checks: &[PreparedPathCheck<'_>],
        scratch: &mut String,
    ) -> bool {
        let mut assembled = false;
        for chk in checks {
            match chk {
                PreparedPathCheck::NoSlash(finder) => {
                    let mut cur = id;
                    let mut matched = false;
                    while cur != NONE {
                        if finder.find(self.folded_bytes(cur)).is_some() {
                            matched = true;
                            break;
                        }
                        cur = self.parent[cur as usize];
                    }
                    if !matched {
                        return false;
                    }
                }
                PreparedPathCheck::WithSlash(term) => {
                    if !assembled {
                        self.assemble_path_into(id, true, scratch);
                        assembled = true;
                    }
                    if !scratch.contains(term) {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn precompute_path_masks<'a>(
        &self,
        path_terms: &'a [String],
        cancel: &CancelToken,
    ) -> Option<Vec<PrecomputedPathTerm<'a>>> {
        let mut masks = Vec::with_capacity(path_terms.len());
        let n = self.parent.len();

        for term in path_terms {
            if cancel.is_cancelled() {
                return None;
            }
            if !term.contains('\\') {
                let finder = Finder::new(term.as_bytes());
                let mut dir_anc_match = vec![0u8; n];
                let mut stack = Vec::new();
                for i in 0..n {
                    if i & 0xFFF == 0 && cancel.is_cancelled() {
                        return None;
                    }
                    if dir_anc_match[i] != 0
                        || (self.flags[i] & FLAG_ALIVE) == 0
                        || ((self.attrs[i] & DIRECTORY) == 0 && self.first_child[i] == NONE)
                    {
                        continue;
                    }
                    let mut cur = i as EntryId;
                    while cur != NONE && dir_anc_match[cur as usize] == 0 {
                        stack.push(cur);
                        cur = self.parent[cur as usize];
                    }
                    let mut inherited = cur != NONE && dir_anc_match[cur as usize] == 2;
                    while let Some(node) = stack.pop() {
                        if !inherited && finder.find(self.folded_bytes(node)).is_some() {
                            inherited = true;
                        }
                        dir_anc_match[node as usize] = if inherited { 2 } else { 1 };
                    }
                }
                masks.push(PrecomputedPathTerm::Component {
                    finder: Box::new(finder),
                    dir_anc_match,
                });
            } else {
                let split_idx = term.rfind('\\').expect("contains backslash");
                let left_with_slash = &term[..=split_idx];
                let left = &term[..split_idx];
                let last_part = &term[split_idx + 1..];
                let mut dir_full_match = vec![false; n];
                let mut dir_ends_left = vec![false; n];

                let mut buf = String::new();
                let mut dfs_stack: Vec<(EntryId, usize, bool)> = Vec::new();
                for &rid in &self.roots {
                    if !self.is_alive(rid) {
                        continue;
                    }
                    buf.clear();
                    dfs_stack.push((rid, 0, false));
                    while let Some((node, prev_len, anc_matched)) = dfs_stack.pop() {
                        if cancel.is_cancelled() {
                            return None;
                        }
                        buf.truncate(prev_len);
                        if !buf.is_empty() && !buf.ends_with('\\') && !buf.ends_with('/') {
                            buf.push('\\');
                        }
                        buf.push_str(self.folded_str(node));

                        let ni = node as usize;
                        let matched = anc_matched || buf.contains(term);
                        dir_full_match[ni] = matched;
                        dir_ends_left[ni] = if buf.ends_with('\\') {
                            buf.ends_with(left_with_slash)
                        } else {
                            left.is_empty() || (!left.ends_with('\\') && buf.ends_with(left))
                        };

                        let saved_len = buf.len();
                        let mut child = self.first_child[ni];
                        while child != NONE {
                            let ci = child as usize;
                            if (self.attrs[ci] & DIRECTORY) != 0 || self.first_child[ci] != NONE {
                                dfs_stack.push((child, saved_len, matched));
                            }
                            child = self.next_sibling[ci];
                        }
                    }
                }

                masks.push(PrecomputedPathTerm::WithSlash {
                    full_term: term.as_str(),
                    last_part,
                    dir_full_match,
                    dir_ends_left,
                });
            }
        }
        Some(masks)
    }

    #[inline]
    fn matches_precomputed_path(&self, id: EntryId, masks: &[PrecomputedPathTerm<'_>]) -> bool {
        let p = self.parent[id as usize];
        for mask in masks {
            match mask {
                PrecomputedPathTerm::Component {
                    finder,
                    dir_anc_match,
                } => {
                    let anc = p != NONE && dir_anc_match[p as usize] == 2;
                    if !anc && finder.find(self.folded_bytes(id)).is_none() {
                        return false;
                    }
                }
                PrecomputedPathTerm::WithSlash {
                    full_term,
                    last_part,
                    dir_full_match,
                    dir_ends_left,
                } => {
                    if p == NONE {
                        if !self.folded_str(id).contains(full_term) {
                            return false;
                        }
                    } else {
                        let pi = p as usize;
                        let ok = dir_full_match[pi]
                            || (dir_ends_left[pi] && self.folded_str(id).starts_with(last_part));
                        if !ok {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }
}

enum PrecomputedPathTerm<'a> {
    Component {
        finder: Box<Finder<'a>>,
        dir_anc_match: Vec<u8>,
    },
    WithSlash {
        full_term: &'a str,
        last_part: &'a str,
        dir_full_match: Vec<bool>,
        dir_ends_left: Vec<bool>,
    },
}
