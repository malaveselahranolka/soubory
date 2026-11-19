//! Win32 `FILE_ATTRIBUTE_*` constants and cloud-placeholder classification.

/// File or directory is read-only.
pub const READONLY: u32 = 0x0000_0001;
/// File or directory is hidden.
pub const HIDDEN: u32 = 0x0000_0002;
/// File or directory is part of or used exclusively by the operating system.
pub const SYSTEM: u32 = 0x0000_0004;
/// Entry is a directory.
pub const DIRECTORY: u32 = 0x0000_0010;
/// File or directory is marked for archive/backup.
pub const ARCHIVE: u32 = 0x0000_0020;
/// Reserved for system use (device).
pub const DEVICE: u32 = 0x0000_0040;
/// Normal file with no other attributes set.
pub const NORMAL: u32 = 0x0000_0080;
/// File used for temporary storage.
pub const TEMPORARY: u32 = 0x0000_0100;
/// File is a sparse file.
pub const SPARSE_FILE: u32 = 0x0000_0200;
/// File or directory has an associated reparse point or is a symbolic link.
pub const REPARSE_POINT: u32 = 0x0000_0400;
/// File or directory is compressed.
pub const COMPRESSED: u32 = 0x0000_0800;
/// Data of the file is not immediately available (offline / placeholder).
pub const OFFLINE: u32 = 0x0000_1000;
/// File or directory is not indexed by the content indexing service.
pub const NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
/// File or directory is encrypted.
pub const ENCRYPTED: u32 = 0x0000_4000;
/// Data integrity support is enabled for the file or directory.
pub const INTEGRITY_STREAM: u32 = 0x0000_8000;
/// Reserved for system use (virtual).
pub const VIRTUAL: u32 = 0x0001_0000;
/// User data stream not to be read by the background data integrity scanner.
pub const NO_SCRUB_DATA: u32 = 0x0002_0000;
/// Entry has no physical representation on the local system and recalls on open.
pub const RECALL_ON_OPEN: u32 = 0x0004_0000;
/// User intent that the file or directory be kept fully present locally.
pub const PINNED: u32 = 0x0008_0000;
/// User intent that the file or directory not be kept fully present locally.
pub const UNPINNED: u32 = 0x0010_0000;
/// File or directory is not fully present locally and recalls data on access.
pub const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Returns `true` if `attrs` indicates a cloud placeholder that must never be hydrated
/// (`OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS`).
#[inline]
#[must_use]
pub const fn is_cloud_placeholder(attrs: u32) -> bool {
    (attrs & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_placeholder_flags() {
        assert!(!is_cloud_placeholder(0));
        assert!(!is_cloud_placeholder(NORMAL | ARCHIVE | PINNED | UNPINNED));
        assert!(!is_cloud_placeholder(REPARSE_POINT | HIDDEN | DIRECTORY));
        assert!(is_cloud_placeholder(OFFLINE));
        assert!(is_cloud_placeholder(RECALL_ON_OPEN));
        assert!(is_cloud_placeholder(RECALL_ON_DATA_ACCESS));
        assert!(is_cloud_placeholder(ARCHIVE | RECALL_ON_DATA_ACCESS));
    }
}
