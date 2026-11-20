//! Persistent preferences in `%LOCALAPPDATA%\fm\prefs.txt` (simple key=value lines).
use crate::app::{App, View};
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("fm")
}

/// Appends a line to `%LOCALAPPDATA%\\fm\\fm.log` (kept under 1 MB).
pub fn log(line: &str) {
    use std::io::Write;
    let path = data_dir().join("fm.log");
    let _ = std::fs::create_dir_all(data_dir());
    if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "{secs} {line}");
    }
}

fn file() -> PathBuf {
    data_dir().join("prefs.txt")
}

pub fn load(app: &mut App) {
    let Ok(text) = std::fs::read_to_string(file()) else {
        return;
    };
    // Notepad and PowerShell write a BOM, which would hide the first key.
    for line in text.trim_start_matches('\u{feff}').lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "show_hidden" => app.prefs.show_hidden = v == "1",
            "folders_first" => app.prefs.folders_first = v != "0",
            "icon_size" => app.prefs.icon_size = v.parse().unwrap_or(64.0f32).clamp(32.0, 256.0),
            "sidebar_w" => {
                app.sidebar_w = v
                    .parse()
                    .unwrap_or(crate::app::DEFAULT_SIDEBAR)
                    .clamp(crate::layout::SIDEBAR_MIN, crate::layout::SIDEBAR_MAX)
            }
            "sidebar" => app.sidebar_on = v != "0",
            "preview" => app.preview_on = v == "1",
            "view" => {
                let view = match v {
                    "icons" => View::Icons,
                    "columns" => View::Columns,
                    "gallery" => View::Gallery,
                    _ => View::List,
                };
                for t in &mut app.tabs {
                    t.view = view;
                }
            }
            "window" => {
                let n: Vec<i32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                if let [l, t, r, b, max] = n[..] {
                    if r - l >= 200 && b - t >= 150 {
                        app.prefs.window = Some(([l, t, r, b], max == 1));
                    }
                }
            }
            "search" => {
                let (text, scope) = v.split_once('\t').map_or((v, None), |(t, s)| (t, Some(PathBuf::from(s))));
                let item = (text.to_string(), scope);
                if !item.0.is_empty() && !app.prefs.saved.contains(&item) {
                    app.prefs.saved.push(item);
                }
            }
            "favorite" => {
                let p = PathBuf::from(v);
                if !app.prefs.favorites.contains(&p) {
                    app.prefs.favorites.push(p);
                }
            }
            _ => {}
        }
    }
}

/// The window's restored rect and maximized state, as Windows keeps them.
fn placement(app: &App) -> Option<([i32; 4], bool)> {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowPlacement, SW_SHOWMAXIMIZED, WINDOWPLACEMENT};
    if app.hwnd.is_invalid() {
        return None;
    }
    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe { GetWindowPlacement(app.hwnd, &mut wp).ok()? };
    let r = wp.rcNormalPosition;
    Some(([r.left, r.top, r.right, r.bottom], wp.showCmd == SW_SHOWMAXIMIZED.0 as u32))
}

/// Puts the window where it was last closed, unless that place is on no monitor any more.
pub fn restore_window(app: &App) -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect};
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SetWindowPlacement, WINDOWPLACEMENT};
    let Some(([l, t, r, b], _)) = app.prefs.window else { return false };
    let rc = RECT { left: l, top: t, right: r, bottom: b };
    unsafe {
        if MonitorFromRect(&rc, MONITOR_DEFAULTTONULL).is_invalid() {
            return false;
        }
        let wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            showCmd: SW_HIDE.0 as u32,
            rcNormalPosition: rc,
            ..Default::default()
        };
        SetWindowPlacement(app.hwnd, &wp).is_ok()
    }
}

pub fn save(app: &App) {
    let view = match app.tab().view {
        View::Icons => "icons",
        View::List => "list",
        View::Columns => "columns",
        View::Gallery => "gallery",
    };
    let mut s = format!(
        "show_hidden={}\nfolders_first={}\nicon_size={}\nsidebar_w={}\nsidebar={}\npreview={}\nview={view}\n",
        app.prefs.show_hidden as u8,
        app.prefs.folders_first as u8,
        app.prefs.icon_size,
        app.sidebar_w,
        app.sidebar_on as u8,
        app.preview_on as u8
    );
    if let Some(([l, t, r, b], max)) = placement(app).or(app.prefs.window) {
        s.push_str(&format!("window={l},{t},{r},{b},{}\n", max as u8));
    }
    for f in &app.prefs.favorites {
        s.push_str(&format!("favorite={}\n", f.display()));
    }
    for (text, scope) in &app.prefs.saved {
        match scope {
            Some(f) => s.push_str(&format!("search={text}\t{}\n", f.display())),
            None => s.push_str(&format!("search={text}\n")),
        }
    }
    let path = file();
    let _ = std::fs::create_dir_all(data_dir());
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}
