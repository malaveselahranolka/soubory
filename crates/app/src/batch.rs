//! Finder's "Rename N Items": replace text, add text, or a name with a running number, shown as a
//! sheet over the file list with a live example. Checked before anything is renamed (invalid
//! names, two items ending up with one name, an existing item in the way), then undone in one
//! step.
use crate::app::{App, Hit, Loc};
use crate::geom::Rect;
use crate::gfx::{Align, Canvas, Font};
use crate::theme::{Color, Mode as ThemeMode, Theme};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Replace,
    Add,
    Format,
}

pub struct Batch {
    pub mode: Mode,
    pub a: String,
    pub b: String,
    /// Add mode: the text goes before the name.
    pub before: bool,
    pub items: Vec<PathBuf>,
    pub dirs: Vec<bool>,
}

pub const MODE: [(&str, Mode); 3] = [("Nahradit text", Mode::Replace), ("Přidat text", Mode::Add), ("Formát", Mode::Format)];
pub const C_CANCEL: u8 = 20;
pub const C_OK: u8 = 21;
pub const C_POS: u8 = 10;

const W: f32 = 500.0;
const PAD: f32 = 20.0;
const ROW: f32 = 28.0;
const LABEL_W: f32 = 120.0;

pub struct Parts {
    pub sheet: Rect,
    pub title: Rect,
    pub modes: [Rect; 3],
    pub row_a: Rect,
    pub row_b: Rect,
    pub field_a: Rect,
    pub field_b: Rect,
    pub pos: [Rect; 2],
    pub example: Rect,
    pub cancel: Rect,
    pub ok: Rect,
}

pub fn parts(app: &App) -> Parts {
    let b = app.lay.body;
    let h = PAD + 22.0 + 12.0 + ROW + 18.0 + ROW + 10.0 + ROW + 14.0 + 18.0 + 18.0 + ROW + PAD;
    let sheet = Rect::new(b.cx() - W / 2.0, b.y + 10.0, W.min(b.w - 16.0), h);
    let inner = sheet.inset(PAD, PAD);
    let mut y = inner.y;
    let title = Rect::new(inner.x, y, inner.w, 22.0);
    y += 22.0 + 12.0;
    let seg_w = (inner.w / 3.0).floor();
    let modes = [0, 1, 2].map(|k| Rect::new(inner.x + seg_w * k as f32, y, seg_w, ROW));
    y += ROW + 18.0;
    let row_a = Rect::new(inner.x, y, inner.w, ROW);
    y += ROW + 10.0;
    let row_b = Rect::new(inner.x, y, inner.w, ROW);
    y += ROW + 14.0;
    let example = Rect::new(inner.x, y, inner.w, 18.0);
    let field = |r: Rect| Rect::new(r.x + LABEL_W + 10.0, r.y, r.w - LABEL_W - 10.0, r.h);
    let fb = field(row_b);
    let half = (fb.w / 2.0).floor();
    let ok = Rect::new(inner.r() - 120.0, inner.b() - ROW, 120.0, ROW);
    Parts {
        sheet,
        title,
        modes,
        row_a,
        row_b,
        field_a: field(row_a),
        field_b: fb,
        pos: [Rect::new(fb.x, fb.y, half, ROW), Rect::new(fb.x + half, fb.y, half, ROW)],
        example,
        cancel: Rect::new(ok.x - 8.0 - 100.0, ok.y, 100.0, ROW),
        ok,
    }
}

/// Where the text fields go (the window's EDITs sit inside these, inset like the search field).
pub fn edit_rect(app: &App, which: usize) -> Rect {
    let p = parts(app);
    let f = if which == 0 { p.field_a } else { p.field_b };
    f.inset(8.0, 5.0)
}

/// Whether field B is a text field in this mode (Add uses a before/after switch instead).
pub fn has_b(mode: Mode) -> bool {
    mode != Mode::Add
}

pub fn labels(mode: Mode) -> (&'static str, &'static str) {
    match mode {
        Mode::Replace => ("Najít:", "Nahradit za:"),
        Mode::Add => ("Text:", "Umístit:"),
        Mode::Format => ("Název:", "Čísla od:"),
    }
}

/// F2 or the menu with several items selected.
pub fn open(app: &mut App) {
    let t = app.tab();
    if !matches!(t.loc, Loc::Folder(_) | Loc::Search { .. } | Loc::Recents | Loc::Tag(_)) {
        return;
    }
    let idx = t.sel.indices();
    if idx.len() < 2 {
        return;
    }
    let items: Vec<PathBuf> = idx.iter().map(|&i| t.path_of(i)).collect();
    let dirs = idx.iter().map(|&i| t.entries[i].is_dir).collect();
    app.batch = Some(Batch { mode: Mode::Replace, a: String::new(), b: String::new(), before: false, items, dirs });
    crate::win::batch_fields(app);
    app.invalidate();
}

pub fn close(app: &mut App) {
    app.batch = None;
    crate::win::batch_fields(app);
    app.invalidate();
}

pub fn set_mode(app: &mut App, mode: Mode) {
    let Some(b) = app.batch.as_mut() else { return };
    if b.mode == mode {
        return;
    }
    b.mode = mode;
    (b.a, b.b) = match mode {
        Mode::Format => ("Soubor".into(), "1".into()),
        _ => (String::new(), String::new()),
    };
    crate::win::batch_fields(app);
    app.invalidate();
}

pub fn click(app: &mut App, code: u8) {
    match code {
        0..=2 => set_mode(app, MODE[code as usize].1),
        C_POS | 11 => {
            if let Some(b) = app.batch.as_mut() {
                b.before = code == C_POS;
            }
            app.invalidate();
        }
        C_CANCEL => close(app),
        C_OK => apply(app),
        _ => {}
    }
}

pub fn hit(app: &App, x: f32, y: f32) -> Hit {
    let Some(b) = &app.batch else { return Hit::None };
    let p = parts(app);
    if !p.sheet.contains(x, y) {
        return Hit::None;
    }
    for (k, r) in p.modes.iter().enumerate() {
        if r.contains(x, y) {
            return Hit::Batch(k as u8);
        }
    }
    if b.mode == Mode::Add {
        for (k, r) in p.pos.iter().enumerate() {
            if r.contains(x, y) {
                return Hit::Batch(C_POS + k as u8);
            }
        }
    }
    if p.cancel.contains(x, y) {
        return Hit::Batch(C_CANCEL);
    }
    if p.ok.contains(x, y) {
        return Hit::Batch(C_OK);
    }
    Hit::Batch(99)
}

fn split(name: &str, dir: bool) -> (&str, &str) {
    match name.rfind('.') {
        Some(k) if k > 0 && !dir => (&name[..k], &name[k..]),
        _ => (name, ""),
    }
}

/// The new names, in the same order. Extensions are kept.
pub fn new_names(names: &[String], dirs: &[bool], mode: Mode, a: &str, b: &str, before: bool) -> Vec<String> {
    let start: u64 = b.trim().parse().unwrap_or(1);
    names
        .iter()
        .zip(dirs)
        .enumerate()
        .map(|(i, (n, &dir))| {
            let (stem, ext) = split(n, dir);
            let stem = match mode {
                Mode::Replace if a.is_empty() => stem.to_string(),
                Mode::Replace => stem.replace(a, b),
                Mode::Add if before => format!("{a}{stem}"),
                Mode::Add => format!("{stem}{a}"),
                Mode::Format if a.trim().is_empty() => format!("{}", start + i as u64),
                Mode::Format => format!("{} {}", a.trim_end(), start + i as u64),
            };
            format!("{stem}{ext}")
        })
        .collect()
}

/// What would happen, or why it cannot.
pub fn plan(items: &[PathBuf], dirs: &[bool], mode: Mode, a: &str, b: &str, before: bool) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let names: Vec<String> = items.iter().map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect();
    let new = new_names(&names, dirs, mode, a, b, before);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let own: std::collections::HashSet<String> = items.iter().map(|p| p.to_string_lossy().to_lowercase()).collect();
    for ((p, old), n) in items.iter().zip(&names).zip(&new) {
        if let Err(e) = fm_fs::validate_name(n) {
            return Err(format!("„{n}“: {e}"));
        }
        let Some(parent) = p.parent() else { continue };
        let target = parent.join(n);
        if !seen.insert(target.to_string_lossy().to_lowercase()) {
            return Err(format!("Dvě položky by se jmenovaly „{n}“."));
        }
        if n == old {
            continue;
        }
        let key = target.to_string_lossy().to_lowercase();
        if target.exists() && !own.contains(&key) {
            return Err(format!("Položka „{n}“ už v té složce je."));
        }
        out.push((p.clone(), target));
    }
    Ok(out)
}

pub fn apply(app: &mut App) {
    let Some(b) = &app.batch else { return };
    let plan = match plan(&b.items, &b.dirs, b.mode, &b.a, &b.b, b.before) {
        Ok(p) => p,
        Err(e) => {
            app.note(&e);
            return;
        }
    };
    if plan.is_empty() {
        close(app);
        return;
    }
    // A target that is another item's current name (a → b while b → c) needs a detour through a
    // temporary name, or the second rename would collide with the first.
    let sources: std::collections::HashSet<String> = plan.iter().map(|(f, _)| f.to_string_lossy().to_lowercase()).collect();
    let chained = plan.iter().any(|(_, t)| sources.contains(&t.to_string_lossy().to_lowercase()));
    let mut steps: Vec<(PathBuf, PathBuf)> = Vec::new();
    if chained {
        for (k, (from, to)) in plan.iter().enumerate() {
            let tmp = from.with_file_name(format!(".fm-rename-{}-{k}", std::process::id()));
            steps.push((from.clone(), tmp.clone()));
            steps.push((tmp, to.clone()));
        }
        // All to temporary names first, then all to the final ones.
        let (first, second): (Vec<_>, Vec<_>) = steps.chunks(2).map(|c| (c[0].clone(), c[1].clone())).unzip();
        steps = first.into_iter().chain(second).collect();
    } else {
        steps = plan.clone();
    }
    let mut done: Vec<crate::undo::Op> = Vec::new();
    let mut failed = None;
    for (from, to) in &steps {
        let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match crate::shell::rename(from, &name) {
            Ok(()) => done.push(crate::undo::Op::Rename { from: from.clone(), to: to.clone() }),
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    if !done.is_empty() {
        crate::undo::push(crate::undo::Op::Group(done));
    }
    let n = plan.len();
    close(app);
    match failed {
        Some(e) => app.note(&format!("Přejmenování se zastavilo: {e} (Ctrl+Z vrátí hotové)")),
        None => app.note(&format!(
            "Přejmenováno: {}",
            match n {
                1 => "1 položka".to_string(),
                2..=4 => format!("{n} položky"),
                _ => format!("{n} položek"),
            }
        )),
    }
    app.reload(None);
}

pub fn paint(app: &App, c: &mut Canvas, t: &Theme) {
    let Some(b) = &app.batch else { return };
    let p = parts(app);
    // Lift the sheet with a soft two-step shadow (Finder does not dim the window behind it).
    for (grow, a) in [(8.0, 0.06), (3.0, 0.10)] {
        c.round(p.sheet.inset(-grow, -grow).offset(0.0, grow / 2.0), 12.0 + grow, Color { r: 0.0, g: 0.0, b: 0.0, a });
    }
    c.round(p.sheet, 12.0, t.card);
    c.round_stroke(p.sheet, 12.0, t.edge, 0.0);
    let n = b.items.len();
    let count = match n {
        2..=4 => format!("{n} položky"),
        _ => format!("{n} položek"),
    };
    c.text(&format!("Přejmenovat {count}"), p.title, Font::BodyStrong, t.ink, Align::Start);

    // Mode switch (segmented, like the view switcher).
    let seg = Rect::new(p.modes[0].x, p.modes[0].y, p.modes[2].r() - p.modes[0].x, ROW);
    c.round(seg, 7.0, crate::paint::over(t.field, t.card));
    for (k, r) in p.modes.iter().enumerate() {
        let on = MODE[k].1 == b.mode;
        if on {
            c.round(r.inset(2.0, 2.0), 5.0, if t.mode == ThemeMode::HighContrast { t.accent } else { t.card });
            c.round_stroke(r.inset(2.0, 2.0), 5.0, t.edge, 0.0);
        } else if app.hover == Hit::Batch(k as u8) {
            c.round(r.inset(2.0, 2.0), 5.0, t.hover);
        }
        let ink = if on && t.mode == ThemeMode::HighContrast { t.on_accent } else if on { t.ink } else { t.ink2 };
        c.text(MODE[k].0, *r, Font::Small, ink, Align::Center);
    }

    let (la, lb) = labels(b.mode);
    let label = |c: &mut Canvas, r: Rect, s: &str| c.text(s, Rect::new(r.x, r.y, LABEL_W, r.h), Font::Body, t.ink2, Align::End);
    label(c, p.row_a, la);
    label(c, p.row_b, lb);
    let field = |c: &mut Canvas, r: Rect| {
        c.round(r, 6.0, crate::paint::over(t.field, t.card));
        c.round_stroke(r, 6.0, t.edge, 0.0);
    };
    field(c, p.field_a);
    if has_b(b.mode) {
        field(c, p.field_b);
    } else {
        let seg = p.field_b;
        c.round(seg, 7.0, crate::paint::over(t.field, t.card));
        for (k, (r, s)) in p.pos.iter().zip(["Za název", "Před název"]).enumerate() {
            let on = b.before == (k == 1);
            if on {
                c.round(r.inset(2.0, 2.0), 5.0, t.card);
                c.round_stroke(r.inset(2.0, 2.0), 5.0, t.edge, 0.0);
            } else if app.hover == Hit::Batch(C_POS + k as u8) {
                c.round(r.inset(2.0, 2.0), 5.0, t.hover);
            }
            c.text(s, *r, Font::Small, if on { t.ink } else { t.ink2 }, Align::Center);
        }
    }

    // Live example (or what is wrong).
    let first = b.items.first().map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let (text, ink) = match plan(&b.items, &b.dirs, b.mode, &b.a, &b.b, b.before) {
        Err(e) => (e, t.danger),
        Ok(_) => {
            let ex = first
                .map(|f| new_names(&[f], &b.dirs[..1], b.mode, &b.a, &b.b, b.before).remove(0))
                .unwrap_or_default();
            (format!("Příklad: {ex}"), t.ink3)
        }
    };
    let ex = c.fit_path(&text, Font::Small, p.example.w);
    c.text(&ex, p.example, Font::Small, ink, Align::Start);

    // Buttons: Cancel, then the accent default.
    let hot = |h: u8| app.hover == Hit::Batch(h);
    c.round(p.cancel, 6.0, if hot(C_CANCEL) { t.hover } else { crate::paint::over(t.field, t.card) });
    c.round_stroke(p.cancel, 6.0, t.edge, 0.0);
    c.text("Zrušit", p.cancel, Font::Body, t.ink, Align::Center);
    c.round(p.ok, 6.0, t.accent);
    if hot(C_OK) {
        c.round(p.ok, 6.0, Color { a: 0.12, ..t.on_accent });
    }
    c.text("Přejmenovat", p.ok, Font::BodyStrong, t.on_accent, Align::Center);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn names() {
        let names = n(&["IMG_001.jpg", "IMG_002.JPG", "Složka"]);
        let dirs = [false, false, true];
        assert_eq!(new_names(&names, &dirs, Mode::Replace, "IMG_", "Léto ", false), n(&["Léto 001.jpg", "Léto 002.JPG", "Složka"]));
        assert_eq!(new_names(&names, &dirs, Mode::Add, "2024 ", "", true), n(&["2024 IMG_001.jpg", "2024 IMG_002.JPG", "2024 Složka"]));
        assert_eq!(new_names(&names, &dirs, Mode::Add, " (kopie)", "", false), n(&["IMG_001 (kopie).jpg", "IMG_002 (kopie).JPG", "Složka (kopie)"]));
        assert_eq!(new_names(&names, &dirs, Mode::Format, "Dovolená", "7", false), n(&["Dovolená 7.jpg", "Dovolená 8.JPG", "Dovolená 9"]));
        assert_eq!(new_names(&n(&[".gitignore"]), &[false], Mode::Add, "x", "", true), n(&["x.gitignore"]));
    }

    #[test]
    fn plan_rejects_collisions() {
        let dir = std::env::temp_dir().join(format!("fm-batch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["a.txt", "b.txt", "keep.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let items = vec![dir.join("a.txt"), dir.join("b.txt")];
        let dirs = [false, false];
        assert!(plan(&items, &dirs, Mode::Format, "keep", "", false).is_ok()); // "keep 1", "keep 2"
        assert!(plan(&items, &dirs, Mode::Replace, "a", "keep", false).unwrap_err().contains("už v té složce"));
        assert!(plan(&items, &dirs, Mode::Replace, "b", "a", false).unwrap_err().contains("Dvě položky"));
        assert!(plan(&items, &dirs, Mode::Add, "?", "", false).is_err());
        // a → b while b → c is fine (handled with temporary names).
        let swap = plan(&items, &dirs, Mode::Format, "", "1", false).unwrap();
        assert_eq!(swap.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
