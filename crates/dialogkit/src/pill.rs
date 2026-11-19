//! The "+ Nový ⌄" button that sits in the dialog's command bar.
//!
//! A layered, never-activating popup drawn with Direct2D + DirectWrite. Its background is almost
//! fully transparent, so the dialog's own command bar shows through and the button reads as part
//! of the dialog. It is deliberately not owned by the dialog: a cross-process owner would attach
//! the two input queues, and a hang in one process could then freeze the other.
use crate::util::*;
use std::cell::RefCell;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::Common::*, Direct2D::*, DirectWrite::*, Dxgi::Common::*, Gdi::*},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Controls::WM_MOUSELEAVE, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::*,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Light,
    Dark,
    /// Legacy (non-themed) dialogs: gray dialog face, so the button gets its own subtle chrome.
    Classic,
    HighContrast,
}

/// Posted to the notify window when the button is pressed.
pub const WM_APP_PILL_PRESSED: u32 = WM_APP + 1;

const LABEL: &str = "Nový";
const ICON_ADD: &str = "\u{E710}";
const ICON_CHEVRON: &str = "\u{E70D}";

struct Pill {
    hwnd: HWND,
    notify: HWND,
    #[allow(dead_code)] // owns the render target's factory
    d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: ID2D1DCRenderTarget,
    icon_family: &'static str,
    hover: bool,
    pressed: bool,
    visible: bool,
    style: Style,
    scale: f32,
    rect: RECT,
}

thread_local! { static PILL: RefCell<Option<Pill>> = const { RefCell::new(None) }; }

pub fn create(notify: HWND) -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let class = wide("FmDialogKitPill");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR(wide(LABEL).as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let target = d2d.CreateDCRenderTarget(&props)?;
        let icon_family = if font_exists(&dwrite, "Segoe Fluent Icons") { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" };
        PILL.with(|p| {
            *p.borrow_mut() = Some(Pill {
                hwnd,
                notify,
                d2d,
                dwrite,
                target,
                icon_family,
                hover: false,
                pressed: false,
                visible: false,
                style: Style::Light,
                scale: 1.0,
                rect: RECT::default(),
            })
        });
        Ok(())
    }
}

fn font_exists(dwrite: &IDWriteFactory, family: &str) -> bool {
    unsafe {
        let mut collection = None;
        if dwrite.GetSystemFontCollection(&mut collection, false).is_err() {
            return false;
        }
        let Some(collection) = collection else { return false };
        let mut index = 0u32;
        let mut exists = BOOL(0);
        collection.FindFamilyName(PCWSTR(wide(family).as_ptr()), &mut index, &mut exists).is_ok() && exists.as_bool()
    }
}

/// Places the button with its left edge at `anchor.left`, vertically filling `anchor`.
pub fn place(anchor: RECT, style: Style, scale: f32) {
    PILL.with(|p| {
        let mut p = p.borrow_mut();
        let Some(p) = p.as_mut() else { return };
        p.style = style;
        p.scale = scale;
        let height = if rect_h(&anchor) > 0 { rect_h(&anchor) } else { (32.0 * scale) as i32 };
        let width = p.measure_width();
        p.rect = RECT { left: anchor.left, top: anchor.top, right: anchor.left + width, bottom: anchor.top + height };
        p.visible = true;
        p.render();
        unsafe {
            let _ = SetWindowPos(p.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    });
}

pub fn hide() {
    PILL.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            if p.visible {
                p.visible = false;
                p.hover = false;
                p.pressed = false;
                unsafe {
                    let _ = ShowWindow(p.hwnd, SW_HIDE);
                }
            }
        }
    });
}

pub fn set_pressed(pressed: bool) {
    PILL.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            p.pressed = pressed;
            if p.visible {
                p.render();
            }
        }
    });
}

/// Keeps the stored position in sync after the window was moved directly.
pub fn shift(dx: i32, dy: i32) {
    PILL.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            p.rect.left += dx;
            p.rect.right += dx;
            p.rect.top += dy;
            p.rect.bottom += dy;
        }
    });
}

pub fn rect() -> Option<RECT> {
    PILL.with(|p| p.borrow().as_ref().filter(|p| p.visible).map(|p| p.rect))
}

pub fn hwnd() -> Option<HWND> {
    PILL.with(|p| p.borrow().as_ref().map(|p| p.hwnd))
}

impl Pill {
    fn px(&self, v: f32) -> f32 {
        (v * self.scale).round()
    }

    fn format(&self, family: &str, size: f32) -> Option<IDWriteTextFormat> {
        unsafe {
            self.dwrite
                .CreateTextFormat(
                    PCWSTR(wide(family).as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("cs-CZ"),
                )
                .ok()
        }
    }

    fn text_width(&self, text: &str, format: &IDWriteTextFormat) -> f32 {
        unsafe {
            let w: Vec<u16> = text.encode_utf16().collect();
            let Ok(layout) = self.dwrite.CreateTextLayout(&w, format, 10_000.0, 1_000.0) else { return 0.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            m.widthIncludingTrailingWhitespace
        }
    }

    /// Mirrors the dialog's own command buttons: 9 pt Segoe UI, 12 px icon, 8 px chevron.
    fn measure_width(&self) -> i32 {
        let (Some(text), Some(icon), Some(chev)) = (self.text_format(), self.format(self.icon_family, self.px(12.0)), self.format(self.icon_family, self.px(8.0))) else {
            return self.px(80.0) as i32;
        };
        let w = self.px(10.0)
            + self.text_width(ICON_ADD, &icon)
            + self.px(6.0)
            + self.text_width(LABEL, &text)
            + self.px(7.0)
            + self.text_width(ICON_CHEVRON, &chev)
            + self.px(10.0);
        w.ceil() as i32
    }

    fn text_format(&self) -> Option<IDWriteTextFormat> {
        self.format("Segoe UI", self.px(12.0))
    }

    fn colors(&self) -> (D2D1_COLOR_F, D2D1_COLOR_F, Option<D2D1_COLOR_F>) {
        let c = |r: f32, g: f32, b: f32, a: f32| D2D1_COLOR_F { r, g, b, a };
        let (text, hover, pressed) = match self.style {
            Style::Light => (c(0.10, 0.10, 0.10, 1.0), c(0.0, 0.0, 0.0, 0.055), c(0.0, 0.0, 0.0, 0.035)),
            Style::Dark => (c(1.0, 1.0, 1.0, 1.0), c(1.0, 1.0, 1.0, 0.065), c(1.0, 1.0, 1.0, 0.04)),
            Style::Classic => (c(0.0, 0.0, 0.0, 1.0), c(0.0, 0.47, 0.84, 0.10), c(0.0, 0.47, 0.84, 0.16)),
            Style::HighContrast => {
                let sys = |i| {
                    let v = unsafe { GetSysColor(i) };
                    c((v & 0xff) as f32 / 255.0, ((v >> 8) & 0xff) as f32 / 255.0, ((v >> 16) & 0xff) as f32 / 255.0, 1.0)
                };
                (sys(COLOR_BTNTEXT), sys(COLOR_HIGHLIGHT), sys(COLOR_HIGHLIGHT))
            }
        };
        let fill = if self.pressed {
            Some(pressed)
        } else if self.hover {
            Some(hover)
        } else {
            None
        };
        (text, hover, fill)
    }

    fn render(&mut self) {
        let (w, h) = (rect_w(&self.rect).max(1), rect_h(&self.rect).max(1));
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let Ok(dib) = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
                return;
            };
            let old = SelectObject(mem, dib.into());
            self.draw(mem, w, h);
            let pos = POINT { x: self.rect.left, y: self.rect.top };
            let size = SIZE { cx: w, cy: h };
            let src = POINT::default();
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8, ..Default::default() };
            let _ = UpdateLayeredWindow(self.hwnd, Some(screen), Some(&pos), Some(&size), Some(mem), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
    }

    fn draw(&self, dc: HDC, w: i32, h: i32) {
        let (text_color, _, fill) = self.colors();
        let (Some(text), Some(icon), Some(chev)) = (self.text_format(), self.format(self.icon_family, self.px(12.0)), self.format(self.icon_family, self.px(8.0))) else {
            return;
        };
        unsafe {
            let rt = &self.target;
            if rt.BindDC(dc, &RECT { left: 0, top: 0, right: w, bottom: h }).is_err() {
                return;
            }
            rt.BeginDraw();
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            // Alpha 1/255 everywhere: invisible, but keeps the whole button clickable.
            rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 / 255.0 }));
            let inset = self.px(2.0);
            let radius = self.px(4.0);
            let shape = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: 0.5, top: inset + 0.5, right: w as f32 - 0.5, bottom: h as f32 - inset - 0.5 },
                radiusX: radius,
                radiusY: radius,
            };
            if self.style == Style::Classic {
                if let Ok(face) = rt.CreateSolidColorBrush(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 0.85 }, None) {
                    rt.FillRoundedRectangle(&shape, &face);
                }
                if let Ok(border) = rt.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.16 }, None) {
                    rt.DrawRoundedRectangle(&shape, &border, 1.0, None);
                }
            }
            if let Some(fill) = fill {
                if let Ok(brush) = rt.CreateSolidColorBrush(&fill, None) {
                    rt.FillRoundedRectangle(&shape, &brush);
                }
            }
            if let Ok(brush) = rt.CreateSolidColorBrush(&text_color, None) {
                let mut x = self.px(10.0);
                for (s, fmt) in [(ICON_ADD, &icon), (LABEL, &text), (ICON_CHEVRON, &chev)] {
                    let ws: Vec<u16> = s.encode_utf16().collect();
                    if let Ok(layout) = self.dwrite.CreateTextLayout(&ws, fmt, 1000.0, h as f32) {
                        let _ = layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                        let mut m = DWRITE_TEXT_METRICS::default();
                        let _ = layout.GetMetrics(&mut m);
                        rt.DrawTextLayout(windows_numerics::Vector2 { X: x, Y: 0.0 }, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                        x += m.widthIncludingTrailingWhitespace + if s == ICON_ADD { self.px(6.0) } else { self.px(7.0) };
                    }
                }
            }
            let _ = rt.EndDraw(None, None);
        }
    }
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match m {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_MOUSEMOVE => {
                let start = PILL.with(|p| {
                    let mut p = p.borrow_mut();
                    let p = p.as_mut()?;
                    (!p.hover).then(|| {
                        p.hover = true;
                        p.render();
                    })
                });
                if start.is_some() {
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut tme);
                }
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                PILL.with(|p| {
                    if let Some(p) = p.borrow_mut().as_mut() {
                        p.hover = false;
                        if p.visible {
                            p.render();
                        }
                    }
                });
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                if let Some(notify) = PILL.with(|p| p.borrow().as_ref().map(|p| p.notify)) {
                    let _ = PostMessageW(Some(notify), WM_APP_PILL_PRESSED, WPARAM(0), LPARAM(0));
                }
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(h, m, w, l)
    }
}

