//! Deterministic seeded synthetic directory tree generator (Czech + English realistic names).

use std::fmt::Write as _;

use fm_fs::attrs::{ARCHIVE, DIRECTORY, HIDDEN};
use fm_fs::fold_into;

use crate::arena::{EntryId, Index, Meta};

/// Simple deterministic 64-bit xorshift PRNG (no external `rand` dependency).
#[derive(Debug, Clone)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    /// Creates a generator from `seed` (mapping `0` to a fixed non-zero constant).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    /// Returns the next pseudo-random `u64`.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Returns a value in `0..bound`.
    #[inline]
    pub fn gen_range(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }
}

const CZECH_WORDS: &[&str] = &[
    "Smlouva",
    "Faktura",
    "Poznámky",
    "Příloha",
    "Výkaz",
    "Rozpočet",
    "Žádost",
    "Zápis",
    "Řízení",
    "Článek",
    "Účet",
    "Šablona",
    "Přehled",
    "Žluťoučký",
    "Kůň",
    "Píseň",
    "Projekt",
    "Úkol",
    "Záloha",
    "Ceník",
    "Dovolená",
    "Výlet",
    "Seznam",
    "Návrh",
    "Dokument",
    "Složka",
    "Archiv",
    "Obrázky",
    "Škola",
    "Práce",
];

const ENGLISH_WORDS: &[&str] = &[
    "Report", "Invoice", "Budget", "Meeting", "Notes", "Summary", "Config", "Settings", "Index",
    "Search", "Engine", "Client", "Server", "Database", "Backup", "Archive", "Draft", "Final",
    "Review", "Design", "Spec", "Readme", "Build", "Utils", "Helper", "Manager", "Service",
    "Account", "Photo", "Video",
];

const LOWER_NAMES: &[&str] = &[
    "readme", "config", "notes", "budget", "report", "invoice", "index", "search", "client",
    "server", "main", "utils", "cache", "schema", "backup", "smlouva", "faktura", "zadost",
    "prehled", "zaloha", "seznam", "archiv",
];

const CAMEL_NAMES: &[&str] = &[
    "searchEngine",
    "userProfile",
    "fileManager",
    "dataExport",
    "cacheLoader",
    "queryParser",
    "indexSnapshot",
    "cloudSync",
];

const DIR_STEMS: &[&str] = &[
    "Projekty",
    "Dokumenty",
    "Stahování",
    "Obrázky",
    "Práce",
    "Archiv",
    "Škola",
    "Vývoj",
    "Clients",
    "Reports",
    "Assets",
    "Backup",
    "Modules",
    "Tests",
    "Logs",
    "Media",
];

const EXTS: &[&str] = &[
    "pdf", "docx", "xlsx", "jpg", "png", "txt", "rs", "mp4", "zip", "csv", "json", "md", "log",
    "toml",
];

/// Generates a deterministic realistic tree with `n` live entries (including root `"C:\\"`).
///
/// Features Czech + English names with diacritics, camelCase, extensions (`pdf`, `docx`,
/// `xlsx`, `jpg`, `png`, `txt`, `rs`, `mp4`, `zip`, ...), depth up to 12, ~10% directories,
/// and ~2% hidden entries.
#[must_use]
pub fn generate(n: usize, seed: u64) -> Index {
    if n == 0 {
        return Index::new();
    }
    let mut rng = XorShift64::new(seed);
    let mut idx = Index::with_capacity(n);
    let root = idx.add_root("C:\\");
    if n == 1 {
        return idx;
    }

    let mut dirs: Vec<(EntryId, u8)> = Vec::with_capacity(n / 8 + 16);
    dirs.push((root, 0));

    let mut name_buf = String::with_capacity(64);
    let mut folded_buf = String::with_capacity(64);
    let base_mtime: i64 = 133_500_000_000_000_000;

    let mut spine_parent = root;
    let spine_depth = if n >= 50 { 11 } else { 0 };
    for d in 1..=spine_depth {
        if idx.len() >= n {
            break;
        }
        name_buf.clear();
        let stem = DIR_STEMS[(d + rng.gen_range(DIR_STEMS.len())) % DIR_STEMS.len()];
        let _ = write!(name_buf, "{stem}_{d}");
        folded_buf.clear();
        fold_into(&name_buf, &mut folded_buf);
        let mtime = base_mtime + (rng.next_u64() % 100_000_000) as i64;
        let id = idx.insert_with_folded(
            spine_parent,
            &name_buf,
            &folded_buf,
            Meta {
                attrs: DIRECTORY,
                size: 0,
                mtime,
                file_id: 0,
            },
        );
        dirs.push((id, d as u8));
        spine_parent = id;
    }

    while idx.len() < n {
        let (parent, parent_depth) = dirs[rng.gen_range(dirs.len())];
        let want_dir = parent_depth < 12 && rng.next_u64().is_multiple_of(10);
        let hidden = rng.next_u64().is_multiple_of(50);
        let mtime = base_mtime + (rng.next_u64() % 500_000_000) as i64;

        let mut collision_seq = 0usize;
        loop {
            name_buf.clear();
            if want_dir {
                format_dir_name(&mut rng, collision_seq, &mut name_buf);
            } else {
                format_file_name(&mut rng, collision_seq, &mut name_buf);
            }
            collision_seq += 1;

            folded_buf.clear();
            fold_into(&name_buf, &mut folded_buf);
            if idx
                .find_child(parent, folded_buf.as_bytes(), &name_buf)
                .is_none()
            {
                break;
            }
        }

        let meta = if want_dir {
            Meta {
                attrs: DIRECTORY | if hidden { HIDDEN } else { 0 },
                size: 0,
                mtime,
                file_id: 0,
            }
        } else {
            let size = 64 + (rng.next_u64() % 10_000_000);
            Meta {
                attrs: ARCHIVE | if hidden { HIDDEN } else { 0 },
                size,
                mtime,
                file_id: 0,
            }
        };

        let id = idx.insert_with_folded(parent, &name_buf, &folded_buf, meta);
        if want_dir {
            dirs.push((id, parent_depth + 1));
        }
    }

    idx
}

fn format_dir_name(rng: &mut XorShift64, collision: usize, out: &mut String) {
    let stem = DIR_STEMS[rng.gen_range(DIR_STEMS.len())];
    let tag = 1 + rng.gen_range(99);
    if collision == 0 {
        match rng.next_u64() % 4 {
            0 => out.push_str(stem),
            1 => {
                let _ = write!(out, "{stem}_{tag}");
            }
            2 => {
                let _ = write!(out, "{stem}-v{tag}");
            }
            _ => {
                let _ = write!(out, "{stem} {tag}");
            }
        }
    } else {
        let _ = write!(out, "{stem}_{tag}_{collision}");
    }
}

fn format_file_name(rng: &mut XorShift64, collision: usize, out: &mut String) {
    let style = rng.next_u64() % 10;
    let ext = EXTS[rng.gen_range(EXTS.len())];
    let tag = 1 + rng.gen_range(999);
    match style {
        0..=2 => {
            let w1 = CZECH_WORDS[rng.gen_range(CZECH_WORDS.len())];
            let sep = match rng.next_u64() % 3 {
                0 => "_",
                1 => "-",
                _ => " ",
            };
            if collision == 0 {
                let _ = write!(out, "{w1}{sep}{tag}.{ext}");
            } else {
                let _ = write!(out, "{w1}{sep}{tag}_{collision}.{ext}");
            }
        }
        3..=5 => {
            let w1 = LOWER_NAMES[rng.gen_range(LOWER_NAMES.len())];
            if collision == 0 {
                let _ = write!(out, "{w1}_{tag}.{ext}");
            } else {
                let _ = write!(out, "{w1}_{tag}_{collision}.{ext}");
            }
        }
        6..=7 => {
            let w1 = ENGLISH_WORDS[rng.gen_range(ENGLISH_WORDS.len())];
            let w2 = ENGLISH_WORDS[rng.gen_range(ENGLISH_WORDS.len())];
            if collision == 0 {
                let _ = write!(out, "{w1}_{w2}_{tag}.{ext}");
            } else {
                let _ = write!(out, "{w1}_{w2}_{tag}_{collision}.{ext}");
            }
        }
        8 => {
            let camel = CAMEL_NAMES[rng.gen_range(CAMEL_NAMES.len())];
            if collision == 0 {
                let _ = write!(out, "{camel}_{tag}.{ext}");
            } else {
                let _ = write!(out, "{camel}_{tag}_{collision}.{ext}");
            }
        }
        _ => {
            let w1 = CZECH_WORDS[rng.gen_range(CZECH_WORDS.len())];
            let w2 = ENGLISH_WORDS[rng.gen_range(ENGLISH_WORDS.len())];
            if collision == 0 {
                let _ = write!(out, "{w1} {w2}.{ext}");
            } else {
                let _ = write!(out, "{w1} {w2}_{collision}.{ext}");
            }
        }
    }
}

/// Generates `count` realistic 1–3-term search queries matching vocabulary from [`generate`].
#[must_use]
pub fn sample_queries(seed: u64, count: usize) -> Vec<String> {
    let mut rng = XorShift64::new(seed);
    let mut out = Vec::with_capacity(count);
    let folded_cz = [
        "smlouva",
        "faktura",
        "poznamky",
        "priloha",
        "vykaz",
        "rozpocet",
        "zadost",
        "zapis",
        "rizeni",
        "clanek",
        "ucet",
        "sablona",
        "prehled",
        "zlutoucky",
        "kun",
        "pisen",
        "zaloha",
        "cenik",
        "dovolena",
        "vylet",
    ];
    for i in 0..count {
        let q = match i % 8 {
            0 => folded_cz[rng.gen_range(folded_cz.len())].to_owned(),
            1 => {
                let w = ENGLISH_WORDS[rng.gen_range(ENGLISH_WORDS.len())].to_ascii_lowercase();
                let ext = EXTS[rng.gen_range(EXTS.len())];
                format!("{w} ext:{ext}")
            }
            2 => {
                let c = folded_cz[rng.gen_range(folded_cz.len())];
                let e = ENGLISH_WORDS[rng.gen_range(ENGLISH_WORDS.len())].to_ascii_lowercase();
                format!("{c} {e}")
            }
            3 => {
                let e = ENGLISH_WORDS[rng.gen_range(ENGLISH_WORDS.len())].to_ascii_lowercase();
                let d = DIR_STEMS[rng.gen_range(DIR_STEMS.len())].to_ascii_lowercase();
                format!("{e} path:{d}")
            }
            4 => {
                let d = DIR_STEMS[rng.gen_range(DIR_STEMS.len())].to_ascii_lowercase();
                format!("folder:{d}")
            }
            5 => {
                let c = folded_cz[rng.gen_range(folded_cz.len())];
                let ext1 = EXTS[rng.gen_range(EXTS.len())];
                let ext2 = EXTS[rng.gen_range(EXTS.len())];
                format!("file:{c} ext:{ext1};{ext2}")
            }
            6 => {
                let camel = CAMEL_NAMES[rng.gen_range(CAMEL_NAMES.len())];
                camel.to_ascii_lowercase()
            }
            _ => {
                let w1 = CZECH_WORDS[rng.gen_range(CZECH_WORDS.len())];
                let num = 1 + rng.gen_range(99);
                format!("{w1} {num}")
            }
        };
        out.push(q);
    }
    out
}
