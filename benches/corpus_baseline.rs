//! Real-corpus workloads for the RAM-first engine (v3).
//!
//!     cargo run --release --example corpus_baseline -- --corpus all
//!     cargo run --release --example corpus_baseline -- --corpus chat
//!     cargo run --release --example corpus_baseline -- --corpus memories
//!
//! `--corpus memories|chat|all` (default `all`) selects the corpus. v3
//! RAM-first is the only binary engine nDB ships, so there is no engine flag
//! and every number describes exactly what production runs.
//!
//! Every measurement runs against a **copy** of a live production database made
//! into a `tempfile::tempdir()` that is deleted on exit. Only the database files
//! are copied (`manifest.json`, `base.gen_*.ndb`, `wal.gen_*.ndb`, `meta.json`);
//! the source folder is only ever read, and `.lock` is never copied. The live
//! folders are production state â€” this program cannot write to them.
//!
//! Every corpus opens through `Database::open_ram`; the engine is durable per
//! commit, so every `set`, `update` and `array_push` here pays the real
//! durable-before-ack cost (there is no other mode).
//!
//! Workloads (all warm, after an untimed warm-up):
//!
//!   memories (5k docs)   get (5,000), mget-style batch of 20 (500),
//!                        hash-indexed `$eq` (200), unindexed scan+sort+limit20
//!                        (200), durable `set` / `update` (50 each)
//!   chat (907 docs)      largest-5 get (200), largest-5 get_raw â€” the HTTP
//!                        JSON-bytes path (200), random get (500), 20 durable
//!                        `array_push` on the largest conversation (+ RSS,
//!                        WAL-growth and `live_doc_bytes`/`ring_bytes` deltas),
//!                        4 reader threads on large docs while one writer pushes
//!                        20 more, then 4 readers + 1 writer *while
//!                        `compact()` runs*, and a 100-push churn soak followed
//!                        by one compaction (start/peak/end RSS, memory and WAL).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ndb::{ram_format, Database, QueryOptions, RamStats, SortDir};
use serde_json::{json, Value};

const MEM_SRC: &str = r"D:\DEV\mcp_server\data\memories";
const CHAT_SRC: &str = r"D:\SRV\LLM-Gateway-Chat\server\data\herrbasan";

const MEM_GET_OPS: usize = 5_000;
const MEM_MGET_BATCH: usize = 20;
const MEM_MGET_OPS: usize = 500;
const MEM_QUERY_OPS: usize = 200;
const MEM_WRITE_OPS: usize = 50;

const CHAT_TOP_DOCS: usize = 5;
const CHAT_TOP_GET_OPS: usize = 200;
/// The pre-`get_raw` HTTP path re-encodes a whole `Value` per hit — a fifth of
/// the samples is enough to rank it.
const CHAT_VALUE_PATH_OPS: usize = 40;
const CHAT_RANDOM_GET_OPS: usize = 500;
const CHAT_PUSH_OPS: usize = 20;
const CHAT_READERS: usize = 4;
const SOAK_PUSHES: usize = 100;
/// Minimum reader observation window for the concurrency rows. The 20-push
/// writer finishes in ~60 ms — too short to rank a p99 over — so readers keep
/// sampling to this floor.
const READER_WINDOW_FLOOR_MS: u64 = 500;

// ─── Deterministic RNG (std only) ───────────────────────────────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

// ─── Latency distribution ───────────────────────────────────────────────────

#[derive(Default, Clone)]
struct Dist {
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn dist(mut samples: Vec<f64>) -> Dist {
    assert!(!samples.is_empty(), "a distribution needs at least one sample");
    samples.sort_by(|a, b| a.partial_cmp(b).expect("finite latencies"));
    let pick = |p: f64| {
        let idx = ((p / 100.0) * samples.len() as f64).ceil() as usize;
        samples[idx.max(1) - 1]
    };
    Dist {
        p50: pick(50.0),
        p95: pick(95.0),
        p99: pick(99.0),
        max: *samples.last().expect("non-empty"),
    }
}

struct Measured {
    /// Calls made in the timed region (one latency sample each).
    ops: usize,
    /// Documents touched by one call — `20` for the mget-style batch.
    units_per_op: usize,
    wall: Duration,
    dist: Dist,
}

impl Measured {
    fn ops_per_s(&self) -> f64 {
        self.ops as f64 / self.wall.as_secs_f64()
    }

    fn print(&self, label: &str) {
        let d = &self.dist;
        let ms = d.p50 >= 1_000.0;
        let (scale, unit) = if ms { (1e-3, "ms") } else { (1.0, "µs") };
        let throughput = if self.units_per_op == 1 {
            format!("{:>12.0} ops/s", self.ops_per_s())
        } else {
            format!(
                "{:>12.0} docs/s",
                self.ops as f64 * self.units_per_op as f64 / self.wall.as_secs_f64()
            )
        };
        println!(
            "  {label:<28} n={:<6} p50 {:>9.2} {unit}  p95 {:>9.2} {unit}  p99 {:>9.2} {unit}  max {:>9.2} {unit}  {throughput}",
            self.ops,
            d.p50 * scale,
            d.p95 * scale,
            d.p99 * scale,
            d.max * scale,
        );
    }
}

fn measure(ops: usize, units_per_op: usize, mut op: impl FnMut(usize)) -> Measured {
    let mut samples = Vec::with_capacity(ops);
    let started = Instant::now();
    for i in 0..ops {
        let t0 = Instant::now();
        op(i);
        samples.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    Measured {
        ops,
        units_per_op,
        wall: started.elapsed(),
        dist: dist(samples),
    }
}

// ─── Process RSS (std + OS API, no crate) ───────────────────────────────────

#[cfg(windows)]
fn rss_bytes() -> Option<usize> {
    // K32GetProcessMemoryInfo writes through the pointer; the compiler cannot
    // see individual field reads.
    #[allow(dead_code)]
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    let mut counters = ProcessMemoryCounters {
        cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
        ..Default::default()
    };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return None;
    }
    Some(counters.working_set_size)
}

#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<usize> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: usize = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn rss_bytes() -> Option<usize> {
    None
}

fn mb(bytes: f64) -> f64 {
    bytes / (1024.0 * 1024.0)
}

fn rss_mb() -> f64 {
    match rss_bytes() {
        Some(bytes) => mb(bytes as f64),
        None => f64::NAN,
    }
}

fn rss_delta(before: usize, after: usize) -> f64 {
    mb(after as f64 - before as f64)
}

// ─── Safe corpus copy ───────────────────────────────────────────────────────

/// The only files a v3 database needs. Anything else in the folder (`_trash/`,
/// `_files/`, legacy `.jsonl` backups, `.lock`) stays behind — and `.lock` in
/// particular must never travel, or the copy would claim to be the live owner.
fn is_database_file(name: &str) -> bool {
    name == "manifest.json"
        || name == "meta.json"
        || ((name.starts_with("base.gen_") || name.starts_with("wal.gen_"))
            && name.ends_with(".ndb"))
}

/// Copy the database files of `src` into `dst`, verifying every byte count.
/// Returns the copied file names with their sizes, in deterministic order.
fn copy_database(src: &Path, dst: &Path) -> Vec<(String, u64)> {
    assert!(
        src.is_dir(),
        "corpus source '{}' is not a directory",
        src.display()
    );
    assert!(
        !dst.starts_with(src),
        "refusing to write '{}' inside the read-only source '{}'",
        dst.display(),
        src.display()
    );
    let mut names: Vec<String> = fs::read_dir(src)
        .unwrap_or_else(|e| panic!("read source '{}': {e}", src.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|e| panic!("read entry of '{}': {e}", src.display()))
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| is_database_file(name))
        .collect();
    names.sort();
    assert!(
        names.iter().any(|n| n == "manifest.json"),
        "corpus source '{}' holds no manifest.json — not a database folder",
        src.display()
    );

    fs::create_dir_all(dst).unwrap_or_else(|e| panic!("create '{}': {e}", dst.display()));
    let mut copied = Vec::with_capacity(names.len());
    for name in names {
        let from = src.join(&name);
        assert!(
            from.is_file(),
            "corpus file '{}' is not a regular file",
            from.display()
        );
        let expected = fs::metadata(&from)
            .unwrap_or_else(|e| panic!("stat '{}': {e}", from.display()))
            .len();
        let to = dst.join(&name);
        let written = fs::copy(&from, &to)
            .unwrap_or_else(|e| panic!("copy '{}' -> '{}': {e}", from.display(), to.display()));
        assert_eq!(
            written, expected,
            "copy of '{}' wrote {written} of {expected} bytes",
            from.display()
        );
        copied.push((name, written));
    }
    copied
}

fn open_corpus(dir: &Path) -> Database {
    Database::open_ram(dir).unwrap_or_else(|e| panic!("open RAM corpus '{}': {e}", dir.display()))
}

/// The active log is always `wal.gen_<N>.ndb`, so this measures WAL growth
/// without trusting engine internals.
fn wal_file_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("list '{}': {e}", dir.display()))
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("wal.gen_") && name.ends_with(".ndb")
        })
        .map(|entry| entry.metadata().map(|meta| meta.len()).unwrap_or(0))
        .sum()
}

fn print_ram_stats(when: &str, stats: &RamStats) {
    println!(
        "    ram {when:<18} live_doc {:>8.2} MB   index {:>7.2} MB   ring {:>7.2} MB   wal {:>8.2} MB   batches {:>5}   fsync {:>7.1} ms   publish {:>7.1} ms",
        mb(stats.live_doc_bytes as f64),
        mb(stats.index_bytes as f64),
        mb(stats.ring_bytes as f64),
        mb(stats.wal_bytes as f64),
        stats.batch_count,
        stats.fsync_us as f64 / 1000.0,
        stats.publish_us as f64 / 1000.0,
    );
}

fn prepare_corpus(label: &str, src: &Path) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir()
        .unwrap_or_else(|e| panic!("create tempdir for '{label}': {e}"));
    let dir = tmp.path().join(format!("db-{label}"));
    let started = Instant::now();
    let copied = copy_database(src, &dir);
    let copy_ms = started.elapsed().as_secs_f64() * 1000.0;
    let total: u64 = copied.iter().map(|(_, bytes)| *bytes).sum();
    println!(
        "  copied {} file(s), {:.2} MB in {:.0} ms → {}",
        copied.len(),
        mb(total as f64),
        copy_ms,
        dir.display()
    );
    for (name, bytes) in &copied {
        println!("    {name:<24} {:>10.2} MB", mb(*bytes as f64));
    }
    (tmp, dir)
}

// ─── Corpus: memories ───────────────────────────────────────────────────────

fn sampled_docs(db: &Database, ids: &[String], n: usize) -> Vec<Value> {
    let stride = (ids.len() / n).max(1);
    ids.iter()
        .step_by(stride)
        .take(n)
        .map(|id| db.get(id).expect("sampled document"))
        .collect()
}

fn most_common_scalar(sample: &[Value], field: &str) -> Option<Value> {
    let mut counts: Vec<(Value, usize)> = Vec::new();
    for value in sample.iter().filter_map(|doc| doc.get(field)) {
        if !matches!(value, Value::String(s) if !s.is_empty()) {
            continue;
        }
        match counts.iter_mut().find(|(seen, _)| seen == value) {
            Some((_, n)) => *n += 1,
            None => counts.push((value.clone(), 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(value, _)| value)
}

fn run_memories() {
    println!("═══ memories — v3 RAM-first ({MEM_SRC}) ═══");
    let (_tmp, dir) = prepare_corpus("memories", Path::new(MEM_SRC));
    let rss_before_open = rss_bytes().expect("RSS before open");

    let started = Instant::now();
    let db = open_corpus(&dir);
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;
    let docs = db.len();
    let ids = {
        // Sorted so the sampled `$eq` category and the random-get id set are
        // stable across runs — `get_all_ids` order is an engine detail.
        let mut ids = db.get_all_ids();
        ids.sort();
        ids
    };
    assert_eq!(ids.len(), docs, "id list and document count disagree");
    println!(
        "  open {:.0} ms   docs {docs}   rss {:.1} MB (+{:.1} MB over pre-open)",
        open_ms,
        rss_mb(),
        rss_delta(rss_before_open, rss_bytes().expect("RSS after open")),
    );
    let indexes = ram_format::read_ram_manifest(&dir)
        .expect("read the copied v3 manifest")
        .indexes;
    println!(
        "  indexes: {}",
        indexes
            .iter()
            .map(|spec| format!("{} {:?}", spec.field, spec.kind))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for spec in &indexes {
        assert!(
            db.has_index(&spec.field),
            "manifest declares an index on '{}' the open handle does not have",
            spec.field
        );
    }
    if let Some(stats) = db.ram_stats() {
        print_ram_stats("after open", &stats);
    }
    println!();

    let mut rng = Rng::new(0x5EED_1234_ABCD);
    for _ in 0..64 {
        let _ = db.get(&ids[rng.below(ids.len())]).expect("get warm-up");
    }

    let get = measure(MEM_GET_OPS, 1, |_| {
        let doc = db
            .get(&ids[rng.below(ids.len())])
            .expect("warm get on memories");
        std::hint::black_box(&doc);
    });
    get.print("warm get");

    let mget = measure(MEM_MGET_OPS, MEM_MGET_BATCH, |_| {
        for _ in 0..MEM_MGET_BATCH {
            let doc = db
                .get(&ids[rng.below(ids.len())])
                .expect("mget member on memories");
            std::hint::black_box(&doc);
        }
    });
    mget.print("warm mget-style batch of 20");

    let sample = sampled_docs(&db, &ids, 400);
    let category = most_common_scalar(&sample, "category").expect("memories carry a category");
    let eq_filter = json!({ "category": { "$eq": category } });
    let mut eq_matches = 0;
    for _ in 0..8 {
        eq_matches = db
            .query(eq_filter.clone())
            .expect("eq warm-up")
            .len();
    }
    let eq = measure(MEM_QUERY_OPS, 1, |_| {
        let rows = db.query(eq_filter.clone()).expect("indexed $eq on memories");
        std::hint::black_box(&rows);
    });
    eq.print(&format!("indexed $eq category={}", category.as_str().unwrap_or("?")));
    println!("    matched {eq_matches} docs, after 8 warm-up rounds");

    let scan_filter = json!({ "timestamp": { "$exists": true } });
    let scan_opts = QueryOptions {
        limit: Some(20),
        offset: None,
        sort_by: Some(("confidence".to_string(), SortDir::Desc)),
        ..QueryOptions::default()
    };
    let mut scan_page = 0;
    for _ in 0..2 {
        scan_page = db
            .query_with(scan_filter.clone(), scan_opts.clone())
            .expect("scan warm-up")
            .len();
    }
    let scan = measure(MEM_QUERY_OPS, 1, |_| {
        let rows = db
            .query_with(scan_filter.clone(), scan_opts.clone())
            .expect("unindexed scan+sort+limit on memories");
        std::hint::black_box(&rows);
    });
    scan.print("unindexed scan+sort+limit(20)");
    println!("    page size {scan_page} (filter timestamp $exists, sort confidence desc)");
    println!();

    let write_ids: Vec<&String> = {
        let mut r = Rng::new(0xC0FF_EE00_1234);
        (0..MEM_WRITE_OPS)
            .map(|_| &ids[r.below(ids.len())])
            .collect()
    };
    let set = measure(MEM_WRITE_OPS, 1, |i| {
        let applied = db
            .set(write_ids[i], "bench_marker", json!(i))
            .expect("durable set on memories");
        assert!(applied, "durable set {} lost its path", write_ids[i]);
    });
    set.print("durable set (Immediate)");

    let mut update_rng = Rng::new(0xBEEF_0001_7777);
    let update_docs: Vec<usize> = (0..MEM_WRITE_OPS)
        .map(|_| update_rng.below(ids.len()))
        .collect();
    let update = measure(MEM_WRITE_OPS, 1, |i| {
        let id = &ids[update_docs[i]];
        let mut doc = db.get(id).expect("read for durable update");
        doc["bench_marker"] = json!(i);
        db.update(id, doc).expect("durable update on memories");
    });
    update.print("durable update (Immediate)");

    println!();
    println!(
        "  memories rss after run: {:.1} MB   wal {:.2} MB on disk",
        rss_mb(),
        mb(wal_file_bytes(&dir) as f64),
    );
    if let Some(stats) = db.ram_stats() {
        print_ram_stats("after run", &stats);
    }
    println!();
}

// ─── Corpus: chat ───────────────────────────────────────────────────────────

fn chat_message(i: usize) -> Value {
    json!({
        "role": "user",
        "content": format!("phase0 array_push probe {i}"),
        "createdAt": "2026-11-25T00:00:00.000Z",
    })
}

fn run_chat() {
    println!("═══ chat_herrbasan — v3 RAM-first ({CHAT_SRC}) ═══");
    let (_tmp, dir) = prepare_corpus("chat_herrbasan", Path::new(CHAT_SRC));
    let rss_before_open = rss_bytes().expect("RSS before open");

    let started = Instant::now();
    let db = Arc::new(open_corpus(&dir));
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;
    let docs = db.len();
    let ids = db.get_all_ids();
    let rss_open = rss_bytes().expect("RSS after open");
    println!(
        "  open {:.0} ms   docs {docs}   rss {:.1} MB (+{:.1} MB over pre-open)",
        open_ms,
        rss_mb(),
        rss_delta(rss_before_open, rss_open),
    );
    if let Some(stats) = db.ram_stats() {
        print_ram_stats("after open", &stats);
    }

    let sized_started = Instant::now();
    let mut sized: Vec<(String, usize)> = ids
        .iter()
        .map(|id| {
            let doc = db.get(id).expect("size pass get");
            (id.clone(), serde_json::to_vec(&doc).expect("encode for size").len())
        })
        .collect();
    sized.sort_by(|a, b| b.1.cmp(&a.1));
    let total_bytes: usize = sized.iter().map(|(_, bytes)| *bytes).sum();
    println!(
        "  size pass {:.0} ms   {:.2} MB across {docs} docs",
        sized_started.elapsed().as_secs_f64() * 1000.0,
        mb(total_bytes as f64),
    );
    for (id, bytes) in sized.iter().take(CHAT_TOP_DOCS) {
        println!("    {id:<24} {:>9.2} MB", mb(*bytes as f64));
    }

    let largest_id = sized[0].0.clone();
    let largest_bytes = sized[0].1;
    let top_ids: Vec<String> = sized.iter().take(CHAT_TOP_DOCS).map(|(id, _)| id.clone()).collect();

    let largest = db.get(&largest_id).expect("read the largest chat doc");
    let pre_push_len = largest
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("largest chat doc '{largest_id}' carries no messages array"))
        .len();
    drop(largest);
    println!("  largest chat doc: {largest_id} ({pre_push_len} messages)");
    println!();

    let mut rng = Rng::new(0x0C4A_75EE_D001);
    for _ in 0..8 {
        let id = &top_ids[rng.below(top_ids.len())];
        let _ = db.get(id).expect("top get warm-up");
    }
    let top_get = measure(CHAT_TOP_GET_OPS, 1, |_| {
        let doc = db
            .get(&top_ids[rng.below(top_ids.len())])
            .expect("get on a largest chat doc");
        std::hint::black_box(&doc);
    });
    top_get.print("warm get, largest 5 docs");

    // The HTTP serialization path itself: the engine hands out the stored bytes
    // mapping, v3 writes the DocVal tree into a pre-sized buffer. No `Value`
    // exists on either side, so this isolates encode cost from materialisation.
    let mut raw_rng = Rng::new(0x0C4A_75EE_D001);
    for _ in 0..8 {
        let id = &top_ids[raw_rng.below(top_ids.len())];
        let _ = db.get_raw_json(id).expect("top get_raw warm-up");
    }
    let top_get_raw = measure(CHAT_TOP_GET_OPS, 1, |_| {
        let bytes = db
            .get_raw_json(&top_ids[raw_rng.below(top_ids.len())])
            .expect("get_raw on a largest chat doc");
        std::hint::black_box(&bytes);
    });
    top_get_raw.print("warm get_raw (JSON bytes), largest 5 docs");

    // The HTTP path as it was before the raw surface existed: build a Value,
    // then hand it to serde_json. This is the number `get_raw` replaces.
    let mut value_rng = Rng::new(0x0C4A_75EE_D001);
    for _ in 0..4 {
        let id = &top_ids[value_rng.below(top_ids.len())];
        let doc = db.get(id).expect("value-path warm-up");
        let _ = serde_json::to_vec(&doc).expect("value-path warm-up encode");
    }
    let top_get_value = measure(CHAT_VALUE_PATH_OPS, 1, |_| {
        let doc = db
            .get(&top_ids[value_rng.below(top_ids.len())])
            .expect("get on a largest chat doc");
        let bytes = serde_json::to_vec(&doc).expect("encode a largest chat doc");
        std::hint::black_box(&bytes);
    });
    top_get_value.print("warm get + to_vec (previous HTTP path), largest 5 docs");

    for _ in 0..32 {
        let _ = db.get(&ids[rng.below(ids.len())]).expect("random get warm-up");
    }
    let random_get = measure(CHAT_RANDOM_GET_OPS, 1, |_| {
        let doc = db.get(&ids[rng.below(ids.len())]).expect("random chat get");
        std::hint::black_box(&doc);
    });
    random_get.print("warm get, random docs");
    println!();

    let rss_before_pushes = rss_bytes().expect("RSS before pushes");
    let wal_before_pushes = wal_file_bytes(&dir);
    let stats_before_pushes = db.ram_stats();
    let push = measure(CHAT_PUSH_OPS, 1, |i| {
        let appended = db
            .array_push(&largest_id, "messages", chat_message(i))
            .expect("durable array_push on the largest chat doc");
        assert!(appended, "array_push {i} did not resolve 'messages'");
    });
    let rss_after_pushes = rss_bytes().expect("RSS after pushes");
    let wal_after_pushes = wal_file_bytes(&dir);
    push.print("durable array_push (Immediate)");
    println!(
        "    largest doc {:.2} MB × {} pushes   rss delta {:+.1} MB ({:.1} → {:.1} MB)",
        mb(largest_bytes as f64),
        CHAT_PUSH_OPS,
        rss_delta(rss_before_pushes, rss_after_pushes),
        mb(rss_before_pushes as f64),
        mb(rss_after_pushes as f64),
    );
    println!(
        "    wal growth {:+} bytes ({:.0} B/op)   wal now {:.2} MB on disk",
        wal_after_pushes as i64 - wal_before_pushes as i64,
        (wal_after_pushes as i64 - wal_before_pushes as i64) as f64 / CHAT_PUSH_OPS as f64,
        mb(wal_after_pushes as f64),
    );
    if let (Some(before), Some(after)) = (stats_before_pushes, db.ram_stats()) {
        println!(
            "    live_doc_bytes {:+} B   ring_bytes {:+} B   (live {:.2} MB, ring {:.2} MB)",
            after.live_doc_bytes as i64 - before.live_doc_bytes as i64,
            after.ring_bytes as i64 - before.ring_bytes as i64,
            mb(after.live_doc_bytes as f64),
            mb(after.ring_bytes as f64),
        );
    }
    let post_len = db
        .get(&largest_id)
        .expect("read back the largest chat doc")
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages array after pushes")
        .len();
    assert_eq!(
        post_len,
        pre_push_len + CHAT_PUSH_OPS,
        "messages length did not grow by {CHAT_PUSH_OPS}"
    );
    println!();

    // 4 readers on the large docs, continuously, while one writer pushes.
    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::with_capacity(CHAT_READERS);
    for t in 0..CHAT_READERS {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        let top_ids = top_ids.clone();
        readers.push(std::thread::spawn(move || -> Vec<f64> {
            let mut rng = Rng::new(0xA11C_E000 + t as u64);
            let mut samples: Vec<f64> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let id = &top_ids[rng.below(top_ids.len())];
                let t0 = Instant::now();
                let doc = db.get(id).expect("concurrent reader get");
                samples.push(t0.elapsed().as_secs_f64() * 1e6);
                std::hint::black_box(&doc);
            }
            samples
        }));
    }

    let mut writer_samples = Vec::with_capacity(CHAT_PUSH_OPS);
    let writer_started = Instant::now();
    for i in 0..CHAT_PUSH_OPS {
        let t0 = Instant::now();
        let appended = db
            .array_push(&largest_id, "messages", chat_message(CHAT_PUSH_OPS + i))
            .expect("durable array_push under concurrent load");
        writer_samples.push(t0.elapsed().as_secs_f64() * 1e6);
        assert!(appended, "concurrent array_push {i} did not resolve 'messages'");
    }
    let writer_wall = writer_started.elapsed();
    let floor = Duration::from_millis(READER_WINDOW_FLOOR_MS);
    if writer_wall < floor {
        std::thread::sleep(floor - writer_wall);
    }
    let window_wall = writer_started.elapsed();
    stop.store(true, Ordering::Relaxed);

    let mut reader_samples = Vec::new();
    for handle in readers {
        reader_samples.append(&mut handle.join().expect("reader thread panicked"));
    }
    assert!(
        !reader_samples.is_empty(),
        "no reader completed a get during the writer window"
    );
    let reader_count = reader_samples.len();
    let reader_dist = dist(reader_samples);
    println!(
        "  4 readers + 1 writer, writer wall {:.0} ms (reader window {:.0} ms), {} reader gets:",
        writer_wall.as_secs_f64() * 1000.0,
        window_wall.as_secs_f64() * 1000.0,
        reader_count
    );
    println!(
        "    reader get                n={:<6} p50 {:>9.2} µs  p95 {:>9.2} µs  p99 {:>9.2} µs  max {:>9.2} µs  {:>12.0} ops/s",
        reader_count,
        reader_dist.p50,
        reader_dist.p95,
        reader_dist.p99,
        reader_dist.max,
        reader_count as f64 / window_wall.as_secs_f64(),
    );
    Measured {
        ops: CHAT_PUSH_OPS,
        units_per_op: 1,
        wall: writer_wall,
        dist: dist(writer_samples),
    }
    .print("writer array_push (Immediate)");
    if let Some(stats) = db.ram_stats() {
        print_ram_stats("after concurrent", &stats);
    }
    println!();

    // Compaction under load: the same 4 readers + 1 writer, with `compact()`
    // itself running in this thread while they work.
    let mut message_index = CHAT_PUSH_OPS * 2;
    let under_load = compact_under_load(&db, &largest_id, &top_ids, message_index);
    message_index += CHAT_PUSH_OPS;
    println!("  compaction under load (4 readers + 1 writer, compact() in flight):");
    println!(
        "    compact() wall           {:.0} ms   (writer window {:.0} ms)",
        under_load.compact_ms,
        under_load.writer_wall.as_secs_f64() * 1000.0
    );
    let during = &under_load.reader_compact;
    println!(
        "    reader get, in compact   n={:<6} p50 {:>9.2} µs  p95 {:>9.2} µs  p99 {:>9.2} µs  max {:>9.2} µs",
        under_load.reader_n_compact, during.p50, during.p95, during.p99, during.max,
    );
    let all = &under_load.reader_all;
    println!(
        "    reader get, whole window n={:<6} p50 {:>9.2} µs  p95 {:>9.2} µs  p99 {:>9.2} µs  max {:>9.2} µs",
        under_load.reader_n, all.p50, all.p95, all.p99, all.max,
    );
    let writer_under = &under_load.writer;
    println!(
        "    writer array_push        n={:<6} p50 {:>9.2} ms  p95 {:>9.2} ms  p99 {:>9.2} ms  max {:>9.2} ms",
        CHAT_PUSH_OPS,
        writer_under.p50 / 1000.0,
        writer_under.p95 / 1000.0,
        writer_under.p99 / 1000.0,
        writer_under.max / 1000.0,
    );
    if let Some(stats) = db.ram_stats() {
        print_ram_stats("after compact", &stats);
    }
    println!();

    run_soak(&dir, &db, &largest_id, message_index);

    println!();
    println!(
        "  chat rss after run: {:.1} MB   wal {:.2} MB on disk",
        rss_mb(),
        mb(wal_file_bytes(&dir) as f64),
    );
    if let Some(stats) = db.ram_stats() {
        print_ram_stats("after run", &stats);
    }
    println!();
}

// ─── Chat: compaction under load & churn soak ──────────────────────────────

struct LoadWindow {
    compact_ms: f64,
    writer_wall: Duration,
    /// Reader samples taken while the compaction was underway (flag read at
    /// the start of each get, so a sample belongs to the window it entered).
    reader_compact: Dist,
    reader_n_compact: usize,
    /// Every reader sample collected from warm-up start to writer finish.
    reader_all: Dist,
    reader_n: usize,
    writer: Dist,
}

fn compact_under_load(
    db: &Arc<Database>,
    largest_id: &str,
    top_ids: &[String],
    message_base: usize,
) -> LoadWindow {
    let stop = Arc::new(AtomicBool::new(false));
    let compacting = Arc::new(AtomicBool::new(false));
    let window_started = Instant::now();

    let mut readers = Vec::with_capacity(CHAT_READERS);
    for t in 0..CHAT_READERS {
        let db = Arc::clone(db);
        let stop = Arc::clone(&stop);
        let compacting = Arc::clone(&compacting);
        let top_ids = top_ids.to_vec();
        readers.push(std::thread::spawn(move || -> Vec<(bool, f64)> {
            let mut rng = Rng::new(0xC0FF_1E00 + t as u64);
            let mut samples: Vec<(bool, f64)> = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let id = &top_ids[rng.below(top_ids.len())];
                let during = compacting.load(Ordering::Relaxed);
                let t0 = Instant::now();
                let doc = db.get(id).expect("compaction reader get");
                samples.push((during, t0.elapsed().as_secs_f64() * 1e6));
                std::hint::black_box(&doc);
            }
            samples
        }));
    }

    let writer = {
        let db = Arc::clone(db);
        let largest_id = largest_id.to_string();
        std::thread::spawn(move || -> (Vec<f64>, Duration) {
            let mut samples = Vec::with_capacity(CHAT_PUSH_OPS);
            let started = Instant::now();
            for i in 0..CHAT_PUSH_OPS {
                let t0 = Instant::now();
                let appended = db
                    .array_push(&largest_id, "messages", chat_message(message_base + i))
                    .expect("durable array_push while compacting");
                samples.push(t0.elapsed().as_secs_f64() * 1e6);
                assert!(
                    appended,
                    "array_push {i} under compaction did not resolve 'messages'"
                );
            }
            (samples, started.elapsed())
        })
    };

    compacting.store(true, Ordering::Relaxed);
    let started = Instant::now();
    db.compact().expect("compact under concurrent load");
    let compact_ms = started.elapsed().as_secs_f64() * 1000.0;
    compacting.store(false, Ordering::Relaxed);

    let (writer_samples, writer_wall) = writer.join().expect("compaction writer panicked");
    let floor = Duration::from_millis(READER_WINDOW_FLOOR_MS);
    let elapsed = window_started.elapsed();
    if elapsed < floor {
        std::thread::sleep(floor - elapsed);
    }
    stop.store(true, Ordering::Relaxed);

    let mut all = Vec::new();
    let mut during = Vec::new();
    for handle in readers {
        for (sampled_during, micros) in handle.join().expect("compaction reader panicked") {
            if sampled_during {
                during.push(micros);
            }
            all.push(micros);
        }
    }
    assert!(
        !during.is_empty(),
        "no reader get completed while compaction was in flight"
    );

    LoadWindow {
        compact_ms,
        writer_wall,
        reader_n_compact: during.len(),
        reader_compact: dist(during),
        reader_n: all.len(),
        reader_all: dist(all),
        writer: dist(writer_samples),
    }
}

/// Churn soak: `SOAK_PUSHES` consecutive durable pushes on the largest doc,
/// then one compaction — the shape of a long-lived chat session.
fn run_soak(dir: &Path, db: &Arc<Database>, largest_id: &str, message_base: usize) {
    println!("  churn soak — {SOAK_PUSHES} consecutive durable array_push on the largest doc:");
    let rss_start = rss_bytes().expect("RSS before soak");
    let before = db.ram_stats();
    let wal_before = wal_file_bytes(dir);

    let mut peak = rss_start;
    let pushes = measure(SOAK_PUSHES, 1, |i| {
        let appended = db
            .array_push(largest_id, "messages", chat_message(message_base + i))
            .expect("durable array_push during soak");
        assert!(appended, "soak array_push {i} did not resolve 'messages'");
        peak = peak.max(rss_bytes().expect("RSS sample during soak"));
    });
    let rss_end = rss_bytes().expect("RSS after soak pushes");
    let mid = db.ram_stats();
    let wal_mid = wal_file_bytes(dir);

    let compact_started = Instant::now();
    db.compact().expect("compact after the soak");
    let compact_ms = compact_started.elapsed().as_secs_f64() * 1000.0;
    let rss_after_compact = rss_bytes().expect("RSS after soak compact");
    let after = db.ram_stats();
    let wal_after = wal_file_bytes(dir);

    pushes.print("soak array_push (Immediate)");
    println!(
        "    rss start {:.1} MB   peak {:.1} MB ({:+.1} MB)   end {:.1} MB ({:+.1} MB)   after compact {:.1} MB ({:+.1} MB)",
        mb(rss_start as f64),
        mb(peak as f64),
        rss_delta(rss_start, peak),
        mb(rss_end as f64),
        rss_delta(rss_start, rss_end),
        mb(rss_after_compact as f64),
        rss_delta(rss_start, rss_after_compact),
    );
    println!(
        "    wal {:.2} MB → {:.2} MB ({:+} B) → {:.2} MB after compact   compact {:.0} ms",
        mb(wal_before as f64),
        mb(wal_mid as f64),
        wal_mid as i64 - wal_before as i64,
        mb(wal_after as f64),
        compact_ms,
    );
    if let (Some(before), Some(mid), Some(after)) = (before, mid, after) {
        println!(
            "    live_doc_bytes {:.2} → {:.2} → {:.2} MB   ring_bytes {:.2} → {:.2} → {:.2} MB",
            mb(before.live_doc_bytes as f64),
            mb(mid.live_doc_bytes as f64),
            mb(after.live_doc_bytes as f64),
            mb(before.ring_bytes as f64),
            mb(mid.ring_bytes as f64),
            mb(after.ring_bytes as f64),
        );
    }
}

// ─── Entry ──────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let corpus = match args.iter().position(|a| a == "--corpus") {
        None => "all".to_string(),
        Some(p) => args
            .get(p + 1)
            .unwrap_or_else(|| panic!("--corpus needs a value: memories | chat | all"))
            .clone(),
    };
    let run_memories_corpus = match corpus.as_str() {
        "all" | "memories" => true,
        "chat" => false,
        other => panic!("unknown --corpus '{other}' (want memories | chat | all)"),
    };
    let run_chat_corpus = match corpus.as_str() {
        "all" | "chat" => true,
        "memories" => false,
        other => panic!("unknown --corpus '{other}' (want memories | chat | all)"),
    };
    println!("nDB real-corpus workloads — v3 RAM-first (release)");
    println!("source folders are opened read-only; every copy lives in a tempdir deleted on exit");
    println!();
    if run_memories_corpus {
        run_memories();
    }
    if run_chat_corpus {
        run_chat();
    }
    println!("done (corpus {corpus})");
}
