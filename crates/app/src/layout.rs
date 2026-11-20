//! Window layout in DIPs (docs/DESIGN.md metrics) and hit-testing targets.
use crate::geom::Rect;

pub const TOOLBAR_H: f32 = 52.0;
pub const CAPTION_W: f32 = 46.0;
pub const CAPTION_H: f32 = 32.0;
pub const SIDEBAR_MIN: f32 = 160.0;
pub const SIDEBAR_MAX: f32 = 320.0;
pub const SIDE_ROW: f32 = 28.0;
pub const SIDE_HEADER: f32 = 28.0;
pub const ROW_H: f32 = 26.0;
pub const HEADER_H: f32 = 28.0;
pub const STATUS_H: f32 = 28.0;
pub const TABS_H: f32 = 32.0;
pub const SCOPE_H: f32 = 36.0;
pub const CARD_RADIUS: f32 = 8.0;
pub const BTN_W: f32 = 32.0;
pub const BTN_H: f32 = 28.0;
pub const SEARCH_W: f32 = 220.0;
pub const INSET: f32 = 8.0;
pub const COL_W: f32 = 240.0;

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub size: (f32, f32),
    pub toolbar: Rect,
    pub caption: [Rect; 3],
    pub sidebar_btn: Rect,
    pub back: Rect,
    pub fwd: Rect,
    pub title: Rect,
    pub views: [Rect; 4],
    pub more: Rect,
    pub search: Rect,
    pub sidebar: Rect,
    pub side_resize: Rect,
    pub card: Rect,
    pub tabs: Option<Rect>,
    /// Search scope bar (Finder): "Tento počítač" or the folder the search started in.
    pub scope: Option<Rect>,
    pub header: Option<Rect>,
    pub body: Rect,
    pub status: Rect,
    pub preview: Option<Rect>,
}

pub struct LayoutIn {
    pub w: f32,
    pub h: f32,
    pub sidebar: Option<f32>,
    pub tabs: bool,
    pub scope: bool,
    pub header: bool,
    pub preview: Option<f32>,
}

pub fn compute(i: &LayoutIn) -> Layout {
    let full = Rect::new(0.0, 0.0, i.w, i.h);
    let toolbar = full.top(TOOLBAR_H);
    let mut caption = [Rect::default(); 3];
    for (k, c) in caption.iter_mut().enumerate() {
        *c = Rect::new(i.w - CAPTION_W * (3 - k) as f32, 0.0, CAPTION_W, CAPTION_H);
    }
    let cy = (TOOLBAR_H - BTN_H) / 2.0;
    let sidebar_btn = Rect::new(12.0, cy, BTN_W, BTN_H);
    let sw = i.sidebar.unwrap_or(0.0);
    let nav_x = if i.sidebar.is_some() {
        sw + INSET
    } else {
        sidebar_btn.r() + 4.0
    };
    let back = Rect::new(nav_x, cy, BTN_W, BTN_H);
    let fwd = back.offset(BTN_W, 0.0);
    // Right group, from the caption buttons leftwards.
    let right_end = i.w - CAPTION_W * 3.0 - 12.0;
    let search_w = if i.w < 760.0 { 150.0 } else { SEARCH_W };
    let search = Rect::new(right_end - search_w, cy, search_w, BTN_H);
    let more = Rect::new(search.x - 8.0 - BTN_W, cy, BTN_W, BTN_H);
    let mut views = [Rect::default(); 4];
    let vx = more.x - 8.0 - BTN_W * 4.0;
    for (k, v) in views.iter_mut().enumerate() {
        *v = Rect::new(vx + BTN_W * k as f32, cy, BTN_W, BTN_H);
    }
    let title = Rect::new(
        fwd.r() + 10.0,
        cy,
        (vx - 12.0 - (fwd.r() + 10.0)).max(0.0),
        BTN_H,
    );

    let below = full.cut_top(TOOLBAR_H);
    let sidebar = below.left(sw);
    let side_resize = Rect::new(sw - 3.0, below.y, 6.0, below.h);
    let card = below.cut_left(sw);
    let mut inner = card;
    let tabs = if i.tabs {
        let t = inner.top(TABS_H);
        inner = inner.cut_top(TABS_H);
        Some(t)
    } else {
        None
    };
    let status = inner.bottom(STATUS_H);
    inner = inner.cut_bottom(STATUS_H);
    let preview = i.preview.map(|pw| {
        let p = inner.right(pw);
        inner = inner.cut_right(pw);
        p
    });
    let scope = if i.scope {
        let s = inner.top(SCOPE_H);
        inner = inner.cut_top(SCOPE_H);
        Some(s)
    } else {
        None
    };
    let header = if i.header {
        let h = inner.top(HEADER_H);
        inner = inner.cut_top(HEADER_H);
        Some(h)
    } else {
        None
    };
    Layout {
        size: (i.w, i.h),
        toolbar,
        caption,
        sidebar_btn,
        back,
        fwd,
        title,
        views,
        more,
        search,
        sidebar,
        side_resize,
        card,
        tabs,
        scope,
        header,
        body: inner,
        status,
        preview,
    }
}

/// List view columns: (id, title, width or 0 = flexible, right aligned).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Col {
    Name,
    Modified,
    Size,
    Kind,
    Location,
}

impl Col {
    pub fn title(self) -> &'static str {
        match self {
            Col::Name => "Název",
            Col::Modified => "Změněno",
            Col::Size => "Velikost",
            Col::Kind => "Druh",
            Col::Location => "Umístění",
        }
    }
}

/// Column rects for a body of width `w` starting at `x`.
pub fn columns(x: f32, w: f32, cols: &[(Col, f32)]) -> Vec<(Col, f32, f32)> {
    let fixed: f32 = cols.iter().filter(|c| c.0 != Col::Name).map(|c| c.1).sum();
    let name_w = (w - fixed - 2.0 * INSET).max(160.0);
    let mut out = Vec::new();
    let mut cx = x + INSET;
    for &(c, cw) in cols {
        let cw = if c == Col::Name { name_w } else { cw };
        out.push((c, cx, cw));
        cx += cw;
    }
    out
}

/// Icon-grid geometry for an icon size.
pub struct Grid {
    pub cols: usize,
    pub tile_w: f32,
    pub tile_h: f32,
    pub x0: f32,
}

pub fn grid(body: &Rect, icon: f32) -> Grid {
    let tile_w = (icon + 52.0).max(96.0);
    let tile_h = icon + 52.0;
    let usable = body.w - 2.0 * 12.0;
    let cols = ((usable / tile_w).floor() as usize).max(1);
    let x0 = body.x + 12.0 + (usable - cols as f32 * tile_w) / 2.0;
    Grid {
        cols,
        tile_w,
        tile_h,
        x0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_regions_do_not_overlap() {
        let l = compute(&LayoutIn {
            w: 1200.0,
            h: 800.0,
            sidebar: Some(208.0),
            tabs: true,
            scope: true,
            header: true,
            preview: None,
        });
        assert_eq!(l.card.x, 208.0);
        assert_eq!(l.scope.unwrap().y, l.tabs.unwrap().b());
        assert_eq!(l.header.unwrap().y, l.scope.unwrap().b());
        assert!(l.search.r() <= l.caption[0].x);
        assert!(l.views[3].r() <= l.more.x && l.more.r() <= l.search.x);
        assert!(l.title.x >= l.fwd.r());
        assert_eq!(l.tabs.unwrap().h, TABS_H);
        assert!((l.body.b() - l.status.y).abs() < 0.01);
        assert!((l.status.b() - 800.0).abs() < 0.01);
    }

    #[test]
    fn name_column_takes_the_rest() {
        let cols = columns(
            0.0,
            900.0,
            &[
                (Col::Name, 0.0),
                (Col::Modified, 150.0),
                (Col::Size, 90.0),
                (Col::Kind, 150.0),
            ],
        );
        let total: f32 = cols.iter().map(|c| c.2).sum();
        assert!((total - (900.0 - 2.0 * INSET)).abs() < 0.01);
    }

    #[test]
    fn grid_fits_at_least_one_column() {
        let g = grid(&Rect::new(0.0, 0.0, 50.0, 100.0), 64.0);
        assert_eq!(g.cols, 1);
        let g = grid(&Rect::new(0.0, 0.0, 1000.0, 100.0), 64.0);
        assert!(g.cols >= 8);
    }
}
