//! Painting of the whole window, plus the geometry helpers hit-testing shares with it.
use crate::app::{App, Cap, Drag, Focus, Hit, Loc, View};
use crate::fsys::{self, SortKey};
use crate::geom::Rect;
use crate::gfx::{Align, Canvas, Font};
use crate::layout::{self, Col, INSET, SIDE_HEADER, SIDE_ROW};
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
/// Finder's tag dots: overlapping circles ending at `right`, each ringed in `bg` so they read as
/// separate chips. Returns the width taken.
pub fn tag_dots(c: &mut Canvas, mask: u8, right: f32, cy: f32, bg: Color) -> f32 {
    const D: f32 = 10.0;
    const STEP: f32 = 6.0;
    let n = mask.count_ones();
    if n == 0 {
        return 0.0;
    }
    let w = D + (n - 1) as f32 * STEP;
    let mut x = right - w + D / 2.0;
    for (i, (hex, _)) in crate::theme::TAGS.iter().enumerate() {
        if mask & (1 << i) != 0 {
            c.circle(x, cy, D / 2.0 + 1.25, bg);
            c.circle(x, cy, D / 2.0, crate::theme::rgb(*hex));
            x += STEP;
        }
    }
    w
}

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
        View::Tiles => tiles_view(app, c, &t),
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
    if let Some(s) = l.scope {
        scope_bar(app, c, &t, s);
    }
    if let Some(p) = l.preview {
        crate::views::paint_inspector(app, c, &t, p);
    }
    drop_highlight(app, c, &t);
    status(app, c, &t);
    crate::batch::paint(app, c, &t);
    crate::palette::paint(app, c, &t);
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
        // Tiles has no segment (Finder's four); it lives in the View menu and Ctrl+5.
        View::Tiles => usize::MAX,
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

// ---- search scope bar ------------------------------------------------------------------------------

/// The scope pills: "Tento počítač" and, when the search started in a folder, that folder.
fn scope_pills(app: &App, c: Option<&Canvas>) -> Vec<(Rect, String, bool)> {
    let Some(bar) = app.lay.scope else { return Vec::new() };
    let Loc::Search { scope, .. } = &app.tab().loc else { return Vec::new() };
    let mut labels = vec![("Tento počítač".to_string(), scope.is_none())];
    if let Some(f) = app.search_prev.as_ref().and_then(|l| l.folder()) {
        labels.push((format!("„{}“", fsys::display_name(f)), scope.as_deref() == Some(f)));
    }
    let mut x = bar.x + 16.0 + 52.0;
    labels
        .into_iter()
        .map(|(label, on)| {
            // Without a canvas (hit testing) use a width estimate that matches Font::Small closely.
            let tw = c.map(|c| c.measure(&label, Font::Small)).unwrap_or(label.chars().count() as f32 * 6.6);
            let r = Rect::new(x, bar.cy() - 12.0, (tw + 20.0).min(320.0), 24.0);
            x = r.r() + 6.0;
            (r, label, on)
        })
        .collect()
}

pub fn scope_hit(app: &App, x: f32, y: f32) -> Hit {
    let rects = app.scope_rects.borrow();
    rects
        .iter()
        .position(|r| r.contains(x, y))
        .map(Hit::Scope)
        .unwrap_or(Hit::Empty)
}

fn scope_bar(app: &App, c: &mut Canvas, t: &Theme, bar: Rect) {
    c.text("Hledat:", Rect::new(bar.x + 16.0, bar.y, 52.0, bar.h), Font::Small, t.ink3, Align::Start);
    let pills = scope_pills(app, Some(c));
    *app.scope_rects.borrow_mut() = pills.iter().map(|p| p.0).collect();
    for (k, (r, label, on)) in pills.iter().enumerate() {
        if *on {
            c.round(*r, 6.0, if t.mode == Mode::HighContrast { t.accent } else { t.pill });
        } else if app.hover == Hit::Scope(k) {
            c.round(*r, 6.0, t.hover);
        }
        let ink = if *on && t.mode == Mode::HighContrast { t.on_accent } else if *on { t.ink } else { t.ink2 };
        c.text(label, r.inset(10.0, 0.0), Font::Small, ink, Align::Center);
    }
    if let Some(s) = crate::search::status() {
        c.text(&s, Rect::new(bar.r() - 260.0, bar.y, 244.0, bar.h), Font::Caption, t.ink3, Align::End);
    }
    c.hline(bar.x, bar.r(), bar.b() - 0.5, t.edge);
}

pub fn side_row_rect(app: &App, k: usize) -> Rect {
    let (oy, h) = side_offsets(app).get(k).copied().unwrap_or((0.0, 0.0));
    let s = app.lay.sidebar;
    Rect::new(s.x, s.y - app.side_scroll + oy, s.w, h).inset(INSET, 1.0)
}

pub fn tab_rect(app: &App, k: usize) -> Option<Rect> {
    app.lay.tabs.and_then(|r| tab_rects(app, r).0.get(k).copied())
}

pub fn crumb_rect(app: &App, k: usize) -> Option<Rect> {
    crumb_layout(app).into_iter().nth(k).map(|c| c.0)
}

/// Where a drag would drop: a tinted ring around the folder, or around the whole view (Finder).
fn drop_highlight(app: &App, c: &mut Canvas, t: &Theme) {
    let Some((r, whole)) = app.drop_hl else { return };
    let ring = if t.mode == Mode::HighContrast { t.focus } else { t.accent_glyph };
    if whole {
        c.push_clip(r);
        c.round_stroke(r.inset(2.0, 2.0), 6.0, ring, 2.0);
        c.pop_clip();
    } else {
        // A row partly under the pinned group header is highlighted only below it.
        let b = app.lay.body;
        let clip = b.intersects(&r).then(|| {
            let top = app.pinned_header().map_or(b.y, |(_, p)| p.b());
            Rect::new(b.x, top, b.w, (b.b() - top).max(0.0))
        });
        if let Some(cr) = clip {
            c.push_clip(cr);
        }
        c.round(r, 6.0, Color { a: 0.16, ..ring });
        c.round_stroke(r.inset(0.75, 0.75), 6.0, ring, 1.5);
        if clip.is_some() {
            c.pop_clip();
        }
    }
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
    ) || app.tab().entries.iter().take(1).any(|e| e.origin.is_some())
    {
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
        let ranked = matches!(tab.loc, Loc::Search { .. }) && tab.by_rank;
        let sorted = !ranked && match col {
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
        _ if matches!(tab.loc, Loc::Shell { .. }) && e.shell.is_some() => {
            crate::shellns::icon_index(e.shell.as_ref().unwrap())
        }
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
    let cols = col_layout(app);
    let (sel_fill, sel_ink) = sel_colors(app, t);
    let renaming = crate::win::renaming_index(app);
    let geo_groups = app.shown_groups().to_vec();
    // Each group's rows: stripes restart and selections do not merge across a header.
    let span = |i: usize| match geo_groups.get(geo_groups.partition_point(|g| g.start <= i).wrapping_sub(1)) {
        Some(g) => (g.start, g.start + g.len),
        None => (0, n),
    };
    let under_pin = pin_clip(app, c);
    for i in app.visible_items() {
        let r = app.row_rect(i);
        let cap = r.inset(INSET, 0.0);
        let selected = app.tab().sel.is(i);
        let (start, end) = span(i);
        let odd = (i - start) % 2 == 1;
        if selected {
            // Adjacent selected rows merge into one capsule, like Finder.
            let prev = i > start && app.tab().sel.is(i - 1);
            let next = i + 1 < end && app.tab().sel.is(i + 1);
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
        } else if odd {
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
                    let mask = crate::tags::mask(&app.tab().path_of(i));
                    if mask != 0 {
                        let bg = if selected {
                            sel_fill
                        } else if odd {
                            over(t.stripe, t.card)
                        } else {
                            t.card
                        };
                        // Right after the name (Finder), pulled in when the name is truncated.
                        let w = 10.0 + 6.0 * (mask.count_ones() as f32 - 1.0);
                        let tw = c.measure(&e.name, Font::Body);
                        let right = (nr.x + tw + 8.0 + w).min(nr.r() - 2.0);
                        tag_dots(c, mask, right, r.cy(), bg);
                        nr = Rect::new(nr.x, nr.y, (right - w - 6.0 - nr.x).max(0.0), nr.h);
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
                    // Folders: their size from the index once it has reported it.
                    let s = if e.is_dir && e.size == 0 {
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
                    let loc = e.origin.clone().unwrap_or_else(|| {
                        e.full
                            .as_ref()
                            .and_then(|p| p.parent())
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    });
                    let loc = c.fit_path(&loc, Font::Body, cell.w - 16.0);
                    c.text(&loc, cell.inset(8.0, 0.0), Font::Body, ink2, Align::Start);
                }
            }
        }
    }
    if under_pin {
        c.pop_clip();
    }
    group_headers(app, c, t);
}

/// Items are not painted under the pinned group header (the card is translucent over Mica, so
/// covering them would not hide them). Returns whether a clip was pushed.
fn pin_clip(app: &App, c: &mut Canvas) -> bool {
    let Some((_, r)) = app.pinned_header() else { return false };
    let b = app.lay.body;
    c.push_clip(Rect::new(b.x, r.b(), b.w, (b.b() - r.b()).max(0.0)));
    true
}

/// Group by headers, then the one pinned to the top over the rows scrolling under it.
fn group_headers(app: &App, c: &mut Canvas, t: &Theme) {
    let groups = app.shown_groups();
    if groups.is_empty() {
        return;
    }
    let b = app.lay.body;
    let pinned = app.pinned_header();
    for (g, gr) in groups.iter().enumerate() {
        let r = app.group_header_rect(g);
        if r.b() <= b.y || r.y >= b.b() || pinned.is_some_and(|(p, _)| p == g) {
            continue;
        }
        group_header(app, c, t, r, &gr.label, gr.len);
    }
    if let Some((g, r)) = pinned {
        group_header(app, c, t, r, &groups[g].label, groups[g].len);
    }
}

fn group_header(app: &App, c: &mut Canvas, t: &Theme, r: Rect, label: &str, count: usize) {
    // Aligned with the items: the list's icon column, or the icon grid's left edge.
    let (left, right) = if app.tab().view.is_grid() {
        let g = app.grid();
        (g.x0 + 8.0, g.x0 + g.cols as f32 * g.tile_w - 8.0)
    } else {
        (r.x + INSET + 6.0, r.r() - INSET)
    };
    let text = Rect::new(left, r.b() - 26.0, (right - left).max(0.0), 22.0);
    let w = c.measure(label, Font::BodyStrong).min(text.w);
    c.text(label, text, Font::BodyStrong, t.ink, Align::Start);
    c.text(&count.to_string(), Rect::new(text.x + w + 8.0, text.y, 80.0, text.h), Font::Small, t.ink3, Align::Start);
    c.hline(left, right, r.b() - 1.0, t.edge);
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
        (None, Loc::Search { scope: None, .. }) if crate::search::status().is_some() => (
            g::SEARCH,
            "Zatím nic".into(),
            "Disk se ještě indexuje, výsledky se doplní samy.".into(),
        ),
        (None, Loc::Search { .. }) => (
            g::SEARCH,
            "Nic nenalezeno".into(),
            "Zkuste kratší nebo jiný výraz.".into(),
        ),
        (None, _) if tab.loading => (
            g::SEARCH,
            "Načítání…".into(),
            "Síťová místa a zařízení mohou chvíli trvat.".into(),
        ),
        (None, Loc::Shell { id, .. }) if crate::shellns::is_recycle_bin(id) => (
            g::EMPTY,
            "Koš je prázdný".into(),
            "Smazané položky tu zůstanou, dokud koš nevysypete.".into(),
        ),
        (None, Loc::Shell { id, .. }) if crate::shellns::is_network(id) => (
            g::EMPTY,
            "V síti nic nevidím".into(),
            "Zapněte zjišťování sítě v Nastavení › Síť a internet.".into(),
        ),
        (None, Loc::Shell { .. }) => (
            g::EMPTY,
            "Nic tu není".into(),
            "Zkuste to znovu (F5).".into(),
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

// ---- tiles view ---------------------------------------------------------------------------------------

/// A tile's icon square and its name line (the rename field goes there).
pub fn tiles_parts(app: &App, i: usize) -> (Rect, Rect) {
    let r = app.tile_rect(i).inset(4.0, 3.0);
    let icon = Rect::new(r.x + 8.0, r.cy() - 24.0, 48.0, 48.0);
    let x = icon.r() + 10.0;
    (icon, Rect::new(x, r.cy() - 27.0, (r.r() - 10.0 - x).max(0.0), 18.0))
}

fn tiles_view(app: &mut App, c: &mut Canvas, t: &Theme) {
    let n = app.tab().entries.len();
    if n == 0 {
        empty_state(app, c, t, app.lay.body);
        return;
    }
    let (sel_fill, sel_ink) = sel_colors(app, t);
    let renaming = crate::win::renaming_index(app);
    let under_pin = pin_clip(app, c);
    for i in app.visible_items() {
        let r = app.tile_rect(i).inset(4.0, 3.0);
        let selected = app.tab().sel.is(i);
        if selected {
            c.round(r, 8.0, sel_fill);
        } else if app.kb && app.tab().sel.focus == Some(i) && app.focus == Focus::Content {
            c.round_stroke(r, 8.0, t.focus, 1.5);
        }
        let e = app.tab().entries[i].clone();
        let dim = if e.is_hidden() { 0.55 } else { 1.0 };
        let (icon, name) = tiles_parts(app, i);
        let idx = ensure_icon(app, i);
        let path = app.tab().path_of(i);
        match crate::thumbs::image(app, c, &path, e.modified, e.attrs, icon.w) {
            Some((bmp, w, h, is_thumb)) => {
                let is_thumb = is_thumb && !e.is_dir;
                let k = (icon.w / w).min(icon.h / h);
                let k = if is_thumb { k } else { k.min(1.0).max(icon.w / w.max(h)) };
                let dst = icon.center(w * k, h * k).snap(c.scale);
                c.bitmap(&bmp, dst, dim);
                if is_thumb {
                    c.round_stroke(dst, 2.0, t.edge, 0.0);
                }
            }
            None => app.icons.draw(c, idx, icon, dim),
        }
        let (ink, ink2) = if selected { (sel_ink, sel_ink) } else { (t.ink, t.ink2) };
        let mut name_r = name;
        let mask = crate::tags::mask(&path);
        if mask != 0 {
            let w = 10.0 + 6.0 * (mask.count_ones() as f32 - 1.0);
            let tw = c.measure(&e.name, Font::Body);
            let right = (name.x + tw + 8.0 + w).min(name.r() - 2.0);
            tag_dots(c, mask, right, name.cy(), if selected { sel_fill } else { t.card });
            name_r = Rect::new(name.x, name.y, (right - w - 6.0 - name.x).max(0.0), name.h);
        }
        if renaming != Some(i) {
            c.text(&e.name, name_r, Font::Body, with_alpha(ink, dim), Align::Start);
        }
        let line = |k: f32| Rect::new(name.x, name.y + 18.0 * k, name.w, 18.0);
        c.text(&fsys::kind_name(&e), line(1.0), Font::Small, with_alpha(ink2, dim), Align::Start);
        // Files show their size; folders theirs once the index has reported it.
        if !e.is_dir || e.size > 0 {
            c.text(&fsys::format_size(e.size), line(2.0), Font::Small, with_alpha(ink2, dim * 0.8), Align::Start);
        }
    }
    if under_pin {
        c.pop_clip();
    }
    group_headers(app, c, t);
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
    let (sel_fill, sel_ink) = sel_colors(app, t);
    let renaming = crate::win::renaming_index(app);
    let under_pin = pin_clip(app, c);
    for i in app.visible_items() {
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
        let mask = crate::tags::mask(&path);
        if mask != 0 {
            // Finder puts the dots just before the name.
            let right = if one_line {
                name.cx() - tw / 2.0 - 5.0
            } else {
                name.x + 12.0 + 6.0 * (mask.count_ones() as f32 - 1.0)
            };
            tag_dots(c, mask, right, name.y + 9.0, t.card);
        }
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
    if under_pin {
        c.pop_clip();
    }
    group_headers(app, c, t);
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
    // The crumbs end where the status text (and the icon-size slider) begin.
    let text_w = gfx.measure(&status_line(app), Font::Caption).min(260.0);
    let slider_w = if app.tab().view == View::Icons { slider_rect(app).w + 24.0 } else { 0.0 };
    let max_x = s.r() - 12.0 - slider_w - text_w - 20.0;
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
        if max_x - x < 24.0 {
            break; // too narrow: the status text keeps its place
        }
        let w = w.min(max_x - x);
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
    let crumbs = crumb_layout(app);
    if let Some(k) = crumbs.iter().position(|(r, ..)| r.contains(x, y)) {
        return Hit::Crumb(k);
    }
    (0..crumbs.len().saturating_sub(1))
        .find(|&k| chevron_rect(crumbs[k].0).contains(x, y))
        .map_or(Hit::None, Hit::CrumbMenu)
}

/// The chevron after a crumb; it opens the folder's subfolders.
pub fn chevron_rect(crumb: Rect) -> Rect {
    Rect::new(crumb.r(), crumb.y, 14.0, crumb.h)
}

/// Screen point under chevron `k` for a menu that opens upward from the path bar.
pub fn chevron_anchor(app: &App, k: usize) -> Option<(f32, f32)> {
    crumb_layout(app).get(k).map(|(r, ..)| {
        let c = chevron_rect(*r);
        (c.x, c.y)
    })
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
            let ch = chevron_rect(*r);
            if app.hover == Hit::CrumbMenu(k) {
                c.round(ch, 4.0, t.hover);
            }
            c.glyph(g::RIGHT, ch, 8, t.ink3);
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
    let text = status_line(app);
    c.text(
        &text,
        Rect::new(right.r() - 260.0, s.y, 260.0, s.h),
        Font::Caption,
        t.ink3,
        Align::End,
    );
}

/// A note for three seconds after it is set, the item count otherwise.
fn status_line(app: &App) -> String {
    match &app.status_note {
        Some((note, at)) if at.elapsed().as_secs_f32() < 3.0 => note.clone(),
        _ => status_text(app),
    }
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
            // Folders count with their indexed size; drives in "Tento počítač" carry capacity.
            .filter(|&&i| !tab.entries[i].is_dir || !matches!(tab.loc, Loc::Computer))
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
    // Free space is read on every paint: never for network paths, whose server may not answer.
    match tab.loc.folder().filter(|p| !fsys::is_remote(p)).and_then(fsys::disk_space) {
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
