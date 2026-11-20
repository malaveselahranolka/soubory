//! Application state, navigation and input handling. Painting lives in paint.rs, Win32 plumbing in
//! win.rs, shell integration in shell.rs.
use crate::fsys::{self, Entry, SortKey};
use crate::geom::Rect;
use crate::gfx::Gfx;
use crate::groups::{self, GroupKey, Spot};
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
    /// A shell namespace folder that is not a plain folder (Recycle Bin, Network, a phone).
    Shell {
        id: crate::shellns::ShellId,
        name: String,
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
            Loc::Shell { name, .. } => name.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Icons,
    List,
    Columns,
    Gallery,
    /// Explorer's Tiles: a 48 px icon with name, kind and size beside it, in a grid.
    Tiles,
}

impl View {
    /// Views laid out as a grid of tiles (icons, tiles).
    pub fn is_grid(self) -> bool {
        matches!(self, View::Icons | View::Tiles)
    }
    /// Views that show Group by headers.
    pub fn shows_groups(self) -> bool {
        matches!(self, View::Icons | View::List | View::Tiles)
    }
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
    /// A slow location (network, device) is still being listed.
    pub loading: bool,
    /// Group by; `groups` partitions `entries` in the icon and list views (see groups.rs).
    pub group: GroupKey,
    pub groups: Vec<groups::Group>,
    /// Bring the focus into view when the pending async list arrives (after a regroup).
    pub reveal_on_load: bool,
    /// Folder sizes arrived under a press, drag or rename: re-sort once it ends (flush_deferred).
    pub resort_pending: bool,
    /// List again when shown: tags changed or a shell listing landed while the tab was hidden.
    pub stale: bool,
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
            loading: false,
            group: GroupKey::None,
            groups: Vec::new(),
            reveal_on_load: false,
            resort_pending: false,
            stale: false,
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
    /// Sorts and groups the current entries again (folder sizes arrived, an item was renamed in
    /// place), keeping the selection, focus and anchor on the same items.
    pub fn resort(&mut self, folders_first: bool) {
        let dir = self.dir();
        let at = |t: &Tab, i: Option<usize>| i.and_then(|i| t.entries.get(i)).map(|e| e.path_in(&dir));
        let selected: Vec<PathBuf> = self.sel.indices().into_iter().map(|i| self.entries[i].path_in(&dir)).collect();
        let (focus, anchor) = (at(self, self.sel.focus), at(self, self.sel.anchor));
        // Recents and a relevance-ordered search keep their own order; only the groups move.
        let ranked = matches!(self.loc, Loc::Recents) || (matches!(self.loc, Loc::Search { .. }) && self.by_rank);
        if !ranked {
            fsys::sort(&mut self.entries, self.sort, self.asc, folders_first);
        }
        groups::apply(self, folders_first);
        let pos: std::collections::HashMap<PathBuf, usize> = self.entries.iter().enumerate().map(|(i, e)| (e.path_in(&dir), i)).collect();
        self.icon_idx = vec![i32::MIN; self.entries.len()];
        let mut sel = Selection::new(self.entries.len());
        sel.set_many(selected.iter().filter_map(|p| pos.get(p).copied()), &[]);
        sel.focus = focus.and_then(|p| pos.get(&p).copied());
        sel.anchor = anchor.and_then(|p| pos.get(&p).copied());
        self.sel = sel;
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
    /// The chevron after crumb k: a menu of that folder's subfolders (Explorer's address bar).
    CrumbMenu(usize),
    IconSlider,
    /// Column view: (column index, row) outside the active column.
    ColRow(usize, usize),
    /// Column view: empty space in a column.
    ColEmpty(usize),
    /// Search scope bar: 0 = the whole computer, 1 = the folder the search started in.
    Scope(usize),
    /// Batch rename sheet controls (see batch.rs codes).
    Batch(u8),
    /// Command palette: visible row k, or elsewhere on its card.
    Palette(usize),
    /// A group header (Group by): a click selects the group, as in Explorer.
    Group(usize),
    PaletteCard,
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
    /// Pressed on an item: a drag starts past 4 px. The item is the focus, which every list
    /// replacement carries along (by path or shell ID), never a row number.
    Pending {
        at: (f32, f32),
    },
    Slider,
}

pub struct Prefs {
    pub show_hidden: bool,
    pub folders_first: bool,
    pub icon_size: f32,
    pub favorites: Vec<PathBuf>,
    /// Saved searches (Finder's smart folders): text and optional folder scope.
    pub saved: Vec<(String, Option<PathBuf>)>,
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
    /// Open "Rename N items" sheet.
    pub batch: Option<crate::batch::Batch>,
    pub palette: Option<crate::palette::Palette>,
    /// The click that just ran a palette row or sheet button: its double-click half is dropped.
    pub eat_dblclk: bool,
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
            saved: Vec::new(),
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
            side_rows: sidebar::build(&prefs),
            side_scroll: 0.0,
            sidebar_w: DEFAULT_SIDEBAR,
            sidebar_on: true,
            preview_on: false,
            hover: Hit::None,
            drop_hl: None,
            search_prev: None,
            batch: None,
            palette: None,
            eat_dblclk: false,
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
        crate::win::sync_search_field(self);
        // A shell listing sets it again; any other place never waits.
        self.tab_mut().loading = false;
        self.tab_mut().stale = false;
        if matches!(self.tab().loc, Loc::Search { .. }) {
            // Results come from the index (async); the current ones stay until they arrive.
            crate::search::run(self);
            // The scope bar comes and goes with a search.
            self.relayout();
            return;
        }
        if matches!(self.tab().loc, Loc::Shell { .. }) {
            crate::shellns::start_list(self);
            return;
        }
        let show_hidden = self.prefs.show_hidden;
        let folders_first = self.prefs.folders_first;
        // By path: names repeat in tag places and Recents (two README.md from two folders).
        let (keep, focus_path): (Vec<PathBuf>, Option<PathBuf>) = {
            let t = self.tab();
            let at = |i: usize| (i < t.entries.len()).then(|| t.path_of(i));
            (t.sel.indices().into_iter().filter_map(at).collect(), t.sel.focus.and_then(at))
        };
        let t = &mut self.tabs[self.cur];
        t.error = None;
        t.resort_pending = false;
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
                    shell: None,
                    origin: None,
                    // A drive carries its capacity; 0 (network, no media) is unknown.
                    sized: false,
                })
                .collect()),
            Loc::Recents => Ok(crate::shell::recent_items(200)),
            Loc::Tag(i) => Ok(crate::tags::items_with(*i)
                .iter()
                .filter_map(|p| fsys::entry_of(p))
                .collect()),
            Loc::Search { .. } | Loc::Shell { .. } => Ok(Vec::new()),
        };
        match res {
            Ok(mut v) => {
                // Drives carry their capacity, not an indexed size.
                if !matches!(t.loc, Loc::Computer) {
                    crate::search::known_sizes(&mut v, &t.dir());
                }
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
        groups::apply(t, folders_first);
        t.icon_idx = vec![i32::MIN; t.entries.len()];
        t.sel = Selection::new(t.entries.len());
        let dir = t.dir();
        let mut hits = Vec::new();
        if select.is_none() && !keep.is_empty() {
            let keep: std::collections::HashSet<PathBuf> = keep.into_iter().collect();
            hits = (0..t.entries.len()).filter(|&i| keep.contains(&t.entries[i].path_in(&dir))).collect();
            t.sel.set_many(hits.iter().copied(), &[]);
        }
        // `select`: a child to select by name (going up, history); otherwise the focus stays.
        let focus = match select {
            Some(name) => t.entries.iter().position(|e| e.name == name),
            None => focus_path.and_then(|p| t.entries.iter().position(|e| e.path_in(&dir) == p)).or(hits.last().copied()),
        };
        if let Some(i) = focus {
            if select.is_some() {
                t.sel.select_only(i);
            } else {
                t.sel.focus = Some(i);
                if hits.is_empty() || t.sel.anchor.is_none() {
                    t.sel.anchor = Some(i);
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
        crate::search::request_sizes(self);
    }

    /// A saved search. The search field shows its text (reload syncs it). A scoped one offers
    /// its folder in the scope bar, and clearing the field goes to that folder; an unscoped one
    /// returns to where the user was.
    pub fn open_search(&mut self, text: String, scope: Option<PathBuf>) {
        if !matches!(self.tab().loc, Loc::Search { .. }) || scope.is_some() {
            self.search_prev = Some(scope.clone().map_or_else(|| self.tab().loc.clone(), Loc::Folder));
        }
        self.tab_mut().by_rank = true;
        self.go(Loc::Search { text, scope });
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
        // Mouse Back button, UIA: no click took focus from an open rename field.
        crate::win::end_rename(self);
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
        crate::win::end_rename(self);
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
        if matches!(self.tab().loc, Loc::Shell { .. }) {
            crate::shellns::open(self, i, new_tab);
            return;
        }
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
                if matches!(self.tab().loc, Loc::Shell { .. }) {
                    crate::shellns::open(self, i, true);
                    continue;
                }
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
        if let Loc::Search { scope, .. } = &loc {
            if scope.is_some() || !matches!(self.tab().loc, Loc::Search { .. }) {
                self.search_prev = Some(scope.clone().map_or_else(|| self.tab().loc.clone(), Loc::Folder));
            }
        }
        let (view, group) = (self.tab().view, self.tab().group);
        self.tabs.insert(self.cur + 1, Tab::new(loc, view));
        self.cur += 1;
        self.tab_mut().group = group;
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

    /// Grouped by tags in a view that shows the groups (tags may have changed meanwhile).
    pub fn groups_by_tags(&self) -> bool {
        self.tab().group == GroupKey::Tags && self.tab().view.shows_groups()
    }

    fn after_tab_switch(&mut self) {
        // Stale: tags changed or a shell listing landed while hidden. A search runs again: its
        // replies are per window, so one that landed meanwhile was dropped or would fill this tab
        // with another tab's results (running bumps the window's generation and drops those).
        let t = self.tab();
        if t.stale || matches!(t.loc, Loc::Search { .. }) {
            self.reload(None);
        } else {
            crate::search::request_sizes(self);
            self.flush_deferred();
        }
        crate::win::sync_search_field(self);
        self.relayout();
        crate::win::watch_current(self);
        crate::win::set_title(self);
        self.invalidate();
    }

    pub fn set_view(&mut self, v: View) {
        crate::win::end_rename(self);
        let shows_groups = |t: &Tab| t.group != GroupKey::None && t.view.shows_groups();
        let before = shows_groups(self.tab());
        self.tab_mut().view = v;
        self.tab_mut().scroll = 0.0;
        crate::views::reset_columns(self);
        if before != shows_groups(self.tab()) {
            // Columns and gallery show the plain sort order, icons and list the grouped one.
            self.reload_and_reveal();
        }
        self.relayout();
        crate::views::sync_columns(self);
        self.reveal_focus();
        self.invalidate();
    }

    pub fn set_sort(&mut self, key: SortKey) {
        crate::win::end_rename(self);
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
            if key == SortKey::Created {
                // Creation dates are not in the index; the query fetches them.
                crate::search::run(self);
            } else {
                crate::search::resort(self);
            }
        } else {
            self.reload(None);
        }
        self.invalidate();
    }

    pub fn set_group(&mut self, key: GroupKey) {
        crate::win::end_rename(self);
        if self.tab().group == key {
            return;
        }
        self.tab_mut().group = key;
        if !self.tab().view.shows_groups() {
            // Columns and gallery show no groups; the key applies when the view comes back.
            if key != GroupKey::None {
                self.note("Skupiny se zobrazují v ikonách a v seznamu");
            }
            return;
        }
        // Async places (search, shell folders) show no stale headers until their list lands.
        let t = self.tab_mut();
        t.groups.clear();
        t.scroll = 0.0;
        // A search re-runs: grouping permuted its relevance order, and dates come with it.
        self.reload_and_reveal();
        self.invalidate();
    }

    /// Reloads and brings the focus into view, also when the list arrives later (search,
    /// shell folders). Armed before the reload, so a list that lands at once consumes it and
    /// a later live update never scrolls on its own.
    fn reload_and_reveal(&mut self) {
        let async_list = matches!(self.tab().loc, Loc::Search { .. } | Loc::Shell { .. });
        self.tab_mut().reveal_on_load = async_list;
        self.reload(None);
        self.reveal_focus();
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

    /// The groups shown in this view, if any. Checked against the list so a stale partition
    /// can never index past it.
    pub fn shown_groups(&self) -> &[groups::Group] {
        let t = self.tab();
        let whole = t.groups.last().is_some_and(|g| g.start + g.len == t.entries.len());
        if t.view.shows_groups() && whole { &t.groups } else { &[] }
    }

    /// Vertical layout of the icon or list view (rows of items under group headers).
    pub fn geo(&self) -> groups::Geo<'_> {
        let (groups, n) = (self.shown_groups(), self.tab().entries.len());
        if self.tab().view.is_grid() {
            let g = self.grid();
            groups::Geo { groups, n, cols: g.cols, item_h: g.tile_h, header_h: layout::GROUP_ICONS_H }
        } else {
            groups::Geo { groups, n, cols: 1, item_h: layout::ROW_H, header_h: layout::GROUP_LIST_H }
        }
    }

    /// Screen y of content y 0 (the first header or item).
    fn content_y0(&self) -> f32 {
        let pad = if self.tab().view.is_grid() { 8.0 } else { 4.0 };
        self.lay.body.y + pad - self.tab().scroll
    }

    pub fn group_header_rect(&self, g: usize) -> Rect {
        let geo = self.geo();
        let b = self.lay.body;
        Rect::new(b.x, self.content_y0() + geo.top(g), b.w, geo.header_h)
    }

    /// The header pinned to the top of the view while its group scrolls under it.
    pub fn pinned_header(&self) -> Option<(usize, Rect)> {
        let geo = self.geo();
        let y0 = self.content_y0();
        let b = self.lay.body;
        geo.pinned(b.y - y0).map(|(g, y)| (g, Rect::new(b.x, y0 + y, b.w, geo.header_h)))
    }

    pub fn group_header_at(&self, x: f32, y: f32) -> Option<usize> {
        if self.shown_groups().is_empty() || !self.lay.body.contains(x, y) {
            return None;
        }
        if let Some((g, r)) = self.pinned_header() {
            if r.contains(x, y) {
                return Some(g);
            }
        }
        match self.geo().at(y - self.content_y0(), 0) {
            Spot::Header(g) => Some(g),
            _ => None,
        }
    }

    /// Items whose rows intersect the view, in order.
    pub fn visible_items(&self) -> std::ops::Range<usize> {
        let b = self.lay.body;
        let n = self.tab().entries.len();
        // Rows only move down with the index, so both ends are binary searches.
        let first_true = |pred: &dyn Fn(usize) -> bool| {
            let (mut lo, mut hi) = (0, n);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if pred(mid) { hi = mid } else { lo = mid + 1 }
            }
            lo
        };
        let lo = first_true(&|i| self.item_rect(i).b() > b.y);
        let hi = first_true(&|i| self.item_rect(i).y >= b.b());
        lo..hi.max(lo)
    }

    /// Row rectangle in list view, or in the active column of column view.
    pub fn row_rect(&self, i: usize) -> Rect {
        let b = self.lay.body;
        if self.tab().view == View::Columns {
            let y = b.y + 4.0 + i as f32 * layout::ROW_H - self.tab().scroll;
            let x = crate::views::col_x(self, self.tab().cols_left.len());
            return Rect::new(x, y, layout::COL_W, layout::ROW_H);
        }
        Rect::new(b.x, self.content_y0() + self.geo().pos(i).0, b.w, layout::ROW_H)
    }

    pub fn grid(&self) -> layout::Grid {
        if self.tab().view == View::Tiles {
            layout::tile_grid(&self.lay.body)
        } else {
            layout::grid(&self.lay.body, self.prefs.icon_size)
        }
    }

    pub fn tile_rect(&self, i: usize) -> Rect {
        let g = self.grid();
        let (y, c) = self.geo().pos(i);
        Rect::new(g.x0 + c as f32 * g.tile_w, self.content_y0() + y, g.tile_w, g.tile_h)
    }

    pub fn content_height(&self) -> f32 {
        let n = self.tab().entries.len();
        match self.tab().view {
            View::Icons | View::Tiles => 16.0 + self.geo().height(),
            View::Gallery => crate::views::strip_width(self),
            View::List => self.geo().height() + 8.0,
            View::Columns => n as f32 * layout::ROW_H + 8.0,
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
            View::Icons | View::Tiles => self.tile_rect(i),
            View::Gallery => crate::views::strip_tile(self, i),
            _ => self.row_rect(i),
        }
    }

    /// Scrolls so the focused item is visible.
    pub fn reveal_focus(&mut self) {
        if let Some(f) = self.tab().sel.focus {
            self.reveal(f);
        }
    }

    /// Scrolls so item `f` is visible.
    pub fn reveal(&mut self, f: usize) {
        if f >= self.tab().entries.len() {
            return;
        }
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
            // Below the pinned group header; a group's first item brings its header along.
            let top = b.y + if self.shown_groups().is_empty() { 0.0 } else { self.geo().header_h };
            // Bottom first, then the top wins: a tile taller than the room sits at the top.
            let mut d = (r.b() - b.b()).max(0.0);
            if r.y - d < top {
                d = r.y - top;
            }
            self.tab_mut().scroll += d;
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
        if self.pinned_header().is_some_and(|(_, r)| r.contains(x, y)) {
            return None;
        }
        match t.view {
            View::Icons | View::Tiles => {
                let g = self.grid();
                if x < g.x0 {
                    return None;
                }
                let c = ((x - g.x0) / g.tile_w) as usize;
                match self.geo().at(y - self.content_y0(), c) {
                    Spot::Item(i) if t.view == View::Tiles => self.tile_rect(i).inset(4.0, 3.0).contains(x, y).then_some(i),
                    Spot::Item(i) => crate::paint::tile_hit(self, i, x, y).then_some(i),
                    _ => None,
                }
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
            View::List => match self.geo().at(y - self.content_y0(), 0) {
                Spot::Item(i) => Some(i),
                _ => None,
            },
        }
    }

    pub fn items_in(&self, band: &Rect) -> Vec<usize> {
        (0..self.tab().entries.len()).filter(|&i| self.item_rect(i).inset(layout::INSET, 2.0).intersects(band)).collect()
    }

    pub fn page_rows(&self) -> isize {
        match self.tab().view {
            View::Icons | View::Tiles => {
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
        if self.palette.is_some() {
            return crate::palette::hit(self, x, y);
        }
        if self.batch.is_some() {
            // The sheet is modal for the window, as in Finder; the title bar still moves it.
            let h = crate::batch::hit(self, x, y);
            if h != Hit::None {
                return h;
            }
            return if l.toolbar.contains(x, y) { Hit::Drag } else { Hit::None };
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
            if let Some(g) = self.group_header_at(x, y) {
                return Hit::Group(g);
            }
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
            Drag::Pending { at } => {
                let (dx, dy) = (x - at.0, y - at.1);
                if dx * dx + dy * dy > 16.0 {
                    self.drag = Drag::None;
                    let Some(i) = self.tab().sel.focus else { return };
                    crate::shell::drag_out(self, i);
                    // The modal drag loop ate the button-up.
                    self.pressed = Hit::None;
                    self.flush_deferred();
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
        if self.palette.is_some() {
            match h {
                Hit::Palette(_) => self.pressed = h,
                Hit::PaletteCard => crate::win::palette_field(self, true),
                Hit::Caption(_) => {
                    crate::palette::close(self);
                    self.pressed = h;
                }
                _ => crate::palette::close(self),
            }
            return;
        }
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
                    self.drag = Drag::Pending { at: (x, y) };
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
            Hit::Group(g) => {
                // Explorer: a header click selects its group (Ctrl adds it).
                self.focus = Focus::Content;
                if let Some(gr) = self.shown_groups().get(g).cloned() {
                    let t = self.tab_mut();
                    if !ctrl {
                        t.sel.clear();
                    }
                    t.sel.anchor = Some(gr.start);
                    t.sel.extend_to(gr.start + gr.len - 1, true);
                    t.sel.focus = Some(gr.start);
                }
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
        if let (Drag::Pending { .. }, Some(f)) = (was, self.tab().sel.focus) {
            // Plain click on an already selected item collapses the selection to it.
            if !ctrl && !shift {
                self.tab_mut().sel.select_only(f);
            }
        }
        if h != pressed {
            self.invalidate();
        } else {
            self.activate(h);
        }
        self.flush_deferred();
    }

    /// A press, drag or rename holds the rows in place: nothing may move the item under the
    /// pointer or the field (search replies, file-change reloads and size re-sorts wait).
    pub fn busy(&self) -> bool {
        !matches!(self.drag, Drag::None) || matches!(self.pressed, Hit::Row(_)) || crate::win::renaming(self)
    }

    /// Runs what waited while the window was busy: a search reply, a re-sort by folder size.
    pub fn flush_deferred(&mut self) {
        if self.busy() {
            return;
        }
        crate::search::replay(self);
        if !self.tab().resort_pending {
            return;
        }
        let folders_first = self.prefs.folders_first;
        let t = self.tab_mut();
        t.resort_pending = false;
        t.resort(folders_first);
        self.invalidate();
    }

    /// What a click on `h` does (also UI Automation's Invoke).
    pub fn activate(&mut self, h: Hit) {
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
                    Some(Row::Item { loc: Loc::Search { text, scope }, .. }) => self.open_search(text, scope),
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
            Hit::Batch(code) => {
                self.eat_dblclk = true;
                crate::batch::click(self, code)
            }
            Hit::Palette(k) => {
                self.eat_dblclk = true;
                crate::palette::run(self, Some(k))
            }
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
            Hit::CrumbMenu(k) => crate::shell::crumb_menu(self, k),
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
            Hit::Empty | Hit::Group(_) => {
                self.tab_mut().sel.clear();
                self.invalidate();
                crate::shell::background_menu(self, screen);
            }
            Hit::SideRow(i) => crate::shell::sidebar_menu(self, i, screen),
            _ => {}
        }
    }

    pub fn wheel(&mut self, x: f32, y: f32, delta: f32, ctrl: bool) {
        if self.palette.is_some() {
            if !matches!(crate::palette::hit(self, x, y), Hit::None) {
                crate::palette::scroll(self, if delta > 0.0 { -3 } else { 3 });
            }
            return;
        }
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
        if self.palette.is_some() {
            crate::palette::key(self, vk);
            return true;
        }
        if self.batch.is_some() {
            match vk {
                VK_ESCAPE => crate::batch::close(self),
                VK_RETURN => crate::batch::apply(self),
                _ => {}
            }
            return true;
        }
        self.kb = true;
        let n = self.tab().entries.len();
        let view = self.tab().view;
        let cols = if view.is_grid() { self.grid().cols as isize } else { 1 };
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
            (VK_UP, _, _, false) | (VK_DOWN, _, _, false) => {
                let down = vk == VK_DOWN;
                // A grouped grid starts each group on a new row: keep the column across headers.
                let d = match self.tab().sel.focus {
                    Some(f) if view.is_grid() && !self.shown_groups().is_empty() => {
                        self.geo().vertical(f, down) as isize - f as isize
                    }
                    _ if down => cols,
                    _ => -cols,
                };
                mv(self, d)
            }
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
            (VK_PRIOR, ..) | (VK_NEXT, ..) => {
                let down = vk == VK_NEXT;
                let d = match self.tab().sel.focus {
                    // Grouped: a screenful down the layout, in the same column.
                    Some(f) if !self.shown_groups().is_empty() => {
                        let geo = self.geo();
                        let page = (self.lay.body.h - geo.header_h - geo.item_h).max(geo.item_h);
                        let y0 = geo.pos(f).0;
                        let mut j = geo.vertical(f, down);
                        loop {
                            let k = geo.vertical(j, down);
                            if k == j || (geo.pos(k).0 - y0).abs() > page {
                                break;
                            }
                            j = k;
                        }
                        j as isize - f as isize
                    }
                    _ if down => self.page_rows(),
                    _ => -self.page_rows(),
                };
                mv(self, d)
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
            (VK_Z, true, false, false) => crate::undo::undo(self),
            (VK_Y, true, false, false) | (VK_Z, true, true, false) => crate::undo::redo(self),
            (VK_D, true, false, false) => crate::shell::duplicate(self),
            (VK_DELETE, false, s, false) => crate::shell::delete(self, s),
            (VK_F2, false, false, false) => crate::win::begin_rename(self),
            (VK_K, true, false, false) => crate::palette::open(self),
            (VK_F5, false, false, false) => {
                self.side_rows = sidebar::build(&self.prefs);
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
            (VK_5, true, false, false) => self.set_view(View::Tiles),
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
        if self.palette.is_some() {
            return;
        }
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
