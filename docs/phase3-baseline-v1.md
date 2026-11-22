# Phase 3.0 — Real-Corpus Baseline (v1, frozen)

Frozen **2026-11-22**. This file is the comparison anchor for Step 3.1: the v2
binary/mmap engine must be measured against these numbers, not against a fresh
recollection of them.

## Provenance

| | |
|---|---|
| Repo | `D:\DEV\nDB` |
| Base commit | `7689caa93b11e93c90836f383a9b6702014f543f` (`chore(serve): add ndb-serve.example.json template and gitignore local config`) |
| Benchmark tree | working-tree change on top of that commit: `benches/phase3_corpus_bench.rs` + the `[[example]]` stanza in `Cargo.toml` (not committed at freeze time) |
| Toolchain | `rustc 1.93.1 (01f6ddf75 2026-02-11)`, `cargo 1.93.1`, release profile (`lto = true`, `codegen-units = 1`, `panic = "abort"`) |
| CPU | AMD Ryzen 9 5950X — 16 cores / 32 threads, 3.4 GHz |
| RAM | 128 GB DDR4 |
| OS | Windows 11 Pro 10.0.26300 (x64) |
| Storage | journals written to `C:` (Samsung 960 EVO 500 GB NVMe, `tempfile::tempdir()`); live corpora read from `D:` (KIOXIA EXCERIA PLUS G3 NVMe) |
| New Cargo dependencies | none — `std` + existing `serde_json` + `tempfile` (dev-dependency already present) |

Reproduce:

```
cargo run --release --example phase3_corpus_bench --manifest-path D:\DEV\nDB\Cargo.toml
```

`NDB_BENCH_MCP_DATA` overrides the corpus root (default `D:\DEV\mcp_server\data`).
A single corpus can be measured alone with `--corpus <name>` — that is what the
parent process does internally, one child per corpus.

## What is measured

Each corpus is **copied** into a work root owned by the parent process
(`tempfile::tempdir()`, removed after the last child exits); nothing under
`D:\DEV\mcp_server\data` is opened for writing. Each corpus then runs in its own
fresh process, so `RSS delta` is that corpus's own cost rather than a leftover of
the previous one. `--corpus <name> --workdir <path>` measures one corpus directly.

| Column | Definition |
|---|---|
| journal MB | on-disk `data.jsonl` after the copy (v1 JSONL, uncompacted as found) |
| docs | live documents after replay (`Database::len()`) |
| open ms | `DbRegistry::mount_with` → `Database::open` + full journal replay |
| index ms | `create_index("id")`, `create_index("category")`, `create_btree_index("score")`, plus the corpus's own eq/range index (named in the detail block) |
| RSS Δ MB | `K32GetProcessMemoryInfo` working-set delta, baseline taken immediately before `open`, end taken after the whole run |
| get | 1,000 random `Database::get` calls on the local handle (deterministic seeded order) |
| mget20 | 100 rounds of `POST /v2/dbs/<db>/docs/mget` with 20 ids over **one** HTTP/1.1 keep-alive connection to a live `ndb serve` (`serve_multi_in_background`, 32 workers, `TCP_NODELAY`) |
| eq | `query_projected` on a hash-indexed field, value = the most frequent value in a 400-doc sample |
| range | `query_projected` on a BTree-indexed field, bounds = the 25th…75th percentile of a 400-doc sample |
| scan | `query_projected` on an **unindexed** field + `sort_by` desc + `limit 20` |
| writers | 8 threads × (25 `PUT` + 25 `PATCH`) on the same DB over 8 keep-alive connections, `Persistence::Immediate` (one fsync per op); throughput = 400 ops / wall clock of the slowest thread |
| compact | `Database::compact()` wall clock, and the file size after the rewrite |

Rounds for `eq` / `range` / `scan` are derived from a warm-up run against a ~2 s
budget (5…200 rounds), so a slow scan stays bounded while a fast indexed lookup
still gets enough samples for a stable p95. `n` in each detail line is the
**pre-pagination match count** — a 39 ms `$eq` means 16,807 documents were
materialized, not that the index lookup took 39 ms.

## Frozen result table

| corpus | journal MB | docs | open ms | index ms | RSS Δ MB | get p50/p95 µs | mget20 p50/p95 µs | eq p50/p95 µs | range p50/p95 µs | scan p50/p95 ms | writers ops/s | w p50/p95/p99 ms | compact ms | after MB |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| memories | 8.83 | 5078 | 101.7 | 6.9 | +67.0 | 1.7 / 2.3 | 122.0 / 156.3 | 200.9 / 205.9 | 6276.2 / 8064.7 | 16.827 / 20.239 | 746.2 | 9.98 / 12.79 / 20.62 | 69.8 | 6.21 |
| notepad | 0.12 | 83 | 9.0 | 0.1 | +1.8 | 0.5 / 1.3 | 117.4 / 163.2 | 37.8 / 39.2 | 4.2 / 4.3 | 0.096 / 0.097 | 799.8 | 9.79 / 11.96 / 12.29 | 6.0 | 0.07 |
| codereview | 0.38 | 203 | 30.2 | 0.2 | +2.8 | 1.2 / 1.7 | 169.1 / 226.0 | 225.8 / 251.5 | 179.5 / 208.8 | 0.541 / 0.558 | 697.3 | 10.12 / 18.16 / 27.29 | 7.4 | 0.39 |
| synthetic-100k | 34.77 | 100000 | 550.9 | 273.1 | +321.8 | 1.8 / 2.4 | 110.9 / 151.9 | 39086.7 / 40667.9 | 112113.9 / 121828.7 | 112.042 / 117.391 | 741.9 | 9.97 / 13.27 / 26.25 | 731.7 | 33.54 |

## Per-corpus detail (verbatim run output)

### memories — `D:\DEV\mcp_server\data\memories\data.jsonl`

```
journal                 8.8 MB   docs    5078
cold boot             101.7 ms   (Database::open + journal replay)
index build             6.9 ms   (id hash, category hash, score btree, confidence btree)
RSS delta             +67.0 MB   (final working set 83.5 MB)
get                     1.7 µs p50 /      2.3 µs p95   (1000 random lookups)
mget(20) HTTP        122.0 µs p50 /    156.3 µs p95   (100 keep-alive rounds)
query $eq             200.9 µs p50 /    205.9 µs p95   (category = "bug", n=254, 200 rounds)
query range          6276.2 µs p50 /   8064.7 µs p95   (confidence in [0.9000, 0.9500], n=3474, 200 rounds)
scan+sort+limit20    16.827 ms p50 /   20.239 ms p95   (embedStatus = "embedded", sort timestamp desc, n=5072, 95 rounds)
writers Immediate     746.2 ops/s   p50 9.98 ms / p95 12.79 ms / p99 20.62 ms   (8 × 25 PUT + 25 PATCH)
compact                69.8 ms   8.83 MB → 6.21 MB
```

The journal is 31,169 physical lines (appends + set/patch records) collapsing to
5,078 live documents — replay, not the file size, is what cold boot measures.

### notepad — `D:\DEV\mcp_server\data\notepad\data.jsonl`

```
journal                 0.1 MB   docs      83
cold boot               9.0 ms   (Database::open + journal replay)
index build             0.1 ms   (id hash, category hash, score btree, type hash, when btree)
RSS delta              +1.8 MB   (final working set 11.6 MB)
get                     0.5 µs p50 /      1.3 µs p95   (1000 random lookups)
mget(20) HTTP        117.4 µs p50 /    163.2 µs p95   (100 keep-alive rounds)
query $eq              37.8 µs p50 /     39.2 µs p95   (type = "task", n=38, 200 rounds)
query range             4.2 µs p50 /      4.3 µs p95   (when in [2026-11-16, 2026-12-19], n=4, 200 rounds)
scan+sort+limit20     0.096 ms p50 /    0.097 ms p95   (owner = "david", sort updatedAt desc, n=59, 200 rounds)
writers Immediate     799.8 ops/s   p50 9.79 ms / p95 11.96 ms / p99 12.29 ms   (8 × 25 PUT + 25 PATCH)
compact                 6.0 ms   0.12 MB → 0.07 MB
```

83 live documents: the file is mostly tombstones and patch records. This corpus
is below the size where the v1/v2 storage difference can matter — it is kept as a
correctness/no-regression reference.

### codereview — `D:\DEV\mcp_server\data\codereview\db\data.jsonl`

```
journal                 0.4 MB   docs     203
cold boot              30.2 ms   (Database::open + journal replay)
index build             0.2 ms   (id hash, category hash, score btree, class hash, createdAt btree)
RSS delta              +2.8 MB   (final working set 17.0 MB)
get                     1.2 µs p50 /      1.7 µs p95   (1000 random lookups)
mget(20) HTTP        169.1 µs p50 /    226.0 µs p95   (100 keep-alive rounds)
query $eq             225.8 µs p50 /    251.5 µs p95   (class = "free-win", n=145, 200 rounds)
query range           179.5 µs p50 /    208.8 µs p95   (createdAt in [2026-11-13T21:37:15.012Z, 2026-11-14T05:29:05.664Z], n=104, 200 rounds)
scan+sort+limit20     0.541 ms p50 /    0.558 ms p95   (status = "new", sort createdAt desc, n=192, 200 rounds)
writers Immediate     697.3 ops/s   p50 10.12 ms / p95 18.16 ms / p99 27.29 ms   (8 × 25 PUT + 25 PATCH)
compact                 7.4 ms   0.38 MB → 0.39 MB
```

Only 203 live documents, but they carry large `evidence` bodies — `$eq` here costs
more than `memories` with 254 matches because every match is deep-cloned.

### synthetic-100k

100,000 deterministic ~350–400-byte JSON docs (`_id`, `id`, `category`, `score`,
`status`, `tags[]`, `body`, `_seq`) written straight to a JSONL journal with a
`_meta` header, then replayed like any other corpus.

```
journal                34.8 MB   docs  100000
cold boot             550.9 ms   (Database::open + journal replay)
index build           273.1 ms   (id hash, category hash, score btree)
RSS delta            +321.8 MB   (final working set 374.0 MB)
get                     1.8 µs p50 /      2.4 µs p95   (1000 random lookups)
mget(20) HTTP        110.9 µs p50 /    151.9 µs p95   (100 keep-alive rounds)
query $eq           39086.7 µs p50 /  40667.9 µs p95   (category = "memory", n=16807, 47 rounds)
query range        112113.9 µs p50 / 121828.7 µs p95   (score in [255.3800, 731.6300], n=48092, 16 rounds)
scan+sort+limit20   112.042 ms p50 /  117.391 ms p95   (status = "archived", sort score desc, n=25094, 17 rounds)
writers Immediate     741.9 ops/s   p50 9.97 ms / p95 13.27 ms / p99 26.25 ms   (8 × 25 PUT + 25 PATCH)
compact               731.7 ms   34.77 MB → 33.54 MB
```

This is the corpus that matters for Phase 3: a 34.8 MB journal costs **321.8 MB**
of process working set (≈9.3× the journal) and **551 ms** to replay, and every
`query_projected` call deep-clones its matches (16,807 docs → 39 ms for an
indexed `$eq`; 48,092 docs → 112 ms for an indexed range). Those three numbers are
the ones the v2 engine must move.

## Phase 3 exit-gate targets derived from this baseline

- **RSS ≥ 3× lower** → synthetic-100k must land at **≤ 107 MB** working set
  (v1: +321.8 MB), memories at **≤ 22.3 MB** (v1: +67.0 MB).
- **Cold boot ≥ 5× faster** → synthetic-100k **≤ 110 ms** (v1: 551 ms), memories
  **≤ 20.3 ms** (v1: 101.7 ms).
- **No p95 query regression** → indexed `$eq`/range p95 must not exceed the v1
  numbers for the same `n`; the full-document clone cost is expected to drop with
  mmap-backed docs.
- **Higher concurrent-writer throughput under `Immediate`** → v1 sits at
  **697–800 ops/s** with p95 12–18 ms because every op is its own fsync (P6).
  Group commit has to beat that without raising p95.

## Notes

- Numbers vary run to run: process scheduling, allocator state and the OS page
  cache. Observed spread across five full runs on this machine is ≈ ±15 % on the
  µs-scale rows and ≈ ±3 % on boot/compact rows. The multi-ms rows (indexed range
  and the scan on 100k docs) moved by < 5 %.
- The journal as found is **uncompacted** — `compact()` at the end of each run
  rewrites it (memories 8.83 → 6.21 MB, synthetic 34.77 → 33.54 MB), so the
  `after MB` column is the post-compaction size of that copy only.
- `RSS Δ` is a working set delta, not retained heap: for the tiny corpora it is
  dominated by allocator granularity, which is why notepad shows +1.8 MB for 83
  documents.
- The benchmark is a plain `[[example]]`, not a Criterion `[[bench]]` — P10 in the
  audit is about the *Criterion* target being misconfigured; this probe is
  intentionally a deterministic, human-readable release-mode run.
