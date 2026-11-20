//! What a command line asks fm to open. fm stands in for Explorer (folder verbs, Win+E, apps
//! that start `explorer.exe /select,…` through the dialogkit watcher), so it reads Explorer's
//! syntax: a path, `/select,<item>`, `/root,<place>`, `::{CLSID}`, `shell:Name`, `file:///`.
use crate::app::Loc;
use crate::fsys::wide;
use crate::shellns::ShellId;
use std::path::{Path, PathBuf};
use windows::Win32::UI::Shell::{Common::ITEMIDLIST, *};
use windows::core::PCWSTR;

#[derive(Debug, PartialEq)]
pub enum Target {
    /// A window on `loc`, with the child named `select` selected.
    Open { loc: Loc, select: Option<String> },
    /// A place only Explorer shows (Control Panel, a library): Explorer gets these arguments.
    Explorer(String),
    /// Nothing there: what to tell the user.
    Missing(String),
}

/// The arguments after the program name exactly as Windows passed them. std's argv rules read
/// `\"` as an escaped quote, so the shell's `"C:\"` for a drive would come out as `C:"`.
pub fn raw_tail() -> String {
    let line = unsafe { windows::Win32::System::Environment::GetCommandLineW().to_string() }.unwrap_or_default();
    let rest = match line.strip_prefix('"') {
        Some(r) => r.split_once('"').map_or("", |(_, t)| t),
        None => line.split_once([' ', '\t']).map_or("", |(_, t)| t),
    };
    rest.trim_start().to_string()
}

/// Explorer's splitting: whitespace separates and quotes group (no escapes); after a switch an
/// unquoted comma separates too (`/select,"C:\a b.txt"`, `/e,/root,C:\x`).
fn tokens(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ' ' | '\t' if !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                }
                started = false;
            }
            ',' if !quoted && cur.starts_with('/') => {
                out.push(std::mem::take(&mut cur));
                started = false;
            }
            _ => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Reads a command line tail. `cwd` resolves relative paths (a handed-over line carries the
/// sender's). fm's own `--flags` before the target are skipped, and one after it ends it.
pub fn parse(raw: &str, cwd: &Path) -> Target {
    let mut it = tokens(raw).into_iter().skip_while(|t| t.starts_with("--")).take_while(|t| !t.starts_with("--"));
    let (mut select, mut plain) = (None, Vec::new());
    while let Some(t) = it.next() {
        match t.to_ascii_lowercase().as_str() {
            "/select" => select = it.next(),
            "/root" => plain = it.next().into_iter().collect(),
            "/e" | "/n" | "/separate" | "/" | "" => {}
            s if s.starts_with('/') => return Target::Explorer(raw.to_string()),
            _ => plain.push(t.trim_start_matches(',').to_string()),
        }
    }
    match select {
        Some(item) => resolve(&item, cwd, true, raw),
        None => resolve(&plain.join(" "), cwd, false, raw),
    }
}

fn default_loc() -> Loc {
    crate::fsys::known_folder(&FOLDERID_Profile).map(Loc::Folder).unwrap_or(Loc::Computer)
}

/// `s` as a place to open; `select`: open its parent with it selected (`/select,`).
fn resolve(s: &str, cwd: &Path, select: bool, raw: &str) -> Target {
    let s = s.trim();
    if s.is_empty() {
        return Target::Open { loc: default_loc(), select: None };
    }
    // The name as given first: a folder can really be called `%APPDATA%`.
    if let Some(p) = existing(s, cwd) {
        let file = p.is_file();
        return open_path(p, select || file);
    }
    let x = crate::win::expand_env(s);
    if x != s {
        if let Some(p) = existing(&x, cwd) {
            let file = p.is_file();
            return open_path(p, select || file);
        }
    }
    let t = shell_place(&x, select, raw);
    // A /select item gone since the caller saw it (a download renamed when it finished): its
    // folder still opens.
    if select && matches!(t, Target::Missing(_)) {
        let dir = Path::new(&x).parent().filter(|p| !p.as_os_str().is_empty()).and_then(|p| existing(&p.to_string_lossy(), cwd));
        if let Some(dir) = dir.filter(|p| p.is_dir()) {
            return Target::Open { loc: Loc::Folder(dir), select: None };
        }
    }
    t
}

/// `s` as an existing file-system path (relative to `cwd`, a `file:` URL, a bare drive).
fn existing(s: &str, cwd: &Path) -> Option<PathBuf> {
    // `\\server` alone: std sees no prefix and would put it on the cwd's drive (the shell's).
    let b = s.as_bytes();
    if b.len() > 2 && matches!(b[0], b'\\' | b'/') && matches!(b[1], b'\\' | b'/') && !matches!(Path::new(s).components().next(), Some(std::path::Component::Prefix(_))) {
        return None;
    }
    let p = if s.get(..5).is_some_and(|h| h.eq_ignore_ascii_case("file:")) {
        from_url(s)?
    } else if s.len() == 2 && s.ends_with(':') {
        // A bare drive is its root (not the current folder on that drive).
        PathBuf::from(format!("{s}\\"))
    } else {
        cwd.join(s)
    };
    // `..` resolved; `C:/x\` and `C:\x\` are the folder `C:\x`; a root keeps its backslash.
    let p: PathBuf = std::path::absolute(&p).unwrap_or(p).components().collect();
    (p.is_dir() || p.is_file()).then_some(p)
}

/// A file-system item: the folder itself, or its parent with it selected.
fn open_path(p: PathBuf, select: bool) -> Target {
    match (select, p.parent(), p.file_name()) {
        (true, Some(parent), Some(name)) => Target::Open {
            loc: Loc::Folder(parent.to_path_buf()),
            select: Some(name.to_string_lossy().into_owned()),
        },
        // A drive root to select: Explorer shows it in This PC.
        (true, _, _) => Target::Open { loc: Loc::Computer, select: None },
        (false, ..) => Target::Open { loc: Loc::Folder(p), select: None },
    }
}

fn from_url(s: &str) -> Option<PathBuf> {
    let w = wide(s);
    let mut buf = vec![0u16; 2048];
    let mut n = buf.len() as u32;
    unsafe { PathCreateFromUrlW(PCWSTR(w.as_ptr()), windows::core::PWSTR(buf.as_mut_ptr()), &mut n, 0).ok()? };
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])))
}

/// `::{CLSID}`, `shell:Name`, `\\server`: what the shell's own parser finds there.
fn shell_place(s: &str, select: bool, raw: &str) -> Target {
    let Some(id) = parse_name(s) else {
        return Target::Missing(format!("Umístění „{s}“ neexistuje."));
    };
    if let Some(p) = fs_path(&id) {
        return open_path(p, select);
    }
    if select {
        return Target::Explorer(raw.to_string());
    }
    if ShellId::known(&FOLDERID_ComputerFolder).as_ref() == Some(&id) {
        return Target::Open { loc: Loc::Computer, select: None };
    }
    let network = ShellId::known(&FOLDERID_NetworkFolder);
    // The Recycle Bin, Network, and computers and shares in it: fm browses these itself.
    let under_network = network.as_ref().is_some_and(|n| unsafe { ILIsParent(n.as_ptr(), id.as_ptr(), false).as_bool() });
    if crate::shellns::is_recycle_bin(&id) || network.as_ref() == Some(&id) || under_network {
        return Target::Open { loc: Loc::Shell { name: id.name(), id }, select: None };
    }
    Target::Explorer(raw.to_string())
}

fn parse_name(s: &str) -> Option<ShellId> {
    let w = wide(s);
    let mut p: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe {
        SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut p, 0, None).ok()?;
        if p.is_null() {
            return None;
        }
        let id = ShellId::from_raw(p);
        ILFree(Some(p));
        Some(id)
    }
}

fn fs_path(id: &ShellId) -> Option<PathBuf> {
    crate::shellns::fs_path(id).filter(|p| p.is_dir() || p.is_file())
}

/// Explorer's arguments for what `loc` shows (fm's "Otevřít v Průzkumníkovi").
pub fn explorer_args(loc: &Loc, focused: Option<&Path>) -> String {
    let quote = |p: &Path| format!("\"{}\"", p.display());
    match loc {
        Loc::Folder(p) => quote(p),
        Loc::Search { scope: Some(p), .. } => quote(p),
        Loc::Computer => "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}".into(),
        Loc::Shell { id, .. } => crate::shellns::parsing_name(id).map(|n| format!("\"{n}\"")).unwrap_or_default(),
        // Recents, tags, a search everywhere: the focused item in its folder.
        _ => focused.map(|p| format!("/select,{}", quote(p))).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(p: &str, select: Option<&str>) -> Target {
        Target::Open { loc: Loc::Folder(PathBuf::from(p)), select: select.map(Into::into) }
    }

    #[test]
    fn tokenizes_like_explorer() {
        assert_eq!(tokens(r#""C:\""#), [r"C:\"]);
        assert_eq!(tokens(r#"/select,"C:\a b\c,d.txt""#), ["/select", r"C:\a b\c,d.txt"]);
        assert_eq!(tokens(r"/e,/root,C:\x"), ["/e", "/root", r"C:\x"]);
        assert_eq!(tokens(r#"/select "C:\x y""#), ["/select", r"C:\x y"]);
        assert_eq!(tokens(r#""" a"#), ["", "a"]);
        assert_eq!(tokens(r"C:\a,b  --dev-x 1"), [r"C:\a,b", "--dev-x", "1"]);
    }

    #[test]
    fn parses_paths_and_switches() {
        let d = std::env::temp_dir().join(format!("fm-target-{}", std::process::id()));
        let sub = d.join("a b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("f,1.txt"), b"x").unwrap();
        let (ds, ss) = (d.to_string_lossy().into_owned(), sub.to_string_lossy().into_owned());
        let cwd = Path::new(r"C:\");

        // A drive the way the shell's "%1" passes it: `"C:\"` must stay the root.
        assert_eq!(parse(r#""C:\""#, cwd), open(r"C:\", None));
        assert_eq!(parse("C:", Path::new(r"C:\Windows")), open(r"C:\", None));
        assert_eq!(parse(&format!(r#""{ss}\""#), cwd), open(&ss, None));
        assert_eq!(parse(&format!(r#""{ss}""#), cwd), open(&ss, None));
        // A file opens its folder with it selected; /select does that for a folder too.
        assert_eq!(parse(&format!(r#""{ss}\f,1.txt""#), cwd), open(&ss, Some("f,1.txt")));
        assert_eq!(parse(&format!(r#"/select,"{ss}\f,1.txt""#), cwd), open(&ss, Some("f,1.txt")));
        assert_eq!(parse(&format!(r#"/e, /select "{ss}""#), cwd), open(&ds, Some("a b")));
        assert_eq!(parse(&format!(r#"/root,"{ss}""#), cwd), open(&ss, None));
        // Unquoted with a space, relative to the sender's folder, forward slashes.
        assert_eq!(parse(&format!("{ds}\\a b"), cwd), open(&ss, None));
        assert_eq!(parse(r#""a b""#, &d), open(&ss, None));
        assert_eq!(parse(&ss.replace('\\', "/"), cwd), open(&ss, None));
        // fm's own flags around the target (tests, --background).
        assert_eq!(parse(&format!(r#""{ss}" --new-instance --dev-view list"#), cwd), open(&ss, None));
        assert_eq!(parse(&format!(r#"--background "{ss}""#), cwd), open(&ss, None));
        // file:/// URLs.
        let url = format!("file:///{}", ss.replace('\\', "/").replace(' ', "%20"));
        assert_eq!(parse(&url, cwd), open(&ss, None));
        // A /select item that is gone: its folder.
        assert_eq!(parse(&format!(r#"/select,"{ss}\gone.txt""#), cwd), open(&ss, None));
        // Nothing there.
        assert!(matches!(parse(&format!(r#""{ds}\nic""#), cwd), Target::Missing(_)));
        // `..`, and names that only look like variables.
        assert_eq!(parse("..", &sub), open(&ds, None));
        let pct = d.join("%OS%");
        std::fs::create_dir_all(&pct).unwrap();
        assert_eq!(parse(&format!(r#""{}""#, pct.display()), cwd), open(&pct.to_string_lossy(), None));
        // A server alone is never a folder on the current drive.
        assert_eq!(existing(r"\\Windows", cwd), None);
        assert_eq!(existing(r"//Windows", cwd), None);
        // Unknown switches go to Explorer as they are.
        assert_eq!(parse("/idlist,:1:2", cwd), Target::Explorer("/idlist,:1:2".into()));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn parses_shell_places() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
        }
        let cwd = Path::new(r"C:\");
        assert_eq!(parse("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}", cwd), Target::Open { loc: Loc::Computer, select: None });
        assert_eq!(parse("shell:MyComputerFolder", cwd), Target::Open { loc: Loc::Computer, select: None });
        let bin = parse("::{645FF040-5081-101B-9F08-00AA002F954E}", cwd);
        assert!(matches!(&bin, Target::Open { loc: Loc::Shell { id, .. }, .. } if crate::shellns::is_recycle_bin(id)), "{bin:?}");
        let net = parse("shell:NetworkPlacesFolder", cwd);
        assert!(matches!(&net, Target::Open { loc: Loc::Shell { id, .. }, .. } if crate::shellns::is_network(id)), "{net:?}");
        let windows = crate::fsys::known_folder(&FOLDERID_Windows).unwrap();
        let w = parse("shell:Windows", cwd);
        assert!(matches!(&w, Target::Open { loc: Loc::Folder(p), select: None } if p.to_string_lossy().eq_ignore_ascii_case(&windows.to_string_lossy())), "{w:?}");
        // Control Panel stays Explorer's.
        let cp = "::{26EE0668-A00A-44D7-9371-BEB064C98683}";
        assert_eq!(parse(cp, cwd), Target::Explorer(cp.into()));
        assert!(matches!(parse("shell:NoSuchPlace", cwd), Target::Missing(_)));
        assert!(matches!(parse("/select,shell:NoSuchPlace", cwd), Target::Missing(_)));
    }
}
