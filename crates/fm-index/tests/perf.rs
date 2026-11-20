use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fm_fs::attrs::ARCHIVE;
use fm_index::r#gen::{XorShift64, generate, sample_queries};
use fm_index::{CancelToken, EntryId, Index, Meta, SearchOptions, parse_query};

fn percentile_us(sorted_us: &[u128], pct: f64) -> f64 {
    if sorted_us.is_empty() {
        return 0.0;
    }
    let idx = (((sorted_us.len() - 1) as f64) * pct).round() as usize;
    sorted_us[idx] as f64 / 1000.0
}

#[test]
#[ignore]
fn perf_2m_suite() {
    let t_gen = Instant::now();
    let mut idx = generate(2_000_000, 0x2026_1119);
    idx.set_volume_serial(0xCAFE_BABE);
    let gen_ms = t_gen.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(idx.len(), 2_000_000);

    let mem_bytes = idx.memory_bytes();
    let bytes_per_entry = mem_bytes as f64 / idx.len() as f64;

    // 1. 300 realistic 1-3-term queries
    let queries: Vec<_> = sample_queries(42, 300)
        .into_iter()
        .map(|s| parse_query(&s))
        .collect();
    let cancel = CancelToken::new();
    let opts = SearchOptions {
        include_hidden: false,
        limit: 1000,
    };

    // Warmup
    for q in &queries[..5] {
        let _ = idx.search(q, &opts, &cancel);
    }

    let mut q_us = Vec::with_capacity(queries.len());
    for q in &queries {
        let t0 = Instant::now();
        let res = idx.search(q, &opts, &cancel).expect("not cancelled");
        let dt = t0.elapsed().as_micros();
        std::hint::black_box(res.total_matches);
        q_us.push(dt);
    }
    q_us.sort_unstable();
    let q_p50_ms = percentile_us(&q_us, 0.50);
    let q_p99_ms = percentile_us(&q_us, 0.99);

    // 2. Refine queries (e.g. "sm" -> "sml", "fa" -> "fak", etc.)
    let refine_pairs = [
        ("sm", "sml"),
        ("fa", "fak"),
        ("po", "poz"),
        ("re", "rep"),
        ("in", "inv"),
        ("bu", "bud"),
        ("no", "not"),
        ("se", "sea"),
        ("žl", "žlu"),
        ("do", "dov"),
    ];
    let mut refine_us = Vec::with_capacity(100);
    for round in 0..10 {
        for &(a, b) in &refine_pairs {
            let qa = parse_query(a);
            let qb = parse_query(&format!("{b} {}", round + 1));
            let prev = idx.search(&qa, &opts, &cancel).unwrap();
            let t0 = Instant::now();
            let refined = idx.refine(&prev, &qb, &opts, &cancel).unwrap();
            let dt = t0.elapsed().as_micros();
            std::hint::black_box(refined.total_matches);
            refine_us.push(dt);
        }
    }
    refine_us.sort_unstable();
    let refine_p50_ms = percentile_us(&refine_us, 0.50);
    let refine_p99_ms = percentile_us(&refine_us, 0.99);

    // 3. Snapshot save & load on disk
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let snap_path = std::env::temp_dir().join(format!("fm_index_perf_{nanos}.fmix"));

    let t_save = Instant::now();
    {
        let file = File::create(&snap_path).expect("create snap");
        let mut bw = BufWriter::with_capacity(1024 * 1024, file);
        idx.save(&mut bw).expect("save snap");
    }
    let save_ms = t_save.elapsed().as_secs_f64() * 1000.0;
    let snap_bytes = fs::metadata(&snap_path).expect("stat snap").len();
    let snap_mb = snap_bytes as f64 / (1024.0 * 1024.0);

    let t_load = Instant::now();
    {
        let file = File::open(&snap_path).expect("open snap");
        let mut br = BufReader::with_capacity(1024 * 1024, file);
        let loaded = Index::load(&mut br, Some(0xCAFE_BABE)).expect("load snap");
        assert_eq!(loaded.len(), 2_000_000);
    }
    let load_ms = t_load.elapsed().as_secs_f64() * 1000.0;
    let _ = fs::remove_file(&snap_path);

    // 4. Live updates: 10k inserts, 10k renames, 10k removes
    let mut rng = XorShift64::new(0x9988_7766);
    let mut dirs = Vec::with_capacity(10_000);
    for id in 0..200_000u32 {
        if idx.is_dir(id) {
            dirs.push(id);
            if dirs.len() == 10_000 {
                break;
            }
        }
    }
    let mut inserted_ids: Vec<EntryId> = Vec::with_capacity(10_000);
    let mut insert_us = Vec::with_capacity(10_000);
    for i in 0..10_000 {
        let parent = dirs[rng.gen_range(dirs.len())];
        let name = format!("LiveNovýSoubor_{i}.txt");
        let meta = Meta {
            attrs: ARCHIVE,
            size: 1234,
            mtime: 133_600_000_000_000_000,
            file_id: 0,
        };
        let t0 = Instant::now();
        let id = idx.insert(parent, &name, meta);
        insert_us.push(t0.elapsed().as_micros());
        inserted_ids.push(id);
    }
    insert_us.sort_unstable();
    let insert_p99_ms = percentile_us(&insert_us, 0.99);

    let mut rename_us = Vec::with_capacity(10_000);
    for (i, &id) in inserted_ids.iter().enumerate() {
        let new_name = format!("Přejmenovaný_{i}.pdf");
        let t0 = Instant::now();
        idx.rename(id, &new_name);
        rename_us.push(t0.elapsed().as_micros());
    }
    rename_us.sort_unstable();
    let rename_p99_ms = percentile_us(&rename_us, 0.99);

    let mut remove_us = Vec::with_capacity(10_000);
    for &id in &inserted_ids {
        let t0 = Instant::now();
        idx.remove(id);
        remove_us.push(t0.elapsed().as_micros());
    }
    remove_us.sort_unstable();
    let remove_p99_ms = percentile_us(&remove_us, 0.99);

    // 5. Cancellation latency (< 20 ms after cancel on 2M entries)
    let cancel_tok = CancelToken::new();
    let cancel_trigger = cancel_tok.clone();
    let heavy_q = parse_query("e");
    let heavy_opts = SearchOptions {
        include_hidden: true,
        limit: 500_000,
    };
    let cancel_elapsed_ms = std::thread::scope(|s| {
        let handle = s.spawn(|| idx.search(&heavy_q, &heavy_opts, &cancel_tok));
        let spin_start = Instant::now();
        while spin_start.elapsed() < Duration::from_micros(500) {
            std::hint::spin_loop();
        }
        let t_cancel = Instant::now();
        cancel_trigger.cancel();
        let res = handle.join().expect("search thread");
        let elapsed = t_cancel.elapsed().as_secs_f64() * 1000.0;
        assert!(res.is_none(), "cancelled search must return None");
        elapsed
    });

    println!("\n| Metric | Measured | Budget | Status |");
    println!("|---|---|---|---|");
    println!("| Tree generation (2M entries) | {gen_ms:.1} ms | - | info |");
    println!(
        "| Memory per entry (capacities) | {bytes_per_entry:.2} B/entry | <= 100 B/entry | {} |",
        if bytes_per_entry <= 100.0 {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!(
        "| Query p50 (300 queries) | {q_p50_ms:.2} ms | < 15 ms | {} |",
        if q_p50_ms < 15.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Query p99 (300 queries) | {q_p99_ms:.2} ms | < 50 ms | {} |",
        if q_p99_ms < 50.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Refine p50 / p99 | {refine_p50_ms:.2} ms / {refine_p99_ms:.2} ms | p50 < 5 ms | {} |",
        if refine_p50_ms < 5.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Snapshot save (2M entries) | {save_ms:.1} ms | <= 1000 ms | {} |",
        if save_ms <= 1000.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Snapshot load (2M entries) | {load_ms:.1} ms | <= 1000 ms | {} |",
        if load_ms <= 1000.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Snapshot file size | {snap_mb:.2} MB | <= 150 MB | {} |",
        if snap_mb <= 150.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Insert p99 (10k ops) | {insert_p99_ms:.3} ms | <= 1 ms | {} |",
        if insert_p99_ms <= 1.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Rename p99 (10k ops) | {rename_p99_ms:.3} ms | <= 1 ms | {} |",
        if rename_p99_ms <= 1.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Remove p99 (10k ops) | {remove_p99_ms:.3} ms | <= 1 ms | {} |",
        if remove_p99_ms <= 1.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "| Cancel latency (2M entries) | {cancel_elapsed_ms:.2} ms | < 20 ms | {} |",
        if cancel_elapsed_ms < 20.0 {
            "PASS"
        } else {
            "FAIL"
        }
    );

    assert!(
        bytes_per_entry <= 100.0,
        "memory {bytes_per_entry:.2} > 100 B/entry"
    );
    assert!(q_p50_ms < 15.0, "query p50 {q_p50_ms:.2} >= 15 ms");
    assert!(q_p99_ms < 50.0, "query p99 {q_p99_ms:.2} >= 50 ms");
    assert!(refine_p50_ms < 5.0, "refine p50 {refine_p50_ms:.2} >= 5 ms");
    assert!(save_ms <= 1000.0, "save {save_ms:.1} > 1000 ms");
    assert!(load_ms <= 1000.0, "load {load_ms:.1} > 1000 ms");
    assert!(snap_mb <= 150.0, "snapshot {snap_mb:.2} > 150 MB");
    assert!(insert_p99_ms <= 1.0, "insert p99 {insert_p99_ms:.3} > 1 ms");
    assert!(rename_p99_ms <= 1.0, "rename p99 {rename_p99_ms:.3} > 1 ms");
    assert!(remove_p99_ms <= 1.0, "remove p99 {remove_p99_ms:.3} > 1 ms");
    assert!(
        cancel_elapsed_ms < 20.0,
        "cancel {cancel_elapsed_ms:.2} >= 20 ms"
    );
}

/// One folder with 40k existing + 40k new children (and 40k all replaced) must merge well under
/// 200 ms in release. On 4056029 these took ~3.6 s and ~7.2 s (sibling scan per child plus a
/// per-entry O(siblings) unlink), all under the index write lock.
#[test]
#[ignore]
fn perf_merge_wide_folder() {
    use fm_fs::attrs::DIRECTORY;
    use fm_index::win::walker::merge_index;

    let m = |attrs, fid| Meta {
        attrs,
        size: 1,
        mtime: 1,
        file_id: fid,
    };
    for all_replaced in [false, true] {
        let n = 40_000u64;
        let mut live = Index::new();
        let r = live.add_root("R:\\");
        live.set_file_id(r, 1);
        let d = live.insert(r, "D", m(DIRECTORY, 2));
        for i in 0..n {
            live.insert(d, &format!("old_{i}.bin"), m(ARCHIVE, 10 + i));
        }
        let mut fresh = Index::new();
        let fr = fresh.add_root("R:\\");
        fresh.set_file_id(fr, 1);
        let fd = fresh.insert(fr, "D", m(DIRECTORY, 2));
        if !all_replaced {
            for i in 0..n {
                fresh.insert(fd, &format!("old_{i}.bin"), m(ARCHIVE, 10 + i));
            }
        }
        for i in 0..n {
            fresh.insert(fd, &format!("new_{i}.bin"), m(ARCHIVE, 10_000_000 + i));
        }
        let t0 = Instant::now();
        assert!(merge_index(&mut live, fresh));
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let expected = if all_replaced { n } else { 2 * n } as usize + 2;
        assert_eq!(live.len(), expected);
        println!(
            "| Merge 40k {} + 40k new (one folder) | {ms:.1} ms | < 200 ms | {} |",
            if all_replaced { "removed" } else { "kept" },
            if ms < 200.0 { "PASS" } else { "FAIL" }
        );
        assert!(
            ms < 200.0,
            "merge took {ms:.1} ms (all_replaced={all_replaced})"
        );
    }
}
