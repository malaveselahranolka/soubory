//! Shell icons (the user's real Windows icons) as Direct2D bitmaps.
//!
//! Icons are looked up by extension with SHGFI_USEFILEATTRIBUTES, which never touches the file.
//! Only local, non-placeholder items whose icon lives in the file itself (exe, lnk, ico, folders
//! customized by desktop.ini) are asked for by path.
use crate::fsys::{Entry, wide};
use crate::gfx::Canvas;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
};
use windows::{
    Win32::{
        Graphics::{Direct2D::*, Imaging::*},
        Storage::FileSystem::*,
        UI::{Controls::*, Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};

const PER_FILE_EXT: &[&str] = &["exe", "lnk", "ico", "url", "scr", "cpl", "msc", "appref-ms"];

struct Pixels {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

#[derive(Default)]
pub struct Icons {
    by_ext: HashMap<String, i32>,
    by_path: HashMap<PathBuf, i32>,
    by_shell: HashMap<crate::shellns::ShellId, i32>,
    lists: Vec<(i32, u32)>, // (SHIL_*, px)
    pixels: HashMap<(i32, i32), Option<Rc<Pixels>>>,
    bitmaps: HashMap<(usize, i32, i32), ID2D1Bitmap>,
}

impl Icons {
    pub fn new() -> Icons {
        let mut lists = Vec::new();
        for id in [SHIL_SMALL, SHIL_LARGE, SHIL_EXTRALARGE, SHIL_JUMBO] {
            if let Ok(il) = unsafe { SHGetImageList::<IImageList>(id as i32) } {
                let (mut cx, mut cy) = (0, 0);
                if unsafe { il.GetIconSize(&mut cx, &mut cy) }.is_ok() {
                    lists.push((id as i32, cx as u32));
                }
            }
        }
        lists.sort_by_key(|l| l.1);
        Icons {
            lists,
            ..Default::default()
        }
    }

    /// System image list index for an entry in `dir`.
    pub fn index(&mut self, dir: &Path, e: &Entry) -> i32 {
        let ext = e.ext().to_ascii_lowercase();
        let per_file = !e.is_placeholder()
            && ((e.is_dir && e.attrs & (FILE_ATTRIBUTE_READONLY.0 | FILE_ATTRIBUTE_SYSTEM.0) != 0)
                || (!e.is_dir && PER_FILE_EXT.contains(&ext.as_str())));
        if per_file {
            let p = e.path_in(dir);
            if let Some(&i) = self.by_path.get(&p) {
                return i;
            }
            let i = sys_index(&p.to_string_lossy(), e.is_dir, false);
            self.by_path.insert(p, i);
            return i;
        }
        let key = if e.is_dir {
            "\u{1}dir".to_string()
        } else {
            ext
        };
        if let Some(&i) = self.by_ext.get(&key) {
            return i;
        }
        let probe = if e.is_dir {
            "folder".to_string()
        } else {
            format!("x.{key}")
        };
        let i = sys_index(&probe, e.is_dir, true);
        self.by_ext.insert(key, i);
        i
    }

    /// The plain folder's index (a folder without its own desktop.ini icon).
    pub fn folder_index(&mut self) -> i32 {
        let key = "\u{1}dir".to_string();
        if let Some(&i) = self.by_ext.get(&key) {
            return i;
        }
        let i = sys_index("folder", true, true);
        self.by_ext.insert(key, i);
        i
    }

    /// Index for a shell item without a file-system path (a network computer, a phone).
    pub fn index_for_shell(&mut self, id: &crate::shellns::ShellId) -> i32 {
        if let Some(&i) = self.by_shell.get(id) {
            return i;
        }
        let i = crate::shellns::icon_index(id);
        self.by_shell.insert(id.clone(), i);
        i
    }

    /// Index for a path (sidebar, drives, path bar).
    pub fn index_for_path(&mut self, p: &Path) -> i32 {
        if let Some(&i) = self.by_path.get(p) {
            return i;
        }
        let i = sys_index(&p.to_string_lossy(), true, false);
        self.by_path.insert(p.to_path_buf(), i);
        i
    }

    /// Draws icon `index` into `r` (DIPs), picking the image list closest to the physical size.
    pub fn draw(&mut self, c: &mut Canvas, index: i32, r: crate::geom::Rect, opacity: f32) {
        if index < 0 {
            return;
        }
        let px = (r.w * c.scale).round() as u32;
        let list = self
            .lists
            .iter()
            .find(|l| l.1 >= px)
            .or(self.lists.last())
            .map(|l| l.0)
            .unwrap_or(SHIL_SMALL as i32);
        let dc_key = c.dc.as_raw() as usize;
        let key = (dc_key, list, index);
        if !self.bitmaps.contains_key(&key) {
            let px = self
                .pixels
                .entry((list, index))
                .or_insert_with(|| icon_pixels(list, index).map(Rc::new))
                .clone();
            let Some(px) = px else { return };
            let Some(bmp) = c.bitmap_from_bgra(px.w, px.h, &px.data) else {
                return;
            };
            self.bitmaps.insert(key, bmp);
        }
        let bmp = &self.bitmaps[&key];
        // Snap to whole pixels so small icons stay crisp.
        let r = r.snap(c.scale);
        c.bitmap(bmp, r, opacity);
    }

    /// Drops bitmaps of a device that went away (or of an offscreen PNG target).
    pub fn forget_device(&mut self, dc: usize) {
        self.bitmaps.retain(|k, _| k.0 != dc);
    }
}

fn sys_index(path: &str, is_dir: bool, by_attrs: bool) -> i32 {
    let mut info = SHFILEINFOW::default();
    let w = wide(path);
    let mut flags = SHGFI_SYSICONINDEX;
    if by_attrs {
        flags |= SHGFI_USEFILEATTRIBUTES;
    }
    let attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let r = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    if r == 0 { -1 } else { info.iIcon }
}

fn icon_pixels(list: i32, index: i32) -> Option<Pixels> {
    unsafe {
        let il: IImageList = SHGetImageList(list).ok()?;
        let hicon = il.GetIcon(index, ILD_TRANSPARENT.0).ok()?;
        let wic: IWICImagingFactory = windows::Win32::System::Com::CoCreateInstance(
            &CLSID_WICImagingFactory,
            None,
            windows::Win32::System::Com::CLSCTX_INPROC_SERVER,
        )
        .ok()?;
        let src = wic.CreateBitmapFromHICON(hicon);
        let _ = DestroyIcon(hicon);
        let src = src.ok()?;
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(
            &src,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let (mut w, mut h) = (0, 0);
        conv.GetSize(&mut w, &mut h).ok()?;
        let mut data = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut data).ok()?;
        Some(Pixels { w, h, data })
    }
}
