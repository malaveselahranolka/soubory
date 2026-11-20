//! Folder listing, Explorer-compatible sorting and localized formatting.
use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::HashMap,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::*,
        Globalization::*,
        Storage::FileSystem::*,
        System::{SystemInformation::GetLocalTime, Time::*},
        UI::Shell::*,
    },
    core::*,
};

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// FILETIME ticks (UTC).
    pub modified: u64,
    pub created: u64,
    pub attrs: u32,
    /// Full path for items outside the current folder (search results, Recents, drives).
    pub full: Option<PathBuf>,
}

impl Entry {
    pub fn ext(&self) -> &str {
        if self.is_dir {
            return "";
        }
        match self.name.rfind('.') {
            Some(i) if i > 0 => &self.name[i + 1..],
            _ => "",
        }
    }
    pub fn is_hidden(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_HIDDEN.0 != 0
    }
    /// OneDrive or other cloud file that is not on disk: never read its content.
    pub fn is_placeholder(&self) -> bool {
        fm_fs::attrs::is_cloud_placeholder(self.attrs)
    }
    pub fn path_in(&self, dir: &Path) -> PathBuf {
        self.full.clone().unwrap_or_else(|| dir.join(&self.name))
    }
}

/// An entry for one path from its attributes alone (never opens the file, so cloud placeholders
/// stay online-only). `None` when it no longer exists.
pub fn entry_of(p: &Path) -> Option<Entry> {
    use windows::Win32::Storage::FileSystem::{GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA};
    let w = wide(&p.to_string_lossy());
    let mut d = WIN32_FILE_ATTRIBUTE_DATA::default();
    unsafe {
        GetFileAttributesExW(
            windows::core::PCWSTR(w.as_ptr()),
            GetFileExInfoStandard,
            (&mut d as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
        )
        .ok()?;
    }
    let ft = |f: windows::Win32::Foundation::FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    Some(Entry {
        name: p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string_lossy().into_owned()),
        is_dir: d.dwFileAttributes & 0x10 != 0,
        size: ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64,
        modified: ft(d.ftLastWriteTime),
        created: ft(d.ftCreationTime),
        attrs: d.dwFileAttributes,
        full: Some(p.to_path_buf()),
    })
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    let s = p.to_string_lossy();
    // Long paths need the \\?\ prefix for the Find* APIs.
    if s.len() > 240 && !s.starts_with(r"\\?\") {
        if let Some(unc) = s.strip_prefix(r"\\") {
            return wide(&format!(r"\\?\UNC\{unc}"));
        }
        return wide(&format!(r"\\?\{s}"));
    }
    wide(&s)
}

fn from_wide(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Lists a folder without opening any file (safe for OneDrive placeholders).
pub fn list_dir(dir: &Path, show_hidden: bool) -> std::io::Result<Vec<Entry>> {
    let pattern = wide_path(&dir.join("*"));
    let mut out = Vec::new();
    unsafe {
        let mut fd = WIN32_FIND_DATAW::default();
        let h = FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            (&mut fd as *mut WIN32_FIND_DATAW).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        );
        let h = match h {
            Ok(h) => h,
            Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(out),
            Err(e) => {
                return Err(std::io::Error::from_raw_os_error(
                    (e.code().0 & 0xFFFF) as i32,
                ));
            }
        };
        loop {
            let name = from_wide(&fd.cFileName);
            if name != "." && name != ".." {
                let hidden = fd.dwFileAttributes & (FILE_ATTRIBUTE_HIDDEN.0) != 0;
                if show_hidden || !hidden {
                    out.push(Entry {
                        is_dir: fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
                        size: ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64,
                        modified: ft(fd.ftLastWriteTime),
                        created: ft(fd.ftCreationTime),
                        attrs: fd.dwFileAttributes,
                        name,
                        full: None,
                    });
                }
            }
            if FindNextFileW(h, &mut fd).is_err() {
                break;
            }
        }
        let _ = FindClose(h);
    }
    Ok(out)
}

fn ft(t: FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Modified,
    Created,
    Size,
    Kind,
}

/// Explorer's name order for the user's locale: case-insensitive, digits as numbers.
pub fn name_cmp(a: &str, b: &str) -> Ordering {
    let (wa, wb): (Vec<u16>, Vec<u16>) = (a.encode_utf16().collect(), b.encode_utf16().collect());
    let r = unsafe {
        CompareStringEx(
            PCWSTR::null(),
            NORM_IGNORECASE | SORT_DIGITSASNUMBERS,
            &wa,
            &wb,
            None,
            None,
            None,
        )
    };
    match r {
        COMPARESTRING_RESULT(1) => Ordering::Less,
        COMPARESTRING_RESULT(3) => Ordering::Greater,
        COMPARESTRING_RESULT(2) => a.cmp(b),
        _ => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

pub fn sort(entries: &mut [Entry], key: SortKey, ascending: bool, folders_first: bool) {
    entries.sort_by(|a, b| {
        if folders_first && a.is_dir != b.is_dir {
            return b.is_dir.cmp(&a.is_dir);
        }
        let primary = match key {
            SortKey::Name => Ordering::Equal,
            SortKey::Modified => a.modified.cmp(&b.modified),
            SortKey::Created => a.created.cmp(&b.created),
            SortKey::Size => {
                if a.is_dir && b.is_dir {
                    Ordering::Equal
                } else {
                    a.size.cmp(&b.size)
                }
            }
            SortKey::Kind => kind_name(a).cmp(&kind_name(b)),
        };
        let ord = primary.then_with(|| name_cmp(&a.name, &b.name));
        if ascending { ord } else { ord.reverse() }
    });
}

thread_local! {
    static KINDS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
    static DISPLAY: RefCell<HashMap<PathBuf, String>> = RefCell::new(HashMap::new());
}

/// Localized type name ("Dokument PDF", "Složka souborů"), cached per extension.
pub fn kind_name(e: &Entry) -> String {
    let key = if e.is_dir {
        "\u{1}dir".to_string()
    } else {
        e.ext().to_lowercase()
    };
    if let Some(k) = KINDS.with(|m| m.borrow().get(&key).cloned()) {
        return k;
    }
    let probe = if e.is_dir {
        "x".to_string()
    } else if key.is_empty() {
        "x".to_string()
    } else {
        format!("x.{key}")
    };
    let attrs = if e.is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let mut info = SHFILEINFOW::default();
    let w = wide(&probe);
    unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        );
    }
    let mut k = from_wide(&info.szTypeName);
    if k.is_empty() {
        k = if key.is_empty() {
            "Soubor".into()
        } else {
            format!("Soubor {}", key.to_uppercase())
        };
    }
    KINDS.with(|m| m.borrow_mut().insert(key, k.clone()));
    k
}

/// Explorer's display name for a folder ("Dokumenty" for ...\Documents), cached.
pub fn display_name(p: &Path) -> String {
    if let Some(n) = DISPLAY.with(|m| m.borrow().get(p).cloned()) {
        return n;
    }
    let mut info = SHFILEINFOW::default();
    let w = wide(&p.to_string_lossy());
    let ok = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_DISPLAYNAME,
        )
    };
    let mut n = from_wide(&info.szDisplayName);
    if ok == 0 || n.is_empty() {
        n = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string_lossy().into_owned());
    }
    DISPLAY.with(|m| m.borrow_mut().insert(p.to_path_buf(), n.clone()));
    n
}

/// "1,17 MB" in the user's locale (Explorer's own formatter).
pub fn format_size(bytes: u64) -> String {
    let mut buf = [0u16; 64];
    unsafe {
        if StrFormatByteSizeEx(bytes, SFBS_FLAGS_ROUND_TO_NEAREST_DISPLAYED_DIGIT, &mut buf).is_ok()
        {
            return from_wide(&buf);
        }
    }
    format!("{bytes} B")
}

fn local_time(t: u64) -> Option<SYSTEMTIME> {
    unsafe {
        let ft = FILETIME {
            dwLowDateTime: t as u32,
            dwHighDateTime: (t >> 32) as u32,
        };
        let mut utc = SYSTEMTIME::default();
        FileTimeToSystemTime(&ft, &mut utc).ok()?;
        let mut local = SYSTEMTIME::default();
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()?;
        Some(local)
    }
}

fn fmt_date(st: &SYSTEMTIME, flags: ENUM_DATE_FORMATS_FLAGS) -> String {
    let mut buf = [0u16; 80];
    let n = unsafe {
        GetDateFormatEx(
            PCWSTR::null(),
            flags,
            Some(st),
            PCWSTR::null(),
            Some(&mut buf),
            PCWSTR::null(),
        )
    };
    if n > 0 {
        from_wide(&buf)
    } else {
        String::new()
    }
}

fn fmt_time(st: &SYSTEMTIME) -> String {
    let mut buf = [0u16; 40];
    let n = unsafe {
        GetTimeFormatEx(
            PCWSTR::null(),
            TIME_NOSECONDS,
            Some(st),
            PCWSTR::null(),
            Some(&mut buf),
        )
    };
    if n > 0 {
        from_wide(&buf)
    } else {
        String::new()
    }
}

/// Finder-style relative date: "Dnes 9:41", "Včera 18:02", else the short date and time.
pub fn format_date(t: u64) -> String {
    if t == 0 {
        return String::new();
    }
    let Some(st) = local_time(t) else {
        return String::new();
    };
    let today = unsafe { GetLocalTime() };
    let day = |s: &SYSTEMTIME| (s.wYear as i32) * 400 + (s.wMonth as i32) * 32 + s.wDay as i32;
    let time = fmt_time(&st);
    if day(&st) == day(&today) {
        return format!("Dnes {time}");
    }
    // Yesterday: compare against today's date minus one day via FILETIME arithmetic.
    let mut ft_today = FILETIME::default();
    unsafe {
        let _ = SystemTimeToFileTime(&today, &mut ft_today);
    }
    let y = ft(ft_today).saturating_sub(24 * 3600 * 10_000_000);
    let y_st = unsafe {
        let f = FILETIME {
            dwLowDateTime: y as u32,
            dwHighDateTime: (y >> 32) as u32,
        };
        let mut s = SYSTEMTIME::default();
        let _ = FileTimeToSystemTime(&f, &mut s);
        s
    };
    if day(&st) == day(&y_st) {
        return format!("Včera {time}");
    }
    format!("{} {time}", fmt_date(&st, DATE_SHORTDATE))
}

/// Long date for Get Info / Quick Look: "pondělí 12. listopadu 2026 14:30".
pub fn format_date_long(t: u64) -> String {
    let Some(st) = local_time(t) else {
        return String::new();
    };
    format!("{} {}", fmt_date(&st, DATE_LONGDATE), fmt_time(&st))
}

pub struct Volume {
    pub root: PathBuf,
    pub label: String,
    pub removable: bool,
    pub network: bool,
    pub total: u64,
    pub free: u64,
}

/// Mounted drives with Explorer's names ("Místní disk (C:)").
pub fn volumes() -> Vec<Volume> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + i as u8) as char);
        let w = wide(&root);
        let kind = unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) };
        // GetDriveTypeW: 0 unknown, 1 no root, 2 removable, 3 fixed, 4 remote, 5 cdrom, 6 ramdisk.
        if kind <= 1 {
            continue;
        }
        let (total, free) = disk_space(Path::new(&root)).unwrap_or((0, 0));
        if kind == 5 && total == 0 {
            continue;
        }
        out.push(Volume {
            label: display_name(Path::new(&root)),
            removable: kind == 2 || kind == 5,
            network: kind == 4,
            root: PathBuf::from(root),
            total,
            free,
        });
    }
    out
}

pub fn disk_space(p: &Path) -> Option<(u64, u64)> {
    let w = wide(&p.to_string_lossy());
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(w.as_ptr()),
            Some(&mut avail),
            Some(&mut total),
            Some(&mut free),
        )
        .ok()?
    };
    Some((total, avail))
}

pub fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn onedrive() -> Option<PathBuf> {
    ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"]
        .iter()
        .filter_map(|v| std::env::var_os(v))
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

/// Parent of a folder; `None` at a drive root.
pub fn parent(p: &Path) -> Option<PathBuf> {
    p.parent()
        .map(Path::to_path_buf)
        .filter(|q| !q.as_os_str().is_empty())
}

/// Breadcrumb components from the root: (path, label).
pub fn crumbs(p: &Path) -> Vec<(PathBuf, String)> {
    let mut v: Vec<(PathBuf, String)> = p
        .ancestors()
        .map(|a| (a.to_path_buf(), display_name(a)))
        .collect();
    v.reverse();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, dir: bool, size: u64, m: u64) -> Entry {
        Entry {
            name: name.into(),
            is_dir: dir,
            size,
            modified: m,
            created: 0,
            attrs: 0,
            full: None,
        }
    }

    #[test]
    fn natural_name_order() {
        let mut v = vec![
            e("file10.txt", false, 0, 0),
            e("File2.txt", false, 0, 0),
            e("file1.txt", false, 0, 0),
            e("Ábc", false, 0, 0),
            e("abd", false, 0, 0),
        ];
        sort(&mut v, SortKey::Name, true, true);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names[..3], ["Ábc", "abd", "file1.txt"]);
        assert_eq!(names[3..], ["File2.txt", "file10.txt"]);
    }

    #[test]
    fn folders_first_and_descending() {
        let mut v = vec![e("b", false, 5, 1), e("a", true, 0, 2), e("c", false, 9, 3)];
        sort(&mut v, SortKey::Size, false, true);
        assert_eq!(
            v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["a", "c", "b"]
        );
    }

    #[test]
    fn lists_temp_dir_with_hidden_toggle() {
        let dir = std::env::temp_dir().join(format!("fm-list-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("h.txt"), b"").unwrap();
        let hw = wide(&dir.join("h.txt").to_string_lossy());
        unsafe { SetFileAttributesW(PCWSTR(hw.as_ptr()), FILE_ATTRIBUTE_HIDDEN).unwrap() };
        let v = list_dir(&dir, false).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v.iter().find(|e| e.name == "a.txt").unwrap().size, 5);
        assert!(v.iter().find(|e| e.name == "sub").unwrap().is_dir);
        assert_eq!(list_dir(&dir, true).unwrap().len(), 3);
        unsafe { SetFileAttributesW(PCWSTR(hw.as_ptr()), FILE_ATTRIBUTE_NORMAL).unwrap() };
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn size_and_date_format() {
        assert!(format_size(1_234_567).contains("MB"));
        assert!(format_date(0).is_empty());
    }
}
