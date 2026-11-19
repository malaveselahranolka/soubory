# Implementation Report: `fm-fs` + `fm-index` Core

## Performance Benchmark (`cargo test -p fm-index --release -- --ignored perf --nocapture`)

| Metric | Measured | Budget | Status |
|---|---|---|---|
| Tree generation (2M entries) | 857.3 ms | - | info |
| Memory per entry (capacities) | 82.34 B/entry | <= 100 B/entry | PASS |
| Query p50 (300 queries) | 9.54 ms | < 15 ms | PASS |
| Query p99 (300 queries) | 19.98 ms | < 50 ms | PASS |
| Refine p50 / p99 | 2.37 ms / 4.55 ms | p50 < 5 ms | PASS |
| Snapshot save (2M entries) | 118.2 ms | <= 1000 ms | PASS |
| Snapshot load (2M entries) | 565.8 ms | <= 1000 ms | PASS |
| Snapshot file size | 108.14 MB | <= 150 MB | PASS |
| Insert p99 (10k ops) | 0.000 ms | <= 1 ms | PASS |
| Rename p99 (10k ops) | 0.000 ms | <= 1 ms | PASS |
| Remove p99 (10k ops) | 0.000 ms | <= 1 ms | PASS |
| Cancel latency (2M entries) | 0.82 ms | < 20 ms | PASS |