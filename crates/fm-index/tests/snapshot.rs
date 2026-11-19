use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};

use fm_index::r#gen::{XorShift64, generate};
use fm_index::{CancelToken, EntryId, Index, SearchOptions, SnapshotError, parse_query};

#[test]
fn snapshot_roundtrip_100k() {
    let mut orig = generate(100_000, 0x7777_8888);
    orig.set_volume_serial(0xDEAD_BEEF);

    let mut buf = Vec::new();
    orig.save(&mut buf).expect("save 100k");

    let loaded = Index::load(Cursor::new(&buf), Some(0xDEAD_BEEF)).expect("load 100k");
    assert_eq!(loaded.len(), orig.len());
    assert_eq!(loaded.volume_serial(), 0xDEAD_BEEF);

    for raw_id in 0..orig.len() as EntryId {
        assert_eq!(loaded.name(raw_id), orig.name(raw_id));
        assert_eq!(loaded.meta(raw_id), orig.meta(raw_id));
        assert_eq!(loaded.is_dir(raw_id), orig.is_dir(raw_id));
        let p = orig.path(raw_id);
        assert_eq!(loaded.path(raw_id), p);
        assert_eq!(loaded.lookup(&p), Some(raw_id));
    }

    let cancel = CancelToken::new();
    let opts = SearchOptions::default();
    for q_str in ["smlouva", "faktura ext:pdf", "folder:projekty", "žluťoučký"] {
        let q = parse_query(q_str);
        let r1 = orig.search(&q, &opts, &cancel).unwrap();
        let r2 = loaded.search(&q, &opts, &cancel).unwrap();
        assert_eq!(r1.total_matches, r2.total_matches);
        assert_eq!(r1.ids, r2.ids);
    }
}

#[test]
fn snapshot_rejects_bad_headers_truncations_and_bitflips_without_panic() {
    let mut idx = generate(2_000, 0x1234_5678);
    idx.set_volume_serial(0xABCD_1234);
    let mut good = Vec::new();
    idx.save(&mut good).expect("save");

    // Wrong magic
    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert!(matches!(
        Index::load(Cursor::new(&bad_magic), None),
        Err(SnapshotError::BadMagic)
    ));

    // Wrong version
    let mut bad_ver = good.clone();
    bad_ver[4..8].copy_from_slice(&99u32.to_le_bytes());
    assert!(matches!(
        Index::load(Cursor::new(&bad_ver), None),
        Err(SnapshotError::UnsupportedVersion(99))
    ));

    // Wrong volume serial
    assert!(matches!(
        Index::load(Cursor::new(&good), Some(0x1111_2222)),
        Err(SnapshotError::VolumeSerialMismatch {
            expected: 0x1111_2222,
            actual: 0xABCD_1234
        })
    ));

    // Forged giant slot_count + body_len on tiny input must fail cleanly without huge allocation
    let mut forged = good[..48].to_vec();
    forged[12..16].copy_from_slice(&2_000_000_000u32.to_le_bytes());
    let forged_body_len: u64 = 2_000_000_000u64 * 35
        + u32::from_le_bytes([forged[24], forged[25], forged[26], forged[27]]) as u64
        + u32::from_le_bytes([forged[28], forged[29], forged[30], forged[31]]) as u64
        + u32::from_le_bytes([forged[32], forged[33], forged[34], forged[35]]) as u64;
    forged[40..48].copy_from_slice(&forged_body_len.to_le_bytes());
    let res = catch_unwind(AssertUnwindSafe(|| Index::load(Cursor::new(&forged), None)));
    assert!(res.is_ok(), "must not panic on forged header");
    assert!(res.unwrap().is_err());

    let mut rng = XorShift64::new(0xfeed_face_babe);

    // 50 random truncations
    for _ in 0..50 {
        let cut = rng.gen_range(good.len());
        let res = catch_unwind(AssertUnwindSafe(|| {
            Index::load(Cursor::new(&good[..cut]), Some(0xABCD_1234))
        }));
        let inner = res.expect("truncation must never panic");
        assert!(inner.is_err(), "truncation at {cut} must return Err");
    }

    // 50 random byte flips
    for _ in 0..50 {
        let mut flipped = good.clone();
        let pos = rng.gen_range(flipped.len());
        let mask = 1u8 << (rng.next_u64() % 8);
        flipped[pos] ^= mask;
        let res = catch_unwind(AssertUnwindSafe(|| {
            Index::load(Cursor::new(&flipped), Some(0xABCD_1234))
        }));
        let inner = res.expect("byte flip must never panic");
        assert!(inner.is_err(), "byte flip at {pos} must return Err");
    }
}

#[test]
fn snapshot_roundtrip_dirty_non_compact() {
    let mut orig = generate(5_000, 0x3344_5566);
    orig.set_volume_serial(0x1234_ABCD);
    // Perform mutations so dead slots and out-of-order arena bytes exist (non-compact mode).
    for id in (10..500u32).step_by(3) {
        if orig.is_alive(id) {
            orig.rename(id, &format!("renamed_extra_long_{id}.txt"));
        }
    }
    for id in (11..500u32).step_by(7) {
        if orig.is_alive(id) && orig.is_dir(id) {
            orig.remove(id);
        }
    }

    let mut buf = Vec::new();
    orig.save(&mut buf).expect("save dirty");
    let loaded = Index::load(Cursor::new(&buf), Some(0x1234_ABCD)).expect("load dirty");
    assert_eq!(loaded.len(), orig.len());
    let cancel = CancelToken::new();
    let opts = SearchOptions::default();
    let r1 = orig
        .search(&parse_query("renamed"), &opts, &cancel)
        .unwrap();
    let r2 = loaded
        .search(&parse_query("renamed"), &opts, &cancel)
        .unwrap();
    assert_eq!(r1.total_matches, r2.total_matches);
    assert_eq!(r1.ids, r2.ids);
}
