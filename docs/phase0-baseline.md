# Phase 0 — Real-Corpus Baseline (frozen)

Frozen **2026-11-25**. This is the measurement anchor for the RAM-first engine
refactor: the replacement engine is judged against these numbers, not against a
recollection of them. Three full runs were taken and all are recorded, so the
run-to-run spread (and one live-corpus change) is visible instead of hidden
behind a single sample.

## Provenance

| | |
|---|---|
| Repo | `D:\DEV\nDB` |
| Commit | `623379ad3edc328048ca945caed3080659c50ec9` (`chore(v2): remove unused napi/ crate and dead full-text search.rs, bump to 2.0.0`) |
| Benchmark tree | working-tree change on that commit: `benches/corpus_baseline.rs` + its `[[example]]` stanza in `Cargo.toml` (not committed at freeze time) |
| Toolchain | `rustc 1.93.1 (01f6ddf75 2026-02-11)`, `cargo 1.93.1`, release profile (`lto = true`, `codegen-units = 1`, `panic = "abort"`) |
| CPU | AMD Ryzen 9 5950X — 16 cores / 32 threads, 3.4 GHz |
| RAM | 128 GB DDR4 |
| OS | Windows 11 Pro 10.0.26300 (x64) |
| Storage | copies and WAL writes on `C:` (Samsung 960 EVO 500 GB NVMe); live corpora read from `D:` (KIOXIA EXCERIA PLUS G3 NVMe) / `D:\SRV` |
| Cargo dependencies added | **none** — `std` + existing `serde_json` + `tempfile` (dev-dependency already present) |

Reproduce:

```
cargo run --release --example corpus_baseline --manifest-path D:\DEV\nDB\Cargo.toml
cargo run --release --example corpus_baseline --manifest-path D:\DEV\nDB\Cargo.toml -- --corpus memories
cargo run --release --example corpus_baseline --manifest-path D:\DEV\nDB\Cargo.toml -- --corpus chat
```

`--corpus` defaults to `all`. Warning-free build check:
`cargo check --all-targets --features fault-io` (0 warnings at freeze time).

## Corpora (read-only, copied)

Both corpora are **copied** into a `tempfile::tempdir()` that is deleted on exit.
The source folders are only read; nothing under them is opened for writing and
`.lock` is deliberately never copied.

| Corpus | Source | Copied files | Docs (open) | On-disk |
|---|---|---|---|---|
| `memories` | `D:\DEV\mcp_server\data\memories` | `manifest.json`, `base.gen_2.ndb`, `wal.gen_2.ndb` | 5,094 | 6.69 MB |
| `chat_herrbasan` | `D:\SRV\LLM-Gateway-Chat\server\data\herrbasan` | `manifest.json`, `meta.json`, `base.gen_<N>.ndb`, `wal.gen_<N>.ndb` | 907 | 353–355 MB (gen_96) / 326 MB (gen_97) |

`doc_count` in the memories manifest is 5,079; the open handle reports 5,094
because the WAL overlay carries 15 post-base writes. Indexes are the ones the
live databases already persist:

- memories: `id` hash, `category` hash, `embedStatus` hash
- chat: `_type` hash, `id` hash, `sessionId` hash, `updatedAt` btree, `createdAt` btree

**The chat corpus is live and changes under the harness.** Runs 1–2 copied
`base.gen_96.ndb` (325.73 MB) plus a 28–29 MB `wal.gen_96.ndb`; between runs 2
and 3 the production service compacted and the copy became `base.gen_97.ndb`
(326.22 MB) with an **empty** WAL. Copy size and cold `open` therefore differ
between runs by design — 198–211 ms of WAL replay on the gen_96 snapshot versus
8 ms mmap-only on the compacted gen_97 snapshot (907 docs, 326 MB of documents,
both times).

## What is measured

Every corpus opens through `Persistence::Immediate`, so **every write pays the
real durable-before-ack cost**: `set` / `update` / `array_push` return only after
the WAL fsync.

> The Phase 0 plan names `Database::open_v2_with_persistence(..., Persistence::Immediate)`.
> No such method exists; the shipped API is `Database::open_v2_with_options` with
> `OpenOptions { format: StorageFormat::V2, persistence: Persistence::Immediate }`,
> which is the same handle behaviour. The example uses that.

| Row | Definition |
|---|---|
| open | `Database::open_v2_with_options` on the copy (base mmap + WAL replay + persisted index load) |
| rss | `K32GetProcessMemoryInfo` working set at that moment (MB) |
| warm get | `Database::get` on random ids after 64 untimed warm-ups |
| mget-style batch of 20 | 20 sequential `Database::get` calls per timed op (in-process; no HTTP hop) |
| indexed `$eq` | `Database::query({"category": {"$eq": …}})`, value = the most frequent category in a 400-doc sample (`architecture`, 340 matches) — the whole result set is materialized |
| unindexed scan+sort+limit(20) | `query_with({"timestamp": {"$exists": true}}, sort = confidence desc, limit 20)` — both fields unindexed, bounded top-K |
| durable set / update | 50 ops each on random existing docs, `Persistence::Immediate` (one fsync per op) |
| warm get, largest 5 docs | random `get` over the 5 largest chat documents (5.61 / 4.67 / 4.18 / 4.13 / 4.07 MB) |
| warm get, random docs | `get` over all 907 chat documents |
| durable `array_push` | 20 appends to `messages` on the largest conversation (5.61 MB) under `Immediate`, RSS before/after |
| 4 readers + 1 writer | 4 threads doing `get` on the largest 5 docs while 1 thread appends 20 `messages` to the largest doc; reader samples are all gets completed in that window |

Percentiles are nearest-rank over the timed samples (`ceil(p/100 × n)`).

## Run 1 (frozen result)

### memories — 5,094 docs

| Workload | n | p50 | p95 | p99 | max | throughput |
|---|---|---|---|---|---|---|
| warm get | 5,000 | 2.40 µs | 7.30 µs | 13.30 µs | 76.80 µs | 317,070 ops/s |
| warm mget-style batch of 20 | 500 | 29.10 µs | 50.10 µs | 68.10 µs | 214.20 µs | 630,171 docs/s |
| indexed `$eq` category | 200 | 467.50 µs | 482.30 µs | 497.60 µs | 505.00 µs | 2,132 ops/s |
| unindexed scan+sort+limit(20) | 200 | 2.74 ms | 2.87 ms | 2.96 ms | 3.01 ms | 364 ops/s |
| durable `set` (Immediate) | 50 | 1.21 ms | 1.57 ms | 3.89 ms | 3.89 ms | 772 ops/s |
| durable `update` (Immediate) | 50 | 1.21 ms | 1.48 ms | 3.74 ms | 3.74 ms | 781 ops/s |

Copy 5 ms · open 18 ms · RSS after run 26.6 MB.

### chat_herrbasan — 907 docs, 325.9 MB of documents

| Workload | n | p50 | p95 | p99 | max | throughput |
|---|---|---|---|---|---|---|
| warm get, largest 5 docs | 200 | 1.27 ms | 2.27 ms | 2.35 ms | 2.91 ms | 762 ops/s |
| warm get, random docs | 500 | 7.00 µs | 1.497 ms | 2.388 ms | 3.175 ms | 4,090 ops/s |
| durable `array_push` (Immediate) | 20 | 65.36 ms | 72.79 ms | 82.60 ms | 82.60 ms | 15 ops/s |
| reader `get` under writer load | 2,510 | 1.684 ms | 3.737 ms | 13.701 ms | 19.051 ms | 1,413 ops/s |
| writer `array_push` under load | 20 | 87.44 ms | 99.73 ms | 104.39 ms | 104.39 ms | 11 ops/s |

Copy 108 ms · open 198 ms · RSS 59.4 MB after open, 932.6 MB before the push
block, 1,065.2 MB after 20 durable pushes on the 5.61 MB doc (**+132.6 MB**,
≈6.6 MB per push). Writer window 1,776 ms.

## Run 2 (variance check)

### memories

| Workload | p50 | p95 | p99 |
|---|---|---|---|
| warm get | 2.40 µs | 7.70 µs | 13.90 µs |
| warm mget-style batch of 20 | 30.10 µs | 46.20 µs | 71.10 µs |
| indexed `$eq` category | 460.60 µs | 470.20 µs | 481.90 µs |
| unindexed scan+sort+limit(20) | 2.69 ms | 2.76 ms | 2.83 ms |
| durable `set` (Immediate) | 1.26 ms | 3.91 ms | 4.34 ms |
| durable `update` (Immediate) | 1.25 ms | 1.38 ms | 1.53 ms |

Copy 5 ms · open 9 ms · RSS after run 26.1 MB.

### chat_herrbasan

| Workload | n | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| warm get, largest 5 docs | 200 | 923.10 µs | 1.825 ms | 2.022 ms | 2.160 ms |
| warm get, random docs | 500 | 6.10 µs | 1.395 ms | 2.195 ms | 2.872 ms |
| durable `array_push` (Immediate) | 20 | 61.38 ms | 64.41 ms | 80.69 ms | 80.69 ms |
| reader `get` under writer load | 2,810 | 1.587 ms | 3.143 ms | 11.803 ms | 14.721 ms |
| writer `array_push` under load | 20 | 82.55 ms | 95.80 ms | 96.01 ms | 96.01 ms |

Copy 97 ms · open 211 ms · RSS 62.2 MB after open, 934.3 → 1,066.9 MB across the
push block (**+132.6 MB**). Writer window 1,689 ms.

A third `--corpus memories` run (isolated) landed at get p50 3.00 µs / p99
16.00 µs, mget p50 52.10 µs, `$eq` p50 467.10 µs, scan p50 2.70 ms — same shape,
sub-millisecond spread on the read paths. Treat ±15 % as noise on the single-digit
µs rows and ±5 % on the ms rows.

## Run 3 (after the live chat DB compacted — `gen_97`)

Same binary, `--corpus all`, taken after the production service compacted the
chat database (fresh base, empty WAL).

### memories

| Workload | n | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| warm get | 5,000 | 2.40 µs | 7.70 µs | 14.50 µs | 86.40 µs |
| warm mget-style batch of 20 | 500 | 35.50 µs | 54.50 µs | 72.70 µs | 289.70 µs |
| indexed `$eq` category | 200 | 456.10 µs | 477.50 µs | 494.80 µs | 499.60 µs |
| unindexed scan+sort+limit(20) | 200 | 2.72 ms | 2.88 ms | 2.93 ms | 2.96 ms |
| durable `set` (Immediate) | 50 | 1.19 ms | 1.53 ms | 1.79 ms | 1.79 ms |
| durable `update` (Immediate) | 50 | 1.19 ms | 1.29 ms | 1.44 ms | 1.44 ms |

Copy 5 ms · open 9 ms · RSS after run 25.9 MB.

### chat_herrbasan (`gen_97`, empty WAL)

| Workload | n | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| cold-ish `open` (mmap, no WAL replay) | 1 | **8 ms** | — | — | — |
| warm get, largest 5 docs | 200 | 905.00 µs | 1.781 ms | 1.909 ms | 1.955 ms |
| warm get, random docs | 500 | 6.40 µs | 1.175 ms | 2.157 ms | 2.585 ms |
| durable `array_push` (Immediate) | 20 | 62.46 ms | 64.20 ms | 65.05 ms | 65.05 ms |
| reader `get` under writer load | 2,816 | 1.462 ms | 3.023 ms | 11.383 ms | 16.931 ms |
| writer `array_push` under load | 20 | 77.00 ms | 82.15 ms | 89.61 ms | 89.61 ms |

Copy 86 ms · open 8 ms · RSS 6.5 MB after open, 884.2 → 1,018.3 MB across the
push block (**+134.1 MB**). Writer window 1,561 ms.

The read and write latencies of the three runs agree within noise; only `open`
moves, because it is dominated by WAL replay depth, not by document count.

## What the numbers say (the RAM-first targets)

- **A `get` on a 5.6 MB conversation costs ~1 ms p50** and scales with document
  size: the v2 read path materializes the whole `serde_json::Value` tree per call
  (random-doc p50 is 6–7 µs because most chat docs are small; p95 is 1.4 MB-scale).
  Structural sharing in `DocVal` is the target: shared subtrees must not be
  re-materialized.
- **One durable `array_push` to a 5.6 MB doc costs 61–65 ms p50** and writes the
  full post-mutation document to the WAL. This is the single largest cost in the
  corpus and the reason the refactor exists. Op-level WAL framing must drop this
  to the patch size, not the document size.
- **The writer blocks readers**: reader p99 goes from 2.0–2.4 ms idle to
  11.8–13.7 ms while 20 durable pushes run, and the writer itself slows by ~30 %
  (65 → 87 ms p50) because readers hold the read lock while materializing.
  Publication must happen after fsync, never across it.
- **`$eq` on a 340-match slice is 460–470 µs** — index probing is cheap, but the
  whole result set is materialized as `Value` docs. Query results should return
  handles/bytes, not clones.
- **RSS tracks allocation churn, not live data**: 325.9 MB of documents, 59–62 MB
  RSS after open, but 932–935 MB RSS after the size pass + push block. The
  allocator keeps the freed 5 MB document copies. `approx_bytes` bookkeeping and
  shared `Arc` subtrees are the target.
- **memories is healthy on reads** (2.4 µs get, 0.46 ms indexed `$eq`, 2.7 ms
  scan+sort+limit over 5k docs) and only the durable write path (~1.2 ms p50 per
  fsync) carries the fsync cost the group commit is meant to amortize.
- **Cold `open` is WAL-replay bound, not document-count bound**: 326 MB / 907 docs
  opens in 8 ms when the base is compacted and the WAL empty, and in 198–211 ms
  when a 28–29 MB WAL has to be replayed. Whatever replaces the snapshot path
  must keep the empty-WAL case at mmap speed.

## Notes

- Reads run on a warm handle after untimed warm-ups. The printed `open` is one
  observation of a cold open on the copy, not a repeated cold-start benchmark —
  `phase3_corpus_bench` owns cold metrics.
- `mget-style` is in-process and sequential. The HTTP `mget` round trip is
  measured by `phase3_corpus_bench`, not here.
- Sample counts are fixed in the source (`MEM_*` / `CHAT_*` constants) so the
  same command produces the same op counts; only latency varies.
