//! Thumbnails and large icons via IShellItemImageFactory on two background STA threads.
//!
//! Cloud placeholders only get what is already in the thumbnail cache, or an icon; their content
//! is never read. Results come back as WM_APP_THUMB and are kept within a memory budget.
use crate::app::App;
use crate::fsys::wide;
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::c_void,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Condvar, Mutex},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Direct2D::ID2D1Bitmap, Gdi::*},
        System::Com::*,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

const BUDGET: usize = 96 << 20;
const MAX_QUEUE: usize = 400;
const MAX_MISSES: usize = 4096;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub path: PathBuf,
    pub px: u32,
    pub mtime: u64,
    /// The shell's icon stands in when there is no thumbnail; false where fm draws its own.
    pub icon: bool,
    /// An online-only file: only the thumbnail cache is asked. Part of the key, so a miss is
    /// asked again once the file is downloaded.
    pub placeholder: bool,
}

struct Req {
    key: Key,
    placeholder: bool,
}

pub struct Res {
    key: Key,
    img: Option<Img>,
}

pub struct Img {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
    /// A real thumbnail (photo, document page) rather than an icon.
    pub thumb: bool,
}

enum Slot {
    Pending,
    Ready(Rc<Img>, u64),
    Failed,
}

struct Queue {
    items: Mutex<Vec<Req>>,
    cv: Condvar,
}

struct State {
    queue: Arc<Queue>,
    slots: HashMap<Key, Slot>,
    bitmaps: HashMap<(usize, Key), ID2D1Bitmap>,
    bytes: usize,
    tick: u64,
    started: bool,
}

thread_local! {
    static S: RefCell<State> = RefCell::new(State {
        queue: Arc::new(Queue { items: Mutex::new(Vec::new()), cv: Condvar::new() }),
        slots: HashMap::new(),
        bitmaps: HashMap::new(),
        bytes: 0,
        tick: 0,
        started: false,
    });
}

fn start(hwnd: HWND, queue: Arc<Queue>) {
    let h = hwnd.0 as usize;
    for _ in 0..2 {
        let q = queue.clone();
        std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
            loop {
                let req = {
                    let mut items = q.items.lock().unwrap();
                    while items.is_empty() {
                        items = q.cv.wait(items).unwrap();
                    }
                    items.pop().unwrap() // newest first: what is on screen now
                };
                let img = load(&req.key.path, req.key.px, req.placeholder, req.key.icon);
                let res = Box::into_raw(Box::new(Res { key: req.key, img }));
                if PostMessageW(Some(HWND(h as *mut c_void)), crate::win::WM_APP_THUMB, WPARAM(0), LPARAM(res as isize)).is_err() {
                    drop(Box::from_raw(res));
                }
            }
        });
    }
}

/// Bucketed pixel size so nearby zoom levels share images.
pub fn bucket(px: f32) -> u32 {
    let px = px.ceil() as u32;
    [48, 64, 96, 128, 160, 192, 256, 384, 512, 768, 1024, 1600].into_iter().find(|&b| b >= px).unwrap_or(1600)
}

/// The image for `path` at `dip` size, or None while it loads (the caller draws the icon).
/// `own`: fm draws this item's icon, so only a real thumbnail is wanted.
/// Returns (bitmap, width DIP, height DIP, is_thumbnail).
pub fn image(app: &App, c: &crate::gfx::Canvas, path: &std::path::Path, mtime: u64, attrs: u32, dip: f32, own: bool) -> Option<(ID2D1Bitmap, f32, f32, bool)> {
    if app.offscreen && app.hwnd.is_invalid() {
        // Offscreen renders load synchronously so the PNG shows the final state.
        let key = Key { path: path.to_path_buf(), px: bucket(dip * c.scale), mtime, icon: !own, placeholder: fm_fs::attrs::is_cloud_placeholder(attrs) };
        let img = S.with(|s| match s.borrow().slots.get(&key) {
            Some(Slot::Ready(img, _)) => Some(img.clone()),
            _ => None,
        });
        let img = match img {
            Some(i) => i,
            None => {
                let i = Rc::new(load(path, key.px, fm_fs::attrs::is_cloud_placeholder(attrs), key.icon)?);
                S.with(|s| s.borrow_mut().slots.insert(key.clone(), Slot::Ready(i.clone(), 0)));
                i
            }
        };
        let bmp = c.bitmap_from_bgra(img.w, img.h, &img.data)?;
        return Some((bmp, img.w as f32 / c.scale, img.h as f32 / c.scale, img.thumb));
    }
    let key = Key { path: path.to_path_buf(), px: bucket(dip * c.scale), mtime, icon: !own, placeholder: fm_fs::attrs::is_cloud_placeholder(attrs) };
    let dc = c.dc.as_raw() as usize;
    S.with(|s| {
        let mut s = s.borrow_mut();
        if !s.started {
            s.started = true;
            let q = s.queue.clone();
            start(crate::win::hub(), q);
        }
        s.tick += 1;
        let tick = s.tick;
        match s.slots.get_mut(&key) {
            Some(Slot::Ready(img, used)) => {
                *used = tick;
                let img = img.clone();
                let bmp = match s.bitmaps.get(&(dc, key.clone())) {
                    Some(b) => b.clone(),
                    None => {
                        let b = c.bitmap_from_bgra(img.w, img.h, &img.data)?;
                        s.bitmaps.insert((dc, key.clone()), b.clone());
                        b
                    }
                };
                Some((bmp, img.w as f32 / c.scale, img.h as f32 / c.scale, img.thumb))
            }
            Some(Slot::Pending) | Some(Slot::Failed) => None,
            None => {
                s.slots.insert(key.clone(), Slot::Pending);
                let mut items = s.queue.items.lock().unwrap();
                items.push(Req { key, placeholder: fm_fs::attrs::is_cloud_placeholder(attrs) });
                if items.len() > MAX_QUEUE {
                    let dropped = items.remove(0);
                    drop(items);
                    s.slots.remove(&dropped.key);
                } else {
                    drop(items);
                }
                s.queue.cv.notify_one();
                None
            }
        }
    })
}

pub fn arrived(_w: WPARAM, l: LPARAM) {
    let res = unsafe { Box::from_raw(l.0 as *mut Res) };
    S.with(|s| {
        let mut s = s.borrow_mut();
        let tick = s.tick;
        match res.img {
            Some(img) => {
                s.bytes += img.data.len();
                s.slots.insert(res.key, Slot::Ready(Rc::new(img), tick));
            }
            None => {
                s.slots.insert(res.key, Slot::Failed);
            }
        }
        if s.bytes > BUDGET {
            evict(&mut s);
        }
    });
}

fn evict(s: &mut State) {
    let mut ready: Vec<(u64, Key, usize)> = s
        .slots
        .iter()
        .filter_map(|(k, v)| match v {
            Slot::Ready(img, used) => Some((*used, k.clone(), img.data.len())),
            _ => None,
        })
        .collect();
    ready.sort_by_key(|r| r.0);
    for (_, k, n) in ready {
        if s.bytes <= BUDGET / 2 {
            break;
        }
        s.slots.remove(&k);
        s.bitmaps.retain(|bk, _| bk.1 != k);
        s.bytes -= n;
    }
}

/// Synchronous variant for Quick Look's fallback image (cached; placeholder-safe).
pub fn image_blocking(c: &crate::gfx::Canvas, path: &std::path::Path, e: &crate::fsys::Entry, dip: f32, own: bool) -> Option<(ID2D1Bitmap, f32, f32)> {
    let key = Key { path: path.to_path_buf(), px: bucket(dip * c.scale), mtime: e.modified, icon: !own, placeholder: e.is_placeholder() };
    let (img, failed) = S.with(|s| match s.borrow().slots.get(&key) {
        Some(Slot::Ready(img, _)) => (Some(img.clone()), false),
        Some(Slot::Failed) => (None, true),
        _ => (None, false),
    });
    if failed {
        return None;
    }
    let img = match img {
        Some(i) => i,
        None => {
            let Some(i) = load(path, key.px, e.is_placeholder(), key.icon) else {
                // Remembered, so Quick Look does not ask the shell again on every paint.
                S.with(|s| s.borrow_mut().slots.insert(key, Slot::Failed));
                return None;
            };
            let i = Rc::new(i);
            S.with(|s| {
                let mut s = s.borrow_mut();
                s.bytes += i.data.len();
                s.slots.insert(key, Slot::Ready(i.clone(), 0));
            });
            i
        }
    };
    let bmp = c.bitmap_from_bgra(img.w, img.h, &img.data)?;
    Some((bmp, img.w as f32 / c.scale, img.h as f32 / c.scale))
}

/// Folder changed: drop failed lookups and pending markers so they are retried.
pub fn refresh() {
    // A file without a thumbnail that fm draws itself stays a miss: its key carries the mtime and
    // the online-only state, so a changed or downloaded file is asked again anyway. Misses are
    // kept up to MAX_MISSES (each new mtime of a growing log adds one), then all asked again.
    // shortcut: a thumbnail provider installed while fm runs is seen at the next bound or restart.
    S.with(|s| {
        let mut s = s.borrow_mut();
        let misses = s.slots.iter().filter(|(k, v)| !k.icon && matches!(v, Slot::Failed)).count();
        let keep = misses <= MAX_MISSES;
        s.slots.retain(|k, v| matches!(v, Slot::Ready(..)) || (keep && !k.icon && matches!(v, Slot::Failed)));
    });
}

pub fn forget_device(_app: &mut App) {
    S.with(|s| s.borrow_mut().bitmaps.clear());
}

/// Forget bitmaps made for a temporary target (PNG renders).
pub fn forget_dc(dc: usize) {
    S.with(|s| s.borrow_mut().bitmaps.retain(|k, _| k.0 != dc));
}

fn load(path: &std::path::Path, px: u32, placeholder: bool, icon: bool) -> Option<Img> {
    unsafe {
        let w = wide(&path.to_string_lossy());
        let item: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None).ok()?;
        let size = SIZE { cx: px as i32, cy: px as i32 };
        let thumb_flags = if placeholder { SIIGBF_THUMBNAILONLY | SIIGBF_INCACHEONLY } else { SIIGBF_THUMBNAILONLY };
        if let Ok(h) = item.GetImage(size, thumb_flags) {
            let img = pixels(h, true);
            let _ = DeleteObject(h.into());
            if img.is_some() {
                return img;
            }
        }
        if !icon {
            return None;
        }
        let h = item.GetImage(size, SIIGBF_ICONONLY).ok()?;
        let img = pixels(h, false);
        let _ = DeleteObject(h.into());
        img
    }
}

/// Reads a 32-bpp HBITMAP as premultiplied BGRA, top-down.
fn pixels(h: HBITMAP, thumb: bool) -> Option<Img> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(h.into(), std::mem::size_of::<BITMAP>() as i32, Some((&mut bm as *mut BITMAP).cast())) == 0 {
            return None;
        }
        let (w, hgt) = (bm.bmWidth as u32, bm.bmHeight.unsigned_abs());
        if w == 0 || hgt == 0 {
            return None;
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(hgt as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut data = vec![0u8; (w * hgt * 4) as usize];
        let dc = GetDC(None);
        let n = GetDIBits(dc, h, 0, hgt, Some(data.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        if n == 0 {
            return None;
        }
        fix_alpha(&mut data);
        Some(Img { w, h: hgt, data, thumb })
    }
}

/// Shell bitmaps come either without alpha (all 0: opaque photo) or with straight alpha.
fn fix_alpha(data: &mut [u8]) {
    if data.chunks_exact(4).all(|p| p[3] == 0) {
        data.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        return;
    }
    let straight = data.chunks_exact(4).any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]);
    if straight {
        for p in data.chunks_exact_mut(4) {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_fixups() {
        let mut opaque = vec![10, 20, 30, 0, 40, 50, 60, 0];
        fix_alpha(&mut opaque);
        assert_eq!(opaque, vec![10, 20, 30, 255, 40, 50, 60, 255]);
        let mut straight = vec![255, 255, 255, 128, 0, 0, 0, 0];
        fix_alpha(&mut straight);
        assert_eq!(straight[..4], [128, 128, 128, 128]);
        let mut pre = vec![100, 100, 100, 128];
        fix_alpha(&mut pre);
        assert_eq!(pre, vec![100, 100, 100, 128]);
        assert_eq!(bucket(60.0), 64);
        assert_eq!(bucket(5000.0), 1600);
    }
}
