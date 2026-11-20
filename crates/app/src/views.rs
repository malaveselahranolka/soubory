//! Column view (Miller columns with a preview column), gallery view, and the preview card shared
//! by the preview column, the gallery and the inspector pane.
use crate::app::{App, Focus, Hit, Loc, View};
use crate::fsys::{self, Entry};
use crate::geom::Rect;
use crate::gfx::{Align, Canvas, Font};
use crate::layout::{COL_W, INSET, ROW_H};
use crate::sel::Selection;
use crate::theme::{Color, Mode, Theme};
use std::path::{Path, PathBuf};

pub struct Pane {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub sel: Selection,
    pub scroll: f32,
    pub icon_idx: Vec<i32>,
}

impl Pane {
    pub fn load(app: &App, dir: &Path) -> Pane {
        let mut entries = fsys::list_dir(dir, app.prefs.show_hidden).unwrap_or_default();
        crate::search::known_sizes(&mut entries, dir);
        let t = app.tab();
        fsys::sort(&mut entries, t.sort, t.asc, app.prefs.folders_first);
        let n = entries.len();
        Pane { dir: dir.to_path_buf(), entries, sel: Selection::new(n), scroll: 0.0, icon_idx: vec![i32::MIN; n] }
    }
}

const PREVIEW_MIN: f32 = 300.0;

// ---- column geometry ------------------------------------------------------------------------------------

pub fn col_x(app: &App, k: usize) -> f32 {
    app.lay.body.x + k as f32 * COL_W - app.tab().col_scroll
}

fn active_index(app: &App) -> usize {
    app.tab().cols_left.len()
}

/// Keeps the active column and whatever is right of it on screen.
pub fn reveal_columns(app: &mut App) {
    let b = app.lay.body;
    let k = active_index(app);
    let right_w = if app.tab().col_right.is_some() { COL_W } else { preview_w(app) };
    let need_r = col_x(app, k) + COL_W + right_w;
    let t = app.tab_mut();
    if need_r > b.r() {
        t.col_scroll += need_r - b.r();
    }
    let x = b.x + k as f32 * COL_W - t.col_scroll;
    if x < b.x {
        t.col_scroll -= b.x - x;
    }
    t.col_scroll = t.col_scroll.max(0.0);
}

fn preview_w(app: &App) -> f32 {
    let b = app.lay.body;
    let used = col_x(app, active_index(app)) + COL_W - b.x;
    (b.w - used).max(PREVIEW_MIN)
}

/// After the selection in the active column changed: show the selected folder's contents (or the
/// file preview) in the next column.
pub fn sync_columns(app: &mut App) {
    if app.tab().view != View::Columns {
        return;
    }
    let t = app.tab();
    let idx = t.sel.indices();
    let want = (idx.len() == 1)
        .then(|| idx[0])
        .filter(|&i| t.entries[i].is_dir)
        .map(|i| t.path_of(i))
        .filter(|p| p.is_absolute());
    let have = t.col_right.as_ref().map(|p| p.dir.clone());
    if want != have {
        let pane = want.map(|d| Pane::load(app, &d));
        app.tab_mut().col_right = pane;
    }
    reveal_columns(app);
}

/// Forces the right pane to reload (folder contents changed).
pub fn refresh_columns(app: &mut App) {
    if app.tab().view == View::Columns {
        let t = app.tab_mut();
        t.col_right = None;
        let left: Vec<(PathBuf, Option<String>)> = t
            .cols_left
            .iter()
            .map(|p| (p.dir.clone(), p.sel.first().map(|i| p.entries[i].name.clone())))
            .collect();
        let mut panes = Vec::new();
        for (dir, sel) in left {
            let mut p = Pane::load(app, &dir);
            if let Some(i) = sel.and_then(|n| p.entries.iter().position(|e| e.name == n)) {
                p.sel.select_only(i);
            }
            panes.push(p);
        }
        app.tab_mut().cols_left = panes;
        sync_columns(app);
    }
}

pub fn reset_columns(app: &mut App) {
    let t = app.tab_mut();
    t.cols_left.clear();
    t.col_right = None;
    t.col_scroll = 0.0;
}

fn take_active(app: &mut App) -> Pane {
    let t = app.tab_mut();
    let dir = t.dir();
    Pane {
        dir,
        entries: std::mem::take(&mut t.entries),
        sel: std::mem::take(&mut t.sel),
        scroll: t.scroll,
        icon_idx: std::mem::take(&mut t.icon_idx),
    }
}

fn set_active(app: &mut App, p: Pane) {
    let t = app.tab_mut();
    t.loc = Loc::Folder(p.dir);
    t.entries = p.entries;
    t.groups.clear();
    t.sel = p.sel;
    t.scroll = p.scroll;
    t.icon_idx = p.icon_idx;
    crate::win::set_title(app);
    crate::win::watch_current(app);
    crate::search::request_sizes(app);
}

/// Right arrow / click into the next column.
pub fn column_right(app: &mut App, select_first: bool) -> bool {
    let Some(right) = app.tab_mut().col_right.take() else { return false };
    if right.entries.is_empty() && select_first {
        app.tab_mut().col_right = Some(right);
        return false;
    }
    let active = take_active(app);
    app.tab_mut().cols_left.push(active);
    set_active(app, right);
    if select_first && !app.tab().entries.is_empty() {
        app.tab_mut().sel.select_only(0);
    }
    sync_columns(app);
    true
}

/// Left arrow: back to the parent column, keeping its selection.
pub fn column_left(app: &mut App) -> bool {
    let Some(parent) = app.tab_mut().cols_left.pop() else { return false };
    let active = take_active(app);
    set_active(app, parent);
    app.tab_mut().col_right = Some(active);
    reveal_columns(app);
    true
}

/// Makes left column `k` active (a click there).
fn activate_left(app: &mut App, k: usize) {
    while app.tab().cols_left.len() > k {
        column_left(app);
    }
}

pub fn columns_hit(app: &App, x: f32, y: f32) -> Hit {
    let t = app.tab();
    let k_active = t.cols_left.len();
    let k = ((x - app.lay.body.x + t.col_scroll) / COL_W).floor();
    if k < 0.0 {
        return Hit::Empty;
    }
    let k = k as usize;
    let row = |scroll: f32, n: usize| {
        let i = ((y - app.lay.body.y - 4.0 + scroll) / ROW_H).floor();
        (i >= 0.0 && (i as usize) < n).then_some(i as usize)
    };
    if k < k_active {
        let p = &t.cols_left[k];
        return row(p.scroll, p.entries.len()).map(|r| Hit::ColRow(k, r)).unwrap_or(Hit::ColEmpty(k));
    }
    if k == k_active {
        return app.item_at(x, y).map(Hit::Row).unwrap_or(Hit::Empty);
    }
    if k == k_active + 1 {
        if let Some(p) = &t.col_right {
            return row(p.scroll, p.entries.len()).map(|r| Hit::ColRow(k, r)).unwrap_or(Hit::ColEmpty(k));
        }
    }
    Hit::Empty
}

/// The pane shown in column `k` other than the active one (left columns, or the right one).
pub fn pane(app: &App, k: usize) -> Option<&Pane> {
    let t = app.tab();
    if k < t.cols_left.len() {
        t.cols_left.get(k)
    } else if k == t.cols_left.len() + 1 {
        t.col_right.as_ref()
    } else {
        None
    }
}

pub fn col_rect(app: &App, k: usize) -> Rect {
    let b = app.lay.body;
    Rect::new(col_x(app, k), b.y, COL_W, b.h)
}

pub fn col_row_rect(app: &App, k: usize, r: usize) -> Rect {
    let scroll = pane(app, k).map(|p| p.scroll).unwrap_or(0.0);
    Rect::new(col_x(app, k), app.lay.body.y + 4.0 + r as f32 * ROW_H - scroll, COL_W, ROW_H).inset(INSET, 1.0)
}

/// Click in a non-active column: make it active and select the row.
pub fn column_click(app: &mut App, k: usize, row: Option<usize>) {
    let active = active_index(app);
    if k < active {
        activate_left(app, k);
    } else if k == active + 1 {
        column_right(app, false);
    }
    app.focus = Focus::Content;
    match row {
        Some(r) => app.tab_mut().sel.select_only(r),
        None => app.tab_mut().sel.clear(),
    }
    sync_columns(app);
}

/// Mouse wheel over a column scrolls that column.
pub fn column_wheel(app: &mut App, x: f32, step: f32) {
    let body = app.lay.body;
    let k = ((x - body.x + app.tab().col_scroll) / COL_W).floor().max(0.0) as usize;
    let active = active_index(app);
    let max = |n: usize| (n as f32 * ROW_H + 8.0 - body.h).max(0.0);
    let t = app.tab_mut();
    if k < active {
        let p = &mut t.cols_left[k];
        p.scroll = (p.scroll - step).clamp(0.0, max(p.entries.len()));
    } else if k == active {
        t.scroll -= step;
        app.clamp_scroll();
    } else if let Some(p) = &mut t.col_right {
        p.scroll = (p.scroll - step).clamp(0.0, max(p.entries.len()));
    }
}

// ---- column painting --------------------------------------------------------------------------------------

fn sel_colors(app: &App, t: &Theme) -> (Color, Color) {
    if (app.active && app.focus == Focus::Content) || t.mode == Mode::HighContrast { (t.accent, t.on_accent) } else { (t.sel_inactive, t.ink) }
}

pub fn paint_columns(app: &mut App, c: &mut Canvas, t: &Theme) {
    let b = app.lay.body;
    let active = active_index(app);
    // Left columns.
    for k in 0..active {
        let x = col_x(app, k);
        if x + COL_W < b.x || x > b.r() {
            continue;
        }
        let (scroll, n) = { let p = &app.tab().cols_left[k]; (p.scroll, p.entries.len()) };
        for i in visible(scroll, n, b.h) {
            let r = Rect::new(x, b.y + 4.0 + i as f32 * ROW_H - scroll, COL_W, ROW_H);
            let e = app.tab().cols_left[k].entries[i].clone();
            let selected = app.tab().cols_left[k].sel.is(i);
            let dir = app.tab().cols_left[k].dir.clone();
            let idx = crate::fileicons::own_in(&mut app.icons, &e, &dir, false).ok_or_else(|| cached_icon(app, &dir, &e, Slot::Left(k, i)));
            let mask = crate::tags::mask(&e.path_in(&dir));
            column_row(app, c, t, r, &e, idx, mask, if selected { Some((t.sel_inactive, t.ink)) } else { None });
        }
        c.vline(x + COL_W, b.y, b.b(), t.edge);
    }
    // Active column.
    let x = col_x(app, active);
    let (sel_fill, sel_ink) = sel_colors(app, t);
    let n = app.tab().entries.len();
    let renaming = crate::win::renaming_index(app);
    if n == 0 {
        let msg = if app.tab().error.is_some() { "Složku nelze otevřít" } else { "Prázdná složka" };
        c.text(msg, Rect::new(x, b.y + 12.0, COL_W, 24.0), Font::Small, t.ink3, Align::Center);
    }
    for i in visible(app.tab().scroll, n, b.h) {
        let r = app.row_rect(i);
        let e = app.tab().entries[i].clone();
        let dir = app.tab().dir();
        // The current place's items, as in the other views (shell places keep the shell's icons).
        let idx = crate::paint::own_icon(app, &e).ok_or_else(|| crate::paint::ensure_icon(app, i));
        let selected = app.tab().sel.is(i);
        if app.kb && app.tab().sel.focus == Some(i) && !selected && app.focus == Focus::Content {
            c.round_stroke(r.inset(INSET / 2.0, 1.0), 6.0, t.focus, 1.5);
        }
        let mut e2 = e.clone();
        if renaming == Some(i) {
            e2.name.clear();
        }
        let mask = crate::tags::mask(&e.path_in(&dir));
        column_row(app, c, t, r, &e2, idx, mask, selected.then_some((sel_fill, sel_ink)));
    }
    c.vline(x + COL_W, b.y, b.b(), t.edge);
    // Right: the selected folder's contents, or a preview of the selected file.
    let rx = x + COL_W;
    if app.tab().col_right.is_some() {
        let (scroll, n) = { let p = app.tab().col_right.as_ref().unwrap(); (p.scroll, p.entries.len()) };
        if n == 0 {
            c.text("Prázdná složka", Rect::new(rx, b.y + 12.0, COL_W, 24.0), Font::Small, t.ink3, Align::Center);
        }
        for i in visible(scroll, n, b.h) {
            let r = Rect::new(rx, b.y + 4.0 + i as f32 * ROW_H - scroll, COL_W, ROW_H);
            let p = app.tab().col_right.as_ref().unwrap();
            let (e, dir) = (p.entries[i].clone(), p.dir.clone());
            let idx = crate::fileicons::own_in(&mut app.icons, &e, &dir, false).ok_or_else(|| cached_icon(app, &dir, &e, Slot::Right(i)));
            let mask = crate::tags::mask(&e.path_in(&dir));
            column_row(app, c, t, r, &e, idx, mask, None);
        }
        c.vline(rx + COL_W, b.y, b.b(), t.edge);
    } else if let Some(f) = app.tab().sel.focus.filter(|&f| app.tab().sel.count() == 1 && app.tab().sel.is(f)) {
        let e = app.tab().entries[f].clone();
        let path = app.tab().path_of(f);
        let r = Rect::new(rx, b.y, preview_w(app).max(b.r() - rx), b.h);
        preview_card(app, c, t, r, &path, &e, true);
    }
}

enum Slot {
    Left(usize, usize),
    Right(usize),
}

fn cached_icon(app: &mut App, dir: &Path, e: &Entry, slot: Slot) -> i32 {
    let cur = match &slot {
        Slot::Left(k, i) => app.tab().cols_left[*k].icon_idx[*i],
        Slot::Right(i) => app.tab().col_right.as_ref().unwrap().icon_idx[*i],
    };
    if cur != i32::MIN {
        return cur;
    }
    let idx = app.icons.index(dir, e);
    let t = app.tab_mut();
    match slot {
        Slot::Left(k, i) => t.cols_left[k].icon_idx[i] = idx,
        Slot::Right(i) => t.col_right.as_mut().unwrap().icon_idx[i] = idx,
    }
    idx
}

fn visible(scroll: f32, n: usize, h: f32) -> std::ops::Range<usize> {
    let first = (scroll / ROW_H).floor().max(0.0) as usize;
    let last = (((scroll + h) / ROW_H).ceil() as usize + 1).min(n);
    first.min(last)..last
}

#[allow(clippy::too_many_arguments)]
fn column_row(app: &mut App, c: &mut Canvas, t: &Theme, r: Rect, e: &Entry, icon: Result<crate::fileicons::Own, i32>, tags: u8, sel: Option<(Color, Color)>) {
    let cap = r.inset(INSET / 2.0, 1.0);
    let (ink, bg) = match sel {
        Some((fill, ink)) => {
            c.round(cap, 6.0, fill);
            (ink, fill)
        }
        None => (t.ink, t.card),
    };
    let dim = if e.is_hidden() { 0.55 } else { 1.0 };
    let ir = Rect::new(cap.x + 6.0, r.cy() - 8.0, 16.0, 16.0);
    match icon {
        // Glyphs take the text colour on the accent fill, keep their own on the inactive one.
        Ok(o) => crate::fileicons::draw(c, t, o, e, ir, sel.filter(|s| s.0 == t.accent).map(|s| s.1), dim),
        Err(idx) => app.icons.draw(c, idx, ir, dim),
    }
    let mut name_r = Rect::new(cap.x + 28.0, r.y, cap.w - 28.0 - if e.is_dir { 20.0 } else { 6.0 }, r.h);
    if tags != 0 {
        let w = crate::paint::tag_dots(c, tags, name_r.r() - 2.0, r.cy(), bg);
        name_r = name_r.cut_right(w + 6.0);
    }
    c.text(&e.name, name_r, Font::Body, Color { a: ink.a * dim, ..ink }, Align::Start);
    if e.is_dir {
        c.icon(crate::paint::g::RIGHT, crate::phosphor::Weight::Bold, Rect::new(cap.r() - 20.0, r.y, 16.0, r.h), 10.0, if sel.is_some() { ink } else { t.ink3 });
    }
}

// ---- gallery --------------------------------------------------------------------------------------------------

const STRIP_H: f32 = 96.0;
const TILE: f32 = 64.0;
const TILE_STEP: f32 = 76.0;

pub fn gallery_parts(app: &App) -> (Rect, Rect) {
    let b = app.lay.body;
    (b.cut_bottom(STRIP_H), b.bottom(STRIP_H))
}

pub fn strip_tile(app: &App, i: usize) -> Rect {
    let s = gallery_parts(app).1;
    Rect::new(s.x + 16.0 + i as f32 * TILE_STEP - app.tab().scroll, s.y + (s.h - TILE) / 2.0, TILE, TILE)
}

pub fn strip_width(app: &App) -> f32 {
    32.0 + app.tab().entries.len() as f32 * TILE_STEP
}

pub fn paint_gallery(app: &mut App, c: &mut Canvas, t: &Theme) {
    let (main, strip) = gallery_parts(app);
    c.hline(strip.x, strip.r(), strip.y, t.edge);
    let n = app.tab().entries.len();
    let focus = app.tab().sel.focus.filter(|&f| f < n).or((n > 0).then_some(0));
    if let Some(f) = focus {
        let e = app.tab().entries[f].clone();
        let path = app.tab().path_of(f);
        big_preview(app, c, t, main.inset(32.0, 24.0).cut_bottom(48.0), &path, &e);
        let cap = main.bottom(56.0).inset(32.0, 0.0);
        c.text(&e.name, cap.top(28.0), Font::BodyStrong, t.ink, Align::Center);
        c.text(&kind_and_size(&e), cap.cut_top(28.0).top(20.0), Font::Small, t.ink2, Align::Center);
    }
    c.push_clip(strip);
    for i in 0..n {
        let r = strip_tile(app, i);
        if r.r() < strip.x || r.x > strip.r() {
            continue;
        }
        let e = app.tab().entries[i].clone();
        let selected = app.tab().sel.is(i);
        let path = app.tab().path_of(i);
        let own = crate::paint::own_icon(app, &e);
        match crate::paint::thumb(app, c, &path, &e, own, TILE) {
            Some((bmp, w, h, _)) => {
                let k = (TILE / w).min(TILE / h);
                c.bitmap(&bmp, r.center(w * k, h * k).snap(c.scale), 1.0);
            }
            None => match own {
                Some(o) => crate::fileicons::draw(c, t, o, &e, r.center(48.0, 48.0), None, 1.0),
                None => {
                    let idx = crate::paint::ensure_icon(app, i);
                    app.icons.draw(c, idx, r.center(48.0, 48.0), 1.0);
                }
            },
        }
        if selected {
            let (fill, _) = sel_colors(app, t);
            c.round_stroke(r.inset(-4.0, -4.0), 8.0, fill, 2.5);
        }
    }
    c.pop_clip();
}

fn kind_and_size(e: &Entry) -> String {
    if !e.size_known() { fsys::kind_name(e) } else { format!("{} – {}", fsys::kind_name(e), fsys::format_size(e.size)) }
}

/// Large image (thumbnail or icon) fitted into `r`.
fn big_preview(app: &mut App, c: &mut Canvas, t: &Theme, r: Rect, path: &Path, e: &Entry) {
    let side = r.w.min(r.h).max(32.0);
    let own = crate::paint::own_icon(app, e);
    match crate::paint::thumb(app, c, path, e, own, side.min(1600.0 / c.scale)) {
        Some((bmp, w, h, is_thumb)) => {
            let is_thumb = is_thumb && !e.is_dir;
            let k = if is_thumb { (r.w / w).min(r.h / h).min(4.0) } else { (side.min(256.0) / w.max(h)).min(r.w / w) };
            let dst = r.center(w * k, h * k).snap(c.scale);
            c.bitmap(&bmp, dst, 1.0);
            if is_thumb {
                c.round_stroke(dst, 2.0, t.edge, 0.0);
            }
        }
        None => {
            let s = side.min(128.0);
            if let Some(o) = own {
                crate::fileicons::draw(c, t, o, e, r.center(s, s), None, 1.0);
                return;
            }
            let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let idx = match &e.shell {
                Some(id) if e.full.is_none() => crate::shellns::icon_index(id),
                _ if e.full.is_some() && e.is_dir => app.icons.index_for_path(path),
                _ => app.icons.index(&dir, e),
            };
            app.icons.draw(c, idx, r.center(s, s), 1.0);
        }
    }
}

// ---- preview card (preview column, inspector) ------------------------------------------------------------------

pub fn preview_card(app: &mut App, c: &mut Canvas, t: &Theme, r: Rect, path: &Path, e: &Entry, column: bool) {
    let pad = 20.0;
    let inner = r.inset(pad, pad);
    if inner.w < 60.0 {
        return;
    }
    let img_h = (inner.w * if column { 0.75 } else { 0.8 }).min(inner.h * 0.5).min(320.0);
    big_preview(app, c, t, inner.top(img_h), path, e);
    let mut y = inner.y + img_h + 16.0;
    let name_h = c.g.measure_wrapped(&e.name, Font::BodyStrong, inner.w).min(60.0);
    c.text_wrapped(&e.name, Rect::new(inner.x, y, inner.w, name_h + 2.0), Font::BodyStrong, t.ink, Align::Center);
    y += name_h + 4.0;
    c.text(&kind_and_size(e), Rect::new(inner.x, y, inner.w, 18.0), Font::Small, t.ink2, Align::Center);
    y += 26.0;
    let mask = crate::tags::mask(path);
    if mask != 0 {
        let count = mask.count_ones() as f32;
        let mut x = inner.cx() - count * 8.0 + 4.0;
        for i in 0..7 {
            if mask & (1 << i) != 0 {
                c.circle(x, y + 5.0, 5.0, Theme::tag(i));
                x += 16.0;
            }
        }
        y += 18.0;
    }
    c.hline(inner.x, inner.r(), y, t.edge);
    y += 10.0;
    c.text("Informace", Rect::new(inner.x, y, inner.w, 20.0), Font::CaptionStrong, t.ink3, Align::Start);
    y += 22.0;
    let mut rows: Vec<(&str, String)> = vec![("Vytvořeno", fsys::format_date_long(e.created)), ("Změněno", fsys::format_date_long(e.modified))];
    if e.size_known() {
        rows.push(("Velikost", format!("{} ({} bajtů)", fsys::format_size(e.size), group_digits(e.size))));
    }
    if let Some(parent) = path.parent() {
        rows.push(("Umístění", parent.to_string_lossy().into_owned()));
    }
    if e.is_placeholder() {
        rows.push(("Stav", "Jen online (OneDrive)".into()));
    }
    for (label, value) in rows {
        if value.is_empty() || y + 18.0 > inner.b() {
            continue;
        }
        let lw = 76.0;
        c.text(label, Rect::new(inner.x, y, lw, 18.0), Font::Caption, t.ink3, Align::Start);
        let value = if label == "Umístění" { c.fit_path(&value, Font::Small, inner.w - lw) } else { value };
        c.text(&value, Rect::new(inner.x + lw, y, inner.w - lw, 18.0), Font::Small, t.ink, Align::End);
        y += 20.0;
    }
}

fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (k, ch) in s.chars().enumerate() {
        if k > 0 && (s.len() - k) % 3 == 0 {
            out.push('\u{a0}');
        }
        out.push(ch);
    }
    out
}

/// Inspector pane (Ctrl+Shift+P): the focused item, or the current folder.
pub fn paint_inspector(app: &mut App, c: &mut Canvas, t: &Theme, r: Rect) {
    c.vline(r.x, r.y, r.b(), t.edge);
    let tab = app.tab();
    let item = tab.sel.focus.filter(|&f| tab.sel.is(f)).or(tab.sel.first()).map(|i| (tab.path_of(i), tab.entries[i].clone()));
    match item {
        Some((path, e)) => preview_card(app, c, t, r, &path, &e, false),
        None => {
            if let Some(dir) = tab.loc.folder().map(Path::to_path_buf) {
                let e = Entry {
                    name: fsys::display_name(&dir),
                    is_dir: true,
                    size: 0,
                    modified: 0,
                    created: 0,
                    // Read-only marks it as possibly customised: fm's icon decision then asks the
                    // shell's (cached) icon for this path instead of reading attributes on every paint.
                    attrs: 0x10 | 0x01,
                    full: Some(dir.clone()),
                    shell: None,
                    origin: None,
                    sized: false,
                };
                preview_card(app, c, t, r, &dir, &e, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_groups() {
        assert_eq!(group_digits(1234567), "1\u{a0}234\u{a0}567");
        assert_eq!(group_digits(12), "12");
    }

    #[test]
    fn visible_range_is_clamped() {
        assert_eq!(visible(0.0, 3, 500.0), 0..3);
        assert_eq!(visible(260.0, 5, 100.0), 5..5);
    }
}
