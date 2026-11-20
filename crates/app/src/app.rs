//! Application state, navigation and input handling. Painting lives in paint.rs, Win32 plumbing in
//! win.rs, shell integration in shell.rs.
use crate::fsys::{self, Entry, SortKey};
use crate::geom::Rect;
use crate::gfx::Gfx;
use crate::icons::Icons;
use crate::layout::{self, Col, Layout, LayoutIn};
use crate::sel::{History, Selection, TypeSelect};
use crate::sidebar::{self, Row};
use crate::theme::Theme;
use std::{
    path::{Path, PathBuf},
    time::Instant,
};
use windows::Win32::{Foundation::*, UI::Input::KeyboardAndMouse::*};

#[derive(Clone, Debug, PartialEq)]
pub enum Loc {
    Folder(PathBuf),
    Computer,
    Recents,
    Tag(usize),
    Search {
        text: String,
        scope: Option<PathBuf>,
    },
}

impl Loc {
    pub fn folder(&self) -> Option<&Path> {
        match self {
            Loc::Folder(p) => Some(p),
            _ => None,
        }
    }
    pub fn title(&self) -> String {
        match self {
            Loc::Folder(p) => fsys::display_name(p),
            Loc::Computer => "Tento počítač".into(),
            Loc::Recents => "Nedávné".into(),
            Loc::Tag(i) => crate::theme::TAGS[*i].1.into(),
            Loc::Search { text, .. } => format!("Hledání „{text}“"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Icons,
    List,
    Columns,
    Gallery,
}

pub struct Tab {
    pub loc: Loc,
    pub entries: Vec<Entry>,
    pub icon_idx: Vec<i32>,
    pub sel: Selection,
    pub scroll: f32,
    pub hist: History<(Loc, Option<String>)>,
    pub view: View,
    pub sort: SortKey,
    pub asc: bool,
    pub error: Option<String>,
    pub loaded_at: Option<Instant>,
    /// Column view: folders to the left of the active column (each selects the next one).
    pub cols_left: Vec<crate::views::Pane>,
    /// Column view: contents of the folder selected in the active column.
    pub col_right: Option<crate::views::Pane>,
    /// Column view: horizontal scroll.
    pub col_scroll: f32,
    /// Search results in relevance order until a column header is clicked.
    pub by_rank: bool,
}

impl Tab {
    pub fn new(loc: Loc, view: View) -> Tab {
        Tab {
            loc,
            entries: Vec::new(),
            icon_idx: Vec::new(),
            sel: Selection::new(0),
            scroll: 0.0,
            hist: History::default(),
            view,
            sort: SortKey::Name,
            asc: true,
            error: None,
            loaded_at: None,
            cols_left: Vec::new(),
            col_right: None,
            col_scroll: 0.0,
            by_rank: true,
        }
    }
    pub fn dir(&self) -> PathBuf {
        self.loc.folder().map(Path::to_path_buf).unwrap_or_default()
    }
    pub fn path_of(&self, i: usize) -> PathBuf {
        self.entries[i].path_in(&self.dir())
    }
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.sel
            .indices()
            .into_iter()
            .map(|i| self.path_of(i))
            .collect()
    }
    pub fn focused_name(&self) -> Option<String> {
        self.sel
            .focus
            .and_then(|i| self.entries.get(i))
            .map(|e| e.name.clone())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cap {
    Min,
    Max,
    Close,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Hit {
    #[default]
    None,
    Caption(Cap),
    Drag,
    SidebarBtn,
    Back,
    Fwd,
    View(usize),
    More,
    Search,
    SideRow(usize),
    SideResize,
    Tab(usize),
    TabClose(usize),
    NewTab,
    Header(Col),
    Row(usize),
    Empty,
    Crumb(usize),
    IconSlider,
    /// Column view: (column index, row) outside the active column.
    ColRow(usize, usize),
    /// Column view: empty space in a column.
    ColEmpty(usize),
    /// Search scope bar: 0 = the whole computer, 1 = the folder the search started in.
    Scope(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Content,
    Sidebar,
    Search,
}

pub enum Drag {
    None,
    Sidebar {
        start_x: f32,
        start_w: f32,
    },
    Band {
        origin: (f32, f32),
        now: (f32, f32),
        keep: Vec<usize>,
    },
    Pending {
        at: (f32, f32),
        index: usize,
    },
    Slider,
}

pub struct Prefs {
    pub show_hidden: bool,
    pub folders_first: bool,
    pub icon_size: f32,
    pub favorites: Vec<PathBuf>,
    /// Normal (restored) window rect in workspace pixels, and whether it was maximized.
    pub window: Option<([i32; 4], bool)>,
}

pub struct App {
    pub hwnd: HWND,
    pub gfx: Option<Gfx>,
    /// The renderer while a frame is being painted (`gfx` is moved out during that time).
    pub gfx_ref: Option<std::rc::Rc<Gfx>>,
    pub theme: Theme,
    pub icons: Icons,
    pub scale: f32,
    pub w: f32,
    pub h: f32,
    pub lay: Layout,
    pub tabs: Vec<Tab>,
    pub cur: usize,
    pub side_rows: Vec<Row>,
    pub side_scroll: f32,
    pub sidebar_w: f32,
    pub sidebar_on: bool,
    pub preview_on: bool,
    pub hover: Hit,
    /// Drop target under a drag: the rect, and whether it is the whole folder view.
    pub drop_hl: Option<(Rect, bool)>,
    /// Where the tab was before typing into search (Esc goes back there).
    pub search_prev: Option<Loc>,
    /// Scope pill rects from the last frame, for hit testing.
    pub scope_rects: std::cell::RefCell<Vec<Rect>>,
    pub pressed: Hit,
    pub focus: Focus,
    pub active: bool,
    pub kb: bool,
    pub maximized: bool,
    pub mica: bool,
    pub prefs: Prefs,
    pub drag: Drag,
    pub tsel: TypeSelect,
    pub search_text: String,
    pub status_note: Option<(String, Instant)>,
    pub last_click: Option<(Instant, usize)>,
    /// Set by `--render`: paint without a window.
    pub offscreen: bool,
}

pub const DEFAULT_SIDEBAR: f32 = 208.0;

impl App {
    pub fn new(start: Loc) -> App {
        let theme = Theme::detect();
        let prefs = Prefs {
            show_hidden: false,
            folders_first: true,
            icon_size: 64.0,
            favorites: Vec::new(),
            window: None,
        };
        let mut app = App {
            hwnd: HWND::default(),
            gfx: Gfx::new().ok(),
            gfx_ref: None,
            theme,
            icons: Icons::new(),
            scale: 1.0,
            w: 1100.0,
            h: 700.0,
            lay: Layout::default(),
            tabs: vec![Tab::new(start, View::List)],
            cur: 0,
            side_rows: sidebar::build(&prefs.favorites),
            side_scroll: 0.0,
            sidebar_w: DEFAULT_SIDEBAR,
            sidebar_on: true,
            preview_on: false,
            hover: Hit::None,
            drop_hl: None,
            search_prev: None,
            scope_rects: std::cell::RefCell::new(Vec::new()),
            pressed: Hit::None,
            focus: Focus::Content,
            active: true,
            kb: false,
            maximized: false,
            mica: false,
            prefs,
            drag: Drag::None,
            tsel: TypeSelect::default(),
            search_text: String::new(),
            status_note: None,
            last_click: None,
            offscreen: false,
        };
        app.reload(None);
        app.relayout();
        app
    }

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.cur]
    }
    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.cur]
    }

    pub fn relayout(&mut self) {
        let t = self.tab();
        self.lay = layout::compute(&LayoutIn {
            w: self.w,
            h: self.h,
            sidebar: self.sidebar_on.then_some(self.sidebar_w),
            tabs: self.tabs.len() > 1,
            scope: matches!(t.loc, Loc::Search { .. }),
            header: matches!(t.view, View::List)
                || matches!(t.loc, Loc::Search { .. } | Loc::Recents),
            preview: (self.preview_on && self.w > 900.0).then_some(280.0),
        });
        self.clamp_scroll();
    }

    pub fn invalidate(&self) {
        if !self.offscreen {
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    // ---- loading and navigation --------------------------------------------------------------

    /// (Re)loads the current tab's location, keeping selection by name.
    pub fn reload(&mut self, select: Option<&str>) {
        if matches!(self.tab().loc, Loc::Search { .. }) {
            // Results come from the index (async); the current ones stay until they arrive.
            crate::search::run(self);
            return;
        }
        let show_hidden = self.prefs.show_hidden;
        let folders_first = self.prefs.folders_first;
        let keep: Vec<String> = {
            let t = self.tab();
            t.sel
                .indices()
                .iter()
                .filter_map(|&i| t.entries.get(i).map(|e| e.name.clone()))
                .collect()
        };
        let focus_name = select
            .map(str::to_string)
            .or_else(|| self.tab().focused_name());
        let t = &mut self.tabs[self.cur];
        t.error = None;
        let res: std::io::Result<Vec<Entry>> = match &t.loc {
            Loc::Folder(p) => fsys::list_dir(p, show_hidden),
            Loc::Computer => Ok(fsys::volumes()
                .into_iter()
                .map(|v| Entry {
                    name: v.label,
                    is_dir: true,
                    size: v.total,
                    modified: 0,
                    created: 0,
                    attrs: 0x10,
                    full: Some(v.root),
                })
                .collect()),
            Loc::Recents => Ok(crate::shell::recent_items(200)),
            Loc::Tag(i) => Ok(crate::tags::items_with(*i)
                .iter()
                .filter_map(|p| fsys::entry_of(p))
                .collect()),
            Loc::Search { .. } => Ok(Vec::new()),
        };
        match res {
            Ok(mut v) => {
                if !matches!(t.loc, Loc::Recents) {
                    fsys::sort(&mut v, t.sort, t.asc, folders_first);
                }
                t.entries = v;
            }
            Err(e) => {
                t.entries.clear();
                t.error = Some(describe_io(&e));
            }
        }
        t.icon_idx = vec![i32::MIN; t.entries.len()];
        t.sel = Selection::new(t.entries.len());
        let mut sel_any = false;
        for (i, e) in t.entries.iter().enumerate() {
            if keep.iter().any(|k| k == &e.name) && select.is_none() {
                t.sel.toggle(i);
                sel_any = true;
            }
        }
        if let Some(name) = &focus_name {
            if let Some(i) = t.entries.iter().position(|e| &e.name == name) {
                if select.is_some() || !sel_any {
                    if select.is_some() {
                        t.sel.select_only(i);
                    } else {
                        t.sel.focus = Some(i);
                        t.sel.anchor = Some(i);
                    }
                } else {
                    t.sel.focus = Some(i);
                }
            }
        }
        t.loaded_at = Some(Instant::now());
        self.relayout();
        if self.tab().view == View::Columns {
            self.tab_mut().col_right = None;
            crate::views::sync_columns(self);
        }
        if select.is_some() {
            self.reveal_focus();
        }
        crate::win::watch_current(self);
        crate::win::set_title(self);
    }

    pub fn go(&mut self, loc: Loc) {
        self.go_select(loc, None);
    }

    pub fn go_select(&mut self, loc: Loc, select: Option<String>) {
        let t = self.tab_mut();
        if t.loc == loc {
            return;
        }
        let from = (t.loc.clone(), t.focused_name());
        t.hist.push(from);
        t.loc = loc;
        t.scroll = 0.0;
        crate::views::reset_columns(self);
        self.reload(select.as_deref());
        self.focus = Focus::Content;
        self.invalidate();
    }

    pub fn back(&mut self) {
        let t = self.tab_mut();
        let cur = (t.loc.clone(), t.focused_name());
        if let Some((loc, sel)) = t.hist.back(cur) {
            t.loc = loc;
            t.scroll = 0.0;
            crate::views::reset_columns(self);
            self.reload(sel.as_deref());
            self.invalidate();
        }
    }

    pub fn forward(&mut self) {
        let t = self.tab_mut();
        let cur = (t.loc.clone(), t.focused_name());
        if let Some((loc, sel)) = t.hist.forward(cur) {
            t.loc = loc;
            t.scroll = 0.0;
            crate::views::reset_columns(self);
            self.reload(sel.as_deref());
            self.invalidate();
        }
    }

    pub fn up(&mut self) {
        let t = self.tab();
        match &t.loc {
            Loc::Folder(p) => match fsys::parent(p) {
                Some(parent) => {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                    self.go_select(Loc::Folder(parent), name);
                }
                None => {
                    let label = fsys::display_name(p);
                    self.go_select(Loc::Computer, Some(label));
                }
            },
            Loc::Search { scope: Some(s), .. } => {
                let s = s.clone();
                self.go(Loc::Folder(s));
            }
            _ => {}
        }
    }

    /// Opens entry `i`: folders navigate, files open with their default app.
    pub fn open(&mut self, i: usize, new_tab: bool) {
        let t = self.tab();
        let Some(e) = t.entries.get(i) else { return };
        let path = t.path_of(i);
        if e.is_dir {
            if new_tab {
                self.new_tab(Loc::Folder(path));
            } else {
                self.go(Loc::Folder(path));
            }
        } else {
            crate::shell::open_path(self.hwnd, &path);
        }
    }

    pub fn open_selection(&mut self) {
        let idx = self.tab().sel.indices();
        if idx.len() == 1 {
            self.open(idx[0], false);
        } else {
            for i in idx {
                let t = self.tab();
                if t.entries[i].is_dir {
                    self.new_tab(Loc::Folder(t.path_of(i)));
                } else {
                    crate::shell::open_path(self.hwnd, &t.path_of(i));
                }
            }
        }
    }

    pub fn new_tab(&mut self, loc: Loc) {
        let view = self.tab().view;
        self.tabs.insert(self.cur + 1, Tab::new(loc, view));
        self.cur += 1;
        self.reload(None);
        self.invalidate();
    }

    pub fn close_tab(&mut self, i: usize) {
        if self.tabs.len() <= 1 {
            crate::win::close(self);
            return;
        }
        self.tabs.remove(i);
        if self.cur >= self.tabs.len() || self.cur > i {
            self.cur = self.cur.saturating_sub(1).min(self.tabs.len() - 1);
        }
        self.after_tab_switch();
    }

    pub fn switch_tab(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.cur {
            self.cur = i;
            self.after_tab_switch();
        }
    }

    fn after_tab_switch(&mut self) {
        self.relayout();
        crate::win::watch_current(self);
        crate::win::set_title(self);
        self.invalidate();
    }

    pub fn set_view(&mut self, v: View) {
        self.tab_mut().view = v;
        self.tab_mut().scroll = 0.0;
        crate::views::reset_columns(self);
        self.relayout();
        crate::views::sync_columns(self);
        self.reveal_focus();
        self.invalidate();
    }

    pub fn set_sort(&mut self, key: SortKey) {
        let t = self.tab_mut();
        if t.sort == key {
            t.asc = !t.asc;
        } else {
            t.sort = key;
            t.asc = !matches!(key, SortKey::Modified | SortKey::Created | SortKey::Size);
        }
        if matches!(self.tab().loc, Loc::Search { .. }) {
            if std::mem::replace(&mut self.tab_mut().by_rank, false) {
                // First click leaves relevance order for this column, ascending.
                let t = self.tab_mut();
                t.asc = !matches!(key, SortKey::Modified | SortKey::Created | SortKey::Size);
            }
            crate::search::resort(self);
        } else {
            self.reload(None);
        }
        self.invalidate();
    }

    pub fn toggle_hidden(&mut self) {
        self.prefs.show_hidden = !self.prefs.show_hidden;
        self.note(if self.prefs.show_hidden {
            "Skryté soubory jsou zobrazené"
        } else {
            "Skryté soubory jsou skryté"
        });
        self.reload(None);
    }

    pub fn note(&mut self, s: &str) {
        self.status_note = Some((s.to_string(), Instant::now()));
        crate::win::arm_note_timer(self);
        self.invalidate();
    }

    pub fn gfx(&self) -> Option<&Gfx> {
        self.gfx_ref.as_deref().or(self.gfx.as_ref())
    }

    // ---- geometry of the content area ---------------------------------------------------------

    /// Row rectangle in list view, or in the active column of column view.
    pub fn row_rect(&self, i: usize) -> Rect {
        let b = self.lay.body;
        let y = b.y + 4.0 + i as f32 * layout::ROW_H - self.tab().scroll;
        if self.tab().view == View::Columns {
            let x = crate::views::col_x(self, self.tab().cols_left.len());
            return Rect::new(x, y, layout::COL_W, layout::ROW_H);
        }
        Rect::new(b.x, y, b.w, layout::ROW_H)
    }

    pub fn grid(&self) -> layout::Grid {
        layout::grid(&self.lay.body, self.prefs.icon_size)
    }

    pub fn tile_rect(&self, i: usize) -> Rect {
        let g = self.grid();
        let (r, c) = (i / g.cols, i % g.cols);
        Rect::new(g.x0 + c as f32 * g.tile_w, self.lay.body.y + 8.0 + r as f32 * g.tile_h - self.tab().scroll, g.tile_w, g.tile_h)
    }

    pub fn content_height(&self) -> f32 {
        let n = self.tab().entries.len();
        match self.tab().view {
            View::Icons => {
                let g = self.grid();
                16.0 + n.div_ceil(g.cols) as f32 * g.tile_h
            }
            View::Gallery => crate::views::strip_width(self),
            _ => n as f32 * layout::ROW_H + 8.0,
        }
    }

    /// Visible extent along the scroll axis (gallery scrolls its strip horizontally).
    fn viewport(&self) -> f32 {
        match self.tab().view {
            View::Gallery => crate::views::gallery_parts(self).1.w,
            _ => self.lay.body.h,
        }
    }

    pub fn clamp_scroll(&mut self) {
        let max = (self.content_height() - self.viewport()).max(0.0);
        let t = self.tab_mut();
        t.scroll = t.scroll.clamp(0.0, max);
    }

    pub fn item_rect(&self, i: usize) -> Rect {
        match self.tab().view {
            View::Icons => self.tile_rect(i),
            View::Gallery => crate::views::strip_tile(self, i),
            _ => self.row_rect(i),
        }
    }

    /// Scrolls so the focused item is visible.
    pub fn reveal_focus(&mut self) {
        let Some(f) = self.tab().sel.focus else { return };
        let r = self.item_rect(f);
        if self.tab().view == View::Gallery {
            let s = crate::views::gallery_parts(self).1;
            let t = self.tab_mut();
            if r.x < s.x + 8.0 {
                t.scroll -= s.x + 8.0 - r.x;
            } else if r.r() > s.r() - 8.0 {
                t.scroll += r.r() - (s.r() - 8.0);
            }
        } else {
            let b = self.lay.body;
            let t = self.tab_mut();
            if r.y < b.y {
                t.scroll -= b.y - r.y;
            } else if r.b() > b.b() {
                t.scroll += r.b() - b.b();
            }
        }
        self.clamp_scroll();
        if self.tab().view == View::Columns {
            crate::views::reveal_columns(self);
        }
    }

    pub fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.lay.body.contains(x, y) {
            return None;
        }
        let t = self.tab();
        match t.view {
            View::Icons => {
                let g = self.grid();
                let ry = y - self.lay.body.y - 8.0 + t.scroll;
                if ry < 0.0 || x < g.x0 {
                    return None;
                }
                let (r, c) = ((ry / g.tile_h) as usize, ((x - g.x0) / g.tile_w) as usize);
                if c >= g.cols {
                    return None;
                }
                let i = r * g.cols + c;
                (i < t.entries.len() && crate::paint::tile_hit(self, i, x, y)).then_some(i)
            }
            View::Gallery => (0..t.entries.len()).find(|&i| crate::views::strip_tile(self, i).contains(x, y)),
            View::Columns => {
                let cx = crate::views::col_x(self, t.cols_left.len());
                if x < cx || x >= cx + layout::COL_W {
                    return None;
                }
                let ry = y - self.lay.body.y - 4.0 + t.scroll;
                let i = (ry / layout::ROW_H).floor();
                (i >= 0.0 && (i as usize) < t.entries.len()).then_some(i as usize)
            }
            View::List => {
                let ry = y - self.lay.body.y - 4.0 + t.scroll;
                let i = (ry / layout::ROW_H).floor();
                (i >= 0.0 && (i as usize) < t.entries.len()).then_some(i as usize)
            }
        }
    }

    pub fn items_in(&self, band: &Rect) -> Vec<usize> {
        (0..self.tab().entries.len()).filter(|&i| self.item_rect(i).inset(layout::INSET, 2.0).intersects(band)).collect()
    }

    pub fn page_rows(&self) -> isize {
        match self.tab().view {
            View::Icons => {
                let g = self.grid();
                ((self.lay.body.h / g.tile_h) as isize).max(1) * g.cols as isize
            }
            View::Gallery => 5,
            _ => ((self.lay.body.h / layout::ROW_H) as isize - 1).max(1),
        }
    }

    // ---- hit testing --------------------------------------------------------------------------

    pub fn hit(&self, x: f32, y: f32) -> Hit {
        let l = &self.lay;
        for (k, c) in l.caption.iter().enumerate() {
            if c.contains(x, y) {
                return Hit::Caption([Cap::Min, Cap::Max, Cap::Close][k]);
            }
        }
        if l.toolbar.contains(x, y) {
            if l.sidebar_btn.contains(x, y) {
                return Hit::SidebarBtn;
            }
            if l.back.contains(x, y) {
                return Hit::Back;
            }
            if l.fwd.contains(x, y) {
                return Hit::Fwd;
            }
            for (k, v) in l.views.iter().enumerate() {
                if v.contains(x, y) {
                    return Hit::View(k);
                }
            }
            if l.more.contains(x, y) {
                return Hit::More;
            }
            if l.search.contains(x, y) {
                return Hit::Search;
            }
            return Hit::Drag;
        }
        if self.sidebar_on && l.side_resize.contains(x, y) {
            return Hit::SideResize;
        }
        if self.sidebar_on && l.sidebar.contains(x, y) {
            return crate::paint::side_row_at(self, y)
                .map(Hit::SideRow)
                .unwrap_or(Hit::Drag);
        }
        if let Some(tabs) = l.tabs {
            if tabs.contains(x, y) {
                return crate::paint::tab_hit(self, x, y);
            }
        }
        if let Some(s) = l.scope {
            if s.contains(x, y) {
                return crate::paint::scope_hit(self, x, y);
            }
        }
        if let Some(h) = l.header {
            if h.contains(x, y) {
                return crate::paint::header_hit(self, x)
                    .map(Hit::Header)
                    .unwrap_or(Hit::Empty);
            }
        }
        if l.status.contains(x, y) {
            return crate::paint::status_hit(self, x, y);
        }
        if l.body.contains(x, y) && self.tab().view == View::Columns {
            return crate::views::columns_hit(self, x, y);
        }
        if l.body.contains(x, y) {
            return self.item_at(x, y).map(Hit::Row).unwrap_or(Hit::Empty);
        }
        Hit::None
    }

    // ---- mouse --------------------------------------------------------------------------------

    pub fn mouse_move(&mut self, x: f32, y: f32) {
        match &mut self.drag {
            Drag::Sidebar { start_x, start_w } => {
                let w = (*start_w + x - *start_x).clamp(layout::SIDEBAR_MIN, layout::SIDEBAR_MAX);
                if (w - self.sidebar_w).abs() > 0.1 {
                    self.sidebar_w = w;
                    self.relayout();
                    self.invalidate();
                }
                return;
            }
            Drag::Band { now, .. } => {
                *now = (x, y);
                self.update_band();
                self.autoscroll(y);
                self.invalidate();
                return;
            }
            Drag::Pending { at, index } => {
                let (dx, dy) = (x - at.0, y - at.1);
                if dx * dx + dy * dy > 16.0 {
                    let i = *index;
                    self.drag = Drag::None;
                    crate::shell::drag_out(self, i);
                }
                return;
            }
            Drag::Slider => {
                crate::paint::slider_set(self, x);
                return;
            }
            Drag::None => {}
        }
        let h = self.hit(x, y);
        if h != self.hover {
            self.hover = h;
            self.invalidate();
        }
    }

    fn update_band(&mut self) {
        if let Drag::Band { .. } = &self.drag {
            let band = self.band_rect().unwrap();
            let Drag::Band { keep, .. } = &self.drag else {
                return;
            };
            let keep = keep.clone();
            let items = self.items_in(&band);
            self.tab_mut().sel.set_many(items, &keep);
        }
    }

    /// Rubber band in window coordinates.
    pub fn band_rect(&self) -> Option<Rect> {
        match &self.drag {
            Drag::Band { origin, now, .. } => Some(Rect::from_points(
                (origin.0, origin.1 - self.tab().scroll),
                *now,
            )),
            _ => None,
        }
    }

    fn autoscroll(&mut self, y: f32) {
        let b = self.lay.body;
        let d = if y < b.y {
            y - b.y
        } else if y > b.b() {
            y - b.b()
        } else {
            0.0
        };
        if d != 0.0 {
            self.tab_mut().scroll += d.clamp(-40.0, 40.0);
            self.clamp_scroll();
        }
    }

    pub fn mouse_down(&mut self, x: f32, y: f32, ctrl: bool, shift: bool) {
        let h = self.hit(x, y);
        self.pressed = h;
        self.kb = false;
        match h {
            Hit::SideResize => {
                self.drag = Drag::Sidebar {
                    start_x: x,
                    start_w: self.sidebar_w,
                }
            }
            Hit::Row(i) => {
                self.focus = Focus::Content;
                let t = self.tab_mut();
                if shift {
                    t.sel.extend_to(i, ctrl);
                } else if ctrl {
                    t.sel.toggle(i);
                } else if !t.sel.is(i) {
                    t.sel.select_only(i);
                } else {
                    t.sel.focus = Some(i);
                }
                if !shift && !ctrl {
                    self.drag = Drag::Pending {
                        at: (x, y),
                        index: i,
                    };
                }
            }
            Hit::Empty => {
                self.focus = Focus::Content;
                let keep = if ctrl || shift {
                    self.tab().sel.indices()
                } else {
                    Vec::new()
                };
                if keep.is_empty() {
                    self.tab_mut().sel.clear();
                }
                // The origin is kept in content space so it scrolls with the items.
                let sy = self.tab().scroll;
                self.drag = Drag::Band {
                    origin: (x, y + sy),
                    now: (x, y),
                    keep,
                };
            }
            Hit::ColRow(k, r) => crate::views::column_click(self, k, Some(r)),
            Hit::ColEmpty(k) => crate::views::column_click(self, k, None),
            Hit::IconSlider => {
                self.drag = Drag::Slider;
                crate::paint::slider_set(self, x);
            }
            Hit::Search => crate::win::focus_search(self),
            _ => {}
        }
        crate::views::sync_columns(self);
        self.invalidate();
    }

    pub fn mouse_up(&mut self, x: f32, y: f32, ctrl: bool, shift: bool) {
        let was = std::mem::replace(&mut self.drag, Drag::None);
        let h = self.hit(x, y);
        let pressed = std::mem::take(&mut self.pressed);
        if let Drag::Pending { index, .. } = was {
            // Plain click on an already selected item collapses the selection to it.
            if !ctrl && !shift {
                self.tab_mut().sel.select_only(index);
            }
        }
        if h != pressed {
            self.invalidate();
            return;
        }
        match h {
            Hit::Caption(c) => crate::win::caption(self, c),
            Hit::SidebarBtn => {
                self.sidebar_on = !self.sidebar_on;
                self.relayout();
            }
            Hit::Back => self.back(),
            Hit::Fwd => self.forward(),
            Hit::View(k) => {
                self.set_view([View::Icons, View::List, View::Columns, View::Gallery][k])
            }
            Hit::More => crate::shell::more_menu(self),
            Hit::SideRow(i) => {
                self.focus = Focus::Sidebar;
                match self.side_rows.get(i).cloned() {
                    Some(Row::Item { loc, .. }) => self.go(loc),
                    Some(Row::Tag(t)) => self.go(Loc::Tag(t)),
                    _ => {}
                }
            }
            Hit::Tab(i) => self.switch_tab(i),
            Hit::TabClose(i) => self.close_tab(i),
            Hit::NewTab => {
                let loc = self.tab().loc.clone();
                self.new_tab(loc);
            }
            Hit::Header(c) => match c {
                Col::Name => self.set_sort(SortKey::Name),
                Col::Modified => self.set_sort(SortKey::Modified),
                Col::Size => self.set_sort(SortKey::Size),
                Col::Kind => self.set_sort(SortKey::Kind),
                Col::Location => {}
            },
            Hit::Scope(k) => {
                let folder = self.search_prev.as_ref().and_then(|l| l.folder()).map(Path::to_path_buf);
                if let Loc::Search { scope, .. } = &mut self.tab_mut().loc {
                    *scope = if k == 0 { None } else { folder };
                }
                self.tab_mut().scroll = 0.0;
                crate::search::run(self);
            }
            Hit::Crumb(k) => {
                if let Some(p) = crate::paint::crumb_path(self, k) {
                    self.go(p);
                }
            }
            _ => {}
        }
        self.invalidate();
    }

    pub fn double_click(&mut self, x: f32, y: f32) {
        match self.hit(x, y) {
            Hit::Row(i) if self.tab().view == View::Columns && self.tab().entries[i].is_dir => {
                crate::views::column_right(self, true);
                self.invalidate();
            }
            Hit::Row(i) => self.open(i, false),
            Hit::Drag => crate::win::caption(self, Cap::Max),
            Hit::Empty if self.lay.tabs.is_some_and(|t| t.contains(x, y)) => {
                let loc = self.tab().loc.clone();
                self.new_tab(loc);
            }
            _ => {}
        }
    }

    pub fn middle_click(&mut self, x: f32, y: f32) {
        match self.hit(x, y) {
            Hit::Row(i) => self.open(i, true),
            Hit::Tab(i) | Hit::TabClose(i) => self.close_tab(i),
            Hit::SideRow(i) => {
                if let Some(Row::Item { loc, .. }) = self.side_rows.get(i).cloned() {
                    self.new_tab(loc);
                }
            }
            _ => {}
        }
    }

    pub fn right_click(&mut self, x: f32, y: f32, screen: POINT) {
        match self.hit(x, y) {
            Hit::Row(i) => {
                if !self.tab().sel.is(i) {
                    self.tab_mut().sel.select_only(i);
                }
                self.invalidate();
                crate::shell::item_menu(self, screen);
            }
            Hit::Empty => {
                self.tab_mut().sel.clear();
                self.invalidate();
                crate::shell::background_menu(self, screen);
            }
            Hit::SideRow(i) => crate::shell::sidebar_menu(self, i, screen),
            _ => {}
        }
    }

    pub fn wheel(&mut self, x: f32, y: f32, delta: f32, ctrl: bool) {
        if ctrl && self.tab().view == View::Icons {
            self.prefs.icon_size =
                (self.prefs.icon_size + delta.signum() * 16.0).clamp(32.0, 256.0);
            self.relayout();
            self.invalidate();
            return;
        }
        let lines = crate::win::wheel_lines() as f32;
        let step = delta / 120.0 * lines * layout::ROW_H;
        if self.sidebar_on && self.lay.sidebar.contains(x, y) {
            let max = (crate::paint::sidebar_height(self) - self.lay.sidebar.h).max(0.0);
            self.side_scroll = (self.side_scroll - step).clamp(0.0, max);
        } else {
            match self.tab().view {
                View::Columns => {
                    crate::views::column_wheel(self, x, step);
                    self.invalidate();
                    return;
                }
                _ => self.tab_mut().scroll -= step,
            }
            self.clamp_scroll();
            if matches!(self.drag, Drag::Band { .. }) {
                self.update_band();
            }
        }
        self.invalidate();
    }

    // ---- keyboard -----------------------------------------------------------------------------

    pub fn key(&mut self, vk: VIRTUAL_KEY, ctrl: bool, shift: bool, alt: bool) -> bool {
        self.kb = true;
        let n = self.tab().entries.len();
        let view = self.tab().view;
        let cols = if view == View::Icons { self.grid().cols as isize } else { 1 };
        let mv = |app: &mut App, d: isize| {
            app.tab_mut().sel.move_by(d, shift, ctrl && !shift);
            app.reveal_focus();
            app.invalidate();
        };
        match (vk, ctrl, shift, alt) {
            (VK_LEFT, false, _, true) | (VK_BROWSER_BACK, ..) => self.back(),
            (VK_RIGHT, false, _, true) | (VK_BROWSER_FORWARD, ..) => self.forward(),
            (VK_OEM_4, true, false, false) => self.back(),
            (VK_OEM_6, true, false, false) => self.forward(),
            (VK_UP, false, _, true)
            | (VK_UP, true, false, false)
            | (VK_BACK, false, false, false) => self.up(),
            (VK_DOWN, true, false, false) | (VK_RETURN, false, false, false) => {
                self.open_selection()
            }
            (VK_RETURN, false, false, true) | (VK_I, true, false, false) => {
                crate::shell::properties(self)
            }
            (VK_UP, _, _, false) => mv(self, -cols),
            (VK_DOWN, _, _, false) => mv(self, cols),
            (VK_LEFT, _, _, false) if cols > 1 || view == View::Gallery => mv(self, -1),
            (VK_RIGHT, _, _, false) if cols > 1 || view == View::Gallery => mv(self, 1),
            (VK_LEFT, false, false, false) if view == View::Columns => {
                crate::views::column_left(self);
                self.invalidate();
            }
            (VK_RIGHT, false, false, false) if view == View::Columns => {
                crate::views::column_right(self, true);
                self.invalidate();
            }
            (VK_HOME, ..) => mv(self, -(n as isize)),
            (VK_END, ..) => mv(self, n as isize),
            (VK_PRIOR, ..) => {
                let p = self.page_rows();
                mv(self, -p)
            }
            (VK_NEXT, ..) => {
                let p = self.page_rows();
                mv(self, p)
            }
            (VK_SPACE, true, false, false) => {
                if let Some(f) = self.tab().sel.focus {
                    self.tab_mut().sel.toggle(f);
                    self.invalidate();
                }
            }
            (VK_SPACE, false, false, false) => crate::quicklook::toggle(self),
            (VK_A, true, false, false) => {
                self.tab_mut().sel.select_all();
                self.invalidate();
            }
            (VK_C, true, false, false) => crate::shell::verb_on_selection(self, "copy"),
            (VK_X, true, false, false) => crate::shell::verb_on_selection(self, "cut"),
            (VK_V, true, false, false) => crate::shell::paste(self),
            (VK_C, true, true, false) => crate::shell::copy_path(self),
            (VK_DELETE, false, s, false) => crate::shell::delete(self, s),
            (VK_F2, false, false, false) => crate::win::begin_rename(self),
            (VK_F5, false, false, false) => {
                self.side_rows = sidebar::build(&self.prefs.favorites);
                self.reload(None);
                self.invalidate();
            }
            (VK_N, true, true, false) => crate::shell::new_folder(self),
            (VK_N, true, false, false) => crate::win::new_window(self),
            (VK_T, true, false, false) => {
                let loc = self.tab().loc.clone();
                self.new_tab(loc);
            }
            (VK_W, true, false, false) | (VK_F4, true, false, false) => self.close_tab(self.cur),
            (VK_TAB, true, s, false) => {
                let n = self.tabs.len();
                let i = if s {
                    (self.cur + n - 1) % n
                } else {
                    (self.cur + 1) % n
                };
                self.switch_tab(i);
            }
            (VK_1, true, false, false) => self.set_view(View::Icons),
            (VK_2, true, false, false) => self.set_view(View::List),
            (VK_3, true, false, false) => self.set_view(View::Columns),
            (VK_4, true, false, false) => self.set_view(View::Gallery),
            (VK_OEM_PERIOD, true, true, false) => self.toggle_hidden(),
            (VK_F, true, false, false)
            | (VK_E, true, false, false)
            | (VK_F3, false, false, false) => crate::win::focus_search(self),
            (VK_L, true, false, false) | (VK_D, false, false, true) | (VK_G, true, true, false) => {
                crate::win::go_to_folder(self)
            }
            (VK_P, true, true, false) => {
                self.preview_on = !self.preview_on;
                self.relayout();
                self.invalidate();
            }
            (VK_S, true, true, false) | (VK_OEM_2, true, false, false) => {
                self.sidebar_on = !self.sidebar_on;
                self.relayout();
                self.invalidate();
            }
            (VK_OEM_PLUS, true, ..) | (VK_ADD, true, ..) => self.wheel(0.0, 0.0, 120.0, true),
            (VK_OEM_MINUS, true, ..) | (VK_SUBTRACT, true, ..) => {
                self.wheel(0.0, 0.0, -120.0, true)
            }
            (VK_ESCAPE, ..) => {
                if matches!(self.tab().loc, Loc::Search { .. }) {
                    crate::win::clear_search(self);
                } else {
                    self.tab_mut().sel.clear();
                    self.invalidate();
                }
            }
            (VK_APPS, ..) | (VK_F10, false, true, false) => {
                let pt = crate::paint::menu_point_for_focus(self);
                if self.tab().sel.count() > 0 {
                    crate::shell::item_menu(self, pt);
                } else {
                    crate::shell::background_menu(self, pt);
                }
            }
            _ => return false,
        }
        crate::views::sync_columns(self);
        true
    }

    /// Printable character: type-to-select.
    pub fn char_typed(&mut self, ch: char) {
        if ch.is_control() || (ch == ' ' && !self.tsel.active(Instant::now())) {
            return;
        }
        let prefix = self.tsel.push(ch, Instant::now());
        let t = self.tab();
        let names: Vec<&str> = t.entries.iter().map(|e| e.name.as_str()).collect();
        if let Some(i) = TypeSelect::find(&prefix, &names, t.sel.focus) {
            self.tab_mut().sel.select_only(i);
            self.reveal_focus();
            crate::views::sync_columns(self);
            self.invalidate();
        }
    }
}

pub fn describe_io(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(5) => "K této složce nemáte přístup.".into(),
        Some(2) | Some(3) => "Složka neexistuje nebo byla přesunuta.".into(),
        Some(21) => "Zařízení není připravené.".into(),
        Some(53) | Some(67) => "Síťové umístění není dostupné.".into(),
        _ => format!("Složku nelze otevřít ({e})."),
    }
}
