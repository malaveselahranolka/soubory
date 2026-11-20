//! The pick protocol between `fm_dialog_shim.dll` (inside the dialog's host) and fm.exe. Pure code,
//! shared by both sides (fm depends on this crate as an rlib, as dialogkit does on dialog-hook).
//!
//! 1. The shim creates the pipe `\\.\pipe\fm-pick-<pid>-<random>` and offers the request to the
//!    resident fm's door (`WM_COPYDATA`, [`COPY_PICK`]). Only an fm that answers [`ACK`] took it;
//!    otherwise it starts `fm.exe --pick <pipe>` (only an exe carrying [`MARKER`]).
//! 2. fm connects, the shim checks the client's process id and writes the request again as the
//!    pipe's first message, so both ways in read it from there.
//! 3. fm sends [`Msg::Ready`] once its window is up (this ends the shim's connect clock), then
//!    exactly one [`Msg::Picked`] or [`Msg::Cancelled`]. A pipe that breaks without an answer
//!    means the picker is gone: the shim shows the system dialog instead.
//!
//! Every message is UTF-16 text, `\0`-separated items; the first item names the version.

/// `COPYDATASTRUCT::dwData` of a pick request sent to fm's door window.
pub const COPY_PICK: usize = 0x666D_0004;
/// What a door that took the request returns. An older fm answers 0 to an unknown request.
pub const ACK: isize = 0x504B_4F4B;
pub const PIPE_PREFIX: &str = r"\\.\pipe\fm-pick-";
/// An RT_RCDATA resource in fm.exe (crates/app/build.rs): this exe understands `--pick`. An older
/// fm would read `--pick` as a folder to open, so the shim never starts one without it.
pub const MARKER_ID: u16 = 0x666D;
pub const MARKER: &str = "fm-pick=1";

const VERSION: &str = "fm-pick=1";
const READY: &str = "fm-pick-ready=1";
const REPLY: &str = "fm-pick-reply=1";
const SEP: char = '\u{1f}';

/// What the host asked its Open dialog for (the recorded `IFileDialog` setters).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub pipe: String,
    /// The dialog's owner window (`HWND` as an integer), 0 for none.
    pub owner: isize,
    pub title: String,
    pub ok: String,
    /// `SetFolder`: always start here.
    pub folder: String,
    /// `SetDefaultFolder`: start here when there is no last folder.
    pub default_folder: String,
    /// `SetFileName`.
    pub name: String,
    /// `SetFileTypes`: (display name, `*.png;*.jpg`).
    pub filters: Vec<(String, String)>,
    /// `SetFileTypeIndex`: 1-based, 0 = not set.
    pub index: u32,
    pub multi: bool,
    pub hidden: bool,
}

/// Only our own pipes: fm connects to whatever name the request carries.
pub fn valid_pipe(name: &str) -> bool {
    name.strip_prefix(PIPE_PREFIX)
        .is_some_and(|rest| !rest.is_empty() && rest.len() <= 64 && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

fn clean(s: &str) -> String {
    s.chars().filter(|&c| c != '\0' && c != SEP).collect()
}

fn join(items: &[String]) -> Vec<u16> {
    items.join("\0").encode_utf16().collect()
}

fn split(w: &[u16]) -> Vec<String> {
    String::from_utf16_lossy(w).split('\0').map(str::to_string).collect()
}

impl Request {
    pub fn encode(&self) -> Vec<u16> {
        let mut v = vec![
            VERSION.to_string(),
            format!("pipe={}", clean(&self.pipe)),
            format!("owner={:#x}", self.owner),
            format!("title={}", clean(&self.title)),
            format!("ok={}", clean(&self.ok)),
            format!("folder={}", clean(&self.folder)),
            format!("default={}", clean(&self.default_folder)),
            format!("name={}", clean(&self.name)),
            format!("index={}", self.index),
            format!("multi={}", self.multi as u8),
            format!("hidden={}", self.hidden as u8),
        ];
        v.extend(self.filters.iter().map(|(n, s)| format!("filter={}{SEP}{}", clean(n), clean(s))));
        join(&v)
    }

    /// `None` for another version, a missing or foreign pipe name. Unknown keys are skipped.
    pub fn decode(w: &[u16]) -> Option<Request> {
        let items = split(w);
        if items.first().map(String::as_str) != Some(VERSION) {
            return None;
        }
        let mut r = Request::default();
        for it in &items[1..] {
            let Some((k, v)) = it.split_once('=') else { continue };
            match k {
                "pipe" => r.pipe = v.to_string(),
                "owner" => {
                    r.owner = match v.strip_prefix("0x") {
                        Some(h) => i64::from_str_radix(h, 16).ok()? as isize,
                        None => v.parse().ok()?,
                    }
                }
                "title" => r.title = v.to_string(),
                "ok" => r.ok = v.to_string(),
                "folder" => r.folder = v.to_string(),
                "default" => r.default_folder = v.to_string(),
                "name" => r.name = v.to_string(),
                "index" => r.index = v.parse().unwrap_or(0),
                "multi" => r.multi = v == "1",
                "hidden" => r.hidden = v == "1",
                "filter" => {
                    let (n, s) = v.split_once(SEP).unwrap_or((v, ""));
                    r.filters.push((n.to_string(), s.to_string()));
                }
                _ => {}
            }
        }
        valid_pipe(&r.pipe).then_some(r)
    }
}

/// fm's messages on the pipe.
#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// The picker window is up.
    Ready,
    /// Full paths of the picked files.
    Picked(Vec<String>),
    Cancelled,
}

impl Msg {
    pub fn encode(&self) -> Vec<u16> {
        let items: Vec<String> = match self {
            Msg::Ready => vec![READY.into()],
            Msg::Picked(p) => [REPLY.to_string(), "ok".into()].into_iter().chain(p.iter().map(|s| clean(s))).chain(["end".into()]).collect(),
            Msg::Cancelled => vec![REPLY.into(), "cancel".into(), "end".into()],
        };
        join(&items)
    }

    /// `None` for anything else, including a truncated reply (no closing `end`).
    pub fn decode(w: &[u16]) -> Option<Msg> {
        let items = split(w);
        match items.as_slice() {
            [r] if r == READY => Some(Msg::Ready),
            [r, kind, rest @ .., end] if r == REPLY && end == "end" => match kind.as_str() {
                "cancel" if rest.is_empty() => Some(Msg::Cancelled),
                "ok" if !rest.is_empty() && rest.iter().all(|p| !p.is_empty()) => Some(Msg::Picked(rest.to_vec())),
                _ => None,
            },
            _ => None,
        }
    }
}

/// UTF-16 little-endian bytes, as they travel through the pipe.
pub fn to_bytes(w: &[u16]) -> Vec<u8> {
    w.iter().flat_map(|c| c.to_le_bytes()).collect()
}

pub fn from_bytes(b: &[u8]) -> Option<Vec<u16>> {
    (b.len() % 2 == 0).then(|| b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Request {
        Request {
            pipe: r"\\.\pipe\fm-pick-42-00ab".into(),
            owner: 0x1234_5678,
            title: "Vyberte soubory".into(),
            ok: "Nahrát".into(),
            folder: r"C:\Users\Žluťoučký kůň".into(),
            default_folder: String::new(),
            name: "a=b.txt".into(),
            filters: vec![("Obrázky".into(), "*.png;*.jpg".into()), ("Vše".into(), "*.*".into())],
            index: 2,
            multi: true,
            hidden: false,
        }
    }

    #[test]
    fn request_round_trip() {
        let r = req();
        assert_eq!(Request::decode(&r.encode()), Some(r));
        // Through the pipe's bytes too.
        let r = req();
        assert_eq!(Request::decode(&from_bytes(&to_bytes(&r.encode())).unwrap()), Some(r));
        // NULs inside a value cannot split it.
        let mut r = req();
        r.title = "a\0pipe=x".into();
        assert_eq!(Request::decode(&r.encode()).unwrap().title, "apipe=x");
    }

    #[test]
    fn request_refusals() {
        let mut r = req();
        r.pipe = r"\\.\pipe\other".into();
        assert_eq!(Request::decode(&r.encode()), None, "foreign pipe");
        r.pipe = r"\\.\pipe\fm-pick-..\x".into();
        assert_eq!(Request::decode(&r.encode()), None, "path characters");
        let w: Vec<u16> = "fm-pick=2\0pipe=\\\\.\\pipe\\fm-pick-1".encode_utf16().collect();
        assert_eq!(Request::decode(&w), None, "another version");
        let w: Vec<u16> = "fm-pick=1\0pipe=\\\\.\\pipe\\fm-pick-1\0future=x".encode_utf16().collect();
        assert!(Request::decode(&w).is_some(), "unknown keys are skipped");
        assert_eq!(Request::decode(&[]), None);
        assert_eq!(from_bytes(&[1, 2, 3]), None);
    }

    #[test]
    fn messages() {
        for m in [Msg::Ready, Msg::Cancelled, Msg::Picked(vec![r"C:\a.txt".into(), r"D:\b c\ď.png".into()])] {
            assert_eq!(Msg::decode(&m.encode()), Some(m));
        }
        let full = Msg::Picked(vec![r"C:\a.txt".into()]).encode();
        assert_eq!(Msg::decode(&full[..full.len() - 4]), None, "truncated");
        assert_eq!(Msg::decode(&Msg::Picked(vec![]).encode()), None, "nothing picked");
        assert_eq!(Msg::decode(&req().encode()), None);
    }
}
