//! Thumbnails for icon and gallery views (filled in by the thumbnail worker).
use crate::app::App;
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    Graphics::Direct2D::ID2D1Bitmap,
};

pub fn get(_app: &mut App, _i: usize) -> Option<(ID2D1Bitmap, f32, f32)> {
    None
}

pub fn arrived(_app: &mut App, _w: WPARAM, _l: LPARAM) {}

pub fn forget_device(_app: &mut App) {}
