//! Command palette (Ctrl+K): every command and every sidebar place, filtered as you type
//! (diacritics-insensitive, `nova sl` finds "Nová složka"), run with Enter or a click.
use crate::app::{App, Hit, Loc};
use crate::geom::Rect;
use crate::gfx::{Align, Canvas, Font};
use crate::shell::cmd;
use crate::theme::{Color, Theme};

#[derive(Clone)]
enum Act {
    Own(u32),
    F(fn(&mut App)),
    Go(Loc),
}

#[derive(Clone)]
pub struct Item {
    label: String,
    keys: &'static str,
    act: Act,
}

pub struct Palette {
    all: Vec<Item>,
    /// Indices into `all` that match the query, best first.
    shown: Vec<usize>,
    pub sel: usize,
    top: usize,
}

const W: f32 = 560.0;
const FIELD: f32 = 40.0;
const ROW: f32 = 32.0;
const ROWS: usize = 9;

const COMMANDS: &[(&str, &str, Act)] = &[
    ("Nová složka", "Ctrl+Shift+N", Act::Own(cmd::NEW_FOLDER)),
    ("Nová složka s výběrem", "", Act::F(crate::shell::new_folder_with_selection)),
    ("Nová karta", "Ctrl+T", Act::Own(cmd::NEW_TAB)),
    ("Nové okno", "Ctrl+N", Act::Own(cmd::NEW_WINDOW)),
    ("Duplikovat", "Ctrl+D", Act::F(crate::shell::duplicate)),
    ("Přejmenovat", "F2", Act::F(crate::win::begin_rename)),
    ("Přesunout do koše", "Delete", Act::F(|a| crate::shell::delete(a, false))),
    ("Rychlý náhled", "Mezerník", Act::F(crate::quicklook::toggle)),
    ("Vlastnosti", "Alt+Enter", Act::F(crate::shell::properties)),
    ("Kopírovat cestu", "Ctrl+Shift+C", Act::F(crate::shell::copy_path)),
    ("Vrátit zpět", "Ctrl+Z", Act::F(crate::undo::undo)),
    ("Znovu", "Ctrl+Y", Act::F(crate::undo::redo)),
    ("Vybrat vše", "Ctrl+A", Act::Own(cmd::SELECT_ALL)),
    ("Zobrazit jako ikony", "Ctrl+1", Act::Own(cmd::VIEW0)),
    ("Zobrazit jako seznam", "Ctrl+2", Act::Own(cmd::VIEW0 + 1)),
    ("Zobrazit ve sloupcích", "Ctrl+3", Act::Own(cmd::VIEW0 + 2)),
    ("Zobrazit jako galerii", "Ctrl+4", Act::Own(cmd::VIEW0 + 3)),
    ("Řadit podle názvu", "", Act::Own(cmd::SORT0)),
    ("Řadit podle data změny", "", Act::Own(cmd::SORT0 + 1)),
    ("Řadit podle data vytvoření", "", Act::Own(cmd::SORT0 + 2)),
    ("Řadit podle velikosti", "", Act::Own(cmd::SORT0 + 3)),
    ("Řadit podle druhu", "", Act::Own(cmd::SORT0 + 4)),
    ("Seskupit podle názvu", "", Act::Own(cmd::GROUP0 + 1)),
    ("Seskupit podle druhu", "", Act::Own(cmd::GROUP0 + 2)),
    ("Seskupit podle data změny", "", Act::Own(cmd::GROUP0 + 3)),
    ("Seskupit podle data vytvoření", "", Act::Own(cmd::GROUP0 + 4)),
    ("Seskupit podle velikosti", "", Act::Own(cmd::GROUP0 + 5)),
    ("Seskupit podle štítků", "", Act::Own(cmd::GROUP0 + 6)),
    ("Neseskupovat", "", Act::Own(cmd::GROUP0)),
    ("Zobrazit nebo skrýt skryté soubory", "Ctrl+Shift+.", Act::Own(cmd::HIDDEN)),
    ("Náhledový panel", "Ctrl+Shift+P", Act::Own(cmd::PREVIEW)),
    ("Postranní panel", "Ctrl+/", Act::Own(cmd::SIDEBAR)),
    ("Hledat", "Ctrl+F", Act::F(crate::win::focus_search)),
    ("Uložit hledání", "", Act::Own(cmd::SAVE_SEARCH)),
    ("Přejít na složku…", "Ctrl+L", Act::Own(cmd::GOTO)),
    ("Zpět", "Alt+←", Act::F(App::back)),
    ("Vpřed", "Alt+→", Act::F(App::forward)),
    ("O úroveň výš", "Alt+↑", Act::F(App::up)),
    ("Obnovit", "F5", Act::Own(cmd::REFRESH)),
    ("Otevřít v Terminálu", "", Act::Own(cmd::TERMINAL)),
    ("Vlastnosti složky", "", Act::Own(cmd::PROPS)),
    ("Připojit síťovou jednotku…", "", Act::Own(cmd::MAP_DRIVE)),
    ("Odpojit síťovou jednotku…", "", Act::Own(cmd::UNMAP_DRIVE)),
];

fn items(app: &App) -> Vec<Item> {
    let mut v: Vec<Item> = COMMANDS.iter().map(|(l, k, a)| Item { label: (*l).into(), keys: k, act: a.clone() }).collect();
    for r in &app.side_rows {
        match r {
            crate::sidebar::Row::Item { label, loc, .. } => {
                v.push(Item { label: format!("Přejít: {label}"), keys: "", act: Act::Go(loc.clone()) })
            }
            crate::sidebar::Row::Tag(i) => v.push(Item {
                label: format!("Štítek: {}", crate::theme::TAGS[*i].1),
                keys: "",
                act: Act::Go(Loc::Tag(*i)),
            }),
            crate::sidebar::Row::Header(_) => {}
        }
    }
    v
}

/// Best first: the label starts with the query, then a word starts with each query word, then
/// each query word appears somewhere. Ties keep the list order.
pub fn rank(labels: &[&str], query: &str) -> Vec<usize> {
    let q = fm_fs::fold(query.trim());
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut scored: Vec<(u8, usize)> = labels
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let f = fm_fs::fold(l);
            if !words.iter().all(|w| f.contains(w)) {
                return None;
            }
            let starts = |w: &str| f.split(|c: char| !c.is_alphanumeric()).any(|part| part.starts_with(w));
            let score = if f.starts_with(&q) {
                0
            } else if words.iter().all(|w| starts(w)) {
                1
            } else {
                2
            };
            Some((score, i))
        })
        .collect();
    scored.sort_by_key(|&(s, i)| (s, i));
    scored.into_iter().map(|(_, i)| i).collect()
}

pub fn open(app: &mut App) {
    if app.palette.is_some() || app.batch.is_some() {
        return;
    }
    let all = items(app);
    let shown = (0..all.len()).collect();
    app.palette = Some(Palette { all, shown, sel: 0, top: 0 });
    crate::win::palette_field(app, true);
    app.invalidate();
}

pub fn close(app: &mut App) {
    if app.palette.take().is_some() {
        crate::win::palette_field(app, false);
        app.invalidate();
    }
}

/// The field changed.
pub fn filter(app: &mut App, query: &str) {
    if let Some(p) = app.palette.as_mut() {
        let labels: Vec<&str> = p.all.iter().map(|i| i.label.as_str()).collect();
        p.shown = rank(&labels, query);
        p.sel = 0;
        p.top = 0;
        app.invalidate();
    }
}

/// Keys while the palette is open (from its field or the window). True when handled.
pub fn key(app: &mut App, vk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let Some(p) = app.palette.as_mut() else { return false };
    let n = p.shown.len();
    match vk {
        VK_ESCAPE => close(app),
        VK_RETURN => run(app, None),
        VK_DOWN | VK_UP | VK_NEXT | VK_PRIOR | VK_HOME | VK_END if n > 0 => {
            p.sel = match vk {
                VK_DOWN => (p.sel + 1) % n,
                VK_UP => (p.sel + n - 1) % n,
                VK_NEXT => (p.sel + ROWS).min(n - 1),
                VK_PRIOR => p.sel.saturating_sub(ROWS),
                VK_HOME => 0,
                _ => n - 1,
            };
            if p.sel < p.top {
                p.top = p.sel;
            } else if p.sel >= p.top + ROWS {
                p.top = p.sel + 1 - ROWS;
            }
            app.invalidate();
        }
        _ => return false,
    }
    true
}

/// Wheel over the card: moves the visible window, the selection stays put.
pub fn scroll(app: &mut App, rows: isize) {
    if let Some(p) = app.palette.as_mut() {
        let max = p.shown.len().saturating_sub(ROWS);
        p.top = (p.top as isize + rows).clamp(0, max as isize) as usize;
        app.invalidate();
    }
}

/// Runs the selected row, or visible row `row` (a click). The palette closes first, so a
/// command that opens its own field (Go to Folder, search) gets the focus.
pub fn run(app: &mut App, row: Option<usize>) {
    let Some(p) = app.palette.as_ref() else { return };
    let k = row.map_or(p.sel, |r| p.top + r);
    let Some(item) = p.shown.get(k).and_then(|&i| p.all.get(i)).cloned() else { return };
    close(app);
    match item.act {
        Act::Own(c) => crate::shell::run_command(app, c),
        Act::F(f) => f(app),
        Act::Go(loc) => match loc {
            Loc::Search { text, scope } => app.open_search(text, scope),
            other => app.go(other),
        },
    }
    app.invalidate();
}

pub struct Parts {
    pub card: Rect,
    pub field: Rect,
    pub rows: Vec<Rect>,
}

pub fn parts(app: &App) -> Parts {
    let b = app.lay.body;
    let n = app.palette.as_ref().map_or(0, |p| p.shown.len().min(ROWS));
    let list_h = if n == 0 { ROW } else { n as f32 * ROW };
    let card = Rect::new(b.cx() - W.min(b.w - 24.0) / 2.0, b.y + 12.0, W.min(b.w - 24.0), FIELD + 1.0 + 6.0 + list_h + 6.0);
    let field = Rect::new(card.x + 40.0, card.y + 8.0, card.w - 52.0, FIELD - 16.0);
    let rows = (0..n).map(|k| Rect::new(card.x + 6.0, card.y + FIELD + 7.0 + k as f32 * ROW, card.w - 12.0, ROW)).collect();
    Parts { card, field, rows }
}

pub fn edit_rect(app: &App) -> Rect {
    parts(app).field
}

pub fn hit(app: &App, x: f32, y: f32) -> Hit {
    let p = parts(app);
    match p.rows.iter().position(|r| r.contains(x, y)) {
        Some(k) => Hit::Palette(k),
        None if p.card.contains(x, y) => Hit::PaletteCard,
        None => Hit::None,
    }
}

pub fn paint(app: &App, c: &mut Canvas, t: &Theme) {
    let Some(p) = &app.palette else { return };
    let parts = parts(app);
    let card = parts.card;
    for (grow, a) in [(10.0, 0.07), (3.0, 0.12)] {
        c.round(card.inset(-grow, -grow).offset(0.0, grow / 2.0), 12.0 + grow, Color { r: 0.0, g: 0.0, b: 0.0, a });
    }
    c.round(card, 12.0, t.card);
    c.round_stroke(card, 12.0, t.edge, 0.0);
    c.glyph(crate::paint::g::SEARCH, Rect::new(card.x + 14.0, card.y, 20.0, FIELD), 14, t.ink3);
    c.hline(card.x, card.r(), card.y + FIELD, t.edge);
    if p.shown.is_empty() {
        let r = Rect::new(card.x + 16.0, card.y + FIELD + 6.0, card.w - 32.0, ROW);
        c.text("Žádný příkaz neodpovídá", r, Font::Body, t.ink3, Align::Start);
        return;
    }
    for (k, r) in parts.rows.iter().enumerate() {
        let i = p.top + k;
        let Some(item) = p.shown.get(i).and_then(|&j| p.all.get(j)) else { break };
        let on = i == p.sel;
        if on {
            c.round(*r, 6.0, t.accent);
        } else if app.hover == Hit::Palette(k) {
            c.round(*r, 6.0, t.hover);
        }
        let (ink, ink2) = if on { (t.on_accent, t.on_accent) } else { (t.ink, t.ink3) };
        let text = r.inset(10.0, 0.0);
        let kw = if item.keys.is_empty() { 0.0 } else { c.measure(item.keys, Font::Small) + 8.0 };
        c.text(&item.label, Rect::new(text.x, text.y, text.w - kw, text.h), Font::Body, ink, Align::Start);
        if kw > 0.0 {
            c.text(item.keys, text, Font::Small, ink2, Align::End);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rank;

    #[test]
    fn ranks_prefix_then_word_starts_then_contains() {
        let labels = ["Nová karta", "Nová složka", "Nová složka s výběrem", "Zobrazit jako seznam", "Přejít: Stažené soubory", "Řadit podle názvu"];
        assert_eq!(rank(&labels, "nova sl"), [1, 2]);
        assert_eq!(rank(&labels, "sloz"), [1, 2]);
        assert_eq!(rank(&labels, "stazene"), [4]);
        assert_eq!(rank(&labels, "radit"), [5]);
        assert_eq!(rank(&labels, "seznam"), [3]);
        assert_eq!(rank(&labels, "  ").len(), labels.len());
        assert!(rank(&labels, "xyz").is_empty());
        // "kar" starts a word of "Nová karta" only; "ova" only appears inside words.
        assert_eq!(rank(&labels, "kar"), [0]);
        assert_eq!(rank(&labels, "ova")[..2], [0, 1]);
    }
}
