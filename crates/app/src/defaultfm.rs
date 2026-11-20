//! `fm --set-default` and `fm --restore-explorer`: folders, drives, Win+E, This PC, Recycle Bin
//! and Network open in fm instead of File Explorer for the current user, and back. A port of
//! scripts\set-default-fm.ps1 and scripts\restore-explorer.ps1: the same keys and the same backup
//! under HKCU\Software\fm\Backup (`verb:`, `default:`, `absent:`, `created:`), so each side
//! undoes the other's install. The scripts' headers describe the layout and list the emergency
//! `reg delete` lines. Writes only under HKCU\Software\Classes and HKCU\Software\fm.
//! PowerShell compares strings ignoring case (-eq, -like, -contains), so this does too.
use crate::fsys::wide;
use windows::Win32::{Foundation::*, System::Registry::*};
use windows::core::PCWSTR;

/// HKCU-relative keys: the real ones, or a sandbox under Software\fm-test* in tests.
pub struct Roots {
    pub classes: String,
    pub fm: String,
    pub backup: String,
}

const WIN_E: &str = "{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";
/// This PC, Recycle Bin, Network.
const VIRTUAL: [(&str, &str); 3] = [
    ("{20D04FE0-3AEA-1069-A2D8-08002B30309D}", "Tento počítač"),
    ("{645FF040-5081-101B-9F08-00AA002F954E}", "Koš"),
    ("{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", "Síť"),
];

/// Warnings, then the closing line.
type Report = (Vec<String>, String);

/// Prints the report to stdout (the installer logs it) or the error to stderr; exit code.
pub fn cli(set: bool) -> i32 {
    // An elevated fm would start a second, elevated resident (files opened as admin). Only the
    // elevated half of a split token is refused, as the scripts and the installer do.
    let r = if crate::win::is_elevated() {
        Err("Soubory nesmějí běžet jako správce. Spusťte je bez práv správce.".to_string())
    } else {
        let roots = Roots { classes: r"Software\Classes".into(), fm: r"Software\fm".into(), backup: r"Software\fm\Backup".into() };
        if set {
            std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| set_default(&roots, &exe.to_string_lossy()))
        } else {
            restore(&roots)
        }
    };
    match r {
        Ok((warn, said)) => {
            for w in warn {
                println!("Upozornění: {w}");
            }
            println!("{said}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

pub fn set_default(r: &Roots, exe: &str) -> Result<Report, String> {
    let cls = |rel: &str| format!(r"{}\{rel}", r.classes);
    // The previous version kept one all-or-nothing backup ("WinE.existed"); undo that install first
    // so its keys are not mistaken for another app's and "fm.open" is not saved as the original value.
    if get(&r.backup, "WinE.existed").is_some() {
        restore(r)?;
    }
    for k in [r"Directory\shell\fm.open", r"Drive\shell\fm.open"] {
        if exists(&cls(k)) {
            del_tree(&cls(k))?;
        }
    }
    let cmd = format!(r#""{exe}" "%1""#);
    let bare = format!(r#""{exe}""#);
    // (shell key, verb, command, sets the shell's default verb, name in the closing line)
    let mut entries = vec![
        (r"Directory\shell".to_string(), "open", cmd.clone(), true, "složky"),
        (r"Drive\shell".to_string(), "open", cmd.clone(), true, "disky"),
        (r"Folder\shell".to_string(), "explore", cmd, false, "složky otevírané z prohlížeče"),
        (format!(r"CLSID\{WIN_E}\shell"), "opennewwindow", bare.clone(), false, "Win+E"),
        (format!(r"CLSID\{WIN_E}\shell"), "open", bare, false, "Průzkumník souborů v nabídce Start"),
    ];
    // %1 is not reliable for virtual folders, so the target is fixed.
    for (g, name) in VIRTUAL {
        entries.push((format!(r"CLSID\{g}\shell"), "open", format!(r#""{exe}" "::{g}""#), true, name));
    }

    create(&r.backup)?;
    let (mut warn, mut skipped) = (vec![], vec![]);
    for (shell_rel, verb, cmd, sets_default, name) in &entries {
        let verb_rel = format!(r"{shell_rel}\{verb}");
        let (verb_key, shell) = (cls(&verb_rel), cls(shell_rel));
        let (rec, d, a) = (format!("verb:{verb_rel}"), format!("default:{shell_rel}"), format!("absent:{shell_rel}"));
        let saved = value_names(&r.backup);
        let has = |n: &str| saved.iter().any(|s| s.eq_ignore_ascii_case(n));
        if exists(&verb_key) && (!has(&rec) || !is_fm_verb(&verb_key, text(get(&r.backup, &rec)).as_deref())) {
            // Ours once and taken over since: forget it, so restore leaves it to that app too.
            for n in [&rec, &d, &a] {
                if has(n) {
                    del_value(&r.backup, n);
                }
            }
            warn.push(format!(r"Klíč HKCU\{verb_key} už vytvořila jiná aplikace; nechávám ho být."));
            skipped.push(*name);
            continue;
        }
        // Each change is recorded before it is made, so an interrupted run can still be restored.
        if !exists(&verb_key) {
            let first = first_missing(&r.classes, &verb_rel);
            if first != verb_rel && !in_created(&saved, &first) {
                set(&r.backup, &format!("created:{first}"), &sz(""))?;
            }
        }
        // Refreshed on every run, so a moved install stays ours.
        set(&r.backup, &rec, &sz(cmd))?;
        if *sets_default {
            let cur = get(&shell, "");
            let is = |s: &str| text(cur.clone()).is_some_and(|t| t.eq_ignore_ascii_case(s));
            // "fm.open" is a leftover of the previous version, not another app's value.
            match &cur {
                Some(v) if !is("open") && !is("fm.open") => {
                    // Set by another app since the backup (or before the first run): that is what restore brings back.
                    del_value(&r.backup, &a);
                    set(&r.backup, &d, v)?;
                }
                _ if has(&d) || has(&a) => {}
                Some(v) if is("open") => set(&r.backup, &d, v)?,
                _ => set(&r.backup, &a, &sz(""))?,
            }
        }
        let command = format!(r"{verb_key}\command");
        create(&command)?;
        set(&command, "", &sz(cmd))?;
        // Empty, so HKLM's DelegateExecute does not leak through the merged HKCR.
        set(&command, "DelegateExecute", &sz(""))?;
        if *sets_default {
            set(&shell, "", &sz("open"))?;
        }
    }
    create(&r.fm)?;
    set(&r.fm, "Redirect", &sz(exe))?;

    let said = if skipped.is_empty() {
        "Složky, disky, Win+E, Tento počítač, Koš a Síť se teď otevírají v Souborech. Zpět: fm --restore-explorer".to_string()
    } else {
        format!("Soubory jsou výchozí, ale v Průzkumníkovi zůstává: {} (už je převzala jiná aplikace). Zpět: fm --restore-explorer", skipped.join(", "))
    };
    Ok((warn, said))
}

/// Undoes what the backup records and leaves alone every place another app has taken over since:
/// our verbs while their command is still fm's, then the shells' previous defaults while the
/// default is still "open" (or deleted) and no other app's "open" verb is there, then each key
/// tree set_default created, bottom-up and only while empty. Then Redirect and the backup go.
pub fn restore(r: &Roots) -> Result<Report, String> {
    if !exists(&r.backup) {
        return Ok((vec![], "Soubory nebyly výchozí; není co vracet.".into()));
    }
    let cls = |rel: &str| format!(r"{}\{rel}", r.classes);
    let names = value_names(&r.backup);
    let with = |prefix: &str| -> Vec<(String, String)> {
        names.iter().filter_map(|n| strip_ci(n, prefix).map(|rel| (n.clone(), rel.to_string()))).collect()
    };
    let mut warn = vec![];
    if names.iter().any(|n| n.eq_ignore_ascii_case("WinE.existed")) {
        // Previous version: verb "fm.open" on Directory and Drive, and Win+E's opennewwindow.
        for k in [r"Directory\shell", r"Drive\shell"] {
            let shell = cls(k);
            let old = format!(r"{shell}\fm.open");
            if exists(&old) {
                del_tree(&old)?;
            }
            if let Some(v) = get(&r.backup, &k.replace('\\', ".")) {
                set(&shell, "", &v)?;
            } else if exists(&shell) {
                del_value(&shell, "");
                // That version did not record whether the key existed; drop it when nothing else lives there.
                remove_if_empty(&shell)?;
            }
        }
        if !truthy(get(&r.backup, "WinE.existed")) {
            let root = cls(&format!(r"CLSID\{WIN_E}"));
            let nw = format!(r"{root}\shell\opennewwindow");
            if exists(&nw) {
                del_tree(&nw)?;
            }
            remove_if_empty(&format!(r"{root}\shell"))?;
            remove_if_empty(&root)?;
        }
    } else {
        for (n, rel) in with("verb:") {
            let k = cls(&rel);
            if !exists(&k) {
                continue;
            }
            if is_fm_verb(&k, text(get(&r.backup, &n)).as_deref()) {
                del_tree(&k)?;
            } else {
                warn.push(format!(r"Klíč HKCU\{k} teď patří jiné aplikaci; nechávám ho být."));
            }
        }
        // An "open" verb still there now is another app's. A deleted default (the emergency lines) is
        // put back; any value other than "open" was set by another app since.
        let shells = with("default:").into_iter().map(|(n, rel)| (Some(n), rel)).chain(with("absent:").into_iter().map(|(_, rel)| (None, rel)));
        for (saved, rel) in shells {
            let k = cls(&rel);
            if !exists(&k) {
                continue;
            }
            let cur = text(get(&k, ""));
            if get(&k, "").is_some() && !cur.is_some_and(|t| t.eq_ignore_ascii_case("open")) || exists(&format!(r"{k}\open")) {
                warn.push(format!(r"Výchozí akci HKCU\{k} teď určuje jiná aplikace; nechávám ji být."));
            } else if let Some(n) = saved {
                if let Some(v) = get(&r.backup, &n) {
                    set(&k, "", &v)?;
                }
            } else {
                del_value(&k, "");
            }
        }
        for (_, rel) in with("created:") {
            remove_empty_tree(&cls(&rel))?;
        }
    }
    del_value(&r.fm, "Redirect");
    del_tree(&r.backup)?;
    remove_if_empty(&r.fm)?;
    Ok((warn, "Složky se zase otevírají v Průzkumníkovi.".into()))
}

/// Called only for a verb key with a `verb:` record. It stays ours only while its command is still
/// fm's: the one recorded, or any quoted ...\fm.exe. Without a working command it is ours only when
/// it holds nothing else: our interrupted run leaves an empty verb key, or an empty command key
/// under it (fm writes no value on the verb key itself). Anything more is another app's.
fn is_fm_verb(verb: &str, recorded: Option<&str>) -> bool {
    let command = format!(r"{verb}\command");
    let Some((verb_keys, verb_values)) = counts(verb) else { return false };
    let c = counts(&command);
    if c.is_none_or(|(_, values)| values == 0) {
        return verb_values == 0 && verb_keys == c.is_some() as u32 && c.is_none_or(|(keys, _)| keys == 0);
    }
    let cmd = text(get(&command, "")).unwrap_or_default().to_lowercase();
    recorded.is_some_and(|r| !r.is_empty() && cmd == r.to_lowercase()) || cmd.contains(r#"\fm.exe""#)
}

/// The topmost missing key on the way down to `rel` (relative to `root`).
fn first_missing(root: &str, rel: &str) -> String {
    let mut path = String::new();
    for part in rel.split('\\') {
        if !path.is_empty() {
            path.push('\\');
        }
        path.push_str(part);
        if !exists(&format!(r"{root}\{path}")) {
            break;
        }
    }
    path
}

/// Whether `rel` is a tree recorded as `created:` or inside one.
fn in_created(saved: &[String], rel: &str) -> bool {
    let rel = rel.to_lowercase();
    saved.iter().filter_map(|n| strip_ci(n, "created:")).any(|c| {
        let c = c.to_lowercase();
        rel == c || rel.starts_with(&format!(r"{c}\"))
    })
}

fn strip_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len()).filter(|h| h.eq_ignore_ascii_case(prefix)).map(|_| &s[prefix.len()..])
}

fn remove_if_empty(path: &str) -> Result<(), String> {
    if counts(path) == Some((0, 0)) {
        // RegDeleteKeyW, not a tree delete: it refuses a key that has gained a subkey meanwhile.
        check(unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, pcw(&wide(path))) }, "Nelze smazat", path)?;
    }
    Ok(())
}

fn remove_empty_tree(path: &str) -> Result<(), String> {
    if !exists(path) {
        return Ok(());
    }
    for c in children(path) {
        remove_empty_tree(&format!(r"{path}\{c}"))?;
    }
    remove_if_empty(path)
}

// --- Registry under HKEY_CURRENT_USER. fm.exe is 64-bit, so it sees the 64-bit view, which is
// the one Explorer reads (a 32-bit process would see another view of Software\Classes\CLSID).

/// A value as stored: its kind and raw bytes (REG_EXPAND_SZ unexpanded), so a copy keeps both.
#[derive(Clone, Debug, PartialEq)]
struct Val {
    kind: REG_VALUE_TYPE,
    data: Vec<u8>,
}

fn sz(s: &str) -> Val {
    Val { kind: REG_SZ, data: wide(s).iter().flat_map(|u| u.to_le_bytes()).collect() }
}

/// The text of a REG_SZ or REG_EXPAND_SZ value; None for a missing value or any other kind.
fn text(v: Option<Val>) -> Option<String> {
    let v = v.filter(|v| v.kind == REG_SZ || v.kind == REG_EXPAND_SZ)?;
    let u: Vec<u16> = v.data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    Some(String::from_utf16_lossy(&u))
}

/// PowerShell's truth: 0 and "" are false, a missing value too.
fn truthy(v: Option<Val>) -> bool {
    match v {
        None => false,
        Some(v) if v.kind == REG_DWORD || v.kind == REG_QWORD => v.data.iter().any(|&b| b != 0),
        Some(v) if v.kind == REG_SZ || v.kind == REG_EXPAND_SZ => !text(Some(v)).unwrap_or_default().is_empty(),
        Some(_) => true,
    }
}

fn pcw(w: &[u16]) -> PCWSTR {
    PCWSTR(w.as_ptr())
}

fn check(e: WIN32_ERROR, what: &str, path: &str) -> Result<(), String> {
    if e.is_ok() { Ok(()) } else { Err(format!(r"{what} HKCU\{path}: {}", e.to_hresult().message())) }
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn open(path: &str, access: REG_SAM_FLAGS) -> Option<Key> {
    let mut h = HKEY::default();
    unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, pcw(&wide(path)), None, access, &mut h) }.is_ok().then(|| Key(h))
}

fn exists(path: &str) -> bool {
    open(path, KEY_READ).is_some()
}

/// Creates the key and any missing parents; an existing key keeps its values and subkeys.
fn create(path: &str) -> Result<(), String> {
    let mut h = HKEY::default();
    let e = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, pcw(&wide(path)), None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_READ, None, &mut h, None) };
    check(e, "Nelze vytvořit", path)?;
    drop(Key(h));
    Ok(())
}

/// None when the key or the value is missing ("" is the key's default value).
fn get(path: &str, name: &str) -> Option<Val> {
    let k = open(path, KEY_QUERY_VALUE)?;
    let n = wide(name);
    loop {
        let mut len = 0u32;
        if unsafe { RegQueryValueExW(k.0, pcw(&n), None, None, None, Some(&mut len)) }.is_err() {
            return None;
        }
        let mut data = vec![0u8; len as usize];
        let mut kind = REG_VALUE_TYPE::default();
        let e = unsafe { RegQueryValueExW(k.0, pcw(&n), None, Some(&mut kind), Some(data.as_mut_ptr()), Some(&mut len)) };
        if e == ERROR_MORE_DATA {
            continue; // grew in between
        }
        if e.is_err() {
            return None;
        }
        data.truncate(len as usize);
        return Some(Val { kind, data });
    }
}

/// Writes a value into an existing key.
fn set(path: &str, name: &str, v: &Val) -> Result<(), String> {
    let k = open(path, KEY_SET_VALUE).ok_or_else(|| format!(r"Chybí klíč HKCU\{path}"))?;
    check(unsafe { RegSetValueExW(k.0, pcw(&wide(name)), None, v.kind, Some(&v.data)) }, "Nelze zapsat do", path)
}

/// A missing key or value is fine.
fn del_value(path: &str, name: &str) {
    if let Some(k) = open(path, KEY_SET_VALUE) {
        unsafe {
            let _ = RegDeleteValueW(k.0, pcw(&wide(name)));
        }
    }
}

/// Deletes the key with everything under it; a missing key is fine.
fn del_tree(path: &str) -> Result<(), String> {
    let e = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, pcw(&wide(path))) };
    if e == ERROR_FILE_NOT_FOUND { Ok(()) } else { check(e, "Nelze smazat", path) }
}

/// (subkeys, values), the default value counted when set; None for a missing key.
fn counts(path: &str) -> Option<(u32, u32)> {
    let k = open(path, KEY_READ)?;
    let (mut keys, mut values) = (0u32, 0u32);
    let e = unsafe { RegQueryInfoKeyW(k.0, None, None, None, Some(&mut keys), None, None, Some(&mut values), None, None, None, None) };
    e.is_ok().then_some((keys, values))
}

fn children(path: &str) -> Vec<String> {
    let mut out = vec![];
    let Some(k) = open(path, KEY_READ) else { return out };
    let mut buf = [0u16; 256];
    for i in 0.. {
        let mut len = buf.len() as u32;
        if unsafe { RegEnumKeyExW(k.0, i, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) }.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    out
}

fn value_names(path: &str) -> Vec<String> {
    let mut out = vec![];
    let Some(k) = open(path, KEY_READ) else { return out };
    let mut buf = vec![0u16; 16384];
    for i in 0.. {
        let mut len = buf.len() as u32;
        if unsafe { RegEnumValueW(k.0, i, Some(windows::core::PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) }.is_err() {
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
    }
    out
}

#[cfg(test)]
mod tests {
    //! The scenarios of scripts\dev\test-default.ps1, against a sandbox key per test under
    //! HKCU\Software\fm-test-rs (tests run in parallel), plus the real scripts undoing fm's
    //! install and the other way round. Fake roots prove the logic only; Windows never reads them.
    use super::*;

    const EXE: &str = r"C:\Zkouška Souborů\fm.exe";
    const FOREIGN: &str = r#""C:\Other\other.exe" "%1""#;

    fn cmd(exe: &str) -> String {
        format!(r#""{exe}" "%1""#)
    }

    fn dword(n: u32) -> Val {
        Val { kind: REG_DWORD, data: n.to_le_bytes().to_vec() }
    }

    /// Every key under `path` with every value (name, kind, data), one sorted line per key.
    fn dump(path: &str) -> String {
        fn walk(p: &str, rel: &str, out: &mut Vec<String>) {
            let mut vals: Vec<String> = value_names(p)
                .into_iter()
                .map(|n| {
                    let v = get(p, &n).unwrap();
                    let shown = if v.kind == REG_SZ || v.kind == REG_EXPAND_SZ { text(Some(v.clone())).unwrap() } else { format!("{:?}", v.data) };
                    format!("{n}={}:{shown}", v.kind.0)
                })
                .collect();
            vals.sort();
            out.push(format!("{rel} {{{}}}", vals.join("; ")));
            for c in children(p) {
                walk(&format!(r"{p}\{c}"), &format!(r"{rel}\{c}"), out);
            }
        }
        if !exists(path) {
            return "<nokey>".into();
        }
        let mut out = vec![];
        walk(path, "", &mut out);
        out.sort();
        out.join("\n")
    }

    /// The real keys fm --set-default touches; no test may change them.
    fn real_state() -> String {
        let mut keys = vec![r"Software\fm".to_string()];
        for k in [r"Directory\shell", r"Drive\shell", r"Folder\shell", &format!(r"CLSID\{WIN_E}")] {
            keys.push(format!(r"Software\Classes\{k}"));
        }
        for (g, _) in VIRTUAL {
            keys.push(format!(r"Software\Classes\CLSID\{g}\shell"));
        }
        keys.iter().map(|k| dump(k)).collect::<Vec<_>>().join("\n")
    }

    struct Sb {
        root: String,
        r: Roots,
        real: String,
    }

    impl Sb {
        fn new(name: &str) -> Sb {
            let root = format!(r"Software\fm-test-rs\{name}");
            assert!(root.starts_with(r"Software\fm-test"));
            del_tree(&root).unwrap();
            create(&format!(r"{root}\Classes")).unwrap();
            let r = Roots { classes: format!(r"{root}\Classes"), fm: format!(r"{root}\fm"), backup: format!(r"{root}\fm\Backup") };
            Sb { root, r, real: real_state() }
        }
        fn c(&self, rel: &str) -> String {
            format!(r"{}\{rel}", self.r.classes)
        }
        fn def(&self, rel: &str) -> Option<String> {
            text(get(&self.c(rel), ""))
        }
        fn names(&self) -> Vec<String> {
            let mut n = value_names(&self.r.backup);
            n.sort();
            n
        }
        fn has(&self, name: &str) -> bool {
            self.names().iter().any(|n| n == name)
        }
        fn put(&self, rel: &str, name: &str, v: &Val) {
            create(&self.c(rel)).unwrap();
            set(&self.c(rel), name, v).unwrap();
        }
        fn del(&self, rel: &str) {
            del_tree(&self.c(rel)).unwrap();
        }
        fn set(&self) -> Report {
            set_default(&self.r, EXE).unwrap()
        }
        fn restore(&self) -> Report {
            restore(&self.r).unwrap()
        }
        fn dump(&self) -> String {
            dump(&self.root)
        }
        /// An empty Directory\shell (as on a real machine), another app's default verb for drives
        /// and Recycle Bin (that one REG_EXPAND_SZ), an empty Network shell key.
        fn seed(&self) {
            create(&self.c(r"Directory\shell")).unwrap();
            self.put(r"Drive\shell", "", &sz("other.open"));
            let expand = Val { kind: REG_EXPAND_SZ, ..sz("%SystemRoot%") };
            self.put(&format!(r"CLSID\{}\shell", VIRTUAL[1].0), "", &expand);
            create(&self.c(&format!(r"CLSID\{}\shell", VIRTUAL[2].0))).unwrap();
        }
        /// What the previous version wrote when Directory\shell was absent and Win+E was free.
        fn seed_legacy(&self, exe: &str) {
            create(&self.r.backup).unwrap();
            set(&self.r.backup, "Directory.shell.absent", &dword(1)).unwrap();
            set(&self.r.backup, "Drive.shell", &sz("other.open")).unwrap();
            set(&self.r.backup, "WinE.existed", &dword(0)).unwrap();
            for s in [r"Directory\shell", r"Drive\shell"] {
                self.put(&format!(r"{s}\fm.open\command"), "", &sz(&cmd(exe)));
                set(&self.c(s), "", &sz("fm.open")).unwrap();
                set(&self.c(&format!(r"{s}\fm.open")), "", &sz("Otevřít v Souborech")).unwrap();
            }
            let nw = format!(r"CLSID\{WIN_E}\shell\opennewwindow\command");
            self.put(&nw, "", &sz(&format!(r#""{exe}""#)));
            self.put(&nw, "DelegateExecute", &sz(""));
        }
        /// The reg delete lines of the scripts' headers, aimed at the sandbox.
        fn emergency(&self) {
            for (verb, _, shell) in layout() {
                self.del(&verb);
                if let Some(s) = shell {
                    del_value(&self.c(&s), "");
                }
            }
            del_value(&self.r.fm, "Redirect");
        }
    }

    impl Drop for Sb {
        fn drop(&mut self) {
            let _ = del_tree(&self.root);
            let _ = remove_if_empty(r"Software\fm-test-rs");
            if !std::thread::panicking() {
                assert_eq!(real_state(), self.real, "real keys changed");
            }
        }
    }

    /// (verb key, command, shell whose default verb becomes "open") as set_default writes them.
    fn layout() -> Vec<(String, String, Option<String>)> {
        let bare = format!(r#""{EXE}""#);
        let mut v = vec![
            (r"Directory\shell\open".to_string(), cmd(EXE), Some(r"Directory\shell".to_string())),
            (r"Drive\shell\open".into(), cmd(EXE), Some(r"Drive\shell".into())),
            (r"Folder\shell\explore".into(), cmd(EXE), None),
            (format!(r"CLSID\{WIN_E}\shell\opennewwindow"), bare.clone(), None),
            (format!(r"CLSID\{WIN_E}\shell\open"), bare, None),
        ];
        for (g, _) in VIRTUAL {
            v.push((format!(r"CLSID\{g}\shell\open"), format!(r#""{EXE}" "::{g}""#), Some(format!(r"CLSID\{g}\shell"))));
        }
        v
    }

    fn check_layout(sb: &Sb, skip: &[&str]) {
        for (verb, want, shell) in layout() {
            if skip.contains(&verb.as_str()) {
                continue;
            }
            let k = sb.c(&format!(r"{verb}\command"));
            assert_eq!(get(&k, ""), Some(sz(&want)), "{verb} command");
            assert_eq!(get(&k, "DelegateExecute"), Some(sz("")), "{verb} DelegateExecute");
            if let Some(s) = shell {
                assert_eq!(sb.def(&s).as_deref(), Some("open"), "{s} default verb");
            }
        }
        assert!(!exists(&sb.c(r"Folder\shell\open")), "Folder\\shell\\open created");
        assert!(!exists(&sb.c(r"Directory\shell\fm.open")) && !exists(&sb.c(r"Drive\shell\fm.open")), "old fm.open verb left");
        assert_eq!(text(get(&sb.r.fm, "Redirect")).as_deref(), Some(EXE));
    }

    #[test]
    fn clean_install_backup_second_run_restore() {
        let sb = Sb::new("clean");
        sb.seed();
        let p1 = sb.dump();
        let (warn, said) = sb.set();
        assert!(warn.is_empty(), "{warn:?}");
        assert!(said.starts_with("Složky, disky, Win+E"), "{said}");
        check_layout(&sb, &[]);
        let (pc, bin, net) = (VIRTUAL[0].0, VIRTUAL[1].0, VIRTUAL[2].0);
        let mut want = vec![
            format!(r"absent:CLSID\{pc}\shell"),
            format!(r"absent:CLSID\{net}\shell"),
            r"absent:Directory\shell".into(),
            format!(r"created:CLSID\{pc}"),
            format!(r"created:CLSID\{WIN_E}"),
            "created:Folder".into(),
            format!(r"default:CLSID\{bin}\shell"),
            r"default:Drive\shell".into(),
            format!(r"verb:CLSID\{pc}\shell\open"),
            format!(r"verb:CLSID\{WIN_E}\shell\open"),
            format!(r"verb:CLSID\{WIN_E}\shell\opennewwindow"),
            format!(r"verb:CLSID\{bin}\shell\open"),
            format!(r"verb:CLSID\{net}\shell\open"),
            r"verb:Directory\shell\open".into(),
            r"verb:Drive\shell\open".into(),
            r"verb:Folder\shell\explore".into(),
        ];
        want.sort();
        assert_eq!(sb.names(), want, "one backup entry per touched place");
        assert_eq!(text(get(&sb.r.backup, r"verb:Directory\shell\open")), Some(cmd(EXE)));
        assert_eq!(text(get(&sb.r.backup, &format!(r"verb:CLSID\{WIN_E}\shell\opennewwindow"))), Some(format!(r#""{EXE}""#)));
        assert_eq!(text(get(&sb.r.backup, r"default:Drive\shell")).as_deref(), Some("other.open"));
        let rb = get(&sb.r.backup, &format!(r"default:CLSID\{bin}\shell")).unwrap();
        assert!(rb.kind == REG_EXPAND_SZ && text(Some(rb)).as_deref() == Some("%SystemRoot%"), "previous default saved raw, with its kind");
        let once = sb.dump();
        assert!(sb.set().0.is_empty());
        assert_eq!(sb.dump(), once, "second run changes nothing (backup included)");

        let (warn, said) = sb.restore();
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(said, "Složky se zase otevírají v Průzkumníkovi.");
        assert_eq!(sb.dump(), p1, "restore returns exactly the prior state (kinds included)");
        assert!(!exists(&sb.r.fm), "backup and Redirect removed");
        assert_eq!(sb.restore().1, "Soubory nebyly výchozí; není co vracet.");
        assert_eq!(sb.dump(), p1, "restore without a backup is a no-op");
    }

    #[test]
    fn other_app_adds_keys_inside_created_trees() {
        let sb = Sb::new("inside");
        sb.seed();
        let p1 = sb.dump();
        sb.set();
        sb.put(r"Folder\shell\other\command", "", &sz(FOREIGN));
        let pc = format!(r"CLSID\{}", VIRTUAL[0].0);
        sb.put(&pc, "Foreign", &sz("x"));
        sb.restore();
        assert_eq!(sb.def(r"Folder\shell\other\command").as_deref(), Some(FOREIGN));
        assert_eq!(children(&sb.c("Folder")), ["shell"]);
        assert_eq!(children(&sb.c(r"Folder\shell")), ["other"]);
        assert_eq!(counts(&sb.c(&pc)), Some((0, 1)), "foreign value on created This PC CLSID survives, alone");
        assert_eq!(counts(&sb.c("Folder")).unwrap().1 + counts(&sb.c(r"Folder\shell")).unwrap().1, 0);
        assert!(!exists(&sb.r.fm));
        sb.del("Folder");
        sb.del(&pc);
        assert_eq!(sb.dump(), p1, "otherwise exactly the prior state");
    }

    #[test]
    fn other_app_takes_places_over_after_set_default() {
        let sb = Sb::new("takeover");
        sb.seed();
        let p1 = sb.dump();
        sb.set();
        sb.put(r"Directory\shell", "", &sz("openinfiles"));
        sb.put(r"Directory\shell\openinfiles\command", "", &sz(FOREIGN));
        sb.put(&format!(r"CLSID\{WIN_E}\shell\opennewwindow\command"), "", &sz(FOREIGN));
        let (warn, _) = sb.restore();
        assert!(warn.len() == 2 && warn.iter().all(|w| w.contains("jiné aplikac") || w.contains("jiná aplikace")), "{warn:?}");
        assert_eq!(sb.def(r"Directory\shell").as_deref(), Some("openinfiles"));
        assert_eq!(sb.def(r"Directory\shell\openinfiles\command").as_deref(), Some(FOREIGN));
        assert_eq!(sb.def(&format!(r"CLSID\{WIN_E}\shell\opennewwindow\command")).as_deref(), Some(FOREIGN));
        assert!(!exists(&sb.c(r"Directory\shell\open")) && !exists(&sb.c(&format!(r"CLSID\{WIN_E}\shell\open"))));
        assert_eq!(sb.def(r"Drive\shell").as_deref(), Some("other.open"));
        assert!(!exists(&sb.r.fm));
        sb.del(r"Directory\shell\openinfiles");
        sb.del(&format!(r"CLSID\{WIN_E}"));
        del_value(&sb.c(r"Directory\shell"), "");
        assert_eq!(sb.dump(), p1, "otherwise exactly the prior state");
    }

    #[test]
    fn emergency_lines_then_other_app_then_restore() {
        let sb = Sb::new("emergency");
        sb.seed();
        let p1 = sb.dump();
        sb.set();
        sb.emergency();
        sb.put(r"Directory\shell\open\command", "", &sz(FOREIGN));
        sb.put(r"Directory\shell", "", &sz("open"));
        sb.restore();
        assert_eq!(sb.def(r"Directory\shell\open\command").as_deref(), Some(FOREIGN), "other app's open verb kept");
        assert_eq!(sb.def(r"Directory\shell").as_deref(), Some("open"), "and its default");
        assert_eq!(sb.def(r"Drive\shell").as_deref(), Some("other.open"), "deleted default comes back");
        let bin = get(&sb.c(&format!(r"CLSID\{}\shell", VIRTUAL[1].0)), "").unwrap();
        assert!(bin.kind == REG_EXPAND_SZ && text(Some(bin)).as_deref() == Some("%SystemRoot%"), "with its kind");
        sb.del(r"Directory\shell\open");
        del_value(&sb.c(r"Directory\shell"), "");
        assert_eq!(sb.dump(), p1, "otherwise exactly the prior state");
    }

    #[test]
    fn other_apps_verb_without_command_at_a_recorded_place() {
        let sb = Sb::new("handler");
        sb.seed();
        let p1 = sb.dump();
        let seed_handler = || {
            sb.del(r"Folder\shell\explore");
            sb.put(r"Folder\shell\explore", "ExplorerCommandHandler", &sz("{11111111-2222-3333-4444-555555555555}"));
        };
        sb.set();
        seed_handler();
        let p7 = dump(&sb.c(r"Folder\shell\explore"));
        assert_eq!(sb.restore().0.len(), 1, "restore warns");
        assert_eq!(dump(&sb.c(r"Folder\shell\explore")), p7, "restore leaves it");
        sb.del("Folder");
        assert_eq!(sb.dump(), p1);
        sb.set();
        seed_handler();
        assert_eq!(sb.set().0.len(), 1, "set-default warns");
        assert_eq!(dump(&sb.c(r"Folder\shell\explore")), p7, "set-default leaves it");
        assert!(!sb.has(r"verb:Folder\shell\explore"), "and drops the record");
        sb.restore();
        assert_eq!(dump(&sb.c(r"Folder\shell\explore")), p7, "a later restore keeps it too");
        sb.del("Folder");
        assert_eq!(sb.dump(), p1, "otherwise exactly the prior state");
    }

    #[test]
    fn stale_record_never_claims_another_apps_key() {
        let sb = Sb::new("stale");
        sb.seed();
        let p1 = sb.dump();
        sb.set();
        sb.del(r"Folder\shell\explore");
        sb.put(r"Folder\shell\explore\command", "", &sz(FOREIGN));
        sb.put(r"Drive\shell", "", &sz("files.open"));
        sb.del(r"Directory\shell\open\command");
        assert_eq!(sb.set().0.len(), 1, "foreign explore skipped with a warning");
        assert_eq!(sb.def(r"Folder\shell\explore\command").as_deref(), Some(FOREIGN));
        assert!(!sb.has(r"verb:Folder\shell\explore"), "its record dropped");
        assert_eq!(sb.def(r"Directory\shell\open\command"), Some(cmd(EXE)), "our interrupted verb (no command) completed");
        assert_eq!(sb.def(r"Drive\shell").as_deref(), Some("open"), "default changed by another app taken back");
        assert_eq!(text(get(&sb.r.backup, r"default:Drive\shell")).as_deref(), Some("files.open"), "that app's value saved");
        sb.restore();
        assert_eq!(sb.def(r"Folder\shell\explore\command").as_deref(), Some(FOREIGN));
        assert_eq!(sb.def(r"Drive\shell").as_deref(), Some("files.open"), "restore brings back that app's default");
        sb.del("Folder");
        sb.put(r"Drive\shell", "", &sz("other.open"));
        assert_eq!(sb.dump(), p1, "otherwise exactly the prior state");
    }

    #[test]
    fn other_apps_verbs_there_before() {
        let sb = Sb::new("before");
        sb.seed();
        let foreign = [r"Folder\shell\explore".to_string(), format!(r"CLSID\{WIN_E}\shell\opennewwindow"), r"Drive\shell\open".into()];
        for v in &foreign {
            sb.put(&format!(r"{v}\command"), "", &sz(FOREIGN));
        }
        let p2 = sb.dump();
        let (warn, said) = sb.set();
        assert!(warn.len() == 3 && warn.iter().all(|w| w.contains("jiná aplikace")), "{warn:?}");
        assert!(said.contains("zůstává: disky, složky otevírané z prohlížeče, Win+E ("), "{said}");
        check_layout(&sb, &foreign.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(sb.def(r"Folder\shell\explore\command").as_deref(), Some(FOREIGN));
        assert_eq!(get(&sb.c(r"Folder\shell\explore\command"), "DelegateExecute"), None);
        assert_eq!(sb.def(r"Drive\shell").as_deref(), Some("other.open"), "Drive default kept");
        assert!(!sb.names().iter().any(|n| n.contains("Folder") || n.contains("opennewwindow") || n.contains("Drive")), "nothing recorded for foreign keys");
        assert!(sb.has(&format!(r"verb:CLSID\{WIN_E}\shell\open")) && !sb.has(&format!(r"created:CLSID\{WIN_E}")));
        sb.restore();
        assert_eq!(sb.dump(), p2, "restore keeps the foreign keys, prior state exact");
    }

    #[test]
    fn migration_from_the_previous_version() {
        let sb = Sb::new("legacy");
        sb.seed();
        sb.del(r"Directory\shell");
        let p3 = sb.dump();
        sb.seed_legacy(EXE);
        sb.restore();
        assert_eq!(sb.dump(), p3, "restore undoes an old install exactly");
        sb.seed_legacy(EXE);
        assert!(sb.set().0.is_empty(), "old Win+E key not treated as foreign");
        check_layout(&sb, &[]);
        assert!(!sb.has("WinE.existed") && !sb.has("Drive.shell"), "old backup format replaced");
        assert_eq!(text(get(&sb.r.backup, r"default:Drive\shell")).as_deref(), Some("other.open"), "original value kept, not fm.open");
        assert!(sb.has(r"created:Directory\shell") && sb.has(r"absent:Directory\shell"));
        sb.restore();
        assert_eq!(sb.dump(), p3, "restore after migration returns the prior state");
    }

    /// Runs scripts\<name> on the sandbox in 64-bit Windows PowerShell.
    fn script(sb: &Sb, name: &str, exe: Option<&str>) {
        let path = format!(r"{}\..\..\scripts\{name}", env!("CARGO_MANIFEST_DIR"));
        let hk = |k: &str| format!(r"HKCU:\{k}");
        let mut args: Vec<String> = vec!["-NoProfile".into(), "-NonInteractive".into(), "-ExecutionPolicy".into(), "Bypass".into(), "-File".into(), path];
        args.extend(["-ClassesRoot".into(), hk(&sb.r.classes), "-FmRoot".into(), hk(&sb.r.fm), "-BackupRoot".into(), hk(&sb.r.backup)]);
        if let Some(exe) = exe {
            args.extend(["-Exe".into(), exe.into()]);
        }
        let ps = format!(r"{}\System32\WindowsPowerShell\v1.0\powershell.exe", std::env::var("SystemRoot").unwrap());
        let out = std::process::Command::new(ps).args(&args).output().unwrap();
        assert!(out.status.success(), "{name}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn scripts_and_fm_undo_each_other() {
        let sb = Sb::new("interop");
        sb.seed();
        // set-default-fm.ps1 wants an existing fm.exe.
        let dir = std::env::temp_dir().join("fm-test-rs-interop");
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("fm.exe");
        std::fs::write(&exe, b"").unwrap();
        let exe = exe.to_str().unwrap();
        let p1 = sb.dump();

        script(&sb, "set-default-fm.ps1", Some(exe));
        let by_script = sb.dump();
        assert!(by_script != p1 && by_script.contains(r"verb:Directory\shell\open="), "the script ran");
        assert!(sb.restore().0.is_empty());
        assert_eq!(sb.dump(), p1, "fm restores the script's install exactly");
        set_default(&sb.r, exe).unwrap();
        assert_eq!(sb.dump(), by_script, "fm writes exactly what the script writes, backup included");
        script(&sb, "restore-explorer.ps1", None);
        assert_eq!(sb.dump(), p1, "the script restores fm's install exactly");

        // An old install migrated by each side, then restored by the other.
        sb.del(r"Directory\shell");
        let p3 = sb.dump();
        sb.seed_legacy(exe);
        script(&sb, "set-default-fm.ps1", Some(exe));
        let migrated = sb.dump();
        assert!(migrated != p3 && !migrated.contains("fm.open"), "the script migrated");
        sb.restore();
        assert_eq!(sb.dump(), p3, "fm restores the script's migration");
        sb.seed_legacy(exe);
        set_default(&sb.r, exe).unwrap();
        assert_eq!(sb.dump(), migrated, "fm migrates exactly as the script does");
        script(&sb, "restore-explorer.ps1", None);
        assert_eq!(sb.dump(), p3, "the script restores fm's migration");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
