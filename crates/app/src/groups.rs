//! Group by (Explorer's "Seskupit podle", Finder's "Použít skupiny"). The entries of one group are
//! contiguous in the tab's list, in the tab's sort order, and each group gets a header in the
//! icon and list views. This module is the pure part: buckets, ordering and the vertical layout.
use crate::fsys::{self, Entry, SortKey};
use std::cmp::Ordering;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GroupKey {
    #[default]
    None,
    Name,
    Kind,
    Modified,
    Created,
    Size,
    Tags,
}

/// Menu order: key, menu label, prefs value, palette label.
pub const KEYS: [(GroupKey, &str, &str, &str); 7] = [
    (GroupKey::None, "Žádné", "none", "Neseskupovat"),
    (GroupKey::Name, "Název", "name", "Seskupit podle názvu"),
    (GroupKey::Kind, "Druh", "kind", "Seskupit podle druhu"),
    (GroupKey::Modified, "Datum změny", "modified", "Seskupit podle data změny"),
    (GroupKey::Created, "Datum vytvoření", "created", "Seskupit podle data vytvoření"),
    (GroupKey::Size, "Velikost", "size", "Seskupit podle velikosti"),
    (GroupKey::Tags, "Štítky", "tags", "Seskupit podle štítků"),
];

impl GroupKey {
    pub fn pref(self) -> &'static str {
        KEYS.iter().find(|k| k.0 == self).map_or("none", |k| k.2)
    }
    pub fn from_pref(s: &str) -> GroupKey {
        KEYS.iter().find(|k| k.2 == s).map(|k| k.0).unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub label: String,
    pub start: usize,
    pub len: usize,
}

/// One entry's group: `pin` -1/1 keeps a group first/last whatever the direction ("Složky",
/// "Bez data"); otherwise groups are ordered by `rank`.
#[derive(Clone, Debug, PartialEq)]
struct Bucket {
    pin: i8,
    rank: i64,
    label: String,
}

fn bucket(pin: i8, rank: i64, label: impl Into<String>) -> Bucket {
    Bucket { pin, rank, label: label.into() }
}

/// A local calendar day: (day number, year, month).
pub type Day = (i64, u16, u16);

pub struct Ctx {
    pub key: GroupKey,
    pub sort: SortKey,
    pub asc: bool,
    /// The list is in its own order (search relevance, Recents), not sorted by `sort`.
    pub ranked: bool,
    pub folders_first: bool,
    pub today: Day,
}

/// Groups a tab's freshly loaded entries (icon and list views only). Call it before the tab's
/// selection and icon cache are rebuilt for the new order.
pub fn apply(t: &mut crate::app::Tab, folders_first: bool) {
    use crate::app::{Loc, View};
    t.groups.clear();
    // Drives in "Tento počítač" are not grouped (all would share one kind).
    if t.group == GroupKey::None || !matches!(t.view, View::Icons | View::List) || matches!(t.loc, Loc::Computer) {
        return;
    }
    let ranked = matches!(t.loc, Loc::Recents) || (matches!(t.loc, Loc::Search { .. }) && t.by_rank);
    let ctx = Ctx { key: t.group, sort: t.sort, asc: t.asc, ranked, folders_first, today: today() };
    let dir = t.dir();
    let when = |ft: u64| fsys::local_time(ft).map(|st| day(&st));
    let mask = |e: &Entry| crate::tags::mask(&e.path_in(&dir));
    t.groups = arrange(&mut t.entries, &ctx, &when, &mask);
}

fn day(st: &windows::Win32::Foundation::SYSTEMTIME) -> Day {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    let date = SYSTEMTIME { wYear: st.wYear, wMonth: st.wMonth, wDay: st.wDay, ..Default::default() };
    let mut ft = FILETIME::default();
    let ok = unsafe { windows::Win32::System::Time::SystemTimeToFileTime(&date, &mut ft) }.is_ok();
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    (if ok { (ticks / 864_000_000_000) as i64 } else { 0 }, st.wYear, st.wMonth)
}

fn today() -> Day {
    day(&unsafe { windows::Win32::System::SystemInformation::GetLocalTime() })
}

const MONTHS: [&str; 12] = [
    "Leden", "Únor", "Březen", "Duben", "Květen", "Červen", "Červenec", "Srpen", "Září", "Říjen", "Listopad", "Prosinec",
];

/// Finder's date groups: Dnes, Včera, Předchozích 7 dní, Předchozích 30 dní, then the months of
/// this year, then years. Higher rank = newer.
fn date_bucket(when: Option<Day>, today: Day) -> Bucket {
    let Some((day, year, month)) = when else { return bucket(1, 0, "Bez data") };
    let ago = today.0 - day;
    const RECENT: i64 = 10_000_000;
    match ago {
        i64::MIN..=0 => bucket(0, RECENT + 4, "Dnes"),
        1 => bucket(0, RECENT + 3, "Včera"),
        2..=7 => bucket(0, RECENT + 2, "Předchozích 7 dní"),
        8..=30 => bucket(0, RECENT + 1, "Předchozích 30 dní"),
        _ if year == today.1 => bucket(0, year as i64 * 100 + month as i64, MONTHS[(month.clamp(1, 12) - 1) as usize]),
        _ => bucket(0, year as i64 * 100, year.to_string()),
    }
}

/// Explorer's size classes.
fn size_bucket(e: &Entry, folders_first: bool) -> Bucket {
    if e.is_dir {
        return bucket(if folders_first { -1 } else { 1 }, 0, "Složky");
    }
    const K: u64 = 1024;
    let (rank, label) = match e.size {
        0 => (0, "Prázdné"),
        s if s < 16 * K => (1, "Drobné (do 16 kB)"),
        s if s < K * K => (2, "Malé (16 kB – 1 MB)"),
        s if s < 128 * K * K => (3, "Střední (1–128 MB)"),
        s if s < K * K * K => (4, "Velké (128 MB – 1 GB)"),
        s if s < 4 * K * K * K => (5, "Obrovské (1–4 GB)"),
        _ => (6, "Gigantické (nad 4 GB)"),
    };
    bucket(0, rank, label)
}

/// Explorer's name ranges; Č and Ž go with C and Z.
fn name_bucket(name: &str) -> Bucket {
    let first = name.chars().next().map(|c| fm_fs::fold(&c.to_string()).chars().next().unwrap_or(c));
    match first {
        Some('0'..='9') => bucket(0, 0, "0–9"),
        Some('a'..='h') => bucket(0, 1, "A–H"),
        Some('i'..='p') => bucket(0, 2, "I–P"),
        Some('q'..='z') => bucket(0, 3, "Q–Z"),
        _ => bucket(1, 0, "Ostatní"),
    }
}

fn tag_bucket(mask: u8) -> Bucket {
    match (0..7).find(|i| mask & (1 << i) != 0) {
        Some(i) => bucket(0, i as i64, crate::theme::TAGS[i].1),
        None => bucket(1, 0, "Bez štítku"),
    }
}

/// Reorders `entries` (already in the tab's sort order) so each group is contiguous, keeping the
/// order inside a group, and returns the groups in display order. `when` gives an entry's local
/// day for the date keys, `mask` its tags.
pub fn arrange(
    entries: &mut Vec<Entry>,
    ctx: &Ctx,
    when: &dyn Fn(u64) -> Option<Day>,
    mask: &dyn Fn(&Entry) -> u8,
) -> Vec<Group> {
    if ctx.key == GroupKey::None || entries.is_empty() {
        return Vec::new();
    }
    let mut b: Vec<Bucket> = entries
        .iter()
        .map(|e| match ctx.key {
            GroupKey::Name => name_bucket(&e.name),
            GroupKey::Kind if e.is_dir && ctx.folders_first => bucket(-1, 0, fsys::kind_name(e)),
            GroupKey::Kind => bucket(0, 0, fsys::kind_name(e)),
            GroupKey::Modified => date_bucket((e.modified != 0).then(|| when(e.modified)).flatten(), ctx.today),
            GroupKey::Created => date_bucket((e.created != 0).then(|| when(e.created)).flatten(), ctx.today),
            GroupKey::Size => size_bucket(e, ctx.folders_first),
            GroupKey::Tags => tag_bucket(mask(e)),
            GroupKey::None => unreachable!(),
        })
        .collect();
    if ctx.key == GroupKey::Kind {
        // Kinds rank by their names in the user's collation; each distinct kind is collated
        // once (a big folder has many files but few kinds). Equal-collating names still get
        // distinct ranks, so two labels never interleave.
        let distinct: std::collections::HashSet<&str> = b.iter().filter(|x| x.pin == 0).map(|x| x.label.as_str()).collect();
        let mut names: Vec<&str> = distinct.into_iter().collect();
        names.sort_by(|a, b| fsys::name_cmp(a, b).then_with(|| a.cmp(b)));
        let rank: std::collections::HashMap<String, i64> = names.iter().enumerate().map(|(i, n)| (n.to_string(), i as i64)).collect();
        for x in b.iter_mut().filter(|x| x.pin == 0) {
            x.rank = rank[&x.label];
        }
    }
    // Each key's natural direction is its sort's default (dates and sizes newest/largest first);
    // sorting by the same column flips it.
    let (same, natural_desc) = match ctx.key {
        _ if ctx.ranked => (false, matches!(ctx.key, GroupKey::Modified | GroupKey::Created | GroupKey::Size)),
        GroupKey::Name => (ctx.sort == SortKey::Name, false),
        GroupKey::Kind => (ctx.sort == SortKey::Kind, false),
        GroupKey::Modified => (ctx.sort == SortKey::Modified, true),
        GroupKey::Created => (ctx.sort == SortKey::Created, true),
        GroupKey::Size => (ctx.sort == SortKey::Size, true),
        GroupKey::Tags | GroupKey::None => (false, false),
    };
    let desc = if same { !ctx.asc } else { natural_desc };
    let order = |x: &Bucket, y: &Bucket| -> Ordering {
        x.pin.cmp(&y.pin).then_with(|| if desc { y.rank.cmp(&x.rank) } else { x.rank.cmp(&y.rank) })
    };
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by(|&i, &j| order(&b[i], &b[j])); // stable: the sort order stays inside a group
    let mut taken: Vec<Option<Entry>> = std::mem::take(entries).into_iter().map(Some).collect();
    let mut groups: Vec<Group> = Vec::new();
    for (pos, &i) in idx.iter().enumerate() {
        entries.push(taken[i].take().expect("each index once"));
        let label = std::mem::take(&mut b[i].label);
        match groups.last_mut() {
            Some(g) if g.label == label => g.len += 1,
            _ => groups.push(Group { label, start: pos, len: 1 }),
        }
    }
    groups
}

// ---- vertical layout ----------------------------------------------------------------------------------

/// Where a content-space point falls.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Spot {
    Header(usize),
    Item(usize),
    Nothing,
}

/// Layout of `n` items in rows of `cols`, each group starting a new row under a header strip.
/// Without groups it is one plain grid (no header). Content y starts at 0.
pub struct Geo<'a> {
    pub groups: &'a [Group],
    pub n: usize,
    pub cols: usize,
    pub item_h: f32,
    pub header_h: f32,
}

impl Geo<'_> {
    fn spans(&self) -> usize {
        if self.groups.is_empty() { 1 } else { self.groups.len() }
    }
    /// (start, len, header height) of span `g`.
    fn span(&self, g: usize) -> (usize, usize, f32) {
        match self.groups.get(g) {
            Some(x) => (x.start, x.len, self.header_h),
            None => (0, self.n, 0.0),
        }
    }
    fn block(&self, g: usize) -> f32 {
        let (_, len, hh) = self.span(g);
        hh + len.div_ceil(self.cols.max(1)) as f32 * self.item_h
    }
    /// Content y of span `g`'s header (its first row when there are no groups).
    pub fn top(&self, g: usize) -> f32 {
        (0..g.min(self.spans())).map(|k| self.block(k)).sum()
    }
    pub fn height(&self) -> f32 {
        self.top(self.spans())
    }
    pub fn group_of(&self, i: usize) -> usize {
        self.groups.partition_point(|g| g.start <= i).saturating_sub(1)
    }
    /// Content y of item `i`'s row, and its column.
    pub fn pos(&self, i: usize) -> (f32, usize) {
        let g = self.group_of(i);
        let (start, _, hh) = self.span(g);
        let local = i.saturating_sub(start);
        let cols = self.cols.max(1);
        (self.top(g) + hh + (local / cols) as f32 * self.item_h, local % cols)
    }
    pub fn at(&self, y: f32, col: usize) -> Spot {
        if y < 0.0 {
            return Spot::Nothing;
        }
        let cols = self.cols.max(1);
        let mut t = 0.0;
        for g in 0..self.spans() {
            let b = self.block(g);
            if y < t + b {
                let (start, len, hh) = self.span(g);
                if y < t + hh {
                    return Spot::Header(g);
                }
                let local = ((y - t - hh) / self.item_h) as usize * cols + col;
                return if col < cols && local < len { Spot::Item(start + local) } else { Spot::Nothing };
            }
            t += b;
        }
        Spot::Nothing
    }
    /// The header that sticks to the top while its group scrolls under it: (group, content y)
    /// for a viewport whose top edge is at content y `view_top`.
    pub fn pinned(&self, view_top: f32) -> Option<(usize, f32)> {
        if self.groups.is_empty() || view_top <= 0.0 {
            return None;
        }
        let mut t = 0.0;
        for g in 0..self.groups.len() {
            let next = t + self.block(g);
            if view_top < next {
                return (view_top > t).then(|| (g, view_top.min(next - self.header_h)));
            }
            t = next;
        }
        None
    }
    /// Up/Down in a grouped grid: the item above or below in the same column; a group's short
    /// last row and the next group's first row are neighbours. Stays put at either end.
    pub fn vertical(&self, i: usize, down: bool) -> usize {
        let cols = self.cols.max(1);
        let g = self.group_of(i);
        let (start, len, _) = self.span(g);
        let (row, col) = ((i - start) / cols, (i - start) % cols);
        let rows = len.div_ceil(cols);
        if down {
            if row + 1 < rows {
                return start + ((row + 1) * cols + col).min(len - 1);
            }
            if g + 1 < self.spans() {
                let (s, l, _) = self.span(g + 1);
                return s + col.min(l - 1);
            }
        } else {
            if row > 0 {
                return start + (row - 1) * cols + col;
            }
            if g > 0 {
                let (s, l, _) = self.span(g - 1);
                let last = l.div_ceil(cols) - 1;
                return s + (last * cols + col).min(l - 1);
            }
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, size: u64, modified: u64) -> Entry {
        Entry {
            name: name.into(),
            is_dir: false,
            size,
            modified,
            created: 0,
            attrs: 0,
            full: None,
            shell: None,
            origin: None,
        }
    }

    fn ctx(key: GroupKey) -> Ctx {
        Ctx { key, sort: SortKey::Name, asc: true, ranked: false, folders_first: true, today: (1000, 2026, 11) }
    }

    fn labels(g: &[Group]) -> Vec<(&str, usize, usize)> {
        g.iter().map(|g| (g.label.as_str(), g.start, g.len)).collect()
    }

    #[test]
    fn dates_bucket_like_finder() {
        let t = (1000, 2026, 11);
        assert_eq!(date_bucket(Some((1001, 2026, 11)), t).label, "Dnes"); // clock skew
        assert_eq!(date_bucket(Some((1000, 2026, 11)), t).label, "Dnes");
        assert_eq!(date_bucket(Some((999, 2026, 11)), t).label, "Včera");
        assert_eq!(date_bucket(Some((993, 2026, 11)), t).label, "Předchozích 7 dní");
        assert_eq!(date_bucket(Some((970, 2026, 10)), t).label, "Předchozích 30 dní");
        assert_eq!(date_bucket(Some((900, 2026, 8)), t).label, "Srpen");
        assert_eq!(date_bucket(Some((600, 2025, 10)), t).label, "2025");
        assert_eq!(date_bucket(None, t).label, "Bez data");
        // Newer is higher, months above years.
        assert!(date_bucket(Some((900, 2026, 8)), t).rank > date_bucket(Some((600, 2025, 12)), t).rank);
    }

    #[test]
    fn groups_are_contiguous_and_keep_the_sort_order() {
        // Sorted by name; sizes alternate between two classes.
        let mut v = vec![entry("a", 10, 0), entry("b", 2 << 20, 0), entry("c", 20, 0), entry("d", 3 << 20, 0)];
        let g = arrange(&mut v, &ctx(GroupKey::Size), &|_| None, &|_| 0);
        // Largest class first (the size sort's default direction).
        assert_eq!(labels(&g), vec![("Střední (1–128 MB)", 0, 2), ("Drobné (do 16 kB)", 2, 2)]);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["b", "d", "a", "c"]);
    }

    #[test]
    fn same_column_sort_flips_group_order_and_pins_stay() {
        let day = |t: u64| Some((t as i64, 2026u16, 11u16));
        let mut v = vec![entry("old", 0, 900), entry("new", 0, 1000), entry("none", 0, 0)];
        let mut c = ctx(GroupKey::Modified);
        let g = arrange(&mut v.clone(), &c, &day, &|_| 0);
        assert_eq!(g.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(), ["Dnes", "Listopad", "Bez data"]);
        c.sort = SortKey::Modified;
        c.asc = true;
        let g = arrange(&mut v, &c, &day, &|_| 0);
        assert_eq!(g.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(), ["Listopad", "Dnes", "Bez data"]);
    }

    #[test]
    fn names_fold_diacritics() {
        assert_eq!(name_bucket("Čokoláda").label, "A–H");
        assert_eq!(name_bucket("žába").label, "Q–Z");
        assert_eq!(name_bucket("2024 faktury").label, "0–9");
        assert_eq!(name_bucket("_tmp").label, "Ostatní");
        assert_eq!(name_bucket("").label, "Ostatní");
    }

    #[test]
    fn tags_use_the_first_tag() {
        let mut v = vec![entry("x", 0, 0), entry("y", 0, 0), entry("z", 0, 0)];
        let masks = |e: &Entry| match e.name.as_str() {
            "x" => 0b100,
            "y" => 0b110,
            _ => 0,
        };
        let g = arrange(&mut v, &ctx(GroupKey::Tags), &|_| None, &masks);
        let want = [crate::theme::TAGS[1].1, crate::theme::TAGS[2].1, "Bez štítku"];
        assert_eq!(g.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(), want);
        assert_eq!(v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["y", "x", "z"]);
    }

    fn groups_of(lens: &[usize]) -> Vec<Group> {
        let mut s = 0;
        lens.iter()
            .map(|&len| {
                let g = Group { label: String::new(), start: s, len };
                s += len;
                g
            })
            .collect()
    }

    /// Every item's own row maps back to it; headers map to their group.
    #[test]
    fn hit_round_trip() {
        for (lens, cols) in [(vec![3usize, 1, 5], 2usize), (vec![1], 1), (vec![4, 4], 4), (vec![7, 2, 9], 3)] {
            let groups = groups_of(&lens);
            let n: usize = lens.iter().sum();
            for plain in [false, true] {
                let geo = Geo { groups: if plain { &[] } else { &groups }, n, cols, item_h: 26.0, header_h: 32.0 };
                for i in 0..n {
                    let (y, c) = geo.pos(i);
                    assert_eq!(geo.at(y + 13.0, c), Spot::Item(i), "item {i} lens {lens:?} cols {cols} plain {plain}");
                }
                if !plain {
                    for g in 0..groups.len() {
                        assert_eq!(geo.at(geo.top(g) + 1.0, 0), Spot::Header(g));
                    }
                }
                assert_eq!(geo.at(geo.height() + 1.0, 0), Spot::Nothing);
                assert_eq!(geo.at(-1.0, 0), Spot::Nothing);
            }
        }
    }

    #[test]
    fn vertical_keeps_the_column_across_groups() {
        // cols 3: group 0 = [0 1 2][3 4], group 1 = [5 6 7][8], group 2 = [9]
        let groups = groups_of(&[5, 4, 1]);
        let geo = Geo { groups: &groups, n: 10, cols: 3, item_h: 80.0, header_h: 36.0 };
        assert_eq!(geo.vertical(1, true), 4);
        assert_eq!(geo.vertical(2, true), 4); // short last row: nearest item
        assert_eq!(geo.vertical(4, true), 6); // into the next group, same column
        assert_eq!(geo.vertical(3, true), 5);
        assert_eq!(geo.vertical(8, true), 9);
        assert_eq!(geo.vertical(9, true), 9); // stays at the end
        assert_eq!(geo.vertical(6, false), 4); // up into the previous group's last row
        assert_eq!(geo.vertical(7, false), 4);
        assert_eq!(geo.vertical(9, false), 8);
        assert_eq!(geo.vertical(0, false), 0);
    }

    #[test]
    fn pinned_header_slides_out_under_the_next() {
        let groups = groups_of(&[2, 2]);
        let geo = Geo { groups: &groups, n: 4, cols: 1, item_h: 10.0, header_h: 5.0 };
        // Blocks: g0 = 0..25, g1 = 25..50.
        assert_eq!(geo.pinned(0.0), None);
        assert_eq!(geo.pinned(3.0), Some((0, 3.0)));
        assert_eq!(geo.pinned(22.0), Some((0, 20.0))); // pushed up by g1's header
        assert_eq!(geo.pinned(25.0), None); // g1's header is in its own place
        assert_eq!(geo.pinned(30.0), Some((1, 30.0)));
    }
}
