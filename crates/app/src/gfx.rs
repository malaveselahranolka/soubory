//! Direct2D/DirectWrite drawing on a DirectComposition swap chain, plus the same paint path into
//! a WIC bitmap for PNG output (`fm --render`).
use crate::geom::Rect;
use crate::theme::Color;
use std::{cell::RefCell, collections::HashMap};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Direct2D::{Common::*, *},
            Direct3D::*,
            Direct3D11::*,
            DirectComposition::*,
            DirectWrite::*,
            Dxgi::{Common::*, *},
            Imaging::*,
        },
        System::Com::*,
    },
    core::*,
};
use windows_numerics::{Matrix3x2, Vector2};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Font {
    /// 13 px regular: rows, sidebar, fields.
    Body,
    /// 13 px semibold.
    BodyStrong,
    /// 12 px: column headers, path bar, buttons.
    Small,
    /// 11 px: captions, section headers, status.
    Caption,
    /// 11 px semibold: sidebar section headers.
    CaptionStrong,
    /// 15 px semibold: window title in the toolbar.
    Title,
    /// 20 px semibold: Quick Look / gallery headings.
    Heading,
    /// 28 px semibold: empty states.
    Display,
    /// Segoe Fluent Icons at the given px.
    Glyph(u8),
    /// 12 px Cascadia Mono for text previews.
    Mono,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Start,
    Center,
    End,
}

pub struct Gfx {
    pub d2d: ID2D1Factory1,
    pub dwrite: IDWriteFactory,
    pub wic: IWICImagingFactory,
    formats: RefCell<HashMap<(Font, u8, bool), IDWriteTextFormat>>,
    ellipsis: RefCell<HashMap<Font, IDWriteInlineObject>>,
    text_family: String,
    display_family: String,
    glyph_family: String,
    mono_family: String,
    dev: Option<Device>,
}

struct Device {
    dc: ID2D1DeviceContext,
    swap: IDXGISwapChain1,
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    size: (u32, u32),
}

impl Gfx {
    pub fn new() -> Result<Gfx> {
        unsafe {
            let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let has = |name: &str| -> bool {
                let mut coll = None;
                if dwrite.GetSystemFontCollection(&mut coll, false).is_err() {
                    return false;
                }
                let Some(coll) = coll else { return false };
                let (mut idx, mut exists) = (0u32, BOOL(0));
                let w: Vec<u16> = name.encode_utf16().chain([0]).collect();
                coll.FindFamilyName(PCWSTR(w.as_ptr()), &mut idx, &mut exists)
                    .is_ok()
                    && exists.as_bool()
            };
            let pick = |names: &[&str]| {
                names
                    .iter()
                    .find(|n| has(n))
                    .unwrap_or(names.last().unwrap())
                    .to_string()
            };
            Ok(Gfx {
                text_family: pick(&["Segoe UI Variable Text", "Segoe UI"]),
                display_family: pick(&["Segoe UI Variable Display", "Segoe UI"]),
                glyph_family: pick(&["Segoe Fluent Icons", "Segoe MDL2 Assets"]),
                mono_family: pick(&["Cascadia Mono", "Consolas"]),
                d2d,
                dwrite,
                wic,
                formats: RefCell::new(HashMap::new()),
                ellipsis: RefCell::new(HashMap::new()),
                dev: None,
            })
        }
    }

    /// Text format for a font, with ellipsis trimming and no wrapping unless `wrap`.
    pub fn format(&self, font: Font, align: Align, wrap: bool) -> IDWriteTextFormat {
        let key = (font, align as u8, wrap);
        if let Some(f) = self.formats.borrow().get(&key) {
            return f.clone();
        }
        let (family, size, weight) = match font {
            Font::Body => (&self.text_family, 13.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::BodyStrong => (&self.text_family, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Small => (&self.text_family, 12.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::Caption => (&self.text_family, 11.0, DWRITE_FONT_WEIGHT_NORMAL),
            Font::CaptionStrong => (&self.text_family, 11.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Title => (&self.display_family, 15.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Heading => (&self.display_family, 20.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Display => (&self.display_family, 28.0, DWRITE_FONT_WEIGHT_SEMI_BOLD),
            Font::Glyph(px) => (&self.glyph_family, px as f32, DWRITE_FONT_WEIGHT_NORMAL),
            Font::Mono => (&self.mono_family, 12.0, DWRITE_FONT_WEIGHT_NORMAL),
        };
        let fam: Vec<u16> = family.encode_utf16().chain([0]).collect();
        let f = unsafe {
            let f = self
                .dwrite
                .CreateTextFormat(
                    PCWSTR(fam.as_ptr()),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("cs-CZ"),
                )
                .expect("text format");
            let _ = f.SetTextAlignment(match align {
                Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            let _ = f.SetParagraphAlignment(if wrap {
                DWRITE_PARAGRAPH_ALIGNMENT_NEAR
            } else {
                DWRITE_PARAGRAPH_ALIGNMENT_CENTER
            });
            let _ = f.SetWordWrapping(if wrap {
                DWRITE_WORD_WRAPPING_EMERGENCY_BREAK
            } else {
                DWRITE_WORD_WRAPPING_NO_WRAP
            });
            if !matches!(font, Font::Glyph(_)) {
                let sign = self.ellipsis_for(font, &f);
                let trim = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                let _ = f.SetTrimming(&trim, sign.as_ref());
            }
            f
        };
        self.formats.borrow_mut().insert(key, f.clone());
        f
    }

    fn ellipsis_for(&self, font: Font, f: &IDWriteTextFormat) -> Option<IDWriteInlineObject> {
        if let Some(e) = self.ellipsis.borrow().get(&font) {
            return Some(e.clone());
        }
        let e = unsafe { self.dwrite.CreateEllipsisTrimmingSign(f).ok()? };
        self.ellipsis.borrow_mut().insert(font, e.clone());
        Some(e)
    }

    /// Width of a single line of text in DIPs.
    pub fn measure(&self, text: &str, font: Font) -> f32 {
        let fmt = self.format(font, Align::Start, false);
        let w: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self.dwrite.CreateTextLayout(&w, &fmt, 10_000.0, 100.0) else {
                return 0.0;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            m.widthIncludingTrailingWhitespace
        }
    }

    /// Wrapped text height for a given width.
    pub fn measure_wrapped(&self, text: &str, font: Font, width: f32) -> f32 {
        let fmt = self.format(font, Align::Start, true);
        let w: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let Ok(layout) = self.dwrite.CreateTextLayout(&w, &fmt, width, 10_000.0) else {
                return 0.0;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            m.height
        }
    }

    // ---- window swap chain -------------------------------------------------------------------

    /// Creates the composition swap chain for `hwnd` (created with WS_EX_NOREDIRECTIONBITMAP).
    pub fn attach(&mut self, hwnd: HWND, w: u32, h: u32) -> Result<()> {
        unsafe {
            let d3d = take_prewarmed().or_else(create_d3d).ok_or(Error::from(E_FAIL))?;
            crate::win::trace("d3d11_device");
            let dxgi: IDXGIDevice = d3d.cast()?;
            let d2d_dev = self.d2d.CreateDevice(&dxgi)?;
            let dc = d2d_dev.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w.max(1),
                Height: h.max(1),
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
                Stereo: FALSE,
            };
            crate::win::trace("d2d_dc_factory");
            let swap = factory.CreateSwapChainForComposition(&dxgi, &desc, None)?;
            crate::win::trace("swapchain");
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            // Not topmost: child EDITs (search, rename) must stay above the composition visual.
            let target = dcomp.CreateTargetForHwnd(hwnd, false)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            dcomp.Commit()?;
            crate::win::trace("dcomp");
            self.dev = Some(Device {
                dc,
                swap,
                _dcomp: dcomp,
                _target: target,
                _visual: visual,
                size: (w.max(1), h.max(1)),
            });
            Ok(())
        }
    }

    /// A plain HWND render target, for windows that host foreign child windows (Quick Look's
    /// preview handlers), which a composition-only window cannot show.
    pub fn hwnd_target(&self, hwnd: HWND, w: u32, h: u32, scale: f32) -> Result<ID2D1HwndRenderTarget> {
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0 * scale,
            dpiY: 96.0 * scale,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U { width: w.max(1), height: h.max(1) },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        unsafe { self.d2d.CreateHwndRenderTarget(&props, &hp) }
    }

    /// Draws one frame into an HWND render target. False when the target must be recreated.
    pub fn frame_hwnd(&self, rt: &ID2D1HwndRenderTarget, scale: f32, paint: impl FnOnce(&mut Canvas)) -> bool {
        let Ok(dc) = rt.cast::<ID2D1DeviceContext>() else { return false };
        unsafe { dc.SetDpi(96.0 * scale, 96.0 * scale) };
        draw_into(self, &dc, scale, paint)
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if let Some(d) = &mut self.dev {
            let (w, h) = (w.max(1), h.max(1));
            if d.size == (w, h) {
                return;
            }
            unsafe {
                d.dc.SetTarget(None);
                if d.swap
                    .ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
                    .is_ok()
                {
                    d.size = (w, h);
                }
            }
        }
    }

    pub fn has_device(&self) -> bool {
        self.dev.is_some()
    }

    /// Draws one frame into the window. Returns false when the device was lost (caller re-attaches).
    pub fn frame(&self, scale: f32, paint: impl FnOnce(&mut Canvas)) -> bool {
        let Some(d) = &self.dev else { return false };
        unsafe {
            let Ok(surface) = d.swap.GetBuffer::<IDXGISurface>(0) else {
                return false;
            };
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0 * scale,
                dpiY: 96.0 * scale,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: std::mem::ManuallyDrop::new(None),
            };
            let Ok(bmp) = d.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props)) else {
                return false;
            };
            d.dc.SetTarget(&bmp);
            d.dc.SetDpi(96.0 * scale, 96.0 * scale);
            let ok = draw_into(self, &d.dc, scale, paint);
            d.dc.SetTarget(None);
            ok && d.swap.Present(1, DXGI_PRESENT(0)).is_ok()
        }
    }

    pub fn drop_device(&mut self) {
        self.dev = None;
    }

    /// The window's device context, for creating bitmaps that are drawn into the window.
    pub fn dc(&self) -> Option<&ID2D1DeviceContext> {
        self.dev.as_ref().map(|d| &d.dc)
    }

    // ---- offscreen PNG ----------------------------------------------------------------------

    /// Paints into a WIC bitmap of `w`×`h` DIPs at `scale` and writes a PNG.
    pub fn render_png(
        &self,
        path: &std::path::Path,
        w: f32,
        h: f32,
        scale: f32,
        paint: impl FnOnce(&mut Canvas),
    ) -> Result<()> {
        unsafe {
            let (pw, ph) = ((w * scale).round() as u32, (h * scale).round() as u32);
            let bmp = self.wic.CreateBitmap(
                pw,
                ph,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnLoad,
            )?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0 * scale,
                dpiY: 96.0 * scale,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt = self.d2d.CreateWicBitmapRenderTarget(&bmp, &props)?;
            let dc: ID2D1DeviceContext = rt.cast()?;
            if !draw_into(self, &dc, scale, paint) {
                return Err(Error::new(E_FAIL, "draw failed"));
            }
            let w: Vec<u16> = path.to_string_lossy().encode_utf16().chain([0]).collect();
            let stream = self.wic.CreateStream()?;
            stream.InitializeFromFilename(PCWSTR(w.as_ptr()), GENERIC_WRITE.0)?;
            let enc = self
                .wic
                .CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            enc.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.unwrap();
            frame.Initialize(None)?;
            frame.SetSize(pw, ph)?;
            let mut fmt = GUID_WICPixelFormat32bppPBGRA;
            frame.SetPixelFormat(&mut fmt)?;
            frame.WriteSource(&bmp, std::ptr::null())?;
            frame.Commit()?;
            enc.Commit()?;
            Ok(())
        }
    }
}

/// Creating the D3D device is the slowest step of a cold start (driver load); `prewarm` starts
/// it on another thread while the window and the first listing are built, `attach` collects it.
struct SendDevice(ID3D11Device);
// SAFETY: ID3D11Device is free-threaded; only the immediate context is not, and it is not shared.
unsafe impl Send for SendDevice {}
static PREWARM: std::sync::Mutex<Option<std::thread::JoinHandle<Option<SendDevice>>>> = std::sync::Mutex::new(None);

pub fn prewarm() {
    let h = std::thread::spawn(|| create_d3d().map(SendDevice));
    *PREWARM.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
}

fn take_prewarmed() -> Option<ID3D11Device> {
    let h = PREWARM.lock().unwrap_or_else(|e| e.into_inner()).take()?;
    h.join().ok().flatten().map(|d| d.0)
}

/// The power-saving GPU first: DWM composes there anyway, and waking a discrete GPU costs half
/// a second at start and battery afterwards. Then any hardware adapter, then WARP.
fn create_d3d() -> Option<ID3D11Device> {
    unsafe {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        let make = |a: Option<&IDXGIAdapter1>, kind| {
            let mut d3d = None;
            let a: Option<IDXGIAdapter> = a.and_then(|a| a.cast().ok());
            D3D11CreateDevice(a.as_ref(), kind, HMODULE::default(), flags, None, D3D11_SDK_VERSION, Some(&mut d3d), None, None)
                .ok()
                .and(d3d)
        };
        let low: Option<IDXGIAdapter1> = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|f| f.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_MINIMUM_POWER))
            .ok();
        low.as_ref()
            .and_then(|a| make(Some(a), D3D_DRIVER_TYPE_UNKNOWN))
            .or_else(|| make(None, D3D_DRIVER_TYPE_HARDWARE))
            .or_else(|| make(None, D3D_DRIVER_TYPE_WARP))
    }
}

fn draw_into(
    g: &Gfx,
    dc: &ID2D1DeviceContext,
    scale: f32,
    paint: impl FnOnce(&mut Canvas),
) -> bool {
    unsafe {
        let Ok(brush) = dc.CreateSolidColorBrush(
            &Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            None,
        ) else {
            return false;
        };
        dc.BeginDraw();
        dc.SetTransform(&Matrix3x2::identity());
        dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        dc.Clear(Some(&Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        }));
        let mut c = Canvas {
            g,
            dc,
            brush,
            scale,
            clips: 0,
        };
        paint(&mut c);
        while c.clips > 0 {
            c.pop_clip();
        }
        dc.EndDraw(None, None).is_ok()
    }
}

/// What paint code draws with. Coordinates are DIPs.
pub struct Canvas<'a> {
    pub g: &'a Gfx,
    pub dc: &'a ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    pub scale: f32,
    clips: u32,
}

impl Canvas<'_> {
    fn b(&self, c: Color) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&c) };
        &self.brush
    }

    pub fn fill(&mut self, r: Rect, c: Color) {
        if c.a <= 0.0 {
            return;
        }
        unsafe { self.dc.FillRectangle(&r.d2d(), self.b(c)) };
    }

    pub fn round(&mut self, r: Rect, radius: f32, c: Color) {
        if c.a <= 0.0 {
            return;
        }
        let rr = D2D1_ROUNDED_RECT {
            rect: r.d2d(),
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { self.dc.FillRoundedRectangle(&rr, self.b(c)) };
    }

    /// Rounded outline drawn inside `r` (hairline = 1 physical pixel when `width` is 0).
    pub fn round_stroke(&mut self, r: Rect, radius: f32, c: Color, width: f32) {
        let w = if width <= 0.0 {
            1.0 / self.scale
        } else {
            width
        };
        let r = r.inset(w / 2.0, w / 2.0);
        let rr = D2D1_ROUNDED_RECT {
            rect: r.d2d(),
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { self.dc.DrawRoundedRectangle(&rr, self.b(c), w, None) };
    }

    /// Rounded rect with only some corners rounded: [top-left, top-right, bottom-right, bottom-left].
    pub fn round_corners(&mut self, r: Rect, radii: [f32; 4], fill: Color, stroke: Option<Color>) {
        unsafe {
            let Ok(geo) = self.g.d2d.CreatePathGeometry() else {
                return;
            };
            let Ok(sink) = geo.Open() else { return };
            let [tl, tr, br, bl] = radii;
            let arc = |x: f32, y: f32, rad: f32| D2D1_ARC_SEGMENT {
                point: Vector2 { X: x, Y: y },
                size: D2D_SIZE_F {
                    width: rad,
                    height: rad,
                },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_SMALL,
            };
            sink.BeginFigure(
                Vector2 {
                    X: r.x + tl,
                    Y: r.y,
                },
                D2D1_FIGURE_BEGIN_FILLED,
            );
            sink.AddLine(Vector2 {
                X: r.r() - tr,
                Y: r.y,
            });
            if tr > 0.0 {
                sink.AddArc(&arc(r.r(), r.y + tr, tr));
            }
            sink.AddLine(Vector2 {
                X: r.r(),
                Y: r.b() - br,
            });
            if br > 0.0 {
                sink.AddArc(&arc(r.r() - br, r.b(), br));
            }
            sink.AddLine(Vector2 {
                X: r.x + bl,
                Y: r.b(),
            });
            if bl > 0.0 {
                sink.AddArc(&arc(r.x, r.b() - bl, bl));
            }
            sink.AddLine(Vector2 {
                X: r.x,
                Y: r.y + tl,
            });
            if tl > 0.0 {
                sink.AddArc(&arc(r.x + tl, r.y, tl));
            }
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            let _ = sink.Close();
            self.dc.FillGeometry(&geo, self.b(fill), None);
            if let Some(s) = stroke {
                self.dc
                    .DrawGeometry(&geo, self.b(s), 1.0 / self.scale, None);
            }
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, c: Color) {
        let e = D2D1_ELLIPSE {
            point: Vector2 { X: cx, Y: cy },
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { self.dc.FillEllipse(&e, self.b(c)) };
    }

    pub fn circle_stroke(&mut self, cx: f32, cy: f32, radius: f32, c: Color, width: f32) {
        let e = D2D1_ELLIPSE {
            point: Vector2 { X: cx, Y: cy },
            radiusX: radius,
            radiusY: radius,
        };
        unsafe { self.dc.DrawEllipse(&e, self.b(c), width, None) };
    }

    /// Horizontal hairline (1 physical pixel) at `y`.
    pub fn hline(&mut self, x0: f32, x1: f32, y: f32, c: Color) {
        let px = 1.0 / self.scale;
        let y = (y * self.scale).floor() / self.scale;
        self.fill(Rect::new(x0, y, x1 - x0, px), c);
    }

    pub fn vline(&mut self, x: f32, y0: f32, y1: f32, c: Color) {
        let px = 1.0 / self.scale;
        let x = (x * self.scale).floor() / self.scale;
        self.fill(Rect::new(x, y0, px, y1 - y0), c);
    }

    pub fn line(&mut self, a: (f32, f32), b: (f32, f32), c: Color, width: f32) {
        unsafe {
            self.dc.DrawLine(
                Vector2 { X: a.0, Y: a.1 },
                Vector2 { X: b.0, Y: b.1 },
                self.b(c),
                width,
                None,
            )
        };
    }

    /// One line of text, vertically centered in `r`, trimmed with an ellipsis.
    pub fn text(&mut self, s: &str, r: Rect, font: Font, c: Color, align: Align) {
        if s.is_empty() || r.w <= 0.0 {
            return;
        }
        let fmt = self.g.format(font, align, false);
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            self.dc.DrawText(
                &w,
                &fmt,
                &r.d2d(),
                self.b(c),
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// Wrapped text from the top of `r`.
    pub fn text_wrapped(&mut self, s: &str, r: Rect, font: Font, c: Color, align: Align) {
        let fmt = self.g.format(font, align, true);
        let w: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            self.dc.DrawText(
                &w,
                &fmt,
                &r.d2d(),
                self.b(c),
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// A Segoe Fluent Icons glyph centered in `r`.
    pub fn glyph(&mut self, ch: char, r: Rect, px: u8, c: Color) {
        let mut buf = [0u16; 2];
        let s = ch.encode_utf16(&mut buf);
        let fmt = self.g.format(Font::Glyph(px), Align::Center, false);
        unsafe {
            self.dc.DrawText(
                s,
                &fmt,
                &r.d2d(),
                self.b(c),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    pub fn bitmap(&mut self, bmp: &ID2D1Bitmap, r: Rect, opacity: f32) {
        unsafe {
            self.dc.DrawBitmap(
                bmp,
                Some(&r.d2d()),
                opacity,
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                None,
                None,
            );
        }
    }

    pub fn push_clip(&mut self, r: Rect) {
        unsafe {
            self.dc
                .PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED)
        };
        self.clips += 1;
    }

    pub fn pop_clip(&mut self) {
        if self.clips > 0 {
            unsafe { self.dc.PopAxisAlignedClip() };
            self.clips -= 1;
        }
    }

    /// Creates a bitmap usable on this canvas from premultiplied BGRA pixels.
    pub fn bitmap_from_bgra(&self, w: u32, h: u32, pixels: &[u8]) -> Option<ID2D1Bitmap> {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let rt: &ID2D1RenderTarget = self.dc;
        unsafe {
            rt.CreateBitmap(
                D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                Some(pixels.as_ptr().cast()),
                w * 4,
                &props,
            )
            .ok()
        }
    }

    pub fn measure(&self, s: &str, font: Font) -> f32 {
        self.g.measure(s, font)
    }

    /// A path shortened from the left so it fits `w`: "…\\demo\\Projekty".
    pub fn fit_path(&self, s: &str, font: Font, w: f32) -> String {
        if self.measure(s, font) <= w {
            return s.to_string();
        }
        let parts: Vec<&str> = s.split('\\').collect();
        for k in 1..parts.len() {
            let t = format!("…\\{}", parts[k..].join("\\"));
            if self.measure(&t, font) <= w {
                return t;
            }
        }
        format!("…\\{}", parts.last().unwrap_or(&""))
    }

    /// Pixel snap helper.
    pub fn px(&self, v: f32) -> f32 {
        (v * self.scale).round() / self.scale
    }
}
