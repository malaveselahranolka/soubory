//! Design tokens (docs/DESIGN.md). Every colour the app paints comes from here.
use windows::Win32::{
    Graphics::Direct2D::Common::D2D1_COLOR_F,
    Graphics::Gdi::*,
    System::Registry::*,
    UI::{Accessibility::*, WindowsAndMessaging::*},
};

pub type Color = D2D1_COLOR_F;

pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}
pub const fn rgb(hex: u32) -> Color {
    rgba((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, 1.0)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Light,
    Dark,
    HighContrast,
}

/// Finder's seven tag colours, in Finder's order.
pub const TAGS: [(u32, &str); 7] = [
    (0xFF453A, "Červená"),
    (0xFF9F0A, "Oranžová"),
    (0xFFD60A, "Žlutá"),
    (0x32D74B, "Zelená"),
    (0x0A84FF, "Modrá"),
    (0xBF5AF2, "Fialová"),
    (0x8E8E93, "Šedá"),
];

#[derive(Clone, Debug)]
pub struct Theme {
    pub mode: Mode,
    /// Chrome (toolbar + sidebar) when Mica is not drawn behind it.
    pub chrome: Color,
    pub card: Color,
    pub ink: Color,
    pub ink2: Color,
    pub ink3: Color,
    pub ink4: Color,
    pub edge: Color,
    pub hover: Color,
    pub press: Color,
    pub pill: Color,
    /// Solid accent fill behind white `on_accent` text (selection capsule).
    pub accent: Color,
    pub on_accent: Color,
    /// Accent for glyphs and indicators on the chrome.
    pub accent_glyph: Color,
    pub sel_inactive: Color,
    pub field: Color,
    pub focus: Color,
    pub danger: Color,
    pub stripe: Color,
}

impl Theme {
    pub fn detect() -> Theme {
        if high_contrast() {
            Self::high_contrast()
        } else {
            let accent = accent_palette();
            if apps_use_dark() {
                Self::dark(&accent)
            } else {
                Self::light(&accent)
            }
        }
    }

    pub fn light(p: &AccentPalette) -> Theme {
        let fill = pick_fill(p, &[p.base, p.dark1, p.dark2, p.dark3]);
        Theme {
            mode: Mode::Light,
            chrome: rgb(0xEEF0F3),
            card: rgb(0xFFFFFF),
            ink: rgba(0, 0, 0, 0.894),
            ink2: rgba(0, 0, 0, 0.62),
            ink3: rgba(0, 0, 0, 0.58),
            ink4: rgba(0, 0, 0, 0.36),
            edge: rgba(0, 0, 0, 0.08),
            hover: rgba(0, 0, 0, 0.04),
            press: rgba(0, 0, 0, 0.07),
            pill: rgba(0, 0, 0, 0.07),
            accent: fill,
            on_accent: rgb(0xFFFFFF),
            accent_glyph: pick_fill(p, &[p.dark1, p.dark2, p.dark3]),
            sel_inactive: rgb(0xDCDCDC),
            field: rgba(0, 0, 0, 0.05),
            focus: fill,
            danger: rgb(0xC42B1C),
            stripe: rgba(0, 0, 0, 0.022),
        }
    }

    pub fn dark(p: &AccentPalette) -> Theme {
        let fill = pick_fill(p, &[p.dark1, p.dark2, p.dark3]);
        Theme {
            mode: Mode::Dark,
            chrome: rgb(0x202020),
            card: rgb(0x1C1C1C),
            ink: rgb(0xFFFFFF),
            ink2: rgba(255, 255, 255, 0.786),
            ink3: rgba(255, 255, 255, 0.545),
            ink4: rgba(255, 255, 255, 0.363),
            edge: rgba(255, 255, 255, 0.08),
            hover: rgba(255, 255, 255, 0.06),
            press: rgba(255, 255, 255, 0.09),
            pill: rgba(255, 255, 255, 0.08),
            accent: fill,
            on_accent: rgb(0xFFFFFF),
            accent_glyph: p.light2,
            sel_inactive: rgb(0x3A3A3A),
            field: rgba(255, 255, 255, 0.06),
            focus: p.light2,
            danger: rgb(0xFF99A4),
            stripe: rgba(255, 255, 255, 0.025),
        }
    }

    pub fn high_contrast() -> Theme {
        let sys = |i: SYS_COLOR_INDEX| {
            let c = unsafe { GetSysColor(i) };
            rgba(c as u8, (c >> 8) as u8, (c >> 16) as u8, 1.0)
        };
        let (bg, fg, hl, hl_text, gray) = (
            sys(COLOR_WINDOW),
            sys(COLOR_WINDOWTEXT),
            sys(COLOR_HIGHLIGHT),
            sys(COLOR_HIGHLIGHTTEXT),
            sys(COLOR_GRAYTEXT),
        );
        Theme {
            mode: Mode::HighContrast,
            chrome: sys(COLOR_BTNFACE),
            card: bg,
            ink: fg,
            ink2: fg,
            ink3: fg,
            ink4: gray,
            edge: fg,
            hover: Color { a: 0.25, ..hl },
            press: Color { a: 0.4, ..hl },
            pill: hl,
            accent: hl,
            on_accent: hl_text,
            accent_glyph: fg,
            sel_inactive: hl,
            field: bg,
            focus: fg,
            danger: fg,
            stripe: Color { a: 0.0, ..bg },
        }
    }

    pub fn is_dark(&self) -> bool {
        self.mode == Mode::Dark
    }

    pub fn tag(i: usize) -> Color {
        rgb(TAGS[i % TAGS.len()].0)
    }
}

/// Windows' accent ramp (Light3..Dark3), from the Explorer AccentPalette value.
#[derive(Clone, Debug)]
pub struct AccentPalette {
    pub light3: Color,
    pub light2: Color,
    pub light1: Color,
    pub base: Color,
    pub dark1: Color,
    pub dark2: Color,
    pub dark3: Color,
}

impl Default for AccentPalette {
    fn default() -> Self {
        // Windows' default blue.
        AccentPalette {
            light3: rgb(0x99EBFF),
            light2: rgb(0x60CDFF),
            light1: rgb(0x0093F9),
            base: rgb(0x0078D4),
            dark1: rgb(0x005A9E),
            dark2: rgb(0x004275),
            dark3: rgb(0x002642),
        }
    }
}

pub fn accent_palette() -> AccentPalette {
    let mut buf = [0u8; 32];
    let mut size = buf.len() as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent"),
            windows::core::w!("AccentPalette"),
            RRF_RT_REG_BINARY,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .is_ok();
    if !ok || size < 28 {
        return AccentPalette::default();
    }
    let c = |i: usize| rgba(buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2], 1.0);
    AccentPalette {
        light3: c(0),
        light2: c(1),
        light1: c(2),
        base: c(3),
        dark1: c(4),
        dark2: c(5),
        dark3: c(6),
    }
}

/// First candidate on which white text reaches 4.5:1, else the darkest.
fn pick_fill(_p: &AccentPalette, candidates: &[Color]) -> Color {
    for c in candidates {
        if contrast(*c, rgb(0xFFFFFF)) >= 4.5 {
            return *c;
        }
    }
    *candidates.last().unwrap()
}

pub fn luminance(c: Color) -> f32 {
    let f = |v: f32| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
}

pub fn contrast(a: Color, b: Color) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

pub fn apps_use_dark() -> bool {
    let mut value = 1u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            windows::core::w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    r.is_ok() && value == 0
}

pub fn high_contrast() -> bool {
    let mut hc = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            Some((&mut hc as *mut HIGHCONTRASTW).cast()),
            Default::default(),
        )
        .is_ok()
            && (hc.dwFlags.0 & HCF_HIGHCONTRASTON.0) != 0
    }
}

/// Whether the user allows UI animations (Settings → Accessibility → Animation effects).
pub fn animations_enabled() -> bool {
    let mut on = windows::core::BOOL(1);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut on as *mut windows::core::BOOL).cast()),
            Default::default(),
        );
    }
    on.as_bool()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_accent_fill_meets_contrast() {
        let p = AccentPalette::default();
        assert!(contrast(Theme::light(&p).accent, rgb(0xFFFFFF)) >= 4.5);
        assert!(contrast(Theme::dark(&p).accent, rgb(0xFFFFFF)) >= 4.5);
    }

    #[test]
    fn light_accent_falls_back_to_darker_shade() {
        let mut p = AccentPalette::default();
        p.base = rgb(0xFFD60A); // yellow: white text unreadable
        p.dark1 = rgb(0xB8860B);
        p.dark2 = rgb(0x6B4E00);
        let t = Theme::light(&p);
        assert!(contrast(t.accent, rgb(0xFFFFFF)) >= 4.5);
    }

    #[test]
    fn text_tokens_meet_contrast_on_card() {
        for t in [
            Theme::light(&AccentPalette::default()),
            Theme::dark(&AccentPalette::default()),
        ] {
            let over = |c: Color| Color {
                r: c.r * c.a + t.card.r * (1.0 - c.a),
                g: c.g * c.a + t.card.g * (1.0 - c.a),
                b: c.b * c.a + t.card.b * (1.0 - c.a),
                a: 1.0,
            };
            assert!(contrast(over(t.ink), t.card) >= 7.0);
            assert!(contrast(over(t.ink2), t.card) >= 4.5);
            assert!(contrast(over(t.ink3), t.card) >= 4.5);
            assert!(contrast(over(t.ink3), t.chrome) >= 4.5);
        }
    }
}
