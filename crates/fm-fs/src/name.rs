//! Windows file and directory name validation and unique-name generation.

use std::fmt;

/// Maximum length of a Windows path component in UTF-16 code units.
pub const MAX_NAME_UTF16_LEN: usize = 255;

/// Errors returned by [`validate_name`] when a name violates Windows filename rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// The name is empty.
    Empty,
    /// The name is `.` or `..`.
    DotName,
    /// The name contains one of `< > : " / \ | ? *`.
    InvalidChar,
    /// The name contains an ASCII control character (0–31).
    ControlChar,
    /// The name ends with a dot or a space.
    TrailingDotOrSpace,
    /// The name matches a reserved Windows DOS device name.
    ReservedDevice,
    /// The name exceeds 255 UTF-16 code units.
    TooLong,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Empty => "Název nesmí být prázdný",
            Self::DotName => "Název nesmí být . ani ..",
            Self::InvalidChar => "Název nesmí obsahovat znaky < > : \" / \\ | ? *",
            Self::ControlChar => "Název nesmí obsahovat řídicí znaky",
            Self::TrailingDotOrSpace => "Název nesmí končit tečkou ani mezerou",
            Self::ReservedDevice => {
                "Název je rezervovaný systémovým zařízením (CON, PRN, AUX, NUL, COM0–9, LPT0–9)"
            }
            Self::TooLong => "Název nesmí být delší než 255 znaků UTF-16",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for NameError {}

/// Validates `name` against Windows filename rules.
pub fn validate_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name == "." || name == ".." {
        return Err(NameError::DotName);
    }
    for ch in name.chars() {
        if ch <= '\x1F' {
            return Err(NameError::ControlChar);
        }
        if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            return Err(NameError::InvalidChar);
        }
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(NameError::TrailingDotOrSpace);
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    if is_reserved_device_name(stem) {
        return Err(NameError::ReservedDevice);
    }
    if name.encode_utf16().count() > MAX_NAME_UTF16_LEN {
        return Err(NameError::TooLong);
    }
    Ok(())
}

fn is_reserved_device_name(stem: &str) -> bool {
    if stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
    {
        return true;
    }
    let bytes = stem.as_bytes();
    if bytes.len() >= 4
        && (bytes[..3].eq_ignore_ascii_case(b"COM") || bytes[..3].eq_ignore_ascii_case(b"LPT"))
    {
        let suffix = &stem[3..];
        let mut chars = suffix.chars();
        if let (Some(digit), None) = (chars.next(), chars.next()) {
            return matches!(digit, '0'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}');
        }
    }
    false
}

/// Formats a candidate filename for 1-based attempt number `attempt`.
pub(crate) fn format_candidate(stem: &str, ext: &str, attempt: usize) -> String {
    let suffix_len = if attempt <= 1 { 0 } else { 12 };
    let ext_len = if ext.is_empty() { 0 } else { 1 + ext.len() };
    let mut out = String::with_capacity(stem.len() + suffix_len + ext_len);
    out.push_str(stem);
    if attempt >= 2 {
        use std::fmt::Write as _;
        let _ = write!(out, " ({attempt})");
    }
    if !ext.is_empty() {
        out.push('.');
        out.push_str(ext);
    }
    out
}

/// Generates a unique filename or folder name using `"stem"`, `"stem (2)"`, `"stem (3)"`...
///
/// `dir_entries(candidate)` must return `true` if a colliding entry already exists
/// (checked case-insensitively by the caller). `ext` may be empty and must not
/// include a leading dot.
pub fn unique_name(dir_entries: impl Fn(&str) -> bool, stem: &str, ext: &str) -> String {
    for attempt in 1usize.. {
        let candidate = format_candidate(stem, ext, attempt);
        if !dir_entries(&candidate) {
            return candidate;
        }
    }
    unreachable!("unbounded attempt iterator cannot exhaust usize")
}

#[cfg(test)]
mod tests {
    use super::{NameError, unique_name, validate_name};

    #[test]
    fn validate_name_table() {
        let cases: &[(&str, Result<(), NameError>)] = &[
            ("", Err(NameError::Empty)),
            (".", Err(NameError::DotName)),
            ("..", Err(NameError::DotName)),
            ("a<b", Err(NameError::InvalidChar)),
            ("a>b", Err(NameError::InvalidChar)),
            ("a:b", Err(NameError::InvalidChar)),
            ("a\"b", Err(NameError::InvalidChar)),
            ("a/b", Err(NameError::InvalidChar)),
            ("a\\b", Err(NameError::InvalidChar)),
            ("a|b", Err(NameError::InvalidChar)),
            ("a?b", Err(NameError::InvalidChar)),
            ("a*b", Err(NameError::InvalidChar)),
            ("a\x00b", Err(NameError::ControlChar)),
            ("a\x1Fb", Err(NameError::ControlChar)),
            ("a\nb", Err(NameError::ControlChar)),
            ("file.", Err(NameError::TrailingDotOrSpace)),
            ("file ", Err(NameError::TrailingDotOrSpace)),
            ("CON", Err(NameError::ReservedDevice)),
            ("con.txt", Err(NameError::ReservedDevice)),
            ("Prn.tar.gz", Err(NameError::ReservedDevice)),
            ("AUX", Err(NameError::ReservedDevice)),
            ("nul .log", Err(NameError::ReservedDevice)),
            ("COM0", Err(NameError::ReservedDevice)),
            ("com9.bin", Err(NameError::ReservedDevice)),
            ("LPT0", Err(NameError::ReservedDevice)),
            ("lpt9.dat", Err(NameError::ReservedDevice)),
            ("COM¹", Err(NameError::ReservedDevice)),
            ("com².txt", Err(NameError::ReservedDevice)),
            ("COM³", Err(NameError::ReservedDevice)),
            ("LPT¹", Err(NameError::ReservedDevice)),
            ("lpt².log", Err(NameError::ReservedDevice)),
            ("LPT³", Err(NameError::ReservedDevice)),
            ("CONSOLE", Ok(())),
            ("COM10", Ok(())),
            ("LPT10.txt", Ok(())),
            ("COM", Ok(())),
            (".gitignore", Ok(())),
            (" Žluťoučký kůň.txt", Ok(())),
        ];

        for &(name, expected) in cases {
            let actual = validate_name(name);
            assert_eq!(actual, expected, "validate_name({name:?})");
            if let Err(err) = actual {
                assert!(!err.to_string().is_empty());
            }
        }

        // Exact UTF-16 boundary tests (255 vs 256 UTF-16 code units)
        let ok_255 = "a".repeat(255);
        assert_eq!(validate_name(&ok_255), Ok(()));
        let err_256 = "a".repeat(256);
        assert_eq!(validate_name(&err_256), Err(NameError::TooLong));

        // Surrogate pairs: each '😀' is 2 UTF-16 code units (127 * 2 + 1 = 255, 128 * 2 = 256)
        let mut emoji_255 = "😀".repeat(127);
        emoji_255.push('a');
        assert_eq!(emoji_255.encode_utf16().count(), 255);
        assert_eq!(validate_name(&emoji_255), Ok(()));

        let emoji_256 = "😀".repeat(128);
        assert_eq!(emoji_256.encode_utf16().count(), 256);
        assert_eq!(validate_name(&emoji_256), Err(NameError::TooLong));

        assert_eq!(
            NameError::InvalidChar.to_string(),
            "Název nesmí obsahovat znaky < > : \" / \\ | ? *"
        );
    }

    #[test]
    fn unique_name_sequence() {
        let mut existing: Vec<String> = Vec::new();
        let has = |items: &[String], candidate: &str| {
            items
                .iter()
                .any(|x| x.to_lowercase() == candidate.to_lowercase())
        };

        let first = unique_name(|c| has(&existing, c), "Nová složka", "");
        assert_eq!(first, "Nová složka");
        existing.push("NOVÁ SLOŽKA".to_string());

        let second = unique_name(|c| has(&existing, c), "Nová složka", "");
        assert_eq!(second, "Nová složka (2)");
        existing.push(second);

        let third = unique_name(|c| has(&existing, c), "Nová složka", "");
        assert_eq!(third, "Nová složka (3)");

        let mut files = vec!["nový soubor.txt".to_string()];
        let f2 = unique_name(|c| has(&files, c), "Nový soubor", "txt");
        assert_eq!(f2, "Nový soubor (2).txt");
        files.push(f2);
        let f3 = unique_name(|c| has(&files, c), "Nový soubor", "txt");
        assert_eq!(f3, "Nový soubor (3).txt");
    }
}
