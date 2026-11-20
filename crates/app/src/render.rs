//! Offscreen rendering for visual checks: the real paint code into a PNG, no window and no screen
//! capture. `fm --render out.png --path C:\x [--w 1100 --h 700 --scale 1.5 --theme dark
//! --view icons --select 0,2 --hover 3 --tabs 2 --kb --inactive --nosidebar --preview
//! --pick [--ok Nahrát]]`
//! `fm --render out.png --glyphs E700-E7FF` draws a labelled glyph sheet.
//! `fm --render out.png --icons [--scale 1.25]` draws fm's own icons on light, dark and high contrast.
//! `fm --render <dir> --brand` writes the app icon frames and the setup wizard's images (`brand.rs`).
use crate::app::{App, Focus, Hit, Loc, View};
use crate::geom::Rect;
use crate::gfx::{Align, Font};
use crate::theme::{AccentPalette, Theme};
use std::path::PathBuf;
use windows::Win32::System::Com::*;

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

pub fn cli(args: &[String]) -> i32 {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let Some(out) = args.first().map(PathBuf::from) else {
        eprintln!("usage: fm --render out.png [--path dir] ...");
        return 2;
    };
    let w: f32 = arg(args, "--w")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1100.0);
    let h: f32 = arg(args, "--h")
        .and_then(|v| v.parse().ok())
        .unwrap_or(700.0);
    let scale: f32 = arg(args, "--scale")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    if let Some(range) = arg(args, "--glyphs") {
        return glyph_sheet(&out, range, scale);
    }
    if args.iter().any(|a| a == "--brand") {
        return crate::brand::cli(&out);
    }
    if args.iter().any(|a| a == "--icons") {
        return icon_sheet(&out, scale);
    }
    let loc = arg(args, "--path")
        .map(|p| match p.strip_prefix("tag:").and_then(|n| n.parse::<usize>().ok()) {
            Some(n) => Loc::Tag(n),
            None => Loc::Folder(PathBuf::from(p)),
        })
        .unwrap_or(Loc::Computer);
    let mut app = App::new(loc);
    app.offscreen = true;
    app.w = w;
    app.h = h;
    app.scale = scale;
    match arg(args, "--theme") {
        Some("dark") => app.theme = Theme::dark(&AccentPalette::default()),
        Some("light") => app.theme = Theme::light(&AccentPalette::default()),
        Some("hc") => app.theme = Theme::high_contrast(),
        _ => {}
    }
    if let Some(v) = arg(args, "--view") {
        app.tab_mut().view = match v {
            "icons" => View::Icons,
            "columns" => View::Columns,
            "gallery" => View::Gallery,
            "tiles" => View::Tiles,
            _ => View::List,
        };
    }
    app.sidebar_on = !args.iter().any(|a| a == "--nosidebar");
    app.preview_on = args.iter().any(|a| a == "--preview");
    app.active = !args.iter().any(|a| a == "--inactive");
    app.kb = args.iter().any(|a| a == "--kb");
    if let Some(n) = arg(args, "--tabs").and_then(|v| v.parse::<usize>().ok()) {
        for _ in 1..n {
            let loc = Loc::Computer;
            app.tabs.push(crate::app::Tab::new(loc, app.tab().view));
        }
    }
    if let Some(s) = arg(args, "--search") {
        app.search_text = s.to_string();
        let scope = app.tab().loc.folder().map(|p| p.to_path_buf());
        app.tab_mut().loc = Loc::Search {
            text: s.to_string(),
            scope,
        };
        crate::search::run(&mut app);
    }
    if args.iter().any(|a| a == "--pick") {
        // A picker's bottom bar, with the file types an app might give (`--ok` its button).
        app.pick = Some(crate::pick::Picker::offline(fm_dialog_shim::proto::Request {
            ok: arg(args, "--ok").unwrap_or("").into(),
            filters: vec![("Obrázky".into(), "*.png;*.jpg;*.jpeg".into()), ("Všechny soubory".into(), "*.*".into())],
            index: 1,
            multi: true,
            ..Default::default()
        }));
        app.reload(None);
    }
    app.relayout();
    if let Some(sel) = arg(args, "--select") {
        for i in sel.split(',').filter_map(|v| v.parse::<usize>().ok()) {
            if i < app.tab().entries.len() {
                app.tab_mut().sel.toggle(i);
            }
        }
    }
    crate::views::sync_columns(&mut app);
    if let Some(f) = arg(args, "--focus-row").and_then(|v| v.parse::<usize>().ok()) {
        app.tab_mut().sel.focus = Some(f);
    }
    if let Some(hv) = arg(args, "--hover").and_then(|v| v.parse::<usize>().ok()) {
        app.hover = Hit::Row(hv);
    }
    if let Some(hs) = arg(args, "--hover-side").and_then(|v| v.parse::<usize>().ok()) {
        app.hover = Hit::SideRow(hs);
    }
    if args.iter().any(|a| a == "--focus-search") {
        app.focus = Focus::Search;
    }
    if let Some(n) = arg(args, "--note") {
        app.note(n);
    }
    let Some(gfx) = app.gfx.take() else { return 1 };
    let g = std::rc::Rc::new(gfx);
    app.gfx_ref = Some(g.clone());
    let r = g.render_png(&out, w, h, scale, |c| {
        // The window's Mica would be behind the chrome; the PNG shows the opaque fallback.
        app.mica = false;
        crate::paint::paint(&mut app, c);
    });
    match r {
        Ok(()) => {
            println!("{}", out.display());
            0
        }
        Err(e) => {
            eprintln!("render failed: {e}");
            1
        }
    }
}

fn glyph_sheet(out: &std::path::Path, range: &str, scale: f32) -> i32 {
    let Some(gfx) = crate::gfx::Gfx::new().ok() else {
        return 1;
    };
    let (a, b) = range.split_once('-').unwrap_or((range, range));
    let (a, b) = (
        u32::from_str_radix(a, 16).unwrap_or(0xE700),
        u32::from_str_radix(b, 16).unwrap_or(0xE7FF),
    );
    let cols = 16;
    let n = (b - a + 1) as usize;
    let rows = n.div_ceil(cols);
    let (cw, ch) = (64.0, 56.0);
    let (w, h) = (cols as f32 * cw, rows as f32 * ch);
    let theme = Theme::light(&AccentPalette::default());
    let r = gfx.render_png(out, w, h, scale, |c| {
        c.fill(Rect::new(0.0, 0.0, w, h), theme.card);
        for k in 0..n {
            let cp = a + k as u32;
            let (x, y) = ((k % cols) as f32 * cw, (k / cols) as f32 * ch);
            if let Some(chr) = char::from_u32(cp) {
                c.glyph(chr, Rect::new(x, y + 4.0, cw, 30.0), 20, theme.ink);
            }
            c.text(
                &format!("{cp:04X}"),
                Rect::new(x, y + 36.0, cw, 16.0),
                Font::Caption,
                theme.ink3,
                Align::Center,
            );
        }
    });
    if r.is_ok() { 0 } else { 1 }
}

/// fm's icons on each theme: today's Segoe sidebar glyphs for comparison, the Phosphor sidebar set
/// idle (bold) and selected (fill), file glyphs at list size, pages and folders at icon-view sizes.
fn icon_sheet(out: &std::path::Path, scale: f32) -> i32 {
    use crate::fileicons::Own;
    use crate::phosphor::{Ico, Weight};
    let Some(gfx) = crate::gfx::Gfx::new().ok() else {
        return 1;
    };
    let pal = AccentPalette::default();
    let themes = [Theme::light(&pal), Theme::dark(&pal), Theme::high_contrast()];
    let side = [
        Ico::ClockCounterClockwise, Ico::House, Ico::Monitor, Ico::FileText, Ico::DownloadSimple, Ico::Image,
        Ico::MusicNotes, Ico::FilmStrip, Ico::Cloud, Ico::HardDrive, Ico::Usb, Ico::Network, Ico::DesktopTower,
        Ico::Folder, Ico::Eject, Ico::Trash, Ico::FileMagnifyingGlass,
    ];
    let segoe = [
        '\u{E81C}', '\u{E80F}', '\u{E7F4}', '\u{E8A5}', '\u{E896}', '\u{EB9F}', '\u{EC4F}', '\u{E714}', '\u{E753}',
        '\u{EDA2}', '\u{E88E}', '\u{E968}', '\u{E7F8}', '\u{E8B7}', '\u{F847}', '\u{E74D}', '\u{E721}',
    ];
    let files = [
        "Poznámky.txt", "README.md", "Smlouva.pdf", "Nabídka.docx", "Rozpočet.xlsx", "Prezentace.pptx", "Fotky.zip",
        "Logo.png", "Podcast.mp3", "Video.mp4", "main.rs", "config.json", "Inter.ttf", "Windows.iso", "záloha.dat",
    ];
    let entry = |name: &str, dir: bool| crate::fsys::Entry {
        name: name.into(),
        is_dir: dir,
        size: 0,
        modified: 0,
        created: 0,
        attrs: 0,
        full: None,
        shell: None,
        origin: None,
        sized: false,
    };
    let (w, band) = (1220.0, 480.0);
    let h = band * themes.len() as f32;
    let r = gfx.render_png(out, w, h, scale, |c| {
        for (n, t) in themes.iter().enumerate() {
            let y0 = n as f32 * band;
            c.fill(Rect::new(0.0, y0, w, band), t.card);
            for (s, y) in [("Segoe dnes 16", 10.0), ("Bold 16 | 18", 40.0), ("Fill 18 vybraná", 70.0), ("Soubory 16", 100.0)] {
                c.text(s, Rect::new(12.0, y0 + y, 150.0, 24.0), Font::Caption, t.ink3, Align::Start);
            }
            for (k, ico) in side.iter().enumerate() {
                let x = 170.0 + k as f32 * 44.0;
                c.glyph(segoe[k], Rect::new(x, y0 + 10.0, 24.0, 24.0), 16, t.ink2);
                c.icon(*ico, Weight::Bold, Rect::new(x, y0 + 40.0, 20.0, 24.0), 16.0, t.ink2);
                c.icon(*ico, Weight::Bold, Rect::new(x + 20.0, y0 + 40.0, 22.0, 24.0), 18.0, t.ink2);
                c.icon(*ico, Weight::Fill, Rect::new(x, y0 + 70.0, 24.0, 24.0), 18.0, t.accent_glyph);
            }
            let mut all: Vec<(&str, bool)> = files.iter().map(|f| (*f, false)).collect();
            all.push(("Faktury", true));
            for (k, (name, dir)) in all.iter().enumerate() {
                let e = entry(name, *dir);
                let own = crate::fileicons::own(&e, false).unwrap_or(Own::Folder(None));
                crate::fileicons::draw(c, t, own, &e, Rect::new(170.0 + k as f32 * 44.0, y0 + 104.0, 16.0, 16.0), None, 1.0);
            }
            let mut x = 12.0;
            for size in [48.0f32, 64.0, 96.0, 128.0] {
                for (name, dir) in [("Faktury", true), ("Dokumenty", true), ("Poznámky.txt", false), ("Smlouva.pdf", false), ("záloha.dat", false)] {
                    let e = entry(name, dir);
                    let own = if name == "Dokumenty" {
                        Own::Folder(Some(Ico::FileText))
                    } else {
                        crate::fileicons::own(&e, false).unwrap_or(Own::Folder(None))
                    };
                    crate::fileicons::draw(c, t, own, &e, Rect::new(x, y0 + 136.0 + (128.0 - size), size, size), None, 1.0);
                    x += size + 6.0;
                }
                x += 12.0;
            }
            for (k, name) in files.iter().enumerate() {
                let e = entry(name, false);
                let own = crate::fileicons::own(&e, false).unwrap_or(Own::Folder(None));
                crate::fileicons::draw(c, t, own, &e, Rect::new(12.0 + k as f32 * 80.0, y0 + 278.0, 72.0, 72.0), None, 1.0);
            }
            for (k, name) in files.iter().take(12).enumerate() {
                let e = entry(name, false);
                let own = crate::fileicons::own(&e, false).unwrap_or(Own::Folder(None));
                crate::fileicons::draw(c, t, own, &e, Rect::new(12.0 + k as f32 * 100.0, y0 + 360.0, 96.0, 96.0), None, 1.0);
            }
        }
    });
    if r.is_ok() { 0 } else { 1 }
}
