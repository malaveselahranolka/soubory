//! Rectangles in device-independent pixels.
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }
    pub fn r(&self) -> f32 {
        self.x + self.w
    }
    pub fn b(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.r() && py >= self.y && py < self.b()
    }
    /// Shrinks by `dx` on left/right and `dy` on top/bottom.
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(
            self.x + dx,
            self.y + dy,
            (self.w - 2.0 * dx).max(0.0),
            (self.h - 2.0 * dy).max(0.0),
        )
    }
    pub fn left(&self, w: f32) -> Rect {
        Rect::new(self.x, self.y, w.min(self.w), self.h)
    }
    pub fn right(&self, w: f32) -> Rect {
        let w = w.min(self.w);
        Rect::new(self.r() - w, self.y, w, self.h)
    }
    pub fn top(&self, h: f32) -> Rect {
        Rect::new(self.x, self.y, self.w, h.min(self.h))
    }
    pub fn bottom(&self, h: f32) -> Rect {
        let h = h.min(self.h);
        Rect::new(self.x, self.b() - h, self.w, h)
    }
    /// What remains after cutting `w` from the left.
    pub fn cut_left(&self, w: f32) -> Rect {
        let w = w.min(self.w);
        Rect::new(self.x + w, self.y, self.w - w, self.h)
    }
    pub fn cut_right(&self, w: f32) -> Rect {
        Rect::new(self.x, self.y, (self.w - w).max(0.0), self.h)
    }
    pub fn cut_top(&self, h: f32) -> Rect {
        let h = h.min(self.h);
        Rect::new(self.x, self.y + h, self.w, self.h - h)
    }
    pub fn cut_bottom(&self, h: f32) -> Rect {
        Rect::new(self.x, self.y, self.w, (self.h - h).max(0.0))
    }
    pub fn offset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.r() && o.x < self.r() && self.y < o.b() && o.y < self.b()
    }
    pub fn union(&self, o: &Rect) -> Rect {
        let (x, y) = (self.x.min(o.x), self.y.min(o.y));
        Rect::new(x, y, self.r().max(o.r()) - x, self.b().max(o.b()) - y)
    }
    /// Rectangle spanned by two points (rubber-band selection).
    pub fn from_points(a: (f32, f32), b: (f32, f32)) -> Rect {
        Rect::new(
            a.0.min(b.0),
            a.1.min(b.1),
            (a.0 - b.0).abs(),
            (a.1 - b.1).abs(),
        )
    }
    /// Centered square-ish box of the given size inside self.
    pub fn center(&self, w: f32, h: f32) -> Rect {
        Rect::new(
            self.x + (self.w - w) / 2.0,
            self.y + (self.h - h) / 2.0,
            w,
            h,
        )
    }
    pub fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F {
            left: self.x,
            top: self.y,
            right: self.r(),
            bottom: self.b(),
        }
    }
    /// Snaps edges to whole physical pixels for crisp hairlines.
    pub fn snap(&self, scale: f32) -> Rect {
        let s = |v: f32| (v * scale).round() / scale;
        let (x, y) = (s(self.x), s(self.y));
        Rect::new(x, y, s(self.r()) - x, s(self.b()) - y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_and_contains() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!(r.cut_left(30.0), Rect::new(40.0, 20.0, 70.0, 50.0));
        assert_eq!(r.right(30.0), Rect::new(80.0, 20.0, 30.0, 50.0));
        assert_eq!(r.cut_top(10.0).h, 40.0);
        assert!(r.contains(10.0, 20.0) && !r.contains(110.0, 20.0));
        assert_eq!(
            Rect::from_points((5.0, 9.0), (1.0, 3.0)),
            Rect::new(1.0, 3.0, 4.0, 6.0)
        );
        assert_eq!(r.inset(60.0, 0.0).w, 0.0);
    }
}
