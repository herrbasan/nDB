# Phase 3A — Real-corpus verification: v2 mmap vs v3 RAM-first

Measured **2026-11-25** on the live corpora, through the Phase 0 workload set
(`benches/corpus_baseline.rs`, extended with `--engine` and the `get_raw` row).
Raw run, unedited: `docs/phase3-ram-vs-v2-run.log`.

| | |
|---|---|
| Repo | `D:\DEV\nDB` @ `6f9f8ba` + working tree (`get_raw` read surface, `select_records`, streamed HTTP reads) |
| Harness | `benches/corpus_baseline.rs` — working-tree change (`--engine v2\|ram\|both`, `get_raw` / value-path rows) |
| Toolchain | `cargo 1.93.1` release profile (`lto = true`, `codegen-units = 1`, `panic = "abort"`) |
| CPU / RAM / OS | AMD Ryzen 9 5950X (16c/32t) · 128 GB DDR4 · Windows 11 Pro 10.0.26300 |
| Storage | temp copies and WAL writes on `C:` (Samsung 960 EVO NVMe); corpora read from `D:` |
| Build target dir | `D:\DEV\ndb-phase3-target` — **`target\release\ndb.exe` is held open by the running `ndb serve`**, so the release build was redirected (see *Reproducing*) |

Corpus snapshot taken at run time (the folders are live and drift between runs):

| Corpus | Docs | Files copied | v2 on disk |
|---|---|---|---|
| `memories` | 5,103 | `manifest.json`, `base.gen_2.ndb`, `wal.gen_2.ndb` | 6.71 MB |
| `chat_herrbasan` | 911 | `manifest.json`, `meta.json`, `base.gen_98.ndb`, `wal.gen_98.ndb` | 342.06 MB (327.17 MB base + 14.89 MB WAL) |

## Method

- Both engines run **identical workloads with identical sample counts**. `memories`
  and `chat` are copied into separate `tempfile::tempdir()`s — the live folders are
  only read, `.lock` is never copied, and nothing under `D:\DEV\mcp_server\data\memories`
  or `D:\SRV\LLM-Gateway-Chat\server\data\herrbasan` is opened for writing.
- `--engine ram` migrates *its* copy with the shipped CLI:
  `ndb.exe migrate <tempdir> --ram`. That is the production migration path, and it
  is also the parity proof: it builds the v3 base, re-opens the staged folder,
  compares document count, every id, every JSON value, the lineage and every index
  declaration against the source, then publishes and repeats the comparison on the
  live folder. Exit 0 = parity verified twice.
- Every read runs warm after untimed warm-ups. `Persistence::Immediate` on v2; the
  RAM engine's commit path is always durable (WAL append + fsync before publish).
- Percentiles are nearest-rank. `memories` ids are **sorted before sampling**, so
  both engines evaluate the same `$eq` category (`bug`, 254 matches) and the same
  random-id set.
- Three document-read rows on `chat`, each measuring a different layer:
  - `get` — the in-process API: v2 parses (cached) into `Value`, v3 builds a
    `Value` from its `DocVal` tree.
  - `get_raw` — **the HTTP response path**: exact JSON bytes, no `Value` anywhere.
    v2 copies the stored bytes out of the mapping, v3 writes the `DocVal` tree into
    a pre-sized buffer (`approx_bytes`, capped at 8 MiB).
  - `get + to_vec` — the *previous* HTTP path, kept as the before/after baseline:
    materialise a `Value`, then hand it to `serde_json`.
- Concurrency rows keep the readers sampling to a **500 ms floor**
  (`READER_WINDOW_FLOOR_MS`): a v2 writer needs ~1.5 s for its 20 pushes but a RAM
  writer finishes in ~50 ms, which would leave a p99 over far too few samples. The
  v2 window already exceeds the floor, so its numbers are unchanged.
- "WAL growth" is the summed on-disk size of `wal.gen_*.ndb` — the same instrument
  on both sides.

## Side-by-side — `memories`, 5,103 docs

| Workload | n | v2 p50 | v2 p95 | v2 p99 | v3 p50 | v3 p95 | v3 p99 | p50 ratio |
|---|---|---|---|---|---|---|---|---|
| warm `get` | 5,000 | 2.80 µs | 8.40 µs | 16.30 µs | **1.70 µs** | **2.50 µs** | **3.10 µs** | **1.65× faster** |
| mget-style batch of 20 | 500 | 36.20 µs | 55.90 µs | 74.60 µs | **23.80 µs** | **29.20 µs** | **33.10 µs** | **1.52× faster** |
| indexed `$eq` category=bug (254 docs) | 200 | 346.80 µs | 362.90 µs | 383.80 µs | **336.70 µs** | **347.70 µs** | **359.60 µs** | 1.03× faster |
| unindexed scan+sort+limit(20) | 200 | 2.74 ms | 2.96 ms | 3.01 ms | **1.53 ms** | **1.57 ms** | **1.62 ms** | **1.79× faster** |
| durable `set` (Immediate) | 50 | 1.26 ms | 1.82 ms | 2.01 ms | 1.19 ms | 2.27 ms | 2.47 ms | fsync-bound, equal |
| durable `update` (Immediate) | 50 | 1.28 ms | 1.46 ms | 2.05 ms | 1.22 ms | 1.32 ms | 1.41 ms | fsync-bound, equal |

| | v2 | v3 RAM |
|---|---|---|
| copy | 5 ms | 4 ms |
| **migrate** | — | **275 ms**, parity PASS (5,103 docs) |
| open | 20 ms | 59 ms |
| RSS after open | 6.3 MB (+1.3) | 39.2 MB (+14.4) |
| resident document set | mmap, page-cache only | `live_doc_bytes` 11.56 MB + `index_bytes` 0.71 MB (1.8× the 6.71 MB base) |
| RSS after run | 26.0 MB | 40.5 MB |
| WAL after run | 0.20 MB | 0.08 MB |

## Side-by-side — `chat_herrbasan`, 911 docs / 327.11 MB of documents

| Workload | n | v2 p50 | v2 p95 | v2 p99 | v3 p50 | v3 p95 | v3 p99 | p50 ratio |
|---|---|---|---|---|---|---|---|---|
| warm `get`, largest 5 docs | 200 | **1.09 ms** | **2.00 ms** | **2.43 ms** | 1.43 ms | 2.66 ms | 2.83 ms | 1.3× SLOWER |
| warm `get_raw` (JSON bytes), largest 5 docs | 200 | **748.80 µs** | **1041.60 µs** | **1274.20 µs** | 3.50 ms | 5.44 ms | 5.48 ms | 4.7× SLOWER |
| warm `get + to_vec` (previous HTTP path), largest 5 docs | 40 | 5.10 ms | 6.83 ms | 7.02 ms | 5.23 ms | 7.08 ms | 7.34 ms | equal |
| warm `get`, random docs | 500 | **6.30 µs** | **1304.00 µs** | **2452.70 µs** | 9.20 µs | 1873.50 µs | 3664.10 µs | 1.5× SLOWER |
| durable `array_push` (5.61 MB doc) | 20 | 63.61 ms | 65.74 ms | 74.33 ms | **1.20 ms** | **4.26 ms** | **4.99 ms** | **53× faster** |
| reader `get` under writer load | 2,861 / 936 | 1.395 ms | 2.805 ms | 11.56 ms | 1.675 ms | 3.049 ms | **3.60 ms** | p99 **3.2× better** |
| writer `array_push` under load | 20 | 75.95 ms | 78.75 ms | 83.67 ms | **2.62 ms** | **3.57 ms** | **4.69 ms** | **29× faster** |

### The document-read path in detail (5.61 MB conversation)

| | v2 | v3 RAM | what it measures |
|---|---|---|---|
| `get` p50 | 1.09 ms | 1.43 ms | parse/build a `Value` |
| **`get_raw` p50** | **748.80 µs** | **3.50 ms** | exact JSON bytes, zero `Value` (the HTTP response body) |
| `get + to_vec` p50 | 5.10 ms | 5.23 ms | the *previous* HTTP path (Value, then `serde_json`) |

`get_raw` on RAM is **1.5× faster than the path it replaces** (5.23 → 3.50 ms) and
allocates no document `Value` at all. It cannot reach v2's 748 µs: v2 only copies
already-serialized bytes out of the mapping, while RAM owns a tree and must encode
it. Of that encode, most is inherent: the document is newline-heavy chat text, so
escape expansion writes a short run per line, and `serde_json` pays the same cost
(its `to_vec` on the same document is ~3.8 ms on its own). Pre-sizing the buffer
matters — with no reserve the same path measures 4.32 ms.

### Cost of one durable `array_push` on the 5.61 MB conversation

| | v2 | v3 RAM | ratio |
|---|---|---|---|
| latency p50 | 63.61 ms | **1.20 ms** | 53× |
| latency p99 | 74.33 ms | **4.99 ms** | 15× |
| WAL bytes written per op | 5,883,580 B (5.61 MB — the whole document) | **192 B (the patch)** | **30,600× less** |
| WAL total after 20 ops | 127.11 MB on disk | 3,830 B on disk | — |
| process RSS delta | **+131.9 MB** (913.7 → 1,045.5 MB) | **+0.0 MB** (563.0 → 563.0 MB) | — |

### `open`, migration and resident footprint

| | v2 | v3 RAM |
|---|---|---|
| copy | 88 ms | 85 ms |
| **migrate** | — | **8,250 ms** (≈40 MB/s), parity PASS (911 docs, 5 indexes) |
| v3 artefacts written | — | base + WAL **343,036,746 B**, v2 originals kept in `_v2_backup/` (358,673,302 B) |
| open | **113 ms** | **1,952 ms** (~17× slower) |
| RSS after open | **34.0 MB** (+27.4) | **553.6 MB** (+528.5) |
| `live_doc_bytes` / `index_bytes` | mmap + 0.23 MB index | **511.71 MB / 0.23 MB** |
| size pass (`get` + `to_vec` over every doc) | 967 ms | **545 ms** |

### Live compaction under concurrent load (4 readers + 1 writer, `compact()` in flight)

| | v2 | v3 RAM |
|---|---|---|
| `compact()` wall | 1,548 ms | 1,988 ms (1.3× slower) |
| writer window | 2,114 ms | 64 ms |
| reader `get` during compaction | n=2,497 · p50 1.672 ms · p99 **11.56 ms** · max 14.61 ms | n=3,580 · p50 1.718 ms · p99 **3.51 ms** · max 4.52 ms |
| writer `array_push` | p50 106.25 ms · p95 123.09 ms · p99 **209.36 ms** | p50 2.71 ms · p95 4.52 ms · p99 **12.73 ms** |
| RAM stats after compact | — | live_doc 511.73 MB, ring 0.16 MB, wal 0.00 MB, 60 batches, 90.5 ms fsync, 52.0 ms publish |

Both compactors are non-blocking (v2 rotates a mirror WAL, v3 freezes the doc set and
writes the base with no lock held). Neither stalls readers or the writer; RAM's
compaction is simply slower because it serializes 511 MB of `DocVal` trees while v2
copies mmap slots.

### Churn soak — 100 consecutive durable pushes, then one compaction

| | v2 | v3 RAM |
|---|---|---|
| push p50 / p95 / p99 | 61.52 / 64.27 / 66.05 ms | **1.20 / 1.39 / 1.57 ms** |
| RSS start | 777.3 MB | 599.7 MB |
| RSS peak (sampled per push) | **1,371.9 MB (+594.6 MB)** | **599.7 MB (+0.0 MB)** |
| RSS after compact | 607.0 MB (−170.3 MB) | 599.7 MB (+0.0 MB) |
| WAL before → after 100 pushes | 112.29 MB → **674.29 MB (+589,299,630 B)** | 0.00 MB → **0.02 MB (+19,260 B)** |
| WAL after compact | 0.00 MB | 0.00 MB |
| `compact()` | 1,356 ms | 1,797 ms |
| `live_doc_bytes` after compact | n/a | 511.73 MB |
| `ring_bytes` after compact | n/a | **0.00 MB** |

## What the numbers say

1. **The writer refactor's target is hit, and then some.** One durable append to a
   5.6 MB conversation went from 63.61 ms to 1.20 ms p50 (53×), the WAL write per op
   from 5.61 MB to **192 B** (30,600×), and the process RSS no longer grows at all
   (+0.0 MB vs +131.9 MB for 20 pushes; +594.6 MB over a 100-push soak). The v2 soak
   needed 1.37 GB of RSS and a 674 MB WAL to record 100 tiny appends.
2. **The writer no longer blocks readers.** Reader p99 under 20 concurrent durable
   pushes: 11.56 ms → **3.60 ms**; during a full compaction: 11.56 ms → **3.51 ms**.
3. **`memories` reads are now ahead of v2 across the board**, and the indexed query
   regression is gone (see below).
4. **The large-document HTTP read is 1.5× faster than the path it replaced**
   (5.23 → 3.50 ms) with no `Value` allocation, and it no longer serializes under
   the state read lock — selection hands back `Arc<DocRecord>` handles and encoding
   runs after the lock is released. v2 remains ~4.7× faster on that row because it
   copies pre-serialized bytes out of the mapping instead of encoding a tree.

### Regressions this run exposes (open items, not blockers)

- **Large-document `get` is 1.3× slower** (1.09 ms → 1.43 ms for 5.61 MB) and the
  random-doc p99 is 1.5× worse (2.45 → 3.66 ms). `DocVal::to_value` rebuilds the
  whole `Value` tree per call — the structural-sharing item Phase 0 named as the
  target is still open for the in-process read path.
- **`get_raw` cannot beat an mmap copy** (3.50 ms vs 748 µs). A RAM store owns a
  tree, so the HTTP body costs an encode; the only way past this is caching
  serialized bytes per record, which trades away the memory win the engine exists for.
- **Open is 17× slower and 16× more resident** on the chat corpus: 113 ms →
  1,952 ms and 34.0 MB → 553.6 MB, because the design keeps all 511.71 MB of
  documents live. That is the intended trade (a store of ~340 MB serving a few
  hundred MB of hot documents), but it is a real cost at boot and on small hosts.
- **`live_doc_bytes` runs 1.8× the source bytes** (11.56 MB for a 6.71 MB base on
  `memories`; 511.71 MB for 327.11 MB of chat JSON). Acceptable at these sizes,
  worth re-measuring at 10× the document count.
- **Migration costs ≈40 MB/s** (8.25 s for 342 MB), single-threaded DocVal encoding.
  It is a one-off maintenance action, verified end-to-end by the CLI's parity check.

## What this slice changed

| Change | Effect |
|---|---|
| `RamEngine::select_records` returns `(total, Vec<Arc<DocRecord>>)`; the read lock covers selection only | `query_with` / `query_projected` convert with `DocVal::to_value` / `project_to_value` — **no JSON encode + `from_slice` round trip**, and no serialization under the lock |
| `query_raw` / new `query_raw_projected` serialize straight from the trees into pre-sized buffers (`approx_bytes`, 8 MiB cap) | **no `serde_json::Value` allocation** on the query HTTP path, with or without `fields` |
| `write_json_str` copies unescaped runs whole and skips eight bytes at a time (SWAR escape detection) | serializing 5.61 MB of chat text: 8.52 ms → **3.50 ms** |
| `Database::get_raw_json` / `mget_raw_json` / `query_raw_projected` exposed; `V2Engine::get_raw_json` copies the stored bytes | `GET …/doc/{id}`, `POST …/docs/mget` and `POST …/query` answer RAM databases via `Response::JsonRaw`; v2 keeps its existing document paths and shares only the (byte-identical) query code |
| `benches/corpus_baseline.rs`: `chat get_raw largest-5` plus the previous-path row | the HTTP serialization path is measured on both engines |

### Before / after — this slice (p50)

| Workload | Phase 3A (before) | now | v2 reference |
|---|---|---|---|
| `memories` indexed `$eq` (254 docs) | 1.49 ms | **336.70 µs** | 346.80 µs |
| `chat` `get_raw` largest-5 | 5.23 ms (no raw surface: `get + to_vec`) | **3.50 ms** | 748.80 µs |
| `chat` durable `array_push` | 1.20 ms | 1.20 ms | 63.61 ms |

## Reproducing

```powershell
# target\release\ndb.exe is locked by the running ndb serve — build elsewhere:
cargo build --release --bin ndb --example corpus_baseline --target-dir D:\DEV\ndb-phase3-target

D:\DEV\ndb-phase3-target\release\examples\corpus_baseline.exe --engine both --corpus all
D:\DEV\ndb-phase3-target\release\examples\corpus_baseline.exe --engine ram  --corpus memories
D:\DEV\ndb-phase3-target\release\examples\corpus_baseline.exe --corpus chat      # both engines
```

`--corpus` keeps its Phase 0 meaning (`memories | chat | all`, default `all`);
`--engine` defaults to `both`. `--engine ram` requires `ndb.exe` **next to the
example binary** in the same cargo profile — it shells out to it for the migration
and fails loudly with the build command if it is missing.

Verification: `cargo test --features fault-io` (all suites green, including the new
`tests/phase21_ram_http_contract_tests.rs` RAM HTTP contract tests) and
`cargo check --all-targets --features fault-io` — **0 warnings** after this change.
