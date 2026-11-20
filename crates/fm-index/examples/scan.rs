use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use fm_index::win::walker::{WalkOptions, walk_root};
use fm_index::{CancelToken, SearchOptions, parse_query};

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "C:\\".to_owned());

    let cancel = AtomicBool::new(false);

    let t_start = Instant::now();
    let mut last_log = Instant::now();

    let (idx, stats) = walk_root(&root, WalkOptions::default(), &cancel, |entries| {
        if last_log.elapsed().as_millis() >= 500 {
            eprintln!(
                "... {entries} entries ({:.1} s)",
                t_start.elapsed().as_secs_f64()
            );
            last_log = Instant::now();
        }
    })
    .expect("walk not cancelled");
    let _ = cancel.load(Ordering::Relaxed);

    let mem_bytes = idx.memory_bytes();
    let b_per_entry = if idx.is_empty() {
        0.0
    } else {
        mem_bytes as f64 / idx.len() as f64
    };

    println!("Root:        {root}");
    println!("Entries:     {}", idx.len());
    println!("Dirs:        {}", stats.dirs);
    println!("Files:       {}", stats.files);
    println!("Denied:      {}", stats.denied);
    println!("Errors:      {}", stats.errors);
    println!(
        "Cold time:   {} ms ({:.2} s)",
        stats.millis,
        stats.millis as f64 / 1000.0
    );
    println!(
        "Memory:      {:.2} MiB ({b_per_entry:.2} B/entry)",
        mem_bytes as f64 / (1024.0 * 1024.0)
    );

    let tok = CancelToken::new();
    let opts = SearchOptions {
        include_hidden: false,
        limit: 100,
    };
    let queries = [
        "pdf",
        "faktura 2024",
        "ext:jpg",
        "path:windows system32 dll",
        "readme",
    ];
    println!("\nSample queries:");
    for q_str in queries {
        let q = parse_query(q_str);
        let t0 = Instant::now();
        let res = idx.search(&q, &opts, &tok).expect("not cancelled");
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!(
            "  {q_str:<28} -> {:>7} matches in {ms:.2} ms",
            res.total_matches
        );
    }

    // Folder sizes (list view): the root and each of its folders, timings only.
    if let Some(rid) = idx.lookup(&root) {
        let t0 = Instant::now();
        let (_, files) = idx.subtree_size(rid);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!("
Folder size of the root: {files} files summed in {ms:.1} ms");
        let tops: Vec<_> = idx.iter_ids().filter(|&id| idx.parent(id) == Some(rid) && idx.is_dir(id)).collect();
        let t0 = Instant::now();
        for &id in &tops {
            let _ = idx.subtree_size(id);
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!("Folder sizes of its {} folders (a list of the root): {ms:.1} ms", tops.len());
    }
}
