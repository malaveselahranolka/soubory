//! Match-tier classification, camelCase/word-boundary detection, and partial-sort ranking.

use std::cmp::Ordering;

use crate::arena::{EntryId, Index};
use memchr::memmem::Finder;

/// Match quality tier for a single query term against an entry name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum TermTier {
    Exact = 0,
    Prefix = 1,
    WordBoundary = 2,
    Substring = 3,
}

/// Compact per-match ranking record stored in flat arrays for fast partial sorting.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RankEntry {
    pub(crate) tier: u8,
    pub(crate) name_chars: u16,
    pub(crate) id: EntryId,
    pub(crate) mtime: i64,
}

/// Computes the combined ranking tier (`best_tier * 4 + worst_tier`) and character count
/// for an already-matched entry whose first term matched at `first_pos`.
#[inline]
pub(crate) fn score_entry(
    idx: &Index,
    id: EntryId,
    finders: &[Finder<'_>],
    term_lens: &[usize],
    first_pos: usize,
) -> RankEntry {
    let folded = idx.folded_bytes(id);

    let t0 = classify_term(idx, id, folded, &finders[0], term_lens[0], first_pos);
    let mut best = t0 as u8;
    let mut worst = best;

    for (finder, &tlen) in finders[1..].iter().zip(&term_lens[1..]) {
        if let Some(pos) = finder.find(folded) {
            let t = classify_term(idx, id, folded, finder, tlen, pos) as u8;
            if t < best {
                best = t;
            }
            if t > worst {
                worst = t;
            }
        }
    }

    let name_bytes = idx.raw_display_bytes(id);
    let char_len = utf8_char_count(name_bytes);

    RankEntry {
        tier: best * 4 + worst,
        name_chars: char_len.min(u16::MAX as usize) as u16,
        id,
        mtime: idx.mtime[id as usize],
    }
}

#[inline]
pub(crate) fn make_filter_only_rank(idx: &Index, id: EntryId) -> RankEntry {
    let char_len = utf8_char_count(idx.raw_display_bytes(id));
    RankEntry {
        tier: 0,
        name_chars: char_len.min(u16::MAX as usize) as u16,
        id,
        mtime: idx.mtime[id as usize],
    }
}

#[inline]
fn utf8_char_count(bytes: &[u8]) -> usize {
    if bytes.is_ascii() {
        bytes.len()
    } else {
        bytes.iter().filter(|&&b| (b & 0xC0) != 0x80).count()
    }
}

#[inline]
fn classify_term(
    idx: &Index,
    id: EntryId,
    folded: &[u8],
    finder: &Finder<'_>,
    term_len: usize,
    first_pos: usize,
) -> TermTier {
    if first_pos == 0 {
        if folded.len() == term_len {
            return TermTier::Exact;
        }
        return TermTier::Prefix;
    }
    let has_display = idx.has_distinct_display(id);
    if is_sep_boundary(folded, first_pos) {
        return TermTier::WordBoundary;
    }
    if has_display && is_camelcase_boundary(folded, idx.display_str_unchecked(id), first_pos) {
        return TermTier::WordBoundary;
    }

    let mut offset = first_pos + 1;
    while offset + term_len <= folded.len() {
        if let Some(rel) = finder.find(&folded[offset..]) {
            let pos = offset + rel;
            if is_sep_boundary(folded, pos)
                || (has_display
                    && is_camelcase_boundary(folded, idx.display_str_unchecked(id), pos))
            {
                return TermTier::WordBoundary;
            }
            offset = pos + 1;
        } else {
            break;
        }
    }
    TermTier::Substring
}

#[inline]
fn is_sep_boundary(folded: &[u8], pos: usize) -> bool {
    pos == 0 || matches!(folded[pos - 1], b' ' | b'_' | b'-' | b'.')
}

fn is_camelcase_boundary(folded: &[u8], display: &str, folded_pos: usize) -> bool {
    if display.is_ascii() && display.len() == folded.len() {
        let bytes = display.as_bytes();
        let prev = bytes[folded_pos - 1];
        let cur = bytes[folded_pos];
        if prev.is_ascii_lowercase() && cur.is_ascii_uppercase() {
            return true;
        }
        if prev.is_ascii_uppercase()
            && cur.is_ascii_uppercase()
            && folded_pos + 1 < bytes.len()
            && bytes[folded_pos + 1].is_ascii_lowercase()
        {
            return true;
        }
        return false;
    }

    // Quick reject: if no char after byte 0 is uppercase, camelCase boundary is impossible.
    let mut chars_check = display.chars();
    let _ = chars_check.next();
    if !chars_check.any(char::is_uppercase) {
        return false;
    }

    let mut f_off = 0usize;
    let mut prev_ch: Option<char> = None;
    let mut iter = display.chars();
    while let Some(ch) = iter.next() {
        let c_len = fm_fs::fold::folded_char_byte_len(ch);
        if c_len == 0 {
            continue;
        }
        if f_off == folded_pos {
            if let Some(p) = prev_ch {
                if p.is_lowercase() && ch.is_uppercase() {
                    return true;
                }
                if p.is_uppercase()
                    && ch.is_uppercase()
                    && iter
                        .find(|&n| fm_fs::fold::folded_char_byte_len(n) > 0)
                        .is_some_and(char::is_lowercase)
                {
                    return true;
                }
            }
            return false;
        }
        if f_off > folded_pos {
            return false;
        }
        f_off += c_len;
        prev_ch = Some(ch);
    }
    false
}

/// Partially sorts `entries` so the top `limit` ranked entries come first in exact rank order.
pub(crate) fn rank_top_limit(idx: &Index, entries: &mut [RankEntry], limit: usize) -> Vec<EntryId> {
    if limit == 0 || entries.is_empty() {
        return Vec::new();
    }
    let mut path_a = String::new();
    let mut path_b = String::new();

    let mut cmp = |a: &RankEntry, b: &RankEntry| -> Ordering {
        a.tier
            .cmp(&b.tier)
            .then_with(|| a.name_chars.cmp(&b.name_chars))
            .then_with(|| b.mtime.cmp(&a.mtime))
            .then_with(|| compare_paths(idx, a.id, b.id, &mut path_a, &mut path_b))
    };

    let keep = limit.min(entries.len());
    if keep < entries.len() {
        entries.select_nth_unstable_by(keep, &mut cmp);
    }
    entries[..keep].sort_unstable_by(cmp);
    entries[..keep].iter().map(|e| e.id).collect()
}

fn compare_paths(
    idx: &Index,
    a: EntryId,
    b: EntryId,
    buf_a: &mut String,
    buf_b: &mut String,
) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    if idx.parent[a as usize] == idx.parent[b as usize] {
        return idx
            .display_str_unchecked(a)
            .cmp(idx.display_str_unchecked(b));
    }
    idx.assemble_path_into(a, false, buf_a);
    idx.assemble_path_into(b, false, buf_b);
    buf_a.as_str().cmp(buf_b.as_str())
}
