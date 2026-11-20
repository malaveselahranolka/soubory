//! Quick Look (Space): a floating preview window that follows the selection.
//!
//! Images are decoded with WIC on a worker thread, text files are shown as text (UTF-8, UTF-16 or
//! the Czech ANSI code page), everything else goes to the file type's Windows preview handler
//! (PDF, Office, …) running out of process in prevhost. Cloud placeholders are never read: they
//! show the cached thumbnail or the icon and say how to download them.
use crate::app::App;
use crate::fsys::{self, Entry, wide};
use crate::geom::Rect;
use crate::gfx::{Align, Canvas, Font, Gfx};
use crate::theme::Theme;
use std::{
    cell::RefCell,
    ffi::c_void,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::*, Dwm::*, Gdi::*, Imaging::*},
        System::Com::*,
        UI::{
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::*,
            Shell::{PropertiesSystem::*, *},
            WindowsAndMessaging::*,
        },
    },
    core::*,
};

const WM_APP_QL_IMAGE: u32 = WM_APP + 30;
const BAR_H: f32 = 44.0;
const TEXT_LIMIT: usize = 512 * 1024;
const PREVIEW_HANDLER_KEY: &str = "{8895b1c6-b41f-4c1c-a562-0d564250836f}";

enum Content {
    Loading,
    Image { w: u32, h: u32, data: Vec<u8> },
    Text(String),
    Handler,
    Icon,
    Cloud,
}

struct Ql {
    hwnd: HWND,
    host: HWND,
    gfx: Gfx,
    rt: Option<ID2D1HwndRenderTarget>,
    bmp: Option<ID2D1Bitmap>,
    path: PathBuf,
    entry: Entry,
    index: (usize, usize),
    content: Content,
    handler: Option<IPreviewHandler>,
    scroll: f32,
    text_h: f32,
    scale: f32,
    generation: u64,
    hover_open: bool,
    theme: Theme,
    previewing: bool,
}

thread_local! {
    static QL: RefCell<Option<Box<Ql>>> = const { RefCell::new(None) };
}

pub fn is_open() -> bool {
    QL.with(|q| q.borrow().is_some())
}

pub fn toggle(app: &mut App) {
    if is_open() {
        close();
        return;
    }
    open(app);
}

fn current_item(app: &App) -> Option<(PathBuf, Entry, (usize, usize))> {
    let t = app.tab();
    let i = t.sel.focus.filter(|&f| t.sel.is(f)).or(t.sel.first())?;
    Some((t.path_of(i), t.entries[i].clone(), (i + 1, t.entries.len())))
}

fn open(app: &mut App) {
    let Some((path, entry, index)) = current_item(app) else { return };
    let Ok(gfx) = Gfx::new() else { return };
    unsafe {
        let hinst = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        let class = w!("FmQuickLook");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let scale = app.scale;
        let (w, h) = initial_size(app, &entry);
        // Centered over the main window.
        let mut mr = RECT::default();
        let _ = GetWindowRect(app.hwnd, &mut mr);
        let (pw, ph) = ((w * scale) as i32, (h * scale) as i32);
        let x = (mr.left + mr.right - pw) / 2;
        let y = (mr.top + mr.bottom - ph) / 2;
        let title = wide(&entry.name);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            x,
            y.max(mr.top),
            pw,
            ph,
            Some(app.hwnd),
            None,
            Some(hinst.into()),
            None,
        ) else {
            return;
        };
        let dark = BOOL::from(app.theme.is_dark());
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, (&dark as *const BOOL).cast(), 4);
        let backdrop = DWMSBT_TRANSIENTWINDOW;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, (&backdrop as *const DWM_SYSTEMBACKDROP_TYPE).cast(), 4);
        let host = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_CLIPCHILDREN,
            0,
            0,
            1,
            1,
            Some(hwnd),
            None,
            Some(hinst.into()),
            None,
        )
        .unwrap_or_default();
        let ql = Box::new(Ql {
            hwnd,
            host,
            gfx,
            rt: None,
            bmp: None,
            path: PathBuf::new(),
            entry: entry.clone(),
            index,
            content: Content::Loading,
            handler: None,
            scroll: 0.0,
            text_h: 0.0,
            scale: GetDpiForWindow(hwnd) as f32 / 96.0,
            generation: 0,
            hover_open: false,
            theme: app.theme.clone(),
            previewing: false,
        });
        QL.with(|q| *q.borrow_mut() = Some(ql));
        load(app, path, entry, index);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}

fn initial_size(app: &App, e: &Entry) -> (f32, f32) {
    let (mw, mh) = (app.w.max(600.0), app.h.max(400.0));
    let ext = e.ext().to_ascii_lowercase();
    if is_text(&ext) {
        ((mw * 0.7).clamp(560.0, 980.0), (mh * 0.85).clamp(420.0, 900.0))
    } else {
        ((mw * 0.8).clamp(560.0, 1200.0), (mh * 0.85).clamp(420.0, 900.0))
    }
}

pub fn close() {
    let ql = QL.with(|q| q.borrow_mut().take());
    if let Some(ql) = ql {
        unsafe {
            if let Some(h) = &ql.handler {
                let _ = h.Unload();
            }
            let _ = DestroyWindow(ql.hwnd);
        }
    }
}

/// Called after every main-window paint: follow the selection.
pub fn follow(app: &mut App) {
    if !is_open() {
        return;
    }
    let Some((path, entry, index)) = current_item(app) else { return };
    let same = QL.with(|q| q.borrow().as_ref().is_some_and(|ql| ql.path == path && ql.index == index));
    if !same {
        load(app, path, entry, index);
    }
}

fn is_text(ext: &str) -> bool {
    matches!(
        ext,
        "txt" | "md" | "markdown" | "log" | "ini" | "cfg" | "conf" | "toml" | "yaml" | "yml" | "json" | "jsonc" | "xml" | "csv" | "tsv" | "rs" | "c" | "h" | "cpp" | "hpp" | "cs" | "java" | "kt" | "go" | "py" | "rb" | "php" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "css" | "scss" | "less" | "html" | "htm" | "vue" | "svelte" | "sh" | "bash" | "ps1" | "psm1" | "bat" | "cmd" | "sql" | "lua" | "swift" | "dart" | "gitignore" | "editorconfig" | "env" | "reg" | "srt" | "vtt" | "nfo" | "properties" | "gradle" | "lock"
    )
}

fn is_image(ext: &str) -> bool {
    matches!(ext, "jpg" | "jpeg" | "jfif" | "png" | "gif" | "bmp" | "dib" | "tif" | "tiff" | "ico" | "webp" | "heic" | "heif" | "avif" | "jxr" | "wdp" | "dds")
}

fn load(_app: &mut App, path: PathBuf, entry: Entry, index: (usize, usize)) {
    let ext = entry.ext().to_ascii_lowercase();
    with_ql(|ql| {
        unsafe {
            if let Some(h) = ql.handler.take() {
                let _ = h.Unload();
            }
            let _ = ShowWindow(ql.host, SW_HIDE);
            let title = wide(&entry.name);
            let _ = SetWindowTextW(ql.hwnd, PCWSTR(title.as_ptr()));
        }
        ql.generation += 1;
        ql.path = path.clone();
        ql.entry = entry.clone();
        ql.index = index;
        ql.scroll = 0.0;
        ql.bmp = None;
        ql.previewing = false;
        ql.content = if entry.is_placeholder() {
            Content::Cloud
        } else if entry.is_dir {
            Content::Icon
        } else if is_image(&ext) {
            spawn_decode(ql.hwnd, path.clone(), ql.generation);
            Content::Loading
        } else if is_text(&ext) || (entry.size < 64 * 1024 && ext.is_empty()) {
            match read_text(&path) {
                Some(t) => Content::Text(t),
                None => Content::Icon,
            }
        } else {
            match preview_handler(&ext, &path) {
                Some(h) => {
                    ql.handler = Some(h);
                    Content::Handler
                }
                None => Content::Icon,
            }
        };
        ql.text_h = 0.0;
        layout_host(ql);
        unsafe {
            let _ = InvalidateRect(Some(ql.hwnd), None, false);
        }
    });
}

fn content_rect(ql: &Ql) -> Rect {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(ql.hwnd, &mut rc);
    }
    let (w, h) = (rc.right as f32 / ql.scale, rc.bottom as f32 / ql.scale);
    Rect::new(0.0, BAR_H, w, (h - BAR_H).max(0.0))
}

fn layout_host(ql: &mut Ql) {
    let r = content_rect(ql);
    let px = RECT {
        left: (r.x * ql.scale) as i32,
        top: (r.y * ql.scale) as i32,
        right: (r.r() * ql.scale) as i32,
        bottom: (r.b() * ql.scale) as i32,
    };
    unsafe {
        let _ = SetWindowPos(ql.host, None, px.left, px.top, px.right - px.left, px.bottom - px.top, SWP_NOZORDER | SWP_NOACTIVATE);
        if let Some(h) = &ql.handler {
            let local = RECT { left: 0, top: 0, right: px.right - px.left, bottom: px.bottom - px.top };
            if matches!(ql.content, Content::Handler) && !ql.previewing {
                ql.previewing = true;
                let _ = ShowWindow(ql.host, SW_SHOW);
                let r = h.SetWindow(ql.host, &local).and_then(|_| h.SetRect(&local)).and_then(|_| h.DoPreview());
                if let Err(e) = r {
                    crate::prefs::log(&format!("preview handler failed for {}: {e}", ql.path.display()));
                    ql.content = Content::Icon;
                    let _ = ShowWindow(ql.host, SW_HIDE);
                }
            } else {
                let _ = h.SetRect(&local);
            }
        }
    }
}

/// The file type's preview handler, out of process (prevhost), initialised with the file.
fn preview_handler(ext: &str, path: &Path) -> Option<IPreviewHandler> {
    if ext.is_empty() {
        return None;
    }
    unsafe {
        let mut buf = [0u16; 64];
        let mut len = buf.len() as u32;
        let e = wide(&format!(".{ext}"));
        let key = wide(PREVIEW_HANDLER_KEY);
        AssocQueryStringW(ASSOCF_INIT_DEFAULTTOSTAR, ASSOCSTR_SHELLEXTENSION, PCWSTR(e.as_ptr()), PCWSTR(key.as_ptr()), Some(PWSTR(buf.as_mut_ptr())), &mut len).ok().ok()?;
        let clsid = CLSIDFromString(PCWSTR(buf.as_ptr())).ok()?;
        let h: IPreviewHandler = match CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER) {
            Ok(h) => h,
            Err(e) => {
                crate::prefs::log(&format!("preview handler {} for .{ext} not created: {e}", String::from_utf16_lossy(&buf[..len as usize])));
                return None;
            }
        };
        let w = wide(&path.to_string_lossy());
        if let Ok(init) = h.cast::<IInitializeWithItem>() {
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(w.as_ptr()), None) {
                if init.Initialize(&item, STGM_READ.0).is_ok() {
                    return Some(h);
                }
            }
        }
        if let Ok(init) = h.cast::<IInitializeWithFile>() {
            if init.Initialize(PCWSTR(w.as_ptr()), STGM_READ.0).is_ok() {
                return Some(h);
            }
        }
        if let Ok(init) = h.cast::<IInitializeWithStream>() {
            if let Ok(stream) = SHCreateStreamOnFileEx(PCWSTR(w.as_ptr()), STGM_READ.0 | STGM_SHARE_DENY_NONE.0, 0, false, None) {
                if init.Initialize(&stream, STGM_READ.0).is_ok() {
                    return Some(h);
                }
            }
        }
        crate::prefs::log(&format!("preview handler for .{ext} could not be initialised"));
        None
    }
}

fn read_text(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.by_ref().take(TEXT_LIMIT as u64).read_to_end(&mut buf).ok()?;
    let truncated = buf.len() >= TEXT_LIMIT;
    let mut s = decode_text(&buf)?;
    if truncated {
        s.push_str("\n\n…");
    }
    Some(s)
}

/// UTF-8 (with or without BOM), UTF-16 LE/BE with BOM, else Windows-1250 (Czech ANSI).
/// Binary content (NULs in the first 4 KB without a UTF-16 BOM) is rejected.
fn decode_text(b: &[u8]) -> Option<String> {
    if b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xFE, 0xFF]) {
        let le = b[0] == 0xFF;
        let units: Vec<u16> = b[2..].chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        return Some(String::from_utf16_lossy(&units));
    }
    let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b);
    if b.iter().take(4096).any(|&c| c == 0) {
        return None;
    }
    match std::str::from_utf8(b) {
        Ok(s) => Some(s.to_string()),
        // Only a multibyte sequence cut off by the size limit: still UTF-8.
        Err(e) if e.error_len().is_none() => Some(String::from_utf8_lossy(b).into_owned()),
        Err(_) => Some(cp1250(b)),
    }
}

fn cp1250(b: &[u8]) -> String {
    use windows::Win32::Globalization::*;
    unsafe {
        let n = MultiByteToWideChar(1250, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), b, None);
        let mut out = vec![0u16; n.max(0) as usize];
        MultiByteToWideChar(1250, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), b, Some(&mut out));
        String::from_utf16_lossy(&out)
    }
}

struct Decoded {
    generation: u64,
    w: u32,
    h: u32,
    data: Vec<u8>,
}

fn spawn_decode(hwnd: HWND, path: PathBuf, generation: u64) {
    let h = hwnd.0 as usize;
    std::thread::spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let result = decode_image(&path, 2560);
        let msg = result.map(|(w, hh, data)| Box::new(Decoded { generation, w, h: hh, data }));
        let ptr = msg.map(Box::into_raw).unwrap_or(std::ptr::null_mut());
        if PostMessageW(Some(HWND(h as *mut c_void)), WM_APP_QL_IMAGE, WPARAM(generation as usize), LPARAM(ptr as isize)).is_err() && !ptr.is_null() {
            drop(Box::from_raw(ptr));
        }
        CoUninitialize();
    });
}

/// Decodes the first frame, applies EXIF orientation, scales to fit `max` px, premultiplied BGRA.
fn decode_image(path: &Path, max: u32) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let w = wide(&path.to_string_lossy());
        let dec = wic.CreateDecoderFromFilename(PCWSTR(w.as_ptr()), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let (mut fw, mut fh) = (0, 0);
        frame.GetSize(&mut fw, &mut fh).ok()?;
        let orientation = exif_orientation(&frame);
        let k = (max as f32 / fw.max(fh) as f32).min(1.0);
        let (sw, sh) = (((fw as f32 * k).round() as u32).max(1), ((fh as f32 * k).round() as u32).max(1));
        let scaler = wic.CreateBitmapScaler().ok()?;
        scaler.Initialize(&frame, sw, sh, WICBitmapInterpolationModeHighQualityCubic).ok()?;
        let mut src: IWICBitmapSource = scaler.cast().ok()?;
        if let Some(t) = orientation {
            let rot = wic.CreateBitmapFlipRotator().ok()?;
            rot.Initialize(&src, t).ok()?;
            src = rot.cast().ok()?;
        }
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(&src, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let (mut ow, mut oh) = (0, 0);
        conv.GetSize(&mut ow, &mut oh).ok()?;
        let mut data = vec![0u8; (ow * oh * 4) as usize];
        conv.CopyPixels(std::ptr::null(), ow * 4, &mut data).ok()?;
        Some((ow, oh, data))
    }
}

fn exif_orientation(frame: &IWICBitmapFrameDecode) -> Option<WICBitmapTransformOptions> {
    unsafe {
        let reader = frame.GetMetadataQueryReader().ok()?;
        let mut v = windows::Win32::System::Com::StructuredStorage::PROPVARIANT::default();
        let q = w!("/app1/ifd/{ushort=274}");
        reader.GetMetadataByName(q, &mut v).ok()?;
        let n = v.Anonymous.Anonymous.Anonymous.uiVal;
        match n {
            3 => Some(WICBitmapTransformRotate180),
            6 => Some(WICBitmapTransformRotate90),
            8 => Some(WICBitmapTransformRotate270),
            2 => Some(WICBitmapTransformFlipHorizontal),
            4 => Some(WICBitmapTransformFlipVertical),
            _ => None,
        }
    }
}

// ---- window ------------------------------------------------------------------------------------------

fn paint(ql: &mut Ql, theme: &Theme) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(ql.hwnd, &mut rc);
    }
    let (pw, ph) = (rc.right.max(1) as u32, rc.bottom.max(1) as u32);
    if ql.rt.is_none() {
        ql.rt = ql.gfx.hwnd_target(ql.hwnd, pw, ph, ql.scale).ok();
        ql.bmp = None;
    }
    let Some(rt) = ql.rt.clone() else { return };
    unsafe {
        let _ = rt.Resize(&windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U { width: pw, height: ph });
    }
    let (w, h) = (pw as f32 / ql.scale, ph as f32 / ql.scale);
    let gfx_ptr: *const Gfx = &ql.gfx;
    let ok = unsafe { &*gfx_ptr }.frame_hwnd(&rt, ql.scale, |c| draw(ql, c, theme, w, h));
    if !ok {
        ql.rt = None;
        ql.bmp = None;
    }
}

fn draw(ql: &mut Ql, c: &mut Canvas, t: &Theme, w: f32, h: f32) {
    c.fill(Rect::new(0.0, 0.0, w, h), t.card);
    // Bar: position and the Open button.
    let bar = Rect::new(0.0, 0.0, w, BAR_H);
    c.fill(bar, t.chrome);
    c.hline(0.0, w, BAR_H - 1.0 / c.scale, t.edge);
    let (i, n) = ql.index;
    c.text(&format!("{i} z {n}"), bar.inset(16.0, 0.0), Font::Small, t.ink2, Align::Start);
    let open = open_button(w);
    c.round(open, 6.0, if ql.hover_open { t.press } else { t.field });
    c.round_stroke(open, 6.0, t.edge, 0.0);
    c.text("Otevřít", open, Font::Small, t.ink, Align::Center);
    let kind = if ql.entry.is_dir && ql.entry.size == 0 { fsys::kind_name(&ql.entry) } else { format!("{} – {}", fsys::kind_name(&ql.entry), fsys::format_size(ql.entry.size)) };
    c.text(&kind, Rect::new(120.0, 0.0, (open.x - 136.0).max(0.0), BAR_H), Font::Small, t.ink3, Align::Center);

    let area = Rect::new(0.0, BAR_H, w, h - BAR_H);
    match &ql.content {
        Content::Image { w: iw, h: ih, data } => {
            if ql.bmp.is_none() {
                ql.bmp = c.bitmap_from_bgra(*iw, *ih, data);
            }
            if let Some(b) = &ql.bmp {
                let (bw, bh) = (*iw as f32 / c.scale, *ih as f32 / c.scale);
                let inner = area.inset(16.0, 16.0);
                let k = (inner.w / bw).min(inner.h / bh).min(1.0);
                c.bitmap(b, inner.center(bw * k, bh * k).snap(c.scale), 1.0);
            }
        }
        Content::Text(s) => {
            let inner = area.inset(24.0, 16.0);
            if ql.text_h == 0.0 {
                ql.text_h = c.g.measure_wrapped(s, Font::Mono, inner.w);
            }
            c.push_clip(area);
            let r = Rect::new(inner.x, inner.y - ql.scroll, inner.w, ql.text_h + 20.0);
            c.text_wrapped(s, r, Font::Mono, t.ink, Align::Start);
            c.pop_clip();
        }
        Content::Handler => {}
        Content::Loading | Content::Icon | Content::Cloud => {
            let cloud = matches!(ql.content, Content::Cloud);
            let mid = area.center(area.w.min(420.0), 260.0);
            let s = 128.0;
            match crate::thumbs::image_blocking(c, &ql.path, &ql.entry, s) {
                Some((bmp, bw, bh)) => {
                    let k = (s / bw).min(s / bh);
                    c.bitmap(&bmp, mid.top(s).center(bw * k, bh * k).snap(c.scale), 1.0);
                }
                None => {}
            }
            c.text(&ql.entry.name, Rect::new(mid.x, mid.y + s + 16.0, mid.w, 24.0), Font::BodyStrong, t.ink, Align::Center);
            let note = if cloud {
                "Soubor je jen v OneDrivu. Tlačítkem Otevřít ho stáhnete."
            } else if matches!(ql.content, Content::Loading) {
                "Načítá se…"
            } else {
                "Náhled tohoto typu souboru není k dispozici."
            };
            c.text(note, Rect::new(mid.x - 40.0, mid.y + s + 42.0, mid.w + 80.0, 20.0), Font::Small, t.ink3, Align::Center);
        }
    }
}

fn open_button(w: f32) -> Rect {
    Rect::new(w - 16.0 - 84.0, (BAR_H - 28.0) / 2.0, 84.0, 28.0)
}

/// Runs `f` on the Quick Look state with the state moved out of the cell, so messages that a COM
/// call pumps meanwhile (preview handlers live in prevhost) find nothing instead of a borrowed cell.
fn with_ql<R>(f: impl FnOnce(&mut Ql) -> R) -> Option<R> {
    let mut ql = QL.with(|q| q.borrow_mut().take())?;
    let r = f(&mut ql);
    QL.with(|q| {
        let mut slot = q.borrow_mut();
        if slot.is_none() {
            *slot = Some(ql);
        }
    });
    Some(r)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { wndproc_inner(hwnd, msg, w, l) })) {
        Ok(r) => r,
        Err(e) => {
            crate::prefs::log(&format!("panic in Quick Look (msg {msg:#x}): {}", crate::win::panic_text(&e)));
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
    }
}

unsafe fn wndproc_inner(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut ps);
                with_ql(|ql| {
                    let theme = ql.theme.clone();
                    paint(ql, &theme)
                });
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                with_ql(|ql| {
                    layout_host(ql);
                    ql.text_h = 0.0;
                });
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                with_ql(|ql| {
                    ql.scale = (w.0 & 0xFFFF) as f32 / 96.0;
                    ql.rt = None;
                });
                let r = &*(l.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_APP_QL_IMAGE => {
                let ptr = l.0 as *mut Decoded;
                let dec = (!ptr.is_null()).then(|| Box::from_raw(ptr));
                with_ql(|ql| {
                    if w.0 as u64 != ql.generation {
                        return;
                    }
                    ql.content = match dec {
                        Some(d) if d.generation == ql.generation => Content::Image { w: d.w, h: d.h, data: d.data },
                        _ => Content::Icon,
                    };
                    ql.bmp = None;
                });
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((w.0 >> 16) & 0xFFFF) as i16 as f32;
                with_ql(|ql| {
                    if matches!(ql.content, Content::Text(_)) {
                        let r = content_rect(ql);
                        let max = (ql.text_h + 40.0 - r.h).max(0.0);
                        ql.scroll = (ql.scroll - delta / 120.0 * 3.0 * 18.0).clamp(0.0, max);
                    }
                });
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = ((l.0 & 0xFFFF) as i16 as f32, ((l.0 >> 16) & 0xFFFF) as i16 as f32);
                let changed = with_ql(|ql| {
                    let r = content_rect(ql);
                    let hot = open_button(r.w).contains(x / ql.scale, y / ql.scale);
                    let c = hot != ql.hover_open;
                    ql.hover_open = hot;
                    c
                });
                if changed == Some(true) {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = ((l.0 & 0xFFFF) as i16 as f32, ((l.0 >> 16) & 0xFFFF) as i16 as f32);
                let target = with_ql(|ql| {
                    let r = content_rect(ql);
                    open_button(r.w).contains(x / ql.scale, y / ql.scale).then(|| ql.path.clone())
                })
                .flatten();
                if let Some(p) = target {
                    crate::shell::open_path(hwnd, &p);
                    close();
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let vk = VIRTUAL_KEY(w.0 as u16);
                match vk {
                    VK_ESCAPE | VK_SPACE => close(),
                    VK_RETURN => {
                        if let Some(p) = with_ql(|ql| ql.path.clone()) {
                            crate::shell::open_path(hwnd, &p);
                            close();
                        }
                    }
                    VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END => {
                        // Move the selection in the main window; the preview follows.
                        crate::win::forward_key(hwnd, vk);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                close();
                LRESULT(0)
            }
            WM_DESTROY => LRESULT(0),
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_decoding() {
        assert_eq!(decode_text("Žluťoučký kůň".as_bytes()).unwrap(), "Žluťoučký kůň");
        assert_eq!(decode_text(&[0xEF, 0xBB, 0xBF, b'a']).unwrap(), "a");
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("Kůň".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(decode_text(&utf16).unwrap(), "Kůň");
        assert_eq!(decode_text(&[0x8A, b'k', 0x9E]).unwrap(), "Škž"); // Windows-1250
        assert!(decode_text(&[0, 1, 2, 3]).is_none());
        assert!(is_text("rs") && is_image("heic") && !is_text("pdf"));
    }
}
