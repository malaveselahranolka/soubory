//! Selection, history and type-to-select: pure state, no UI.
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Selection {
    sel: Vec<bool>,
    count: usize,
    pub anchor: Option<usize>,
    pub focus: Option<usize>,
}

impl Selection {
    pub fn new(n: usize) -> Self {
        Selection {
            sel: vec![false; n],
            ..Default::default()
        }
    }
    pub fn len(&self) -> usize {
        self.sel.len()
    }
    pub fn count(&self) -> usize {
        self.count
    }
    pub fn is(&self, i: usize) -> bool {
        self.sel.get(i).copied().unwrap_or(false)
    }
    pub fn indices(&self) -> Vec<usize> {
        self.sel
            .iter()
            .enumerate()
            .filter(|(_, s)| **s)
            .map(|(i, _)| i)
            .collect()
    }
    pub fn first(&self) -> Option<usize> {
        self.sel.iter().position(|s| *s)
    }
    fn set(&mut self, i: usize, v: bool) {
        if let Some(s) = self.sel.get_mut(i) {
            if *s != v {
                *s = v;
                if v { self.count += 1 } else { self.count -= 1 }
            }
        }
    }
    pub fn clear(&mut self) {
        self.sel.iter_mut().for_each(|s| *s = false);
        self.count = 0;
    }
    /// Plain click / arrow key.
    pub fn select_only(&mut self, i: usize) {
        self.clear();
        self.set(i, true);
        self.anchor = Some(i);
        self.focus = Some(i);
    }
    /// Ctrl+click.
    pub fn toggle(&mut self, i: usize) {
        let v = !self.is(i);
        self.set(i, v);
        self.anchor = Some(i);
        self.focus = Some(i);
    }
    /// Shift+click / Shift+arrow: anchor..=i, replacing the rest unless `keep` (Ctrl+Shift).
    pub fn extend_to(&mut self, i: usize, keep: bool) {
        let a = self.anchor.unwrap_or(i);
        if !keep {
            self.clear();
        }
        for j in a.min(i)..=a.max(i) {
            self.set(j, true);
        }
        self.anchor = Some(a);
        self.focus = Some(i);
    }
    pub fn select_all(&mut self) {
        for i in 0..self.sel.len() {
            self.set(i, true);
        }
    }
    /// Replaces the selection with a set (rubber band).
    pub fn set_many(&mut self, items: impl IntoIterator<Item = usize>, keep: &[usize]) {
        self.clear();
        for i in keep.iter().copied().chain(items) {
            self.set(i, true);
        }
    }
    /// Keyboard move: returns the new focus index.
    pub fn move_by(&mut self, delta: isize, extend: bool, focus_only: bool) -> Option<usize> {
        let n = self.sel.len();
        if n == 0 {
            return None;
        }
        let cur =
            self.focus
                .map(|f| f as isize)
                .unwrap_or(if delta >= 0 { -1 } else { n as isize });
        let next = (cur + delta).clamp(0, n as isize - 1) as usize;
        if focus_only {
            self.focus = Some(next);
        } else if extend {
            self.extend_to(next, false);
        } else {
            self.select_only(next);
        }
        Some(next)
    }
}

#[derive(Clone, Debug)]
pub struct History<T: Clone> {
    back: Vec<T>,
    fwd: Vec<T>,
}

impl<T: Clone> Default for History<T> {
    fn default() -> Self {
        History {
            back: Vec::new(),
            fwd: Vec::new(),
        }
    }
}

impl<T: Clone> History<T> {
    pub fn push(&mut self, from: T) {
        self.back.push(from);
        self.fwd.clear();
        if self.back.len() > 200 {
            self.back.remove(0);
        }
    }
    pub fn back(&mut self, current: T) -> Option<T> {
        let t = self.back.pop()?;
        self.fwd.push(current);
        Some(t)
    }
    pub fn forward(&mut self, current: T) -> Option<T> {
        let t = self.fwd.pop()?;
        self.back.push(current);
        Some(t)
    }
    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }
    pub fn can_forward(&self) -> bool {
        !self.fwd.is_empty()
    }
}

/// Type-to-select: letters typed within 1 s form a prefix.
#[derive(Default)]
pub struct TypeSelect {
    buf: String,
    last: Option<Instant>,
}

impl TypeSelect {
    pub fn push(&mut self, ch: char, now: Instant) -> String {
        if self
            .last
            .is_none_or(|t| now.duration_since(t) > Duration::from_millis(1000))
        {
            self.buf.clear();
        }
        self.last = Some(now);
        self.buf.push(ch);
        self.buf.clone()
    }
    pub fn active(&self, now: Instant) -> bool {
        self.last
            .is_some_and(|t| now.duration_since(t) <= Duration::from_millis(1000))
            && !self.buf.is_empty()
    }
    /// First name (in display order) whose folded form starts with the folded prefix. A
    /// repeated single letter cycles through matches, like Explorer.
    pub fn find(prefix: &str, names: &[&str], current: Option<usize>) -> Option<usize> {
        let p = fm_fs::fold(prefix);
        let chars: Vec<char> = p.chars().collect();
        let cycling = chars.len() > 1 && chars.iter().all(|c| *c == chars[0]);
        let (needle, start) = if cycling {
            (chars[0].to_string(), current.map(|c| c + 1).unwrap_or(0))
        } else {
            (p.clone(), current.unwrap_or(0))
        };
        let n = names.len();
        (0..n)
            .map(|k| (start + k) % n)
            .find(|&i| fm_fs::fold(names[i]).starts_with(&needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_shift_ctrl() {
        let mut s = Selection::new(10);
        s.select_only(2);
        s.extend_to(5, false);
        assert_eq!(s.indices(), vec![2, 3, 4, 5]);
        s.toggle(8);
        assert_eq!(s.count(), 5);
        s.extend_to(9, true); // anchor moved to 8 by the toggle
        assert_eq!(s.indices(), vec![2, 3, 4, 5, 8, 9]);
        s.extend_to(0, false);
        assert_eq!(s.indices(), (0..=8).collect::<Vec<_>>());
    }

    #[test]
    fn keyboard_moves_clamp() {
        let mut s = Selection::new(3);
        assert_eq!(s.move_by(1, false, false), Some(0));
        assert_eq!(s.move_by(10, true, false), Some(2));
        assert_eq!(s.indices(), vec![0, 1, 2]);
        assert_eq!(s.move_by(-1, false, false), Some(1));
        assert_eq!(s.indices(), vec![1]);
        assert_eq!(Selection::new(0).move_by(1, false, false), None);
    }

    #[test]
    fn history_round_trip() {
        let mut h = History::default();
        h.push(1);
        h.push(2);
        assert_eq!(h.back(3), Some(2));
        assert_eq!(h.forward(2), Some(3));
        assert!(!h.can_forward());
        h.back(3);
        h.push(9);
        assert!(!h.can_forward());
    }

    #[test]
    fn type_select_prefix_and_cycle() {
        let names = ["Alfa", "beta", "Bod", "čaj", "Cesta"];
        assert_eq!(TypeSelect::find("bo", &names, None), Some(2));
        assert_eq!(TypeSelect::find("ca", &names, None), Some(3)); // diacritics folded
        assert_eq!(TypeSelect::find("bb", &names, Some(1)), Some(2)); // cycle
        assert_eq!(TypeSelect::find("x", &names, None), None);
        let mut t = TypeSelect::default();
        let t0 = Instant::now();
        assert_eq!(t.push('a', t0), "a");
        assert_eq!(t.push('b', t0 + Duration::from_millis(300)), "ab");
        assert_eq!(t.push('c', t0 + Duration::from_millis(2000)), "c");
    }
}
