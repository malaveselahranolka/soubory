//! Case and diacritics folding for fast case- and accent-insensitive matching.

use unicode_normalization::char::{decompose_canonical, is_combining_mark};

/// Folds `s` to lowercase without diacritics, applying special character mappings.
///
/// Maps `ß`/`ẞ` to `ss`, `ø`/`Ø` to `o`, `ł`/`Ł` to `l`, and `đ`/`Đ` to `d`.
/// Pure ASCII strings take a fast path with only the output allocation.
#[must_use]
pub fn fold(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    fold_non_ascii_into(s, &mut out);
    out
}

/// Appends the folded form of `s` into `out` without intermediate string allocations.
pub fn fold_into(s: &str, out: &mut String) {
    if s.is_ascii() {
        out.reserve(s.len());
        for &b in s.as_bytes() {
            out.push(b.to_ascii_lowercase() as char);
        }
        return;
    }
    fold_non_ascii_into(s, out);
}

fn fold_non_ascii_into(s: &str, out: &mut String) {
    out.reserve(s.len());
    for ch in s.chars() {
        fold_char_into(ch, out);
    }
}

/// Appends the folded representation of a single character into `out`.
#[inline]
pub fn fold_char_into(ch: char, out: &mut String) {
    if ch.is_ascii() {
        out.push(ch.to_ascii_lowercase());
        return;
    }
    decompose_canonical(ch, |d| {
        if d.is_ascii() {
            out.push(d.to_ascii_lowercase());
            return;
        }
        if is_combining_mark(d) {
            return;
        }
        for lc in d.to_lowercase() {
            if is_combining_mark(lc) {
                continue;
            }
            match lc {
                'ß' => out.push_str("ss"),
                'ø' => out.push('o'),
                'ł' => out.push('l'),
                'đ' => out.push('d'),
                _ => out.push(lc),
            }
        }
    });
}

/// Returns the number of UTF-8 bytes emitted when folding ch.
#[inline]
#[must_use]
pub fn folded_char_byte_len(ch: char) -> usize {
    if ch.is_ascii() {
        return 1;
    }
    let mut len = 0usize;
    decompose_canonical(ch, |d| {
        if d.is_ascii() {
            len += 1;
        } else if !is_combining_mark(d) {
            for lc in d.to_lowercase() {
                if is_combining_mark(lc) {
                    continue;
                }
                len += match lc {
                    'ß' => 2,
                    'ø' | 'ł' | 'đ' => 1,
                    _ => lc.len_utf8(),
                };
            }
        }
    });
    len
}

#[cfg(test)]
mod tests {
    use super::{fold, fold_into};

    #[test]
    fn fold_table() {
        let cases: &[(&str, &str)] = &[
            ("", ""),
            ("abc_123.TXT", "abc_123.txt"),
            ("HELLO WORLD", "hello world"),
            ("Žluťoučký KŮŇ", "zlutoucky kun"),
            (
                "Příliš žluťoučký kůň úpěl ďábelské ódy",
                "prilis zlutoucky kun upel dabelske ody",
            ),
            ("ÁČĎÉĚÍŇÓŘŠŤÚŮÝŽ", "acdeeinorstuuyz"),
            ("Straße", "strasse"),
            ("GROẞE MAßNAHME", "grosse massnahme"),
            ("Ärger mit Öl und Übermut", "arger mit ol und ubermut"),
            ("Łódź, żółć, ąćęńśźż", "lodz, zolc, acenszz"),
            ("WROCŁAW", "wroclaw"),
            ("Søren Kierkegaard Øresund", "soren kierkegaard oresund"),
            ("Đorđević đakovo", "dordevic dakovo"),
            ("e\u{0301} vs é vs \u{0045}\u{030C}", "e vs e vs e"),
            ("İstanbul İZMİR", "istanbul izmir"),
            ("🚀 Rocket 😀 File 📁.txt", "🚀 rocket 😀 file 📁.txt"),
            ("👨‍👩‍👧 rodina.jpg", "👨‍👩‍👧 rodina.jpg"),
            ("camelCaseFile_Name-v2.PDF", "camelcasefile_name-v2.pdf"),
        ];
        for &(input, expected) in cases {
            assert_eq!(fold(input), expected, "fold({input:?})");
            let mut buf = String::from("prefix:");
            fold_into(input, &mut buf);
            assert_eq!(&buf["prefix:".len()..], expected, "fold_into({input:?})");
        }
    }
}
