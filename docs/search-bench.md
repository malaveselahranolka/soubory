# Search benchmarks

## Whole C: drive, 2026-11-20 (after the merged review fixes, `26ad376`)

`cargo run --release -p fm-index --example scan -- C:\` (prints counts only). Release build; the file system cache was probably warm from earlier walks that night, so the walk time is a lower bound for a true cold start.

| Metric | Measured | Budget |
|---|---|---|
| Entries | 4 218 854 (466 720 folders, 3 752 134 files) | — |
| Denied folders / errors | 121 / 0 | — |
| Full walk | 30.3 s | ≤ 90 s, UI usable meanwhile |
| Index memory | 384.7 MiB, 95.6 B/entry | ≤ 100 B/entry |
| `pdf` | 22 185 matches, 8.0 ms | p99 < 50 ms |
| `faktura 2024` | 1 match, 6.0 ms | |
| `ext:jpg` | 31 813 matches, 3.3 ms | |
| `readme` | 39 319 matches, 7.3 ms | |
| `path:windows system32 dll` | 0 matches, 6.4 ms (correct: only `windows` is a path term; the file name must contain both `system32` and `dll`) | |

Earlier run (`d97f340`, before the fixes): 4.2 M entries, 71 s walk, queries 3–10 ms.

## The app, after the merge (`4c4ed0d`, release)

A normal `fm.exe` (not `--new-instance`), started twice in a row, closed with WM_CLOSE:

| | Run 1 (snapshot from the old build) | Run 2 (snapshot from run 1) |
|---|---|---|
| Startup re-walk of C: (background priority) | ~20 s, 119 CPU-s | ~20 s, 116 CPU-s |
| Working set during the walk | 242 → 440 MB (the old snapshot was not usable; rebuilt) | 601 → 817 MB peak (snapshot loaded + fresh walk in parallel) |
| Idle afterwards | 0–3 % of one core (0.2–0.3 CPU-s per 10 s now and then) | 0.0 % over 50 s |
| Working set idle | 440 MB, 29 threads | 451 MB, 29 threads |
| Snapshot | rewritten at exit (294 MB) | rewritten at exit |
| `fm.log` index errors | none | none |

Exit took under 100 ms both times. Before the merge (same night): 40 s walk, 144 CPU-s, 449 MB idle.

## Synthetic suite (2 M entries)

`cargo test -p fm-index --release --test perf -- --ignored --nocapture`, after the fixes:

| Metric | Measured | Budget |
|---|---|---|
| Memory | 90.34 B/entry | ≤ 100 |
| Query p50 / p99 | 6.07 / 10.96 ms | < 15 / < 50 ms |
| Refine p50 / p99 | 1.49 / 2.67 ms | p50 < 5 ms |
| Snapshot save / load / size | 88 ms / 432 ms / 123.4 MB | ≤ 1 s / ≤ 1 s / ≤ 150 MB |
| Insert, rename, remove p99 | ≤ 0.001 ms | ≤ 1 ms |
| Cancel latency | 0.64 ms | < 20 ms |
| Merge: one folder, 40k kept + 40k new | 21.7 ms (was 3.6 s) | < 200 ms |
| Merge: one folder, 40k removed + 40k new | 23.8 ms (was 7.2 s) | < 200 ms |
