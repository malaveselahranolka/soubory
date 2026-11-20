//! fm's own file icons: a Phosphor glyph tinted by type at list size, a tinted page with the
//! type's glyph and extension above that, and amber folders. The shell keeps what only it knows:
//! programs and shortcuts (their icon is the file's own), folders with a custom icon, drives and
//! other virtual items, and thumbnails (callers draw those first).
use crate::fsys::Entry;
use crate::geom::Rect;
use crate::gfx::{Align, Canvas, Font};
use crate::phosphor::{Ico, Weight};
use crate::theme::{Color, Mode, Theme, rgb};
use std::path::{Path, PathBuf};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cat {
    Text,
    Markdown,
    Pdf,
    Doc,
    Sheet,
    Slides,
    Archive,
    Image,
    Audio,
    Video,
    Code,
    Data,
    Font,
    Disk,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Own {
    /// An amber folder, with an emblem on the user's known folders.
    Folder(Option<Ico>),
    File(Cat),
}

/// Files whose icon lives in the file itself (or is the program's own): always the shell's.
pub const SHELL_EXT: &[&str] = &["exe", "lnk", "ico", "url", "scr", "cpl", "msc", "appref-ms", "msi", "msix", "msixbundle", "appx", "appxbundle", "cur", "ani", "website"];

/// The icon fm draws for `e`, or None when the shell's must be used. `virtual_place`: a shell
/// namespace other than the Recycle Bin (This PC, Network, devices), whose items are not files.
pub fn own(e: &Entry, virtual_place: bool) -> Option<Own> {
    // Drives keep their own icons wherever they are listed (search results, columns), and so do
    // items that exist only in the shell's namespace (network computers, phones, a Recycle Bin
    // item without its file), wherever a column carries them.
    if virtual_place || e.full.as_deref().is_some_and(|p| p.parent().is_none()) || (e.full.is_none() && e.shell.is_some()) {
        return None;
    }
    if e.is_dir {
        // desktop.ini folders (Documents, Pictures, a custom icon) mark themselves read-only or
        // system; a cloud placeholder carries those bits for other reasons.
        let custom = !e.is_placeholder() && e.attrs & (FILE_ATTRIBUTE_READONLY.0 | FILE_ATTRIBUTE_SYSTEM.0) != 0;
        return (!custom).then_some(Own::Folder(None));
    }
    let ext = ext_of(e);
    if SHELL_EXT.contains(&ext.as_str()) {
        return None;
    }
    Some(Own::File(category(&ext, &e.name)))
}

/// The extension, lower case: from the full path when there is one, else from the name. Recycle
/// Bin items are listed by display name, which may hide the extension ("Setup 2.1" for
/// Setup 2.1.exe); their $R… file keeps it. Elsewhere the full path ends in the name.
pub fn ext_of(e: &Entry) -> String {
    if e.is_dir {
        return String::new();
    }
    match e.full.as_deref() {
        Some(p) => p.extension().map(|x| x.to_string_lossy().to_ascii_lowercase()).unwrap_or_default(),
        None => e.ext().to_ascii_lowercase(),
    }
}

/// `own_at`, and a read-only or system folder whose shell icon is still the plain folder (its
/// desktop.ini only renames it, as with Program Files or Users) is fm's too. The shell index is
/// looked up once per path and cached, as the shell icon was before.
pub fn own_in(icons: &mut crate::icons::Icons, e: &Entry, dir: &Path, virtual_place: bool) -> Option<Own> {
    let o = own_at(e, dir, virtual_place);
    let drive = e.full.as_deref().is_some_and(|p| p.parent().is_none());
    let shell_only = e.full.is_none() && e.shell.is_some();
    if o.is_none() && e.is_dir && !virtual_place && !drive && !shell_only && icons.index(dir, e) == icons.folder_index() {
        return Some(Own::Folder(None));
    }
    o
}

/// `own` for an entry listed in `dir`, with fm's emblem on the user's known folders (their
/// desktop.ini would otherwise hand them to the shell).
pub fn own_at(e: &Entry, dir: &Path, virtual_place: bool) -> Option<Own> {
    if e.is_dir && !virtual_place {
        let p = e.full.clone().unwrap_or_else(|| dir.join(&e.name));
        if let Some(g) = emblem(&p) {
            return Some(Own::Folder(Some(g)));
        }
    }
    own(e, virtual_place)
}

/// The user's known folders and their glyphs, as the sidebar shows them.
pub fn known_folders() -> &'static [(PathBuf, Ico)] {
    use windows::Win32::UI::Shell::*;
    static KNOWN: std::sync::OnceLock<Vec<(PathBuf, Ico)>> = std::sync::OnceLock::new();
    KNOWN.get_or_init(|| {
        let ids = [
            (FOLDERID_Profile, Ico::House),
            (FOLDERID_Desktop, Ico::Monitor),
            (FOLDERID_Documents, Ico::FileText),
            (FOLDERID_Downloads, Ico::DownloadSimple),
            (FOLDERID_Pictures, Ico::Image),
            (FOLDERID_Music, Ico::MusicNotes),
            (FOLDERID_Videos, Ico::FilmStrip),
        ];
        let mut v: Vec<(PathBuf, Ico)> = ids.iter().filter_map(|(id, g)| crate::fsys::known_folder(id).map(|p| (p, *g))).collect();
        v.extend(crate::fsys::onedrive().map(|p| (p, Ico::Cloud)));
        v
    })
}

fn emblem(p: &Path) -> Option<Ico> {
    let s = p.as_os_str().to_string_lossy();
    let s = s.trim_end_matches('\\');
    known_folders().iter().find(|(k, _)| k.as_os_str().to_string_lossy().trim_end_matches('\\').eq_ignore_ascii_case(s)).map(|(_, g)| *g)
}

/// Type by extension (lower case, no dot). Dotfiles (".gitignore") have no extension and are
/// matched by name.
pub fn category(ext: &str, name: &str) -> Cat {
    use Cat::*;
    match ext {
        "txt" | "text" | "log" | "nfo" | "srt" | "vtt" | "ass" | "diz" => Text,
        "md" | "markdown" | "mdx" | "rst" | "adoc" | "asciidoc" | "org" => Markdown,
        "pdf" | "xps" | "oxps" => Pdf,
        "doc" | "docx" | "docm" | "dot" | "dotx" | "dotm" | "odt" | "ott" | "rtf" | "pages" | "wpd" | "wps" | "epub" | "mobi" => Doc,
        "xls" | "xlsx" | "xlsm" | "xlsb" | "xlt" | "xltx" | "ods" | "ots" | "numbers" | "csv" | "tsv" => Sheet,
        "ppt" | "pptx" | "pptm" | "pps" | "ppsx" | "pot" | "potx" | "odp" | "otp" | "key" => Slides,
        "zip" | "7z" | "rar" | "tar" | "gz" | "tgz" | "bz2" | "tbz" | "tbz2" | "xz" | "txz" | "zst" | "lz" | "lzma" | "lz4" | "cab" | "arj" | "z"
        | "jar" | "war" | "apk" | "aab" | "nupkg" | "whl" | "deb" | "rpm" => Archive,
        "png" | "jpg" | "jpeg" | "jfif" | "jpe" | "gif" | "bmp" | "dib" | "webp" | "tif" | "tiff" | "heic" | "heif" | "avif" | "jxl" | "svg"
        | "svgz" | "psd" | "ai" | "eps" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf" | "srw" | "tga" | "dds"
        | "exr" | "hdr" | "xcf" | "kra" | "afphoto" | "afdesign" | "fig" | "sketch" => Image,
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "oga" | "opus" | "wma" | "aif" | "aiff" | "aifc" | "mid" | "midi" | "amr" | "ape"
        | "alac" | "wv" | "m3u" | "m3u8" | "pls" => Audio,
        "mp4" | "m4v" | "mkv" | "mov" | "avi" | "wmv" | "webm" | "mpg" | "mpeg" | "mpe" | "flv" | "3gp" | "3g2" | "mts" | "m2ts" | "vob"
        | "ogv" | "asf" | "divx" => Video,
        "rs" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "inl" | "cs" | "java" | "kt" | "kts" | "py" | "pyw" | "pyi" | "ipynb"
        | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "go" | "rb" | "php" | "swift" | "m" | "mm" | "lua" | "sh" | "bash" | "zsh"
        | "fish" | "ps1" | "psm1" | "psd1" | "bat" | "cmd" | "vbs" | "sql" | "html" | "htm" | "xhtml" | "css" | "scss" | "sass" | "less"
        | "vue" | "svelte" | "astro" | "r" | "dart" | "scala" | "zig" | "asm" | "s" | "pl" | "pm" | "hs" | "ex" | "exs" | "erl" | "clj"
        | "fs" | "fsx" | "vb" | "ml" | "elm" | "nim" | "gradle" | "groovy" | "tf" | "cmake" | "mk" | "glsl" | "hlsl" | "wgsl" | "sln"
        | "csproj" | "vcxproj" => Code,
        "json" | "jsonc" | "json5" | "jsonl" | "ndjson" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "config" | "xml" | "xaml"
        | "plist" | "env" | "properties" | "lock" | "reg" | "inf" | "manifest" | "editorconfig" | "gitignore" | "gitattributes"
        | "gitmodules" | "npmrc" | "sqlite" | "db" | "sqlite3" => Data,
        "ttf" | "otf" | "ttc" | "otc" | "woff" | "woff2" | "fon" | "fnt" | "pfb" => Font,
        "iso" | "img" | "vhd" | "vhdx" | "vmdk" | "vdi" | "qcow2" | "dmg" | "wim" | "esd" => Disk,
        "" => match name.to_ascii_lowercase().as_str() {
            "makefile" | "dockerfile" | "cmakelists.txt" | "jenkinsfile" | "rakefile" | "gemfile" => Code,
            "license" | "readme" | "changelog" | "authors" | "notice" | "copying" => Text,
            _ => Other,
        },
        _ => Other,
    }
}

impl Cat {
    /// Colour on a light card; dark themes lighten it (`ink`). Each reaches 3:1 on its card.
    pub fn color(self) -> Color {
        rgb(match self {
            Cat::Text | Cat::Markdown | Cat::Font => 0x5B6B82,
            Cat::Pdf => 0xD93A40,
            Cat::Doc => 0x2F6FED,
            Cat::Sheet => 0x1E8E4E,
            Cat::Slides => 0xD9591E,
            Cat::Archive => 0xA86D14,
            Cat::Image => 0x0B8584,
            Cat::Audio => 0x9B47EC,
            Cat::Video => 0xD63C7C,
            Cat::Code | Cat::Data => 0x6E56CF,
            Cat::Disk => 0x475569,
            Cat::Other => 0x7A808A,
        })
    }

    /// Glyph at list size: no letters (they would be 4 px tall); colour tells the rest.
    pub fn small(self) -> Ico {
        match self {
            Cat::Text | Cat::Markdown | Cat::Doc | Cat::Font => Ico::FileText,
            Cat::Archive => Ico::FileZip,
            Cat::Image => Ico::FileImage,
            Cat::Audio => Ico::FileAudio,
            Cat::Video => Ico::FileVideo,
            Cat::Code | Cat::Data => Ico::FileCode,
            Cat::Disk => Ico::Disc,
            Cat::Pdf | Cat::Sheet | Cat::Slides | Cat::Other => Ico::File,
        }
    }

    /// Glyph inside the large page.
    pub fn inner(self) -> Option<Ico> {
        Some(match self {
            Cat::Text | Cat::Doc => Ico::TextAlignLeft,
            Cat::Markdown => Ico::Hash,
            Cat::Pdf => Ico::Article,
            Cat::Sheet => Ico::Table,
            Cat::Slides => Ico::PresentationChart,
            Cat::Archive => Ico::Archive,
            Cat::Image => Ico::Image,
            Cat::Audio => Ico::MusicNotes,
            Cat::Video => Ico::FilmStrip,
            Cat::Code => Ico::Code,
            Cat::Data => Ico::BracketsCurly,
            Cat::Font => Ico::TextAa,
            Cat::Disk => Ico::Disc,
            Cat::Other => return None,
        })
    }
}

/// The extension as the page shows it: up to four letters or digits, upper case.
pub fn label(e: &Entry) -> Option<String> {
    let x = ext_of(e);
    (!x.is_empty() && x.len() <= 4 && x.chars().all(|c| c.is_ascii_alphanumeric())).then(|| x.to_ascii_uppercase())
}

/// The type colour as drawn on this theme (lighter on dark cards).
pub fn ink(t: &Theme, c: Color) -> Color {
    if t.is_dark() { mix(c, rgb(0xFFFFFF), 0.22) } else { c }
}

fn mix(a: Color, b: Color, f: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * f, g: a.g + (b.g - a.g) * f, b: a.b + (b.b - a.b) * f, a: a.a }
}

fn alpha(c: Color, a: f32) -> Color {
    Color { a: c.a * a, ..c }
}

// The page and folder, drawn in a 96-unit box.
const PAGE: &str = "M26 8h34l20 20v50a10 10 0 0 1-10 10H26a10 10 0 0 1-10-10V18a10 10 0 0 1 10-10z";
const FOLD: &str = "M60 8v12a8 8 0 0 0 8 8h12z";
const FOLDER_BACK: &str = "M10 24a6 6 0 0 1 6-6h19.5a6 6 0 0 1 4.2 1.8l6.3 6.2h34a6 6 0 0 1 6 6v6H10z";
const FOLDER_FRONT: &str = "M8 38a6 6 0 0 1 6-6h68a6 6 0 0 1 6 6v38a6 6 0 0 1-6 6H14a6 6 0 0 1-6-6z";

/// Amber, not the accent: a blue accent would turn folders into Finder's.
fn folder_colors(t: &Theme) -> (Color, Color) {
    if t.is_dark() { (rgb(0xC08420), rgb(0xE3A93A)) } else { (rgb(0xDB9A26), rgb(0xF4B740)) }
}

/// Draws `icon` square in `r`. `on`: the colour of a selected item's text, for icons drawn on
/// the accent selection fill. `opacity` dims hidden and cut items.
pub fn draw(c: &mut Canvas, t: &Theme, icon: Own, e: &Entry, r: Rect, on: Option<Color>, opacity: f32) {
    let hc = t.mode == Mode::HighContrast;
    let s = r.w.min(r.h);
    let box_ = Rect::new(c.px(r.x + (r.w - s) / 2.0), c.px(r.y + (r.h - s) / 2.0), s, s);
    match icon {
        Own::Folder(_) if s <= 24.0 => {
            let col = if hc { on.unwrap_or(t.ink) } else { folder_colors(t).0 };
            c.icon(Ico::Folder, Weight::Fill, r, s, alpha(col, opacity));
        }
        Own::Folder(emblem) => {
            let mark = if hc {
                // The back sits on whatever is behind (the selection in tiles); the front is card.
                let back = alpha(on.unwrap_or(t.ink), opacity);
                let ink = alpha(t.ink, opacity);
                c.path(FOLDER_BACK, 96.0, box_, Color { a: 0.0, ..back }, Some((back, 1.0)));
                c.path(FOLDER_FRONT, 96.0, box_, t.card, Some((ink, 1.0)));
                ink
            } else {
                // Dimmed (hidden, cut) folders fade toward the card as solid colours: two translucent
                // fills would show a darker band where the back runs under the front.
                let fade = |col: Color| if opacity < 1.0 { mix(col, t.card, 1.0 - opacity) } else { col };
                let (back, front) = folder_colors(t);
                c.path(FOLDER_BACK, 96.0, box_, fade(back), None);
                c.path(FOLDER_FRONT, 96.0, box_, fade(front), None);
                // Pressed into the front, like Finder's emblems.
                alpha(mix(back, rgb(0x000000), 0.22), 0.9 * opacity)
            };
            if let Some(g) = emblem {
                let gs = (s * 30.0 / 96.0).round();
                c.icon(g, Weight::Fill, Rect::new(box_.x, box_.y + s * 57.0 / 96.0 - gs / 2.0, s, gs), gs, mark);
            }
        }
        Own::File(cat) if s <= 24.0 => {
            let col = match on {
                Some(on) => on,
                None if hc => t.ink,
                None => ink(t, cat.color()),
            };
            c.icon(cat.small(), Weight::Bold, r, s, alpha(col, opacity));
        }
        Own::File(cat) => {
            let k = s / 96.0;
            let col = match on {
                _ if hc => t.ink,
                // On the accent fill (a selected tile) the page takes the selection's text colour.
                Some(on) => on,
                None => ink(t, cat.color()),
            };
            if hc {
                c.path(PAGE, 96.0, box_, t.card, Some((alpha(col, opacity), 1.0)));
            } else {
                let fill = alpha(col, if t.is_dark() { 0.22 } else { 0.14 } * opacity);
                c.path(PAGE, 96.0, box_, fill, Some((alpha(col, 0.34 * opacity), (1.25 * k).max(1.0 / c.scale))));
                c.path(FOLD, 96.0, box_, alpha(col, 0.40 * opacity), None);
            }
            let text = if hc || t.is_dark() || on.is_some() { col } else { mix(col, rgb(0x000000), 0.15) };
            let lbl = (s >= 72.0).then(|| label(e)).flatten();
            match (cat.inner(), &lbl) {
                (Some(g), _) => {
                    let gs = (s * 30.0 / 96.0).round();
                    let gy = if lbl.is_some() { box_.y + s * 24.0 / 96.0 } else { box_.y + (s - gs) / 2.0 + s * 4.0 / 96.0 };
                    c.icon(g, Weight::Bold, Rect::new(box_.x, gy, s, gs), gs, alpha(col, opacity));
                }
                (None, _) => {}
            }
            if let Some(l) = lbl {
                let px = (s * 12.0 / 96.0).round().max(9.0) as u8;
                let top = if cat.inner().is_some() { box_.y + s * 62.0 / 96.0 } else { box_.y + s * 46.0 / 96.0 };
                c.text(&l, Rect::new(box_.x + s * 0.2, top, s * 0.6, px as f32 * 1.6), Font::Label(px), alpha(text, opacity), Align::Center);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

    fn file(name: &str) -> Entry {
        Entry { name: name.into(), is_dir: false, size: 1, modified: 0, created: 0, attrs: 0, full: None, shell: None, origin: None, sized: false }
    }
    fn dir(name: &str, attrs: u32) -> Entry {
        Entry { name: name.into(), is_dir: true, size: 0, modified: 0, created: 0, attrs: attrs | FILE_ATTRIBUTE_DIRECTORY.0, full: None, shell: None, origin: None, sized: false }
    }

    #[test]
    fn shell_keeps_programs_custom_folders_and_virtual_items() {
        for n in ["Setup.exe", "Word.lnk", "app.ICO", "web.url", "installer.msi", "pkg.msix"] {
            assert_eq!(own(&file(n), false), None, "{n}");
        }
        assert_eq!(own(&dir("Dokumenty", FILE_ATTRIBUTE_READONLY.0), false), None);
        assert_eq!(own(&dir("Ikona", FILE_ATTRIBUTE_SYSTEM.0), false), None);
        assert_eq!(own(&file("x.txt"), true), None);
        assert_eq!(own(&dir("C:", 0), true), None);
        if let Some(crate::app::Loc::Shell { id, .. }) = crate::shellns::recycle_bin() {
            // A network computer or phone: a shell item with no file-system path.
            let pc = Entry { shell: Some(id), ..dir("PC-KUCHYNE", 0) };
            assert_eq!(own(&pc, false), None);
            assert_eq!(own_in(&mut crate::icons::Icons::new(), &pc, Path::new(""), false), None);
        }
        let drive = Entry { full: Some(r"D:\".into()), ..dir("Data (D:)", 0) };
        assert_eq!(own(&drive, false), None);
    }

    #[test]
    fn everything_else_is_drawn_by_fm() {
        assert_eq!(own(&dir("Faktury", 0), false), Some(Own::Folder(None)));
        // A OneDrive placeholder folder is read-only for its own reasons; it is not a custom one.
        let ph = fm_fs::attrs::RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_READONLY.0;
        assert!(fm_fs::attrs::is_cloud_placeholder(ph));
        assert_eq!(own(&dir("Cloud", ph), false), Some(Own::Folder(None)));
        // Known folders are read-only for their desktop.ini, yet fm draws them, with an emblem.
        let docs = crate::fsys::known_folder(&windows::Win32::UI::Shell::FOLDERID_Documents).unwrap();
        let name = docs.file_name().unwrap().to_string_lossy().to_string();
        let parent = docs.parent().unwrap();
        assert_eq!(own_at(&dir(&name, FILE_ATTRIBUTE_READONLY.0), parent, false), Some(Own::Folder(Some(Ico::FileText))));
        let upper = std::path::PathBuf::from(parent.to_string_lossy().to_uppercase());
        assert_eq!(own_at(&dir(&name, FILE_ATTRIBUTE_READONLY.0), &upper, false), Some(Own::Folder(Some(Ico::FileText))));
        assert_eq!(own_at(&dir(&name, FILE_ATTRIBUTE_READONLY.0), parent, true), None);
        assert_eq!(own_at(&dir("Jiná", FILE_ATTRIBUTE_READONLY.0), parent, false), None);
        let f = |n: &str| own(&file(n), false);
        assert_eq!(f("Poznámky.TXT"), Some(Own::File(Cat::Text)));
        assert_eq!(f("Smlouva.pdf"), Some(Own::File(Cat::Pdf)));
        assert_eq!(f("Rozpočet.xlsx"), Some(Own::File(Cat::Sheet)));
        assert_eq!(f("kontakty.csv"), Some(Own::File(Cat::Sheet)));
        assert_eq!(f("Fotky.7z"), Some(Own::File(Cat::Archive)));
        assert_eq!(f("app.ts"), Some(Own::File(Cat::Code)));
        assert_eq!(f("config.json"), Some(Own::File(Cat::Data)));
        assert_eq!(f("Windows.iso"), Some(Own::File(Cat::Disk)));
        assert_eq!(f("záloha.dat"), Some(Own::File(Cat::Other)));
        assert_eq!(f("bez přípony"), Some(Own::File(Cat::Other)));
        // A Recycle Bin item shown without its extension: the $R… file still has it.
        let bin = Entry { full: Some(r"C:\$Recycle.Bin\S-1-5-21\$RAB12CD.pdf".into()), ..file("Smlouva") };
        assert_eq!(own(&bin, false), Some(Own::File(Cat::Pdf)));
        assert_eq!(label(&bin).as_deref(), Some("PDF"));
        let prog = Entry { full: Some(r"C:\$Recycle.Bin\S-1-5-21\$RAB12CE.exe".into()), ..file("Setup") };
        assert_eq!(own(&prog, false), None);
        // A hidden extension behind a dotted name: the $R file decides, not "1" or "11".
        let dotted = Entry { full: Some(r"C:\$Recycle.Bin\S-1-5-21\$RAB12CF.exe".into()), ..file("Setup 2.1") };
        assert_eq!(own(&dotted, false), None);
        let faktura = Entry { full: Some(r"C:\$Recycle.Bin\S-1-5-21\$RAB12D0.pdf".into()), ..file("Faktura 2024.11") };
        assert_eq!(own(&faktura, false), Some(Own::File(Cat::Pdf)));
        assert_eq!(label(&faktura).as_deref(), Some("PDF"));
        assert_eq!(f("Makefile"), Some(Own::File(Cat::Code)));
        assert_eq!(f("LICENSE"), Some(Own::File(Cat::Text)));
        // A dotfile has no extension; its name decides.
        assert_eq!(f(".gitignore"), Some(Own::File(Cat::Other)));
        assert_eq!(category("gitignore", ""), Cat::Data);
    }

    #[test]
    fn labels_are_short_extensions() {
        assert_eq!(label(&file("a.docx")).as_deref(), Some("DOCX"));
        assert_eq!(label(&file("a.7z")).as_deref(), Some("7Z"));
        assert_eq!(label(&file("a.markdown")), None);
        assert_eq!(label(&file("README")), None);
        assert_eq!(label(&file("a.ž")), None);
    }

    #[test]
    fn type_colours_reach_3_to_1_on_both_cards() {
        use crate::theme::{AccentPalette, contrast};
        let cats = [
            Cat::Text, Cat::Markdown, Cat::Pdf, Cat::Doc, Cat::Sheet, Cat::Slides, Cat::Archive, Cat::Image, Cat::Audio, Cat::Video, Cat::Code,
            Cat::Data, Cat::Font, Cat::Disk, Cat::Other,
        ];
        for t in [Theme::light(&AccentPalette::default()), Theme::dark(&AccentPalette::default())] {
            for cat in cats {
                let k = contrast(ink(&t, cat.color()), t.card);
                assert!(k >= 3.0, "{cat:?} on {:?}: {k:.2}", t.mode);
            }
        }
    }
}
