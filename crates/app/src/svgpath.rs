//! SVG path data ("M12,4l8,8A4,4,0,0,1,…Z") as absolute segments, and into a Direct2D geometry.
//! Covers the whole path grammar (relative forms, H/V, S/T shorthands, arcs, packed numbers and
//! flags), since the icons come from outside.
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows_numerics::Vector2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Quad(f32, f32, f32, f32),
    Arc { rx: f32, ry: f32, rot: f32, large: bool, sweep: bool, x: f32, y: f32 },
    Close,
}

struct Lex<'a> {
    s: &'a [u8],
    i: usize,
}

impl Lex<'_> {
    fn skip(&mut self) {
        while self.i < self.s.len() && (self.s[self.i].is_ascii_whitespace() || self.s[self.i] == b',') {
            self.i += 1;
        }
    }
    fn done(&mut self) -> bool {
        self.skip();
        self.i >= self.s.len()
    }
    fn at_number(&mut self) -> bool {
        self.skip();
        self.i < self.s.len() && matches!(self.s[self.i], b'0'..=b'9' | b'-' | b'+' | b'.')
    }
    fn num(&mut self) -> Result<f32, String> {
        self.skip();
        let start = self.i;
        let s = self.s;
        let mut i = self.i;
        if i < s.len() && matches!(s[i], b'-' | b'+') {
            i += 1;
        }
        let mut digits = 0;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
        // One dot at most: "0.5.5" is 0.5 then .5.
        if i < s.len() && s[i] == b'.' {
            i += 1;
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
                digits += 1;
            }
        }
        if digits == 0 {
            return Err(format!("number expected at {start}"));
        }
        if i < s.len() && matches!(s[i], b'e' | b'E') {
            let mut j = i + 1;
            if j < s.len() && matches!(s[j], b'-' | b'+') {
                j += 1;
            }
            if j < s.len() && s[j].is_ascii_digit() {
                while j < s.len() && s[j].is_ascii_digit() {
                    j += 1;
                }
                i = j;
            }
        }
        self.i = i;
        std::str::from_utf8(&s[start..i]).ok().and_then(|t| t.parse().ok()).ok_or_else(|| format!("bad number at {start}"))
    }
    /// Arc flags are single characters and may be packed: "a8 8 0 018 8".
    fn flag(&mut self) -> Result<bool, String> {
        self.skip();
        match self.s.get(self.i) {
            Some(b'0') => {
                self.i += 1;
                Ok(false)
            }
            Some(b'1') => {
                self.i += 1;
                Ok(true)
            }
            _ => Err(format!("flag expected at {}", self.i)),
        }
    }
}

/// Parses path data into absolute segments (H/V become lines, S/T become full curves).
pub fn parse(d: &str) -> Result<Vec<Seg>, String> {
    let mut lx = Lex { s: d.as_bytes(), i: 0 };
    let mut out = Vec::new();
    let (mut cx, mut cy) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    // Second control point of the previous cubic / control of the previous quad (for S / T).
    let mut prev_c: Option<(f32, f32)> = None;
    let mut prev_q: Option<(f32, f32)> = None;
    let mut cmd = 0u8;
    loop {
        if lx.done() {
            break;
        }
        let c = lx.s[lx.i];
        if c.is_ascii_alphabetic() {
            lx.i += 1;
            cmd = c;
        } else if cmd == 0 || matches!(cmd, b'Z' | b'z') {
            return Err(format!("command expected at {}", lx.i));
        } else if cmd == b'M' {
            // Further pairs after a moveto are linetos.
            cmd = b'L';
        } else if cmd == b'm' {
            cmd = b'l';
        }
        let rel = cmd.is_ascii_lowercase();
        let (ox, oy) = if rel { (cx, cy) } else { (0.0, 0.0) };
        let mut c_next = None;
        let mut q_next = None;
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let (x, y) = (lx.num()? + ox, lx.num()? + oy);
                out.push(Seg::Move(x, y));
                (cx, cy, sx, sy) = (x, y, x, y);
            }
            b'L' => {
                let (x, y) = (lx.num()? + ox, lx.num()? + oy);
                out.push(Seg::Line(x, y));
                (cx, cy) = (x, y);
            }
            b'H' => {
                let x = lx.num()? + ox;
                out.push(Seg::Line(x, cy));
                cx = x;
            }
            b'V' => {
                let y = lx.num()? + oy;
                out.push(Seg::Line(cx, y));
                cy = y;
            }
            b'C' => {
                let (x1, y1) = (lx.num()? + ox, lx.num()? + oy);
                let (x2, y2) = (lx.num()? + ox, lx.num()? + oy);
                let (x, y) = (lx.num()? + ox, lx.num()? + oy);
                out.push(Seg::Cubic(x1, y1, x2, y2, x, y));
                c_next = Some((x2, y2));
                (cx, cy) = (x, y);
            }
            b'S' => {
                let (x1, y1) = prev_c.map_or((cx, cy), |(px, py)| (2.0 * cx - px, 2.0 * cy - py));
                let (x2, y2) = (lx.num()? + ox, lx.num()? + oy);
                let (x, y) = (lx.num()? + ox, lx.num()? + oy);
                out.push(Seg::Cubic(x1, y1, x2, y2, x, y));
                c_next = Some((x2, y2));
                (cx, cy) = (x, y);
            }
            b'Q' => {
                let (x1, y1) = (lx.num()? + ox, lx.num()? + oy);
                let (x, y) = (lx.num()? + ox, lx.num()? + oy);
                out.push(Seg::Quad(x1, y1, x, y));
                q_next = Some((x1, y1));
                (cx, cy) = (x, y);
            }
            b'T' => {
                let (x1, y1) = prev_q.map_or((cx, cy), |(px, py)| (2.0 * cx - px, 2.0 * cy - py));
                let (x, y) = (lx.num()? + ox, lx.num()? + oy);
                out.push(Seg::Quad(x1, y1, x, y));
                q_next = Some((x1, y1));
                (cx, cy) = (x, y);
            }
            b'A' => {
                let (rx, ry, rot) = (lx.num()?.abs(), lx.num()?.abs(), lx.num()?);
                let (large, sweep) = (lx.flag()?, lx.flag()?);
                let (x, y) = (lx.num()? + ox, lx.num()? + oy);
                out.push(Seg::Arc { rx, ry, rot, large, sweep, x, y });
                (cx, cy) = (x, y);
            }
            b'Z' => {
                out.push(Seg::Close);
                (cx, cy) = (sx, sy);
            }
            _ => return Err(format!("unknown command {:?}", cmd as char)),
        }
        prev_c = c_next;
        prev_q = q_next;
    }
    Ok(out)
}

/// Writes the segments into a geometry sink (nonzero fill, as SVG's default fill-rule).
pub fn build(sink: &ID2D1GeometrySink, segs: &[Seg]) {
    let v = |x: f32, y: f32| Vector2 { X: x, Y: y };
    unsafe {
        sink.SetFillMode(D2D1_FILL_MODE_WINDING);
        let mut open = false;
        let (mut cur, mut start) = ((0.0, 0.0), (0.0, 0.0));
        for s in segs {
            match *s {
                Seg::Move(x, y) => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                    sink.BeginFigure(v(x, y), D2D1_FIGURE_BEGIN_FILLED);
                    open = true;
                    (cur, start) = ((x, y), (x, y));
                }
                Seg::Close => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                    cur = start;
                }
                seg => {
                    if !open {
                        // Drawing after Z goes on from the closed figure's start.
                        sink.BeginFigure(v(cur.0, cur.1), D2D1_FIGURE_BEGIN_FILLED);
                        open = true;
                        start = cur;
                    }
                    match seg {
                        Seg::Line(x, y) => {
                            sink.AddLine(v(x, y));
                            cur = (x, y);
                        }
                        Seg::Cubic(x1, y1, x2, y2, x, y) => {
                            sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: v(x1, y1), point2: v(x2, y2), point3: v(x, y) });
                            cur = (x, y);
                        }
                        Seg::Quad(x1, y1, x, y) => {
                            sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT { point1: v(x1, y1), point2: v(x, y) });
                            cur = (x, y);
                        }
                        Seg::Arc { rx, ry, rot, large, sweep, x, y } => {
                            if rx == 0.0 || ry == 0.0 {
                                sink.AddLine(v(x, y));
                            } else {
                                sink.AddArc(&D2D1_ARC_SEGMENT {
                                    point: v(x, y),
                                    size: D2D_SIZE_F { width: rx, height: ry },
                                    rotationAngle: rot,
                                    sweepDirection: if sweep { D2D1_SWEEP_DIRECTION_CLOCKWISE } else { D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE },
                                    arcSize: if large { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
                                });
                            }
                            cur = (x, y);
                        }
                        Seg::Move(..) | Seg::Close => {}
                    }
                }
            }
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Seg::*;

    #[test]
    fn absolute_and_relative() {
        let s = parse("M10,10h5v5l-5,0zm2 2L4 4").unwrap();
        assert_eq!(s, vec![Move(10.0, 10.0), Line(15.0, 10.0), Line(15.0, 15.0), Line(10.0, 15.0), Close, Move(12.0, 12.0), Line(4.0, 4.0)]);
    }

    #[test]
    fn packed_numbers_flags_and_implicit_lineto() {
        // "0.5.5" is two numbers, "-1-2" too; arc flags packed as "01"; pairs after M are linetos.
        let s = parse("M0.5.5-1-2 3 4a8 8 0 018 8").unwrap();
        assert_eq!(s[0], Move(0.5, 0.5));
        assert_eq!(s[1], Line(-1.0, -2.0));
        assert_eq!(s[2], Line(3.0, 4.0));
        assert_eq!(s[3], Arc { rx: 8.0, ry: 8.0, rot: 0.0, large: false, sweep: true, x: 11.0, y: 12.0 });
        assert_eq!(parse("M1e2,1E-1").unwrap(), vec![Move(100.0, 0.1)]);
    }

    #[test]
    fn shorthand_curves_reflect_controls() {
        let s = parse("M0 0C1 1 2 2 3 3S5 5 6 6Q7 7 8 8T10 10").unwrap();
        assert_eq!(s[2], Cubic(4.0, 4.0, 5.0, 5.0, 6.0, 6.0));
        assert_eq!(s[4], Quad(9.0, 9.0, 10.0, 10.0));
        // S after a non-cubic uses the current point as its first control.
        assert_eq!(parse("M1 1S2 2 3 3").unwrap()[1], Cubic(1.0, 1.0, 2.0, 2.0, 3.0, 3.0));
    }

    #[test]
    fn errors_are_reported() {
        assert!(parse("10 10").is_err());
        assert!(parse("M1").is_err());
        assert!(parse("M0 0A1 1 0 2 0 3 3").is_err());
        assert!(parse("M0 0Z 1 1").is_err());
    }

    #[test]
    fn every_embedded_icon_parses() {
        use crate::phosphor::{Ico, Weight};
        for ico in Ico::ALL {
            for w in [Weight::Bold, Weight::Fill] {
                let segs = parse(ico.path(w)).unwrap_or_else(|e| panic!("{ico:?} {w:?}: {e}"));
                assert!(matches!(segs.first(), Some(Seg::Move(..))), "{ico:?} {w:?}");
                // Everything stays inside the 256 box.
                for s in &segs {
                    let pts: Vec<(f32, f32)> = match *s {
                        Move(x, y) | Line(x, y) => vec![(x, y)],
                        Cubic(a, b, c, d, x, y) => vec![(a, b), (c, d), (x, y)],
                        Quad(a, b, x, y) => vec![(a, b), (x, y)],
                        Arc { x, y, .. } => vec![(x, y)],
                        Close => vec![],
                    };
                    for (x, y) in pts {
                        assert!((-1.0..=257.0).contains(&x) && (-1.0..=257.0).contains(&y), "{ico:?} {w:?} ({x},{y})");
                    }
                }
            }
        }
    }
}
