//! Windows filesystem walker, live USN/RDCW watcher, and multi-volume search service.

pub mod service;
pub mod walker;
pub mod watcher;

use std::io;
use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, GetFileInformationByHandle, GetFullPathNameW, OPEN_EXISTING,
};
use windows::core::PCWSTR;

/// Bitmask check for the `IO_REPARSE_TAG_CLOUD*` family (`0x9000_001A` .. `0x9000_F01A`).
#[inline]
#[must_use]
pub const fn is_cloud_reparse_tag(tag: u32) -> bool {
    (tag & 0xFFFF_0FFF) == 0x9000_001A
}

/// RAII wrapper around a Win32 `HANDLE` that closes it on drop.
pub(crate) struct WinHandle(pub(crate) HANDLE);

// Win32 kernel file/event handles may be moved or closed from another thread.
unsafe impl Send for WinHandle {}
unsafe impl Sync for WinHandle {}

impl Drop for WinHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[inline]
pub(crate) fn wide_null(s: &str) -> Vec<u16> {
    let mut buf = Vec::with_capacity(s.len() + 1);
    buf.extend(s.encode_utf16());
    buf.push(0);
    buf
}

#[inline]
pub(crate) fn wide_into(s: &str, buf: &mut Vec<u16>) {
    buf.clear();
    buf.extend(s.encode_utf16());
    buf.push(0);
}

#[inline]
pub(crate) fn win32_error_code(err: &windows::core::Error) -> Option<u32> {
    let raw = err.code().0 as u32;
    if (raw >> 16) == 0x8007 {
        Some(raw & 0xFFFF)
    } else {
        None
    }
}

#[inline]
pub(crate) fn win32_to_io(err: windows::core::Error) -> io::Error {
    if let Some(code) = win32_error_code(&err) {
        io::Error::from_raw_os_error(code as i32)
    } else {
        io::Error::other(err.message())
    }
}

#[inline]
pub(crate) fn filetime_to_i64(ft: FILETIME) -> i64 {
    (((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)) as i64
}

pub(crate) fn decode_utf16_le_into(bytes: &[u8], out: &mut String) {
    out.clear();
    let (pairs, _) = bytes.as_chunks::<2>();
    if pairs.iter().all(|&[lo, hi]| hi == 0 && lo < 0x80) {
        out.reserve(pairs.len());
        for &[lo, _] in pairs {
            out.push(lo as char);
        }
        return;
    }
    for r in char::decode_utf16(pairs.iter().map(|&c| u16::from_le_bytes(c))) {
        out.push(r.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
}

/// Normalizes a root or folder path for display/index storage (forward slashes to `\`,
/// strips `\\?\` prefix, keeps drive root `X:\`, trims trailing `\` elsewhere).
pub(crate) fn normalize_display_path(raw: &str) -> String {
    let mut s = raw.replace('/', "\\");
    if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
        s = format!("\\\\{rest}");
    } else if let Some(rest) = s.strip_prefix("\\\\?\\") {
        s = rest.to_owned();
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2..].iter().all(|&b| b == b'\\')
    {
        return format!("{}:\\", bytes[0] as char);
    }
    while s.len() > 1 && s.ends_with('\\') {
        s.pop();
    }
    s
}

/// Converts a Windows path into a `\\?\` extended-length path for Win32 calls.
pub(crate) fn to_extended_path(path: &str) -> String {
    let slash_norm = path.replace('/', "\\");
    if let Some(rest) = slash_norm.strip_prefix("\\\\?\\") {
        let bytes = rest.as_bytes();
        if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return format!("{slash_norm}\\");
        }
        return slash_norm;
    }
    if let Some(unc) = slash_norm.strip_prefix("\\\\") {
        return format!("\\\\?\\UNC\\{unc}");
    }
    let bytes = slash_norm.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        if bytes.len() == 2 {
            return format!("\\\\?\\{slash_norm}\\");
        }
        if bytes[2] == b'\\' {
            return format!("\\\\?\\{slash_norm}");
        }
    }
    // Relative path fallback: resolve via GetFullPathNameW first.
    let w = wide_null(&slash_norm);
    let needed = unsafe { GetFullPathNameW(PCWSTR(w.as_ptr()), None, None) };
    if needed > 0 {
        let mut buf = vec![0u16; needed as usize];
        let written = unsafe { GetFullPathNameW(PCWSTR(w.as_ptr()), Some(&mut buf), None) };
        if written > 0 && (written as usize) < buf.len() {
            let full = String::from_utf16_lossy(&buf[..written as usize]);
            if let Some(unc) = full.strip_prefix("\\\\") {
                return format!("\\\\?\\UNC\\{unc}");
            }
            return format!("\\\\?\\{full}");
        }
    }
    let pb = PathBuf::from(&slash_norm);
    format!("\\\\?\\{}", pb.display())
}

#[inline]
pub(crate) fn join_child_path(parent: &str, child: &str) -> String {
    let mut out = String::with_capacity(parent.len() + 1 + child.len());
    out.push_str(parent);
    if !parent.ends_with('\\') {
        out.push('\\');
    }
    out.push_str(child);
    out
}

/// Opens an existing directory handle for listing/watching only (never opens files).
pub(crate) fn open_dir_handle(
    path: &str,
    overlapped: bool,
    wbuf: &mut Vec<u16>,
) -> io::Result<WinHandle> {
    let ext = to_extended_path(path);
    wide_into(&ext, wbuf);
    let flags = FILE_FLAG_BACKUP_SEMANTICS
        | if overlapped {
            FILE_FLAG_OVERLAPPED
        } else {
            FILE_FLAGS_AND_ATTRIBUTES(0)
        };
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wbuf.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }
    .map_err(win32_to_io)?;
    Ok(WinHandle(handle))
}

/// Queries volume serial, 64-bit FileId, attributes, and mtime from an open directory handle.
pub(crate) fn query_dir_info_by_handle(handle: HANDLE) -> io::Result<(u32, u64, u32, i64)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(win32_to_io)?;
    let fid = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    Ok((
        info.dwVolumeSerialNumber,
        fid,
        info.dwFileAttributes,
        filetime_to_i64(info.ftLastWriteTime),
    ))
}
