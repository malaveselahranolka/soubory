//! Offscreen rendering for visual checks: the real paint code into a PNG, no window and no screen
//! capture. `fm --render out.png --path C:\x [--w 1100 --h 700 --scale 1.5 --theme dark
//! --view icons --select 0,2 --hover 3 --tabs 2 --kb --inactive --nosidebar --preview]`
//! `fm --render out.png --glyphs E700-E7FF` draws a labelled glyph sheet.
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
