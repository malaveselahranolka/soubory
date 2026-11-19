//! Binary snapshot serialization and corruption-resilient loading.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};

use rayon::prelude::*;

use crate::arena::{
    EXT_OVERFLOW, EntryId, FLAG_ALIVE, FLAG_HAS_DISPLAY, Index, NONE, is_own_hidden,
    next_instance_id, reserve_arena,
};

const MAGIC: [u8; 4] = *b"FMIX";
const FORMAT_VERSION: u32 = 2;
const HEADER_SIZE: usize = 48;
const MAX_READ_CHUNK_BYTES: usize = 1024 * 1024;
const HDR_FLAG_EXT_OVERFLOW: u32 = 1 << 0;
const HDR_FLAG_COMPACT_OFFSETS: u32 = 1 << 1;

/// Errors returned when loading a binary index snapshot.
#[derive(Debug)]
pub enum SnapshotError {
    /// Underlying I/O error.
    Io(io::Error),
    /// Snapshot header did not start with `FMIX`.
    BadMagic,
    /// Snapshot format version is unsupported.
    UnsupportedVersion(u32),
    /// Snapshot volume serial did not match the expected serial.
    VolumeSerialMismatch {
        /// Expected volume serial.
        expected: u32,
        /// Actual volume serial found in the snapshot header.
        actual: u32,
    },
    /// Snapshot checksum did not match the body contents.
    ChecksumMismatch {
        /// Checksum stored in the snapshot footer.
        expected: u64,
        /// Computed checksum of the snapshot payload.
        actual: u64,
    },
    /// Snapshot ended prematurely before all declared bytes were read.
    Truncated,
    /// Snapshot header or internal indices/offsets are structurally invalid.
    Corrupt(&'static str),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "snapshot I/O error: {err}"),
            Self::BadMagic => f.write_str("invalid snapshot magic (expected FMIX)"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported snapshot version {v}"),
            Self::VolumeSerialMismatch { expected, actual } => write!(
                f,
                "volume serial mismatch: expected {expected:#x}, got {actual:#x}"
            ),
            Self::ChecksumMismatch { expected, actual } => write!(
                f,
                "snapshot checksum mismatch: expected {expected:#x}, got {actual:#x}"
            ),
            Self::Truncated => f.write_str("snapshot is truncated"),
            Self::Corrupt(msg) => write!(f, "corrupt snapshot: {msg}"),
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SnapshotError {
    fn from(err: io::Error) -> Self {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(err)
        }
    }
}

/// Chunk-independent 4-lane 64-bit inline checksum.
struct FastHash64 {
    lanes: [u64; 4],
    buf: [u8; 32],
    buf_len: usize,
    total_len: u64,
}

impl FastHash64 {
    const P1: u64 = 0x9e37_79b1_85eb_ca87;
    const P2: u64 = 0xc2b2_ae3d_27d4_eb4f;
    const P3: u64 = 0x1656_67b1_9e37_79f9;
    const P4: u64 = 0x85eb_ca77_c2b2_ae63;

    fn new() -> Self {
        Self {
            lanes: [
                Self::P1.wrapping_add(Self::P2),
                Self::P2,
                0,
                0u64.wrapping_sub(Self::P1),
            ],
            buf: [0; 32],
            buf_len: 0,
            total_len: 0,
        }
    }

    #[inline]
    fn mix_block(lanes: &mut [u64; 4], block: &[u8; 32]) {
        let (chunks, _) = block.as_chunks::<8>();
        for (lane, &chunk) in lanes.iter_mut().zip(chunks) {
            let val = u64::from_le_bytes(chunk);
            *lane = lane
                .wrapping_add(val.wrapping_mul(Self::P2))
                .rotate_left(31)
                .wrapping_mul(Self::P1);
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.total_len += bytes.len() as u64;
        if self.buf_len > 0 {
            let need = 32 - self.buf_len;
            if bytes.len() < need {
                self.buf[self.buf_len..self.buf_len + bytes.len()].copy_from_slice(bytes);
                self.buf_len += bytes.len();
                return;
            }
            self.buf[self.buf_len..32].copy_from_slice(&bytes[..need]);
            let block = self.buf;
            Self::mix_block(&mut self.lanes, &block);
            self.buf_len = 0;
            bytes = &bytes[need..];
        }

        let (blocks, rem) = bytes.as_chunks::<32>();
        for block in blocks {
            Self::mix_block(&mut self.lanes, block);
        }
        if !rem.is_empty() {
            self.buf[..rem.len()].copy_from_slice(rem);
            self.buf_len = rem.len();
        }
    }

    fn finish(self) -> u64 {
        let mut h = self.lanes[0]
            .rotate_left(1)
            .wrapping_add(self.lanes[1].rotate_left(7))
            .wrapping_add(self.lanes[2].rotate_left(12))
            .wrapping_add(self.lanes[3].rotate_left(18));
        h ^= self.total_len.wrapping_mul(Self::P3);

        let rem = &self.buf[..self.buf_len];
        let (words, tail) = rem.as_chunks::<8>();
        for &w in words {
            let k = u64::from_le_bytes(w)
                .wrapping_mul(Self::P2)
                .rotate_left(31)
                .wrapping_mul(Self::P1);
            h ^= k;
            h = h
                .rotate_left(27)
                .wrapping_mul(Self::P1)
                .wrapping_add(Self::P4);
        }
        for &b in tail {
            h ^= (b as u64).wrapping_mul(Self::P4);
            h = h.rotate_left(11).wrapping_mul(Self::P1);
        }
        h ^= h >> 33;
        h = h.wrapping_mul(Self::P2);
        h ^= h >> 29;
        h = h.wrapping_mul(Self::P3);
        h ^= h >> 32;
        h
    }
}

impl Index {
    /// Serializes the index in little-endian binary format with an inline 64-bit checksum.
    pub fn save(&self, mut w: impl Write) -> io::Result<()> {
        let slot_count = u32::try_from(self.parent.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too many slots"))?;
        let live_count = u32::try_from(self.live_count)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too many live entries"))?;
        let ext_count = u32::try_from(self.exts.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too many extensions"))?;

        let mut ext_blob = Vec::new();
        for ext in &self.exts {
            let b = ext.as_bytes();
            let len = u16::try_from(b.len())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ext too long"))?;
            ext_blob.extend_from_slice(&len.to_le_bytes());
            ext_blob.extend_from_slice(b);
        }

        let compact_offsets = self.arenas_sequential
            && self.dead_arena_bytes == 0
            && self.live_count == self.parent.len();
        let mut hdr_flags = 0u32;
        if self.ext_overflow {
            hdr_flags |= HDR_FLAG_EXT_OVERFLOW;
        }
        if compact_offsets {
            hdr_flags |= HDR_FLAG_COMPACT_OFFSETS;
        }

        let slot_usize = slot_count as usize;
        let per_slot_bytes: u64 = if compact_offsets { 27 } else { 35 };
        let body_len: u64 = (slot_usize as u64) * per_slot_bytes
            + (ext_blob.len() as u64)
            + (self.folded_arena.len() as u64)
            + (self.display_arena.len() as u64);

        let mut header = [0u8; HEADER_SIZE];
        header[0..4].copy_from_slice(&MAGIC);
        header[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        header[8..12].copy_from_slice(&self.volume_serial.to_le_bytes());
        header[12..16].copy_from_slice(&slot_count.to_le_bytes());
        header[16..20].copy_from_slice(&live_count.to_le_bytes());
        header[20..24].copy_from_slice(&ext_count.to_le_bytes());
        let ext_blob_len = u32::try_from(ext_blob.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ext table exceeds 4 GiB"))?;
        let folded_len = u32::try_from(self.folded_arena.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "folded arena exceeds 4 GiB")
        })?;
        let display_len = u32::try_from(self.display_arena.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "display arena exceeds 4 GiB")
        })?;
        header[24..28].copy_from_slice(&ext_blob_len.to_le_bytes());
        header[28..32].copy_from_slice(&folded_len.to_le_bytes());
        header[32..36].copy_from_slice(&display_len.to_le_bytes());
        header[36..40].copy_from_slice(&hdr_flags.to_le_bytes());
        header[40..48].copy_from_slice(&body_len.to_le_bytes());

        w.write_all(&header)?;

        let mut hasher = FastHash64::new();
        hasher.update(&header[4..]);

        let mut scratch = vec![0u8; 64 * 1024];
        write_le_slice(
            &mut w,
            &mut hasher,
            &self.parent,
            u32::to_le_bytes,
            &mut scratch,
        )?;
        write_le_slice(
            &mut w,
            &mut hasher,
            &self.attrs,
            u32::to_le_bytes,
            &mut scratch,
        )?;
        write_le_slice(
            &mut w,
            &mut hasher,
            &self.size,
            u64::to_le_bytes,
            &mut scratch,
        )?;
        write_le_slice(
            &mut w,
            &mut hasher,
            &self.mtime,
            i64::to_le_bytes,
            &mut scratch,
        )?;
        write_le_slice(
            &mut w,
            &mut hasher,
            &self.ext_id,
            u16::to_le_bytes,
            &mut scratch,
        )?;
        if compact_offsets {
            for (flags_chunk, disp_chunk) in self
                .flags
                .chunks(scratch.len())
                .zip(self.display_off.chunks(scratch.len()))
            {
                let out = &mut scratch[..flags_chunk.len()];
                for ((dst, &f), &doff) in out.iter_mut().zip(flags_chunk).zip(disp_chunk) {
                    *dst = f | if doff != NONE { FLAG_HAS_DISPLAY } else { 0 };
                }
                hasher.update(out);
                w.write_all(out)?;
            }
        } else {
            write_bytes_chunked(&mut w, &mut hasher, &self.flags)?;
            write_le_slice(
                &mut w,
                &mut hasher,
                &self.folded_off,
                u32::to_le_bytes,
                &mut scratch,
            )?;
            write_le_slice(
                &mut w,
                &mut hasher,
                &self.display_off,
                u32::to_le_bytes,
                &mut scratch,
            )?;
        }
        write_bytes_chunked(&mut w, &mut hasher, &ext_blob)?;
        write_bytes_chunked(&mut w, &mut hasher, &self.folded_arena)?;
        write_bytes_chunked(&mut w, &mut hasher, &self.display_arena)?;

        w.write_all(&hasher.finish().to_le_bytes())?;
        Ok(())
    }

    /// Deserializes an index from `r`, validating magic, version, optional volume serial,
    /// lengths, checksum, and structural invariants without panicking or over-allocating.
    pub fn load(
        mut r: impl Read,
        expected_volume_serial: Option<u32>,
    ) -> Result<Index, SnapshotError> {
        let mut header = [0u8; HEADER_SIZE];
        read_exact_or_truncated(&mut r, &mut header)?;

        if header[0..4] != MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if version != FORMAT_VERSION {
            return Err(SnapshotError::UnsupportedVersion(version));
        }
        let volume_serial = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        if let Some(expected) = expected_volume_serial
            && volume_serial != expected
        {
            return Err(SnapshotError::VolumeSerialMismatch {
                expected,
                actual: volume_serial,
            });
        }

        let slot_count = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        let live_count = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
        let ext_count = u32::from_le_bytes([header[20], header[21], header[22], header[23]]);
        let ext_blob_len = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
        let folded_len = u32::from_le_bytes([header[28], header[29], header[30], header[31]]);
        let display_len = u32::from_le_bytes([header[32], header[33], header[34], header[35]]);
        let hdr_flags = u32::from_le_bytes([header[36], header[37], header[38], header[39]]);
        let body_len = u64::from_le_bytes([
            header[40], header[41], header[42], header[43], header[44], header[45], header[46],
            header[47],
        ]);

        if slot_count > u32::MAX - 2 || live_count > slot_count || ext_count == 0 {
            return Err(SnapshotError::Corrupt("invalid header counts"));
        }
        if (hdr_flags & !(HDR_FLAG_EXT_OVERFLOW | HDR_FLAG_COMPACT_OFFSETS)) != 0 {
            return Err(SnapshotError::Corrupt("invalid header flags"));
        }
        let compact_offsets = (hdr_flags & HDR_FLAG_COMPACT_OFFSETS) != 0;
        let ext_overflow = (hdr_flags & HDR_FLAG_EXT_OVERFLOW) != 0;
        if compact_offsets && live_count != slot_count {
            return Err(SnapshotError::Corrupt("compact snapshot has dead slots"));
        }

        let slot_usize = slot_count as usize;
        let per_slot_bytes: u64 = if compact_offsets { 27 } else { 35 };
        let expected_body = (slot_usize as u64)
            .checked_mul(per_slot_bytes)
            .and_then(|v| v.checked_add(ext_blob_len as u64))
            .and_then(|v| v.checked_add(folded_len as u64))
            .and_then(|v| v.checked_add(display_len as u64))
            .ok_or(SnapshotError::Corrupt("header length overflow"))?;
        if expected_body != body_len {
            return Err(SnapshotError::Corrupt("body_len mismatch"));
        }

        let mut hasher = FastHash64::new();
        hasher.update(&header[4..]);

        let parent = read_le_vec_bounded(&mut r, &mut hasher, slot_usize, u32::from_le_bytes)?;
        let attrs = read_le_vec_bounded(&mut r, &mut hasher, slot_usize, u32::from_le_bytes)?;
        let size = read_le_vec_bounded(&mut r, &mut hasher, slot_usize, u64::from_le_bytes)?;
        let mtime = read_le_vec_bounded(&mut r, &mut hasher, slot_usize, i64::from_le_bytes)?;
        let ext_id = read_le_vec_bounded(&mut r, &mut hasher, slot_usize, u16::from_le_bytes)?;
        let mut flags = read_bytes_bounded(&mut r, &mut hasher, slot_usize)?;
        let (folded_off, display_off) = if compact_offsets {
            (Vec::new(), Vec::new())
        } else {
            let fo = read_le_vec_bounded(&mut r, &mut hasher, slot_usize, u32::from_le_bytes)?;
            let disp_o = read_le_vec_bounded(&mut r, &mut hasher, slot_usize, u32::from_le_bytes)?;
            (fo, disp_o)
        };
        let ext_blob = read_bytes_bounded(&mut r, &mut hasher, ext_blob_len as usize)?;
        let folded_arena = read_bytes_bounded(&mut r, &mut hasher, folded_len as usize)?;
        let display_arena = read_bytes_bounded(&mut r, &mut hasher, display_len as usize)?;

        let mut trailer = [0u8; 8];
        read_exact_or_truncated(&mut r, &mut trailer)?;
        let expected_checksum = u64::from_le_bytes(trailer);
        let actual_checksum = hasher.finish();
        if expected_checksum != actual_checksum {
            return Err(SnapshotError::ChecksumMismatch {
                expected: expected_checksum,
                actual: actual_checksum,
            });
        }

        let (exts, ext_map) = parse_ext_blob(&ext_blob, ext_count as usize)?;
        let (folded_off, display_off) = if compact_offsets {
            reconstruct_compact_offsets(&mut flags, &folded_arena, &display_arena)?
        } else {
            (folded_off, display_off)
        };

        validate_loaded_data(
            slot_usize,
            live_count as usize,
            exts.len(),
            &parent,
            &attrs,
            &ext_id,
            &flags,
            &folded_off,
            &display_off,
            &folded_arena,
            &display_arena,
        )?;

        let mut idx = Index {
            folded_arena,
            display_arena,
            folded_off,
            display_off,
            parent,
            attrs,
            size,
            mtime,
            ext_id,
            flags,
            first_child: Vec::new(),
            next_sibling: Vec::new(),
            buckets: Vec::new(),
            bucket_occupied: 0,
            roots: Vec::new(),
            exts,
            ext_map,
            ext_overflow,
            live_count: live_count as usize,
            dead_arena_bytes: 0,
            arenas_sequential: compact_offsets,
            volume_serial,
            instance_id: next_instance_id(),
            index_gen: 1,
        };
        idx.rebuild_child_links_and_buckets_checked()
            .map_err(SnapshotError::Corrupt)?;
        Ok(idx)
    }
}
fn reconstruct_compact_offsets(
    flags: &mut [u8],
    folded_arena: &[u8],
    display_arena: &[u8],
) -> Result<(Vec<u32>, Vec<u32>), SnapshotError> {
    let n = flags.len();
    let mut folded_off = Vec::with_capacity(n);
    let mut display_off = Vec::with_capacity(n);
    let mut f_pos = 0usize;
    let mut d_pos = 0usize;

    for f in flags.iter_mut() {
        let has_display = (*f & FLAG_HAS_DISPLAY) != 0;
        *f &= !FLAG_HAS_DISPLAY;
        if (*f & FLAG_ALIVE) == 0 {
            return Err(SnapshotError::Corrupt("compact snapshot has dead entry"));
        }

        let f_off =
            u32::try_from(f_pos).map_err(|_| SnapshotError::Corrupt("folded arena > 4G"))?;
        let f_len = step_arena_entry(folded_arena, f_pos)?;
        folded_off.push(f_off);
        f_pos += f_len;

        if has_display {
            let d_off =
                u32::try_from(d_pos).map_err(|_| SnapshotError::Corrupt("display arena > 4G"))?;
            let d_len = step_arena_entry(display_arena, d_pos)?;
            display_off.push(d_off);
            d_pos += d_len;
        } else {
            display_off.push(NONE);
        }
    }

    if f_pos != folded_arena.len() || d_pos != display_arena.len() {
        return Err(SnapshotError::Corrupt("compact arena trailing bytes"));
    }
    Ok((folded_off, display_off))
}

#[inline]
fn step_arena_entry(arena: &[u8], pos: usize) -> Result<usize, SnapshotError> {
    if pos >= arena.len() {
        return Err(SnapshotError::Corrupt("arena offset out of bounds"));
    }
    let first = arena[pos];
    let (hdr, len) = if first < 255 {
        (1usize, first as usize)
    } else {
        if pos + 3 > arena.len() {
            return Err(SnapshotError::Corrupt("arena extended length truncated"));
        }
        (
            3usize,
            u16::from_le_bytes([arena[pos + 1], arena[pos + 2]]) as usize,
        )
    };
    let end = pos
        .checked_add(hdr)
        .and_then(|v| v.checked_add(len))
        .ok_or(SnapshotError::Corrupt("arena length overflow"))?;
    if end > arena.len() {
        return Err(SnapshotError::Corrupt("arena string extends past end"));
    }
    Ok(hdr + len)
}

fn parse_ext_blob(
    blob: &[u8],
    ext_count: usize,
) -> Result<(Vec<String>, HashMap<String, u16>), SnapshotError> {
    let mut exts = Vec::with_capacity(ext_count.min(4096));
    let mut ext_map = HashMap::with_capacity(ext_count.min(4096));
    let mut pos = 0usize;
    for i in 0..ext_count {
        if pos + 2 > blob.len() {
            return Err(SnapshotError::Corrupt("truncated ext table"));
        }
        let len = u16::from_le_bytes([blob[pos], blob[pos + 1]]) as usize;
        pos += 2;
        if pos + len > blob.len() {
            return Err(SnapshotError::Corrupt("ext string out of bounds"));
        }
        let s = std::str::from_utf8(&blob[pos..pos + len])
            .map_err(|_| SnapshotError::Corrupt("invalid UTF-8 in ext table"))?;
        pos += len;
        let owned = s.to_owned();
        if i > 0 {
            let id = u16::try_from(i).map_err(|_| SnapshotError::Corrupt("ext_count > u16"))?;
            ext_map.insert(owned.clone(), id);
        }
        exts.push(owned);
    }
    if pos != blob.len() {
        return Err(SnapshotError::Corrupt("trailing bytes in ext table"));
    }
    if exts.first().map(String::as_str) != Some("") {
        return Err(SnapshotError::Corrupt("ext[0] must be empty"));
    }
    Ok((exts, ext_map))
}

#[allow(clippy::too_many_arguments)]
fn validate_loaded_data(
    slot_count: usize,
    live_count: usize,
    ext_count: usize,
    parent: &[EntryId],
    attrs: &[u32],
    ext_id: &[u16],
    flags: &[u8],
    folded_off: &[u32],
    display_off: &[u32],
    folded_arena: &[u8],
    display_arena: &[u8],
) -> Result<(), SnapshotError> {
    let chunk_size = 8192usize;
    let num_chunks = slot_count.div_ceil(chunk_size);
    let counted_live: Result<usize, SnapshotError> = (0..num_chunks)
        .into_par_iter()
        .map(|chunk_idx| {
            let start = chunk_idx * chunk_size;
            let end = (start + chunk_size).min(slot_count);
            let mut local_live = 0usize;
            for i in start..end {
                let f = flags[i];
                if (f & FLAG_ALIVE) == 0 {
                    continue;
                }
                if (f & !(FLAG_ALIVE | crate::arena::FLAG_EFFECTIVE_HIDDEN)) != 0 {
                    return Err(SnapshotError::Corrupt("unknown entry flag"));
                }
                local_live += 1;
                let p = parent[i];
                if p != NONE && (p as usize >= slot_count || (flags[p as usize] & FLAG_ALIVE) == 0)
                {
                    return Err(SnapshotError::Corrupt("parent points to invalid/dead slot"));
                }
                let eid = ext_id[i];
                if eid != EXT_OVERFLOW && (eid as usize) >= ext_count {
                    return Err(SnapshotError::Corrupt("ext_id out of range"));
                }
                validate_arena_str(folded_arena, folded_off[i])?;
                if display_off[i] != NONE {
                    validate_arena_str(display_arena, display_off[i])?;
                }
            }
            Ok(local_live)
        })
        .try_reduce(|| 0usize, |a, b| Ok(a + b));

    if counted_live? != live_count {
        return Err(SnapshotError::Corrupt("live_count mismatch"));
    }

    let mut state = vec![0u8; slot_count];
    let mut path = Vec::new();
    for i in 0..slot_count {
        if state[i] != 0 || (flags[i] & FLAG_ALIVE) == 0 {
            continue;
        }
        let mut cur = i as EntryId;
        while cur != NONE {
            let ci = cur as usize;
            if state[ci] == 2 {
                break;
            }
            if state[ci] == 1 {
                return Err(SnapshotError::Corrupt("cycle in parent pointers"));
            }
            state[ci] = 1;
            path.push(ci);
            cur = parent[ci];
        }
        for &node in &path {
            state[node] = 2;
        }
        let mut inherited_hidden =
            cur != NONE && (flags[cur as usize] & crate::arena::FLAG_EFFECTIVE_HIDDEN) != 0;
        for &node in path.iter().rev() {
            inherited_hidden = inherited_hidden || is_own_hidden(attrs[node]);
            let stored = (flags[node] & crate::arena::FLAG_EFFECTIVE_HIDDEN) != 0;
            if stored != inherited_hidden {
                return Err(SnapshotError::Corrupt("inconsistent effective hidden flag"));
            }
        }
        path.clear();
    }

    Ok(())
}

#[inline]
fn validate_arena_str(arena: &[u8], off: u32) -> Result<(), SnapshotError> {
    let i = off as usize;
    if i >= arena.len() {
        return Err(SnapshotError::Corrupt("arena offset out of bounds"));
    }
    let first = arena[i];
    let (hdr, len) = if first < 255 {
        (1usize, first as usize)
    } else {
        if i + 3 > arena.len() {
            return Err(SnapshotError::Corrupt("arena extended length truncated"));
        }
        (
            3usize,
            u16::from_le_bytes([arena[i + 1], arena[i + 2]]) as usize,
        )
    };
    let end = i
        .checked_add(hdr)
        .and_then(|v| v.checked_add(len))
        .ok_or(SnapshotError::Corrupt("arena length overflow"))?;
    if end > arena.len() {
        return Err(SnapshotError::Corrupt("arena string extends past end"));
    }
    std::str::from_utf8(&arena[i + hdr..end])
        .map_err(|_| SnapshotError::Corrupt("invalid UTF-8 in arena"))?;
    Ok(())
}

fn write_bytes_chunked(
    w: &mut impl Write,
    hasher: &mut FastHash64,
    bytes: &[u8],
) -> io::Result<()> {
    hasher.update(bytes);
    w.write_all(bytes)
}

fn write_le_slice<T: Copy, const N: usize>(
    w: &mut impl Write,
    hasher: &mut FastHash64,
    slice: &[T],
    to_le: fn(T) -> [u8; N],
    scratch: &mut [u8],
) -> io::Result<()> {
    let per_chunk = scratch.len() / N;
    for chunk in slice.chunks(per_chunk) {
        let byte_len = chunk.len() * N;
        let (dst_chunks, _) = scratch[..byte_len].as_chunks_mut::<N>();
        for (dst, &val) in dst_chunks.iter_mut().zip(chunk) {
            *dst = to_le(val);
        }
        hasher.update(&scratch[..byte_len]);
        w.write_all(&scratch[..byte_len])?;
    }
    Ok(())
}

fn read_exact_or_truncated(r: &mut impl Read, mut buf: &mut [u8]) -> Result<(), SnapshotError> {
    while !buf.is_empty() {
        match r.read(buf) {
            Ok(0) => return Err(SnapshotError::Truncated),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(SnapshotError::from(e)),
        }
    }
    Ok(())
}

fn read_bytes_bounded(
    r: &mut impl Read,
    hasher: &mut FastHash64,
    total_bytes: usize,
) -> Result<Vec<u8>, SnapshotError> {
    let mut out = Vec::with_capacity(total_bytes.min(MAX_READ_CHUNK_BYTES));
    let mut buf = vec![0u8; total_bytes.min(MAX_READ_CHUNK_BYTES)];
    let mut remaining = total_bytes;
    while remaining > 0 {
        let take = remaining.min(buf.len());
        read_exact_or_truncated(r, &mut buf[..take])?;
        hasher.update(&buf[..take]);
        reserve_arena(&mut out, take);
        out.extend_from_slice(&buf[..take]);
        remaining -= take;
    }
    Ok(out)
}

fn read_le_vec_bounded<T, const N: usize>(
    r: &mut impl Read,
    hasher: &mut FastHash64,
    count: usize,
    from_le: fn([u8; N]) -> T,
) -> Result<Vec<T>, SnapshotError> {
    let max_elems = MAX_READ_CHUNK_BYTES / N;
    let mut out = Vec::with_capacity(count.min(max_elems));
    let mut buf = vec![0u8; count.min(max_elems) * N];
    let mut remaining = count;
    while remaining > 0 {
        let take_elems = remaining.min(max_elems);
        let take_bytes = take_elems * N;
        read_exact_or_truncated(r, &mut buf[..take_bytes])?;
        hasher.update(&buf[..take_bytes]);
        let (chunks, _) = buf[..take_bytes].as_chunks::<N>();
        out.extend(chunks.iter().map(|&c| from_le(c)));
        remaining -= take_elems;
    }
    Ok(out)
}
