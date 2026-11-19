//! Quick Look (Space) and the inspector pane.
use crate::app::App;
use crate::geom::Rect;
use crate::gfx::Canvas;
use crate::theme::Theme;

pub fn toggle(app: &mut App) {
    app.note("Rychlý náhled");
}

pub fn close() {}

pub fn paint_inspector(_app: &mut App, _c: &mut Canvas, _t: &Theme, _r: Rect) {}
