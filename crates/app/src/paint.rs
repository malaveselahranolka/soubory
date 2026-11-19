//! Painting of the whole window, plus the geometry helpers hit-testing shares with it.
use crate::app::{App, Cap, Drag, Focus, Hit, Loc, View};
use crate::fsys::{self, SortKey};
use crate::geom::Rect;
use crate::gfx::{Align, Canvas, Font};
use crate::layout::{self, Col, INSET, ROW_H, SIDE_HEADER, SIDE_ROW};
use crate::sidebar::Row;
use crate::theme::{Color, Mode, Theme};
use windows::Win32::Foundation::POINT;

pub mod g {
    pub const PANE: char = '\u{E8A0}';
    pub const BACK: char = '\u{E72B}';
    pub const FWD: char = '\u{E72A}';
    pub const MORE: char = '\u{E712}';
    pub const SEARCH: char = '\u{E721}';
    pub const MIN: char = '\u{E921}';
    pub const MAX: char = '\u{E922}';
    pub const RESTORE: char = '\u{E923}';
    pub const CLOSE: char = '\u{E8BB}';
    pub const UP: char = '\u{E70E}';
    pub const DOWN: char = '\u{E70D}';
    pub const RIGHT: char = '\u{E76C}';
    pub const ADD: char = '\u{E710}';
    pub const CLOUD: char = '\u{E753}';
    pub const PINNED: char = '\u{E73E}';
    pub const WARN: char = '\u{E7BA}';
    pub const EMPTY: char = '\u{E8B7}';
    pub const VIEW_ICONS: char = '\u{F0E2}';
    pub const VIEW_LIST: char = '\u{EA37}';
    pub const VIEW_COLUMNS: char = '\u{E8C0}';
    pub const VIEW_GALLERY: char = '\u{E8B9}';
}

fn with_alpha(c: Color, a: f32) -> Color {
    Color { a: c.a * a, ..c }
}

/// Opaque colour of `top` composited over `bottom`.
pub fn over(top: Color, bottom: Color) -> Color {
    Color {
        r: top.r * top.a + bottom.r * (1.0 - top.a),
        g: top.g * top.a + bottom.g * (1.0 - top.a),
        b: top.b * top.a + bottom.b * (1.0 - top.a),
        a: 1.0,
    }
}

pub fn paint(app: &mut App, c: &mut Canvas) {
    let t = app.theme.clone();
    let l = app.lay.clone();
    // Chrome: Mica shows through when it is on, otherwise the opaque token.
    if !app.mica || t.mode == Mode::HighContrast {
        c.fill(Rect::new(0.0, 0.0, app.w, app.h), t.chrome);
    }
    toolbar(app, c, &t);
    if app.sidebar_on {
        sidebar(app, c, &t);
    }
    // Content card.
    let radius = if app.sidebar_on && !app.maximized {
        layout::CARD_RADIUS
    } else {
        0.0
    };
    c.round_corners(l.card, [radius, 0.0, 0.0, 0.0], t.card, Some(t.edge));
    if let Some(tr) = l.tabs {
        tabs(app, c, &t, tr);
    }
    c.push_clip(l.body);
    match app.tab().view {
        View::Icons => icon_view(app, c, &t),
        View::Columns => crate::views::paint_columns(app, c, &t),
        View::Gallery => crate::views::paint_gallery(app, c, &t),
        View::List => list_view(app, c, &t),
    }
    band(app, c, &t);
    if let Some(r) = crate::win::rename_rect(app) {
        let f = r.inset(-5.0, -3.0);
        c.round(f, 5.0, t.card);
        c.round_stroke(f, 5.0, t.focus, 1.5);
    }
    c.pop_clip();
    if let Some(h) = l.header {
        header(app, c, &t, h);
    }
    if let Some(p) = l.preview {
        crate::views::paint_inspector(app, c, &t, p);
    }
    status(app, c, &t);
}

// ---- toolbar --------------------------------------------------------------------------------------

fn button(app: &App, c: &mut Canvas, t: &Theme, r: Rect, hit: Hit, glyph: char, enabled: bool) {
    let bg = if !enabled {
        None
    } else if app.pressed == hit && app.hover == hit {
        Some(t.press)
    } else if app.hover == hit {
        Some(t.hover)
    } else {
        None
    };
    if let Some(bg) = bg {
        c.round(r, 6.0, bg);
    }
    let ink = if !enabled {
        t.ink4
    } else if app.active {
        t.ink
    } else {
        t.ink2
    };
    c.glyph(glyph, r, 16, ink);
}

fn toolbar(app: &App, c: &mut Canvas, t: &Theme) {
    let l = &app.lay;
    button(app, c, t, l.sidebar_btn, Hit::SidebarBtn, g::PANE, true);
    let tab = app.tab();
    button(app, c, t, l.back, Hit::Back, g::BACK, tab.hist.can_back());
    button(app, c, t, l.fwd, Hit::Fwd, g::FWD, tab.hist.can_forward());
    c.text(
        &tab.loc.title(),
        l.title,
        Font::Title,
        if app.active { t.ink } else { t.ink2 },
        Align::Start,
    );

    // View switcher: one segmented control.
    let seg = l.views[0].union(&l.views[3]);
    c.round(seg, 7.0, t.field);
    let cur = match tab.view {
        View::Icons => 0,
        View::List => 1,
        View::Columns => 2,
        View::Gallery => 3,
    };
    for (k, (r, gl)) in l
        .views
        .iter()
        .zip([
            g::VIEW_ICONS,
            g::VIEW_LIST,
            g::VIEW_COLUMNS,
            g::VIEW_GALLERY,
        ])
        .enumerate()
    {
        let inner = r.inset(2.0, 2.0);
        if k == cur {
            let fill = if t.is_dark() {
                Color {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.12,
                }
            } else {
                t.card
            };
            c.round(inner, 5.0, fill);
            if !t.is_dark() {
                c.round_stroke(inner, 5.0, t.edge, 0.0);
            }
        } else if app.hover == Hit::View(k) {
            c.round(inner, 5.0, t.hover);
        }
        c.glyph(gl, *r, 14, if k == cur { t.ink } else { t.ink2 });
    }
    button(app, c, t, l.more, Hit::More, g::MORE, true);

    // Search field.
    let fill = over(t.field, t.chrome);
    c.round(l.search, 6.0, fill);
    let edge = if app.focus == Focus::Search {
        t.focus
    } else {
        t.edge
    };
    c.round_stroke(
        l.search,
        6.0,
        edge,
        if app.focus == Focus::Search { 1.5 } else { 0.0 },
    );
    c.glyph(g::SEARCH, l.search.left(30.0), 12, t.ink3);
    if !crate::win::search_active(app) {
        let (txt, ink) = if app.search_text.is_empty() {
            ("Hledat", t.ink3)
        } else {
            (app.search_text.as_str(), t.ink)
        };
        c.text(
            txt,
            l.search.cut_left(30.0).cut_right(8.0),
            Font::Body,
            ink,
            Align::Start,
        );
    }

    // Caption buttons (Windows).
    for (k, r) in l.caption.iter().enumerate() {
        let cap = [Cap::Min, Cap::Max, Cap::Close][k];
        let hit = Hit::Caption(cap);
        let hot = app.hover == hit;
        let pressed = hot && app.pressed == hit;
        let mut ink = if app.active { t.ink } else { t.ink3 };
        if cap == Cap::Close && hot && t.mode != Mode::HighContrast {
            c.fill(
                *r,
                if pressed {
                    crate::theme::rgb(0x94221A)
                } else {
                    crate::theme::rgb(0xC42B1C)
                },
            );
            ink = crate::theme::rgb(0xFFFFFF);
        } else if pressed {
            c.fill(*r, t.press);
        } else if hot {
            c.fill(*r, t.hover);
        }
        let gl = match cap {
            Cap::Min => g::MIN,
            Cap::Max if app.maximized => g::RESTORE,
            Cap::Max => g::MAX,
            Cap::Close => g::CLOSE,
        };
        c.glyph(gl, *r, 10, ink);
    }
}

// ---- sidebar --------------------------------------------------------------------------------------

/// Y offset of each sidebar row (relative to the sidebar top, before scrolling).
fn side_offsets(app: &App) -> Vec<(f32, f32)> {
    let mut y = 4.0;
    let mut out = Vec::with_capacity(app.side_rows.len());
    for (k, row) in app.side_rows.iter().enumerate() {
        let h = match row {
            Row::Header(_) => {
                if k > 0 {
                    y += 10.0;
                }
                SIDE_HEADER
            }
            _ => SIDE_ROW,
        };
        out.push((y, h));
        y += h;
    }
    out
}

pub fn sidebar_height(app: &App) -> f32 {
    side_offsets(app)
        .last()
        .map(|(y, h)| y + h + 12.0)
        .unwrap_or(0.0)
}

pub fn side_row_at(app: &App, y: f32) -> Option<usize> {
    let top = app.lay.sidebar.y - app.side_scroll;
    side_offsets(app)
        .iter()
        .position(|(oy, h)| y >= top + oy && y < top + oy + h)
        .filter(|&k| !matches!(app.side_rows[k], Row::Header(_)))
}

fn side_selected(app: &App, row: &Row) -> bool {
    match row {
        Row::Item { loc, .. } => &app.tab().loc == loc,
        Row::Tag(i) => app.tab().loc == Loc::Tag(*i),
        Row::Header(_) => false,
    }
}

fn sidebar(app: &App, c: &mut Canvas, t: &Theme) {
    let s = app.lay.sidebar;
    c.push_clip(s);
    let top = s.y - app.side_scroll;
    for ((oy, h), (k, row)) in side_offsets(app)
        .into_iter()
        .zip(app.side_rows.iter().enumerate())
    {
        let r = Rect::new(s.x, top + oy, s.w, h);
        if r.b() < s.y || r.y > s.b() {
            continue;
        }
        match row {
            Row::Header(title) => {
                c.text(
                    title,
                    Rect::new(r.x + 18.0, r.y + 6.0, r.w - 26.0, r.h - 6.0),
                    Font::CaptionStrong,
                    t.ink3,
                    Align::Start,
                );
            }
            _ => {
                let pill = r.inset(INSET, 1.0);
                let selected = side_selected(app, row);
                if selected {
                    c.round(
                        pill,
                        6.0,
                        if t.mode == Mode::HighContrast {
                            t.accent
                        } else {
                            t.pill
                        },
                    );
                    // Fluent selection indicator.
                    let ind = Rect::new(pill.x, pill.cy() - 8.0, 3.0, 16.0);
                    c.round(ind, 1.5, t.accent_glyph);
                } else if app.hover == Hit::SideRow(k) {
                    c.round(pill, 6.0, t.hover);
                }
                let ink = if selected && t.mode == Mode::HighContrast {
                    t.on_accent
                } else {
                    t.ink
                };
                let icon = Rect::new(pill.x + 8.0, pill.y, 20.0, pill.h);
                let label_r = Rect::new(icon.r() + 8.0, pill.y, pill.r() - icon.r() - 16.0, pill.h);
                match row {
                    Row::Item {
                        label,
                        glyph,
                        eject,
                        ..
                    } => {
                        c.glyph(
                            *glyph,
                            icon,
                            16,
                            if t.mode == Mode::HighContrast {
                                ink
                            } else {
                                t.accent_glyph
                            },
                        );
                        let lr = if *eject {
                            label_r.cut_right(20.0)
                        } else {
                            label_r
                        };
                        c.text(label, lr, Font::Body, ink, Align::Start);
                        if *eject {
                            c.glyph(
                                crate::sidebar::glyph::EJECT,
                                label_r.right(20.0),
                                12,
                                t.ink3,
                            );
                        }
                    }
                    Row::Tag(i) => {
                        c.circle(icon.cx(), icon.cy(), 5.0, Theme::tag(*i));
                        c.text(
                            crate::theme::TAGS[*i].1,
                            label_r,
                            Font::Body,
                            ink,
                            Align::Start,
                        );
                    }
                    Row::Header(_) => {}
                }
            }
        }
    }
    c.pop_clip();
    if app.hover == Hit::SideResize || matches!(app.drag, Drag::Sidebar { .. }) {
        c.vline(s.r() - 1.0, s.y, s.b(), t.edge);
    }
}

// ---- tabs -----------------------------------------------------------------------------------------

fn tab_rects(app: &App, r: Rect) -> (Vec<Rect>, Rect) {
    let plus = r.right(36.0);
    let area = r.cut_right(36.0);
    let n = app.tabs.len() as f32;
    let w = area.w / n;
    (
        (0..app.tabs.len())
            .map(|k| Rect::new(area.x + w * k as f32, r.y, w, r.h))
            .collect(),
        plus,
    )
}

pub fn tab_hit(app: &App, x: f32, y: f32) -> Hit {
    let Some(r) = app.lay.tabs else {
        return Hit::None;
    };
    let (rects, plus) = tab_rects(app, r);
    if plus.contains(x, y) {
        return Hit::NewTab;
    }
    for (k, tr) in rects.iter().enumerate() {
        if tr.contains(x, y) {
            if tr.left(28.0).contains(x, y) {
                return Hit::TabClose(k);
            }
            return Hit::Tab(k);
        }
    }
    Hit::Empty
}

fn tabs(app: &App, c: &mut Canvas, t: &Theme, r: Rect) {
    let strip = over(t.field, t.card);
    let radius = if app.sidebar_on && !app.maximized {
        layout::CARD_RADIUS
    } else {
        0.0
    };
    c.round_corners(r, [radius, 0.0, 0.0, 0.0], strip, None);
    let (rects, plus) = tab_rects(app, r);
    for (k, tr) in rects.iter().enumerate() {
        let active = k == app.cur;
        if active {
            c.fill(*tr, t.card);
            c.vline(tr.x, tr.y, tr.b(), t.edge);
            c.vline(tr.r(), tr.y, tr.b(), t.edge);
        } else {
            if app.hover == Hit::Tab(k) || app.hover == Hit::TabClose(k) {
                c.fill(*tr, t.hover);
            }
            c.hline(tr.x, tr.r(), tr.b() - 1.0 / c.scale, t.edge);
        }
        let title = app.tabs[k].loc.title();
        c.text(
            &title,
            tr.inset(32.0, 0.0),
            if active { Font::Small } else { Font::Small },
            if active { t.ink } else { t.ink2 },
            Align::Center,
        );
        if app.hover == Hit::Tab(k) || app.hover == Hit::TabClose(k) {
            let x = tr.left(28.0).center(18.0, 18.0).offset(6.0, 0.0);
            if app.hover == Hit::TabClose(k) {
                c.round(x, 4.0, t.hover);
            }
            c.glyph(g::CLOSE, x, 8, t.ink2);
        }
    }
    if app.hover == Hit::NewTab {
        c.round(plus.center(26.0, 24.0), 5.0, t.hover);
    }
    c.glyph(g::ADD, plus, 12, t.ink2);
    c.hline(plus.x, plus.r(), plus.b() - 1.0 / c.scale, t.edge);
}

// ---- list view --------------------------------------------------------------------------------------

pub fn list_cols(app: &App) -> Vec<(Col, f32)> {
    let mut v = vec![(Col::Name, 0.0)];
    if matches!(
        app.tab().loc,
        Loc::Search { .. } | Loc::Recents | Loc::Tag(_)
    ) {
        v.push((Col::Location, 220.0));
    }
    v.push((Col::Modified, 150.0));
    v.push((Col::Size, 92.0));
    v.push((Col::Kind, 150.0));
    // Drop optional columns on narrow windows, kind first (Finder does the same).
    let mut budget = app.lay.body.w - 260.0;
    v.retain(|(c, w)| {
        if *c == Col::Name {
            return true;
        }
        budget -= w;
        budget >= 0.0
    });
    v
}

fn col_layout(app: &App) -> Vec<(Col, f32, f32)> {
    let b = app.lay.body;
    layout::columns(b.x, b.w, &list_cols(app))
}

pub fn name_col_width(app: &App) -> f32 {
    col_layout(app).first().map(|c| c.2).unwrap_or(200.0)
}

pub fn header_hit(app: &App, x: f32) -> Option<Col> {
    col_layout(app)
        .into_iter()
        .find(|(_, cx, cw)| x >= *cx && x < cx + cw)
        .map(|c| c.0)
}

fn header(app: &App, c: &mut Canvas, t: &Theme, h: Rect) {
    let tab = app.tab();
    for (k, (col, cx, cw)) in col_layout(app).into_iter().enumerate() {
        let r = Rect::new(cx, h.y, cw, h.h);
        if app.hover == Hit::Header(col) {
            c.round(r.inset(2.0, 3.0), 5.0, t.hover);
        }
        let sorted = match col {
            Col::Name => tab.sort == SortKey::Name,
            Col::Modified => tab.sort == SortKey::Modified,
            Col::Size => tab.sort == SortKey::Size,
            Col::Kind => tab.sort == SortKey::Kind,
            Col::Location => false,
        };
        let right = col == Col::Size;
        let pad = if col == Col::Name { 28.0 } else { 8.0 };
        let tr = Rect::new(r.x + pad, r.y, r.w - pad - 22.0, r.h);
        let ink = if sorted { t.ink } else { t.ink2 };
        c.text(
            col.title(),
            tr,
            if sorted { Font::Small } else { Font::Small },
            ink,
            if right { Align::End } else { Align::Start },
        );
        if sorted {
            let gx = if right {
                tr.r() + 2.0
            } else {
                tr.x + c.measure(col.title(), Font::Small) + 4.0
            };
            c.glyph(
                if tab.asc { g::UP } else { g::DOWN },
                Rect::new(gx, r.y, 14.0, r.h),
                8,
                t.ink2,
            );
        }
        if k > 0 {
            c.vline(r.x, r.y + 8.0, r.b() - 8.0, t.edge);
        }
    }
    c.hline(h.x, h.r(), h.b() - 1.0 / c.scale, t.edge);
}

fn ensure_icon(app: &mut App, i: usize) -> i32 {
    let tab = &app.tabs[app.cur];
    if tab.icon_idx[i] != i32::MIN {
        return tab.icon_idx[i];
    }
    let dir = tab.dir();
    let e = tab.entries[i].clone();
    let idx = match &e.full {
        Some(p) if matches!(tab.loc, Loc::Computer) => app.icons.index_for_path(p),
        Some(p) => {
            let parent = p.parent().map(|q| q.to_path_buf()).unwrap_or_default();
            app.icons.index(&parent, &e)
        }
        None => app.icons.index(&dir, &e),
    };
    app.tabs[app.cur].icon_idx[i] = idx;
    idx
}

fn sel_colors(app: &App, t: &Theme) -> (Color, Color) {
    let focused = app.active && app.focus == Focus::Content;
    if focused || t.mode == Mode::HighContrast {
        (t.accent, t.on_accent)
    } else {
        (t.sel_inactive, t.ink)
    }
}

fn list_view(app: &mut App, c: &mut Canvas, t: &Theme) {
    let b = app.lay.body;
    let n = app.tab().entries.len();
    if n == 0 {
        empty_state(app, c, t, b);
        return;
    }
    let first = (app.tab().scroll / ROW_H) as usize;
    let last = (((app.tab().scroll + b.h) / ROW_H).ceil() as usize + 1).min(n);
    let cols = col_layout(app);
    let (sel_fill, sel_ink) = sel_colors(app, t);
    let renaming = crate::win::renaming_index(app);
    for i in first..last {
        let r = app.row_rect(i);
        let cap = r.inset(INSET, 0.0);
        let selected = app.tab().sel.is(i);
        if selected {
            // Adjacent selected rows merge into one capsule, like Finder.
            let prev = i > 0 && app.tab().sel.is(i - 1);
            let next = app.tab().sel.is(i + 1);
            let rad = 6.0;
            c.round_corners(
                cap,
                [
                    if prev { 0.0 } else { rad },
                    if prev { 0.0 } else { rad },
                    if next { 0.0 } else { rad },
                    if next { 0.0 } else { rad },
                ],
                sel_fill,
                None,
            );
        } else if i % 2 == 1 {
            c.round(cap, 6.0, t.stripe);
        }
        if app.kb && app.tab().sel.focus == Some(i) && app.focus == Focus::Content && !selected {
            c.round_stroke(cap, 6.0, t.focus, 1.5);
        }
        let e = app.tab().entries[i].clone();
        let dim = if e.is_hidden() { 0.55 } else { 1.0 };
        let ink = if selected { sel_ink } else { t.ink };
        let ink2 = if selected { sel_ink } else { t.ink2 };
        let idx = ensure_icon(app, i);
        for (col, cx, cw) in &cols {
            let cell = Rect::new(*cx, r.y, *cw, r.h);
            match col {
                Col::Name => {
                    app.icons.draw(
                        c,
                        idx,
                        Rect::new(cell.x + 6.0, r.cy() - 8.0, 16.0, 16.0),
                        dim,
                    );
                    let mut nr = Rect::new(cell.x + 28.0, r.y, cell.w - 36.0, r.h);
                    let status = cloud_glyph(&e);
                    if let Some(gl) = status {
                        c.glyph(
                            gl,
                            nr.right(18.0),
                            12,
                            if selected { sel_ink } else { t.ink3 },
                        );
                        nr = nr.cut_right(22.0);
                    }
                    if renaming != Some(i) {
                        c.text(&e.name, nr, Font::Body, with_alpha(ink, dim), Align::Start);
                    }
                }
                Col::Modified => c.text(
                    &fsys::format_date(e.modified),
                    cell.inset(8.0, 0.0),
                    Font::Body,
                    ink2,
                    Align::Start,
                ),
                Col::Size => {
                    let s = if e.is_dir && !matches!(app.tab().loc, Loc::Computer) {
                        "—".to_string()
                    } else {
                        fsys::format_size(e.size)
                    };
                    c.text(
                        &s,
                        Rect::new(cell.x, r.y, cell.w - 22.0, r.h),
                        Font::Body,
                        ink2,
                        Align::End,
                    );
                }
                Col::Kind => c.text(
                    &fsys::kind_name(&e),
                    cell.inset(8.0, 0.0),
                    Font::Body,
                    ink2,
                    Align::Start,
                ),
                Col::Location => {
                    let loc = e
                        .full
                        .as_ref()
                        .and_then(|p| p.parent())
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let loc = c.fit_path(&loc, Font::Body, cell.w - 16.0);
                    c.text(&loc, cell.inset(8.0, 0.0), Font::Body, ink2, Align::Start);
                }
            }
        }
    }
}

fn cloud_glyph(e: &fsys::Entry) -> Option<char> {
    use fm_fs::attrs::*;
    if e.attrs & PINNED != 0 {
        Some(g::PINNED)
    } else if is_cloud_placeholder(e.attrs) {
        Some(g::CLOUD)
    } else {
        None
    }
}

fn empty_state(app: &App, c: &mut Canvas, t: &Theme, b: Rect) {
    let tab = app.tab();
    let (glyph, title, hint) = match (&tab.error, &tab.loc) {
        (Some(err), _) => (
            g::WARN,
            err.clone(),
            "Zkuste to znovu (F5), nebo se vraťte zpět.".to_string(),
        ),
        (None, Loc::Search { text, .. }) if text.is_empty() => (
            g::SEARCH,
            "Začněte psát".into(),
            "Hledá se v názvech všech souborů na discích.".into(),
        ),
        (None, Loc::Search { .. }) => (
            g::SEARCH,
            "Nic nenalezeno".into(),
            "Zkuste kratší nebo jiný výraz.".into(),
        ),
        (None, Loc::Tag(_)) => (
            g::EMPTY,
            "Žádné soubory s tímto štítkem".into(),
            "Štítek přidáte z místní nabídky souboru.".into(),
        ),
        _ => (
            g::EMPTY,
            "Složka je prázdná".into(),
            "Novou složku vytvoříte klávesami Ctrl+Shift+N.".into(),
        ),
    };
    let mid = b.center(b.w.min(420.0), 120.0);
    c.glyph(glyph, mid.top(40.0), 28, t.ink3);
    c.text(
        &title,
        Rect::new(mid.x, mid.y + 48.0, mid.w, 24.0),
        Font::BodyStrong,
        t.ink2,
        Align::Center,
    );
    c.text(
        &hint,
        Rect::new(mid.x, mid.y + 72.0, mid.w, 20.0),
        Font::Small,
        t.ink3,
        Align::Center,
    );
}

// ---- icon view ----------------------------------------------------------------------------------------

pub fn tile_parts(app: &App, i: usize) -> (Rect, Rect) {
    let r = app.tile_rect(i);
    let s = app.prefs.icon_size;
    let icon = Rect::new(r.cx() - s / 2.0, r.y + 6.0, s, s);
    let name = Rect::new(r.x + 4.0, icon.b() + 6.0, r.w - 8.0, 34.0);
    (icon, name)
}

pub fn tile_hit(app: &App, i: usize, x: f32, y: f32) -> bool {
    let (icon, name) = tile_parts(app, i);
    icon.inset(-4.0, -4.0).contains(x, y) || name.contains(x, y)
}

fn icon_view(app: &mut App, c: &mut Canvas, t: &Theme) {
    let b = app.lay.body;
    let n = app.tab().entries.len();
    if n == 0 {
        empty_state(app, c, t, b);
        return;
    }
    let grid = app.grid();
    let first_row = ((app.tab().scroll - 8.0) / grid.tile_h).floor().max(0.0) as usize;
    let rows_vis = (b.h / grid.tile_h).ceil() as usize + 2;
    let (sel_fill, sel_ink) = sel_colors(app, t);
    let renaming = crate::win::renaming_index(app);
    for i in (first_row * grid.cols)..((first_row + rows_vis) * grid.cols).min(n) {
        let (icon, name) = tile_parts(app, i);
        let selected = app.tab().sel.is(i);
        let e = app.tab().entries[i].clone();
        if selected {
            c.round(
                icon.inset(-6.0, -6.0),
                8.0,
                if t.is_dark() {
                    Color {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 0.10,
                    }
                } else {
                    Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 0.08,
                    }
                },
            );
        }
        if app.kb && app.tab().sel.focus == Some(i) && app.focus == Focus::Content && !selected {
            c.round_stroke(icon.inset(-6.0, -6.0), 8.0, t.focus, 1.5);
        }
        let idx = ensure_icon(app, i);
        let dim = if e.is_hidden() { 0.55 } else { 1.0 };
        let path = app.tab().path_of(i);
        match crate::thumbs::image(app, c, &path, e.modified, e.attrs, icon.w) {
            Some((bmp, w, h, is_thumb)) => {
                let is_thumb = is_thumb && !e.is_dir;
                let k = (icon.w / w).min(icon.h / h);
                let k = if is_thumb { k } else { k.min(1.0).max(icon.w / w.max(h)) };
                let r = Rect::new(icon.cx() - w * k / 2.0, icon.b() - h * k, w * k, h * k).snap(c.scale);
                c.bitmap(&bmp, r, dim);
                if is_thumb {
                    // Photos and pages get a hairline frame, like Finder.
                    c.round_stroke(r, 2.0, t.edge, 0.0);
                }
            }
            None => app.icons.draw(c, idx, icon, dim),
        }
        if renaming == Some(i) {
            continue;
        }
        let tw = c.measure(&e.name, Font::Small);
        let one_line = tw <= name.w - 8.0;
        let text_r = if one_line { name.top(18.0) } else { name };
        if selected {
            let bw = (tw + 10.0).min(name.w);
            let cap = if one_line {
                Rect::new(name.cx() - bw / 2.0, name.y, bw, 18.0)
            } else {
                name
            };
            c.round(cap, 5.0, sel_fill);
        }
        let ink = if selected { sel_ink } else { t.ink };
        if one_line {
            c.text(
                &e.name,
                text_r,
                Font::Small,
                with_alpha(ink, dim),
                Align::Center,
            );
        } else {
            c.text_wrapped(
                &e.name,
                text_r.inset(4.0, 1.0),
                Font::Small,
                with_alpha(ink, dim),
                Align::Center,
            );
        }
    }
}

fn band(app: &App, c: &mut Canvas, t: &Theme) {
    if let Some(band) = app.band_rect() {
        c.fill(band, with_alpha(t.accent, 0.15));
        c.round_stroke(band, 0.0, with_alpha(t.accent, 0.7), 0.0);
    }
}

// ---- status / path bar -------------------------------------------------------------------------------------

/// Crumb rectangles and their locations, left to right.
fn crumb_layout(app: &App) -> Vec<(Rect, Loc, String)> {
    let s = app.lay.status;
    let mut out = Vec::new();
    let crumbs: Vec<(Loc, String)> = match &app.tab().loc {
        Loc::Folder(p) => {
            let mut v = vec![(Loc::Computer, "Tento počítač".to_string())];
            v.extend(
                fsys::crumbs(p)
                    .into_iter()
                    .map(|(p, n)| (Loc::Folder(p), n)),
            );
            v
        }
        other => vec![(other.clone(), other.title())],
    };
    let Some(gfx) = app.gfx() else { return out };
    let mut x = s.x + 12.0;
    let max_x = s.r() - 220.0;
    // Collapse leading crumbs when the bar is too narrow.
    let widths: Vec<f32> = crumbs
        .iter()
        .map(|(_, n)| gfx.measure(n, Font::Small) + 10.0)
        .collect();
    let total: f32 = widths.iter().map(|w| w + 14.0).sum();
    let mut skip = 0;
    let mut t = total;
    while t > max_x - x && skip + 1 < crumbs.len() {
        t -= widths[skip] + 14.0;
        skip += 1;
    }
    for (k, ((loc, name), w)) in crumbs.into_iter().zip(widths).enumerate() {
        if k < skip {
            continue;
        }
        let w = w.min(max_x - x).max(24.0);
        out.push((Rect::new(x, s.y + 4.0, w, s.h - 8.0), loc, name));
        x += w + 14.0;
    }
    out
}

pub fn crumb_path(app: &App, k: usize) -> Option<Loc> {
    crumb_layout(app).into_iter().nth(k).map(|c| c.1)
}

fn slider_rect(app: &App) -> Rect {
    app.lay.status.right(150.0).inset(16.0, 0.0)
}

pub fn slider_set(app: &mut App, x: f32) {
    let r = slider_rect(app);
    let f = ((x - r.x) / r.w).clamp(0.0, 1.0);
    let size = (32.0 + f * (256.0 - 32.0)).round() / 8.0 * 8.0;
    if size != app.prefs.icon_size {
        app.prefs.icon_size = size;
        app.relayout();
        app.invalidate();
    }
}

pub fn status_hit(app: &App, x: f32, y: f32) -> Hit {
    if app.tab().view == View::Icons && slider_rect(app).inset(-4.0, 0.0).contains(x, y) {
        return Hit::IconSlider;
    }
    crumb_layout(app)
        .iter()
        .position(|(r, ..)| r.contains(x, y))
        .map(Hit::Crumb)
        .unwrap_or(Hit::None)
}

fn status(app: &App, c: &mut Canvas, t: &Theme) {
    let s = app.lay.status;
    c.hline(s.x, s.r(), s.y, t.edge);
    let crumbs = crumb_layout(app);
    let last = crumbs.len().saturating_sub(1);
    for (k, (r, _, name)) in crumbs.iter().enumerate() {
        if app.hover == Hit::Crumb(k) {
            c.round(*r, 5.0, t.hover);
        }
        c.text(
            name,
            r.inset(5.0, 0.0),
            Font::Small,
            if k == last { t.ink } else { t.ink2 },
            Align::Start,
        );
        if k < last {
            c.glyph(g::RIGHT, Rect::new(r.r(), r.y, 14.0, r.h), 8, t.ink3);
        }
    }
    let tab = app.tab();
    let mut right = s.cut_right(12.0);
    if tab.view == View::Icons {
        let sr = slider_rect(app);
        let f = (app.prefs.icon_size - 32.0) / (256.0 - 32.0);
        c.round(Rect::new(sr.x, sr.cy() - 2.0, sr.w, 4.0), 2.0, t.field);
        c.round(
            Rect::new(sr.x, sr.cy() - 2.0, sr.w * f, 4.0),
            2.0,
            t.accent_glyph,
        );
        let kx = sr.x + sr.w * f;
        c.circle(
            kx,
            sr.cy(),
            7.0,
            if t.is_dark() {
                crate::theme::rgb(0x454545)
            } else {
                crate::theme::rgb(0xFFFFFF)
            },
        );
        c.circle_stroke(kx, sr.cy(), 7.0, t.edge, 1.0);
        c.circle(kx, sr.cy(), 3.5, t.accent_glyph);
        right = right.cut_right(sr.w + 24.0);
    }
    let text = if let Some((note, at)) = &app.status_note {
        if at.elapsed().as_secs_f32() < 3.0 {
            note.clone()
        } else {
            status_text(app)
        }
    } else {
        status_text(app)
    };
    c.text(
        &text,
        Rect::new(right.r() - 260.0, s.y, 260.0, s.h),
        Font::Caption,
        t.ink3,
        Align::End,
    );
}

pub fn status_text(app: &App) -> String {
    let tab = app.tab();
    let n = tab.entries.len();
    let sel = tab.sel.count();
    let items = |n: usize| match n {
        1 => "1 položka".to_string(),
        2..=4 => format!("{n} položky"),
        _ => format!("{n} položek"),
    };
    let base = if sel > 0 {
        let bytes: u64 = tab
            .sel
            .indices()
            .iter()
            .filter(|&&i| !tab.entries[i].is_dir)
            .map(|&i| tab.entries[i].size)
            .sum();
        if bytes > 0 {
            format!("Vybráno {sel} z {n}, {}", fsys::format_size(bytes))
        } else {
            format!("Vybráno {sel} z {n}")
        }
    } else {
        items(n)
    };
    match tab.loc.folder().and_then(fsys::disk_space) {
        Some((_, free)) if sel == 0 => format!("{base}, {} volných", fsys::format_size(free)),
        _ => base,
    }
}

pub fn menu_point_for_focus(app: &App) -> POINT {
    let r = app
        .tab()
        .sel
        .focus
        .map(|f| app.item_rect(f))
        .unwrap_or(app.lay.body);
    crate::win::to_screen(app, r.x + 40.0, r.y + r.h.min(26.0))
}
