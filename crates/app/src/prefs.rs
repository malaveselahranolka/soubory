//! Persistent preferences in `%LOCALAPPDATA%\fm\prefs.txt` (simple key=value lines).
use crate::app::{App, View};
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("fm")
}

fn file() -> PathBuf {
    data_dir().join("prefs.txt")
}

pub fn load(app: &mut App) {
    let Ok(text) = std::fs::read_to_string(file()) else {
        return;
    };
    for line in text.lines() {
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
    for f in &app.prefs.favorites {
        s.push_str(&format!("favorite={}\n", f.display()));
    }
    let path = file();
    let _ = std::fs::create_dir_all(data_dir());
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}
