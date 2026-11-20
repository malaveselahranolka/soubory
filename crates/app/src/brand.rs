//! Brand art drawn with fm's own paint code. `fm --render assets --brand` writes assets\fm.ico
//! (the app icon, 16–256 px, embedded into fm.exe by build.rs) and the setup wizard's images
//! (wizard-<light|dark>-<pct>.png, wizard-small-<pct>.png) that installer\Soubory.iss uses.
use crate::fileicons::{self, FOLDER_BACK, FOLDER_FRONT, FOLD, PAGE};
use crate::geom::Rect;
use crate::gfx::{Canvas, Gfx};
use crate::theme::{AccentPalette, Color, Theme, rgb};
use std::path::Path;
use windows::Win32::Graphics::Direct2D::{Common::*, *};
use windows_numerics::{Matrix3x2, Vector2};

const ICON_SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 96, 256];
/// Inno Setup 6.6+ image areas at 100, 125 … 250 % DPI (its docs' tables): the Welcome/Finished
/// side image and the small header image. The side image is laid out 240 DIPs wide and scaled.
const PCT: [u32; 7] = [100, 125, 150, 175, 200, 225, 250];
const WIZARD: [(f32, f32); 7] = [(202.0, 386.0), (269.0, 515.0), (336.0, 643.0), (403.0, 772.0), (430.0, 824.0), (498.0, 953.0), (534.0, 1022.0)];
const WIZARD_SMALL: [f32; 7] = [58.0, 77.0, 97.0, 116.0, 124.0, 143.0, 159.0];
const WIZARD_W: f32 = 240.0;

pub fn cli(dir: &Path) -> i32 {
    let Ok(gfx) = Gfx::new() else { return 1 };
    let mut ok = true;
    // An .ico of PNG frames (Windows Vista and later read them at every size).
    let mut frames = Vec::new();
    for px in ICON_SIZES {
        let s = px as f32;
        let tmp = dir.join(format!("icon-{px}.png"));
        ok &= gfx.render_png(&tmp, s, s, 1.0, |c| icon(c, Rect::new(0.0, 0.0, s, s))).is_ok();
        frames.push((px, std::fs::read(&tmp).unwrap_or_default()));
        let _ = std::fs::remove_file(&tmp);
    }
    let mut ico = vec![0, 0, 1, 0];
    ico.extend((frames.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * frames.len();
    for (px, png) in &frames {
        let side = if *px >= 256 { 0 } else { *px as u8 };
        ico.extend([side, side, 0, 0, 1, 0, 32, 0]);
        ico.extend((png.len() as u32).to_le_bytes());
        ico.extend((offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in &frames {
        ico.extend(png);
    }
    ok &= frames.iter().all(|(_, png)| !png.is_empty()) && std::fs::write(dir.join("fm.ico"), ico).is_ok();
    let pal = AccentPalette::default();
    for (i, pct) in PCT.iter().enumerate() {
        let (pw, ph) = WIZARD[i];
        let k = pw / WIZARD_W;
        for (name, t) in [("light", Theme::light(&pal)), ("dark", Theme::dark(&pal))] {
            let out = dir.join(format!("wizard-{name}-{pct}.png"));
            ok &= gfx.render_png(&out, WIZARD_W, ph / k, k, |c| wizard(c, &t, ph / k)).is_ok();
        }
        let out = dir.join(format!("wizard-small-{pct}.png"));
        let s = 58.0;
        ok &= gfx.render_png(&out, s, s, WIZARD_SMALL[i] / s, |c| icon(c, Rect::new(0.0, 0.0, s, s))).is_ok();
    }
    if ok { 0 } else { 1 }
}

fn mix(a: Color, b: Color, f: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * f, g: a.g + (b.g - a.g) * f, b: a.b + (b.b - a.b) * f, a: a.a }
}

/// A vertical gradient from `top` at y0 to `bottom` at y1, in the coordinates it is filled in.
fn vgrad(c: &Canvas, y0: f32, y1: f32, top: Color, bottom: Color) -> Option<ID2D1Brush> {
    let stops = [D2D1_GRADIENT_STOP { position: 0.0, color: top }, D2D1_GRADIENT_STOP { position: 1.0, color: bottom }];
    unsafe {
        let rt: &ID2D1RenderTarget = c.dc;
        let coll = rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP).ok()?;
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: Vector2 { X: 0.0, Y: y0 }, endPoint: Vector2 { X: 0.0, Y: y1 } };
        c.dc.CreateLinearGradientBrush(&props, None, &coll).ok().map(|b| b.into())
    }
}

/// Runs `f` with the canvas rotated by `deg` around (x, y).
fn rotated(c: &mut Canvas, deg: f32, x: f32, y: f32, f: impl FnOnce(&mut Canvas)) {
    let mut old = Matrix3x2::identity();
    unsafe {
        c.dc.GetTransform(&mut old);
        c.dc.SetTransform(&(Matrix3x2::rotation_around(deg, Vector2 { X: x, Y: y }) * old));
    }
    f(c);
    unsafe { c.dc.SetTransform(&old) };
}

// The app icon: fm's amber folder holding three type pages, so it reads as "files", not as
// Explorer's plain folder. One palette (the light theme's), as icons do not follow the theme.
const BACK: (u32, u32) = (0xE0A02C, 0xC98319);
const FRONT: (u32, u32) = (0xF8C653, 0xEDA42D);
const PAGES: [u32; 3] = [0x1E8E4E, 0xD93A40, 0x2F6FED];

fn icon(c: &mut Canvas, r: Rect) {
    if r.w < 40.0 { small_icon(c, r) } else { large_icon(c, r) }
}

fn large_icon(c: &mut Canvas, r: Rect) {
    let k = r.w / 96.0;
    let u = |v: f32| v * k;
    if let Some(b) = vgrad(c, 18.0, 40.0, rgb(BACK.0), rgb(BACK.1)) {
        c.path_brush(FOLDER_BACK, 96.0, r, &b);
    }
    // Pages: fm's page shape at 42/96 of the box, right of the tab, fanned around a point below.
    let white = rgb(0xFFFFFF);
    let pb = u(42.0);
    let (px, py) = (r.x + u(35.0), r.y + u(5.5));
    let (cx, cy) = (r.x + u(56.0), r.y + u(110.0));
    for (n, deg) in [(0usize, -11.0f32), (1, 11.0), (2, 0.0)] {
        let col = rgb(PAGES[n]);
        rotated(c, deg, cx, cy, |c| {
            let b = Rect::new(px, py, pb, pb);
            c.path(PAGE, 96.0, b, mix(col, white, 0.88), Some((mix(col, white, 0.4), (0.8 * k).max(0.75))));
            c.path(FOLD, 96.0, b, mix(col, white, 0.55), None);
            let lx = px + pb * 26.0 / 96.0;
            for (i, w) in [36.0f32, 26.0, 32.0].iter().enumerate() {
                let y = py + pb * (36.0 + i as f32 * 10.0) / 96.0;
                c.round(Rect::new(lx, y, pb * w / 96.0, u(1.2)), u(0.6), mix(col, white, 0.3));
            }
        });
    }
    if let Some(b) = vgrad(c, 32.0, 82.0, rgb(FRONT.0), rgb(FRONT.1)) {
        c.path_brush(FOLDER_FRONT, 96.0, r, &b);
    }
    // A light top edge on the front, the folder's lip.
    c.round(Rect::new(r.x + u(14.0), r.y + u(32.6), u(68.0), u(1.2).max(1.0)), u(0.6), Color { a: 0.45, ..white });
}

/// 16–32 px: the same parts on the pixel grid, two pages, no rotation.
fn small_icon(c: &mut Canvas, r: Rect) {
    let k = r.w / 16.0;
    let q = |x: f32, y: f32, w: f32, h: f32| Rect::new(r.x + (x * k).round(), r.y + (y * k).round(), (w * k).round(), (h * k).round());
    let back = rgb(BACK.1);
    c.round(q(1.0, 2.0, 6.0, 3.0), k, back);
    c.round(q(1.0, 3.0, 14.0, 11.0), 1.5 * k, back);
    let white = rgb(0xFFFFFF);
    for (n, (x, y)) in [(2usize, (3.0f32, 3.0f32)), (1, (8.0, 4.0))] {
        let col = rgb(PAGES[n]);
        c.round(q(x, y, 5.0, 8.0), 0.75 * k, mix(col, white, 0.85));
        c.round(q(x + 1.0, y + 1.0, 3.0, 1.0), 0.0, mix(col, white, 0.1));
    }
    c.round(q(1.0, 7.0, 14.0, 7.0), 1.5 * k, rgb(FRONT.0));
}

/// The Welcome/Finished side image: the app icon in a soft amber glow over a honeycomb of fm's own
/// type icons that fades toward the buttons.
fn wizard(c: &mut Canvas, t: &Theme, h: f32) {
    let w = WIZARD_W;
    c.fill(Rect::new(0.0, 0.0, w, h), t.card);
    let s = 128.0;
    let (ix, iy) = (((w - s) / 2.0).round(), 64.0);
    let amber = rgb(FRONT.1);
    let glow = if t.is_dark() { 0.16 } else { 0.22 };
    let stops = [
        D2D1_GRADIENT_STOP { position: 0.0, color: Color { a: glow, ..amber } },
        D2D1_GRADIENT_STOP { position: 1.0, color: Color { a: 0.0, ..amber } },
    ];
    unsafe {
        let rt: &ID2D1RenderTarget = c.dc;
        if let Ok(coll) = rt.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP) {
            let center = Vector2 { X: w / 2.0, Y: iy + s / 2.0 };
            let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES { center, gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 }, radiusX: 170.0, radiusY: 170.0 };
            if let Ok(b) = rt.CreateRadialGradientBrush(&props, None, &coll) {
                c.dc.FillRectangle(&Rect::new(0.0, 0.0, w, h).d2d(), &b);
            }
        }
    }
    icon(c, Rect::new(ix, iy, s, s));
    let rows: [&[&str]; 5] = [
        &["Smlouva.pdf", "Nabídka.docx", "Rozpočet.xlsx"],
        &["Prezentace.pptx", "Logo.png"],
        &["Podcast.mp3", "Video.mp4", "Fotky.zip"],
        &["main.rs", "config.json"],
        &["Poznámky.txt", "Windows.iso", "README.md"],
    ];
    let (size, gap, step) = (52.0, 22.0, 54.0);
    let x0 = ((w - (3.0 * size + 2.0 * gap)) / 2.0).round();
    let y0 = 236.0;
    for (row, names) in rows.iter().enumerate() {
        let y = y0 + row as f32 * step;
        let fade = (1.0 - row as f32 / rows.len() as f32).powf(1.6);
        for (col, name) in names.iter().enumerate() {
            let x = x0 + col as f32 * (size + gap) + if names.len() == 2 { (size + gap) / 2.0 } else { 0.0 };
            let e = crate::fsys::Entry {
                name: (*name).into(),
                is_dir: false,
                size: 0,
                modified: 0,
                created: 0,
                attrs: 0,
                full: None,
                shell: None,
                origin: None,
                sized: false,
            };
            if let Some(own) = fileicons::own(&e, false) {
                fileicons::draw(c, t, own, &e, Rect::new(x, y, size, size), None, 0.12 + 0.88 * fade);
            }
        }
    }
}
