//! Phase 3 — real-corpus v1 vs v2 benchmark (and the frozen v1 baseline).
//!
//!     cargo build --release
//!     cargo run --release --example phase3_corpus_bench                # both
//!     cargo run --release --example phase3_corpus_bench -- --mode v1   # baseline only
//!     cargo run --release --example phase3_corpus_bench -- --mode v2   # head-to-head v2 only
//!
//! Every corpus is a **copy** under a `tempfile::tempdir()`: the live journals
//! in `D:\DEV\mcp_server\data` are opened read-only once, by `fs::copy`, and
//! never written. Override the source root with `NDB_BENCH_MCP_DATA`.
//!
//! Measured per corpus, in one pass through the real code:
//!   cold boot (`Database::open` replay) + index build, steady-state RSS delta,
//!   `get`, batch `mget` over HTTP keep-alive against `ndb serve`, projected
//!   `$eq` / BTree range / unindexed scan+sort+limit, 8 concurrent HTTP writers
//!   under `Immediate` fsync, and `compact()`.
//!
//! The v2 leg first prepares the corpus in its own process (`--mode v2-prep`:
//! copy → `ndb migrate` → create indexes → `compact()`, so the index sections
//! are persisted in `base.gen`), then measures cold open / reads / group-commit
//! writers / non-blocking compaction in a **fresh** process. No index rebuild
//! happens inside the measured window — that is the point of the persisted
//! sections.
//!
//! Units are printed per line; the closing tables use µs for the read paths and
//! ms for the scan and the concurrent writers.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use ndb::server::{serve_multi_in_background, DbRegistry, PersistenceMode, ServerConfig};
use ndb::{Database, QueryOptions, SortDir};
use serde_json::{json, Value};

const GET_ROUNDS: usize = 1_000;
const MGET_BATCH: usize = 20;
const MGET_ROUNDS: usize = 100;
/// Wall-clock budget per query shape; the round count is derived from it.
const QUERY_BUDGET_SECS: f64 = 2.0;
const MAX_QUERY_ROUNDS: usize = 200;
const WRITER_THREADS: usize = 8;
const PUTS_PER_THREAD: usize = 25;
const PATCHES_PER_THREAD: usize = 25;
const WRITE_TARGETS: usize = WRITER_THREADS * 50;

// ─── Corpora ────────────────────────────────────────────────────────────────

struct CorpusSpec {
    name: &'static str,
    source: Option<PathBuf>,
    /// Field carrying the corpus's cardinality for an indexed `$eq`.
    eq_field: &'static str,
    /// Field carrying a numeric/ISO-date axis for an indexed range.
    range_field: &'static str,
    /// Field deliberately *not* indexed, for the filtered scan.
    scan_field: &'static str,
    /// Sort field for the filtered scan (indexed or not — the scan sorts in RAM).
    sort_field: &'static str,
}

impl CorpusSpec {
    fn journal_name(&self) -> &'static str {
        match self.name {
            "memories" => "memories",
            "notepad" => "notepad",
            "codereview" => "codereview",
            "synthetic-100k" => "synthetic",
            other => other,
        }
    }
}

fn corpora() -> Vec<CorpusSpec> {
    let root = PathBuf::from(
        std::env::var("NDB_BENCH_MCP_DATA")
            .unwrap_or_else(|_| r"D:\DEV\mcp_server\data".to_string()),
    );
    let real = |p: &str| -> Option<PathBuf> {
        let path = root.join(p);
        if path.is_file() {
            Some(path)
        } else {
            None
        }
    };
    vec![
        CorpusSpec {
            name: "memories",
            source: real(r"memories\data.jsonl"),
            eq_field: "category",
            range_field: "confidence",
            scan_field: "embedStatus",
            sort_field: "timestamp",
        },
        CorpusSpec {
            name: "notepad",
            source: real(r"notepad\data.jsonl"),
            eq_field: "type",
            range_field: "when",
            scan_field: "owner",
            sort_field: "updatedAt",
        },
        CorpusSpec {
            name: "codereview",
            source: real(r"codereview\db\data.jsonl"),
            eq_field: "class",
            range_field: "createdAt",
            scan_field: "status",
            sort_field: "createdAt",
        },
        CorpusSpec {
            name: "synthetic-100k",
            source: None,
            eq_field: "category",
            range_field: "score",
            scan_field: "status",
            sort_field: "score",
        },
    ]
}

// ─── Deterministic RNG (no new crate) ───────────────────────────────────────

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
    }
}

// ─── Working set (real RSS, no crate) ───────────────────────────────────────

#[cfg(windows)]
fn rss_bytes() -> Option<usize> {
    // Filled in by K32GetProcessMemoryInfo, which writes through the pointer —
    // the compiler cannot see individual field reads.
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

// ─── Minimal HTTP/1.1 keep-alive client (std only) ──────────────────────────

struct Http {
    stream: TcpStream,
    pending: Vec<u8>,
}

impl Http {
    fn connect(addr: SocketAddr) -> Result<Self, String> {
        let stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
        stream.set_nodelay(true).map_err(|e| format!("nodelay: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .map_err(|e| format!("read timeout: {e}"))?;
        Ok(Http {
            stream,
            pending: Vec::new(),
        })
    }

    fn call(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>), String> {
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: ndb-bench\r\nConnection: keep-alive\r\n"
        );
        if let Some(bytes) = body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                bytes.len()
            ));
        }
        head.push_str("\r\n");
        self.stream
            .write_all(head.as_bytes())
            .map_err(|e| format!("write head: {e}"))?;
        if let Some(bytes) = body {
            self.stream
                .write_all(bytes)
                .map_err(|e| format!("write body: {e}"))?;
        }
        self.stream.flush().map_err(|e| format!("flush: {e}"))?;
        self.read_response()
    }

    fn read_response(&mut self) -> Result<(u16, Vec<u8>), String> {
        let mut buf = std::mem::take(&mut self.pending);
        loop {
            if let Some(head_end) = find_headers_end(&buf) {
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut status: Option<u16> = None;
                let mut content_length: Option<usize> = None;
                for (i, line) in head.split("\r\n").enumerate() {
                    if i == 0 {
                        status = line
                            .split_whitespace()
                            .nth(1)
                            .and_then(|s| s.parse::<u16>().ok());
                    } else if let Some((k, v)) = line.split_once(':') {
                        if k.trim().eq_ignore_ascii_case("content-length") {
                            content_length =
                                Some(v.trim().parse().map_err(|_| format!("bad content-length: {v}"))?);
                        }
                        if k.trim().eq_ignore_ascii_case("transfer-encoding")
                            && v.trim().eq_ignore_ascii_case("chunked")
                        {
                            return Err("chunked response is not supported by this client".to_string());
                        }
                    }
                }
                let status = status.ok_or_else(|| format!("no status in response head: {head:?}"))?;
                let len = content_length
                    .ok_or_else(|| format!("response carries no Content-Length: {head:?}"))?;
                let body_start = head_end + 4;
                while buf.len() < body_start + len {
                    let mut chunk = [0u8; 64 * 1024];
                    let n = self
                        .stream
                        .read(&mut chunk)
                        .map_err(|e| format!("read body: {e}"))?;
                    if n == 0 {
                        return Err("connection closed mid-body".to_string());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = buf[body_start..body_start + len].to_vec();
                self.pending = buf[body_start + len..].to_vec();
                return Ok((status, body));
            }
            let mut chunk = [0u8; 64 * 1024];
            let n = self
                .stream
                .read(&mut chunk)
                .map_err(|e| format!("read head: {e}"))?;
            if n == 0 {
                return Err("connection closed while reading response head".to_string());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn must<T>(result: Result<T, String>, what: &str) -> T {
    match result {
        Ok(v) => v,
        Err(e) => panic!("{what}: {e}"),
    }
}

// ─── Synthetic journal ──────────────────────────────────────────────────────

fn body_text(len: usize) -> String {
    const WORDS: [&str; 8] = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ];
    let mut s = String::with_capacity(len + 16);
    let mut i = 0;
    while s.len() < len {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(WORDS[i % WORDS.len()]);
        i += 1;
    }
    s.truncate(len);
    s
}

fn generate_synthetic(path: &Path) {
    const DOCS: usize = 100_000;
    let categories = ["memory", "chat", "notepad", "codereview", "telemetry", "storage"];
    let statuses = ["active", "archived", "pending", "held"];
    let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
    let mut out = String::with_capacity(DOCS * 420);
    out.push_str(
        "{\"_meta\":{\"version\":1,\"created\":\"1763760000\",\
         \"source_epoch_uuid\":\"bench-phase3-synthetic\",\
         \"last_seq\":100000,\"min_available_seq\":1}}\n",
    );
    for i in 0..DOCS {
        let category = categories[rng.below(categories.len())];
        let status = statuses[rng.below(statuses.len())];
        let score = (rng.next_u64() % 100_000) as f64 / 100.0;
        let tags = 2 + rng.below(3);
        let mut tag_list = String::with_capacity(tags * 10);
        for t in 0..tags {
            if t > 0 {
                tag_list.push(',');
            }
            tag_list.push_str(&format!("\"tag-{:02}\"", rng.below(40)));
        }
        let body = body_text(160 + rng.below(120));
        out.push_str(&format!(
            "{{\"_id\":\"syn_{i:06}\",\"id\":{i},\"category\":\"{category}\",\
             \"score\":{score:.2},\"status\":\"{status}\",\"tags\":[{tag_list}],\
             \"body\":\"{body}\",\"_seq\":{}}}\n",
            i + 1,
        ));
    }
    fs::write(path, out.as_bytes()).expect("write synthetic journal");
}

// ─── Query plan resolution ──────────────────────────────────────────────────

enum Bounds {
    Num(f64, f64),
    Text(String, String),
}

/// The value that occurs most often in the sample: a filter that matches a
/// representative slice of the corpus instead of a one-off rarity whose
/// latency would say nothing about index routing.
fn most_common_scalar(sample: &[Value], field: &str) -> Option<Value> {
    let mut counts: Vec<(Value, usize)> = Vec::new();
    for value in sample.iter().filter_map(|doc| doc.get(field)) {
        let usable = value.is_number() || matches!(value, Value::String(s) if !s.is_empty());
        if !usable {
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

fn range_bounds(sample: &[Value], field: &str) -> Option<Bounds> {
    let mut nums: Vec<f64> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for doc in sample {
        match doc.get(field) {
            Some(Value::Number(n)) => {
                if let Some(f) = n.as_f64() {
                    nums.push(f);
                }
            }
            Some(Value::String(s)) if !s.is_empty() => texts.push(s.clone()),
            _ => {}
        }
    }
    if nums.len() >= 4 {
        nums.sort_by(|a, b| a.partial_cmp(b).expect("finite range values"));
        let lo = nums[nums.len() / 4];
        let hi = nums[nums.len() * 3 / 4];
        return Some(Bounds::Num(lo, hi));
    }
    if nums.len() >= 2 {
        let lo = nums.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        return Some(Bounds::Num(lo, hi));
    }
    if texts.len() >= 4 {
        texts.sort();
        return Some(Bounds::Text(
            texts[texts.len() / 4].clone(),
            texts[texts.len() * 3 / 4].clone(),
        ));
    }
    if texts.len() >= 2 {
        texts.sort();
        return Some(Bounds::Text(texts[0].clone(), texts[texts.len() - 1].clone()));
    }
    None
}

fn condition(field: &str, ops: Vec<(&str, Value)>) -> Value {
    let mut inner = serde_json::Map::new();
    for (op, value) in ops {
        inner.insert(op.to_string(), value);
    }
    let mut filter = serde_json::Map::new();
    filter.insert(field.to_string(), Value::Object(inner));
    Value::Object(filter)
}

fn sampled_docs(db: &Database, ids: &[String], n: usize) -> Vec<Value> {
    let stride = (ids.len() / n).max(1);
    ids.iter()
        .step_by(stride)
        .take(n)
        .map(|id| db.get(id).expect("sampled document"))
        .collect()
}

// ─── Measurement helpers ────────────────────────────────────────────────────

fn time_get(db: &Database, ids: &[String], rounds: usize) -> Dist {
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE);
    for _ in 0..32 {
        let _ = db.get(&ids[rng.below(ids.len())]).expect("get warm-up");
    }
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let id = &ids[rng.below(ids.len())];
        let started = Instant::now();
        let doc = db.get(id).expect("get");
        samples.push(started.elapsed().as_secs_f64() * 1e6);
        std::hint::black_box(&doc);
    }
    dist(samples)
}

fn time_mget(http: &mut Http, db_name: &str, ids: &[String], rounds: usize) -> Dist {
    let path = format!("/v2/dbs/{db_name}/docs/mget");
    let mut rng = Rng::new(0xFEED_FACE_1234);
    let mut batch_ids = || -> Vec<&str> {
        (0..MGET_BATCH)
            .map(|_| ids[rng.below(ids.len())].as_str())
            .collect()
    };
    for _ in 0..5 {
        let body = serde_json::to_vec(&json!({"ids": batch_ids()})).expect("encode mget");
        let (status, resp) = must(http.call("POST", &path, Some(&body)), "mget warm-up");
        assert!(
            status == 200,
            "mget warm-up -> {status}: {}",
            String::from_utf8_lossy(&resp)
        );
    }
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let body = serde_json::to_vec(&json!({"ids": batch_ids()})).expect("encode mget");
        let started = Instant::now();
        let (status, resp) = must(http.call("POST", &path, Some(&body)), "mget");
        samples.push(started.elapsed().as_secs_f64() * 1e6);
        assert!(status == 200, "mget -> {status}: {}", String::from_utf8_lossy(&resp));
    }
    dist(samples)
}

/// Timed `query_projected` runs. Rounds are chosen from the warm-up cost so a
/// slow scan stays bounded while a fast indexed lookup still gets enough
/// samples for a stable p95.
fn time_query(db: &Database, filter: &Value, opts: &QueryOptions) -> (Dist, usize, usize) {
    let started = Instant::now();
    let (mut matched, mut rows) = db
        .query_projected(filter.clone(), opts.clone(), None)
        .expect("query warm-up");
    let warm_secs = started.elapsed().as_secs_f64();
    std::hint::black_box(&rows);
    let rounds = ((QUERY_BUDGET_SECS / warm_secs) as usize).clamp(5, MAX_QUERY_ROUNDS);
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        let (total, page) = db
            .query_projected(filter.clone(), opts.clone(), None)
            .expect("query");
        samples.push(started.elapsed().as_secs_f64() * 1e6);
        matched = total;
        rows = page;
        std::hint::black_box(&rows);
    }
    (dist(samples), matched, rounds)
}

fn write_load(addr: SocketAddr, db_name: &str, targets: &[(String, Value)]) -> (f64, Dist) {
    let barrier = Arc::new(Barrier::new(WRITER_THREADS));
    let mut handles = Vec::with_capacity(WRITER_THREADS);
    for t in 0..WRITER_THREADS {
        let mine: Vec<(String, Value)> = targets
            .iter()
            .skip(t)
            .step_by(WRITER_THREADS)
            .cloned()
            .collect();
        assert!(!mine.is_empty(), "writer {t} got no target");
        let barrier = Arc::clone(&barrier);
        let db_name = db_name.to_string();
        handles.push(std::thread::spawn(move || -> (Duration, Vec<f64>) {
            let mut http = must(Http::connect(addr), "writer connect");
            barrier.wait();
            let started = Instant::now();
            let mut latencies = Vec::with_capacity(PUTS_PER_THREAD + PATCHES_PER_THREAD);
            for i in 0..PUTS_PER_THREAD {
                let (id, base) = &mine[i % mine.len()];
                let mut doc = base.clone();
                doc["bench_puts"] = json!(i);
                let body = serde_json::to_vec(&doc).expect("encode PUT");
                let path = format!("/v2/dbs/{db_name}/doc/{id}");
                let t0 = Instant::now();
                let (status, resp) = must(http.call("PUT", &path, Some(&body)), "PUT");
                latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
                assert!(status == 200, "PUT {id} -> {status}: {}", String::from_utf8_lossy(&resp));
            }
            let patch = serde_json::to_vec(&json!({
                "ops": [{"op": "set", "path": "bench_patches", "value": 1}]
            }))
            .expect("encode PATCH");
            for i in 0..PATCHES_PER_THREAD {
                let (id, _) = &mine[i % mine.len()];
                let path = format!("/v2/dbs/{db_name}/doc/{id}");
                let t0 = Instant::now();
                let (status, resp) = must(http.call("PATCH", &path, Some(&patch)), "PATCH");
                latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
                assert!(
                    status == 200,
                    "PATCH {id} -> {status}: {}",
                    String::from_utf8_lossy(&resp)
                );
            }
            (started.elapsed(), latencies)
        }));
    }
    let mut wall = 0.0f64;
    let mut latencies = Vec::with_capacity(WRITER_THREADS * (PUTS_PER_THREAD + PATCHES_PER_THREAD));
    for handle in handles {
        let (elapsed, mut lat) = handle.join().expect("writer thread panicked");
        wall = wall.max(elapsed.as_secs_f64());
        latencies.append(&mut lat);
    }
    let ops = (WRITER_THREADS * (PUTS_PER_THREAD + PATCHES_PER_THREAD)) as f64;
    (ops / wall, dist(latencies))
}

// ─── One corpus ─────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct Row {
    corpus: String,
    /// `v1` or `v2` — the storage engine that produced this row.
    mode: String,
    file_mb: f64,
    docs: usize,
    open_ms: f64,
    index_ms: f64,
    rss_delta_mb: f64,
    rss_final_mb: f64,
    get: Dist,
    mget: Dist,
    eq: Dist,
    range: Dist,
    scan: Dist,
    eq_matched: usize,
    range_matched: usize,
    scan_matched: usize,
    eq_rounds: usize,
    range_rounds: usize,
    scan_rounds: usize,
    ops_per_s: f64,
    write: Dist,
    compact_ms: f64,
    file_mb_after: f64,
    /// `compact()` wall clock while 4 writer threads were mid-flight (v2 only).
    compact_load_ms: f64,
    /// Writer p99 during that compaction window (v2 only).
    write_load_p99: f64,
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl Row {
    fn to_tsv(&self) -> String {
        let f = |v: f64| format!("{v:.6}");
        [
            self.corpus.clone(),
            self.mode.clone(),
            f(self.file_mb),
            self.docs.to_string(),
            f(self.open_ms),
            f(self.index_ms),
            f(self.rss_delta_mb),
            f(self.rss_final_mb),
            f(self.get.p50),
            f(self.get.p95),
            f(self.mget.p50),
            f(self.mget.p95),
            f(self.eq.p50),
            f(self.eq.p95),
            f(self.range.p50),
            f(self.range.p95),
            f(self.scan.p50),
            f(self.scan.p95),
            f(self.ops_per_s),
            f(self.write.p50),
            f(self.write.p95),
            f(self.write.p99),
            f(self.compact_ms),
            f(self.file_mb_after),
            f(self.compact_load_ms),
            f(self.write_load_p99),
        ]
        .join("\t")
    }

    fn from_tsv(line: &str) -> Row {
        const FIELDS: usize = 26;
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            fields.len(),
            FIELDS,
            "summary line has {} fields, want {FIELDS}",
            fields.len()
        );
        let num = |i: usize| -> f64 {
            fields[i]
                .parse()
                .unwrap_or_else(|e| panic!("summary field {i} ('{}') is not a number: {e}", fields[i]))
        };
        let dist = |a: usize, b: usize, c: usize| Dist {
            p50: num(a),
            p95: num(b),
            p99: num(c),
        };
        Row {
            corpus: fields[0].to_string(),
            mode: fields[1].to_string(),
            file_mb: num(2),
            docs: fields[3].parse().expect("docs"),
            open_ms: num(4),
            index_ms: num(5),
            rss_delta_mb: num(6),
            rss_final_mb: num(7),
            get: dist(8, 9, 9),
            mget: dist(10, 11, 11),
            eq: dist(12, 13, 13),
            range: dist(14, 15, 15),
            scan: dist(16, 17, 17),
            eq_matched: 0,
            range_matched: 0,
            scan_matched: 0,
            eq_rounds: 0,
            range_rounds: 0,
            scan_rounds: 0,
            ops_per_s: num(18),
            write: dist(19, 20, 21),
            compact_ms: num(22),
            file_mb_after: num(23),
            compact_load_ms: num(24),
            write_load_p99: num(25),
        }
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn run_corpus(spec: &CorpusSpec, root: &Path) -> Row {
    let dir = root.join(spec.journal_name());
    fs::create_dir_all(&dir).expect("create corpus dir");
    let journal = dir.join("data.jsonl");
    match &spec.source {
        Some(src) => {
            fs::copy(src, &journal).unwrap_or_else(|e| {
                panic!("copy {} -> {}: {e}", src.display(), journal.display())
            });
        }
        None => generate_synthetic(&journal),
    }
    let file_bytes = fs::metadata(&journal).expect("stat journal").len();

    let rss_before = rss_bytes().expect("working-set size is required for the RSS metric");
    let started = Instant::now();
    let registry = DbRegistry::new(None, PersistenceMode::Immediate, 5_000);
    registry
        .mount_with(spec.name, &journal, PersistenceMode::Immediate)
        .unwrap_or_else(|e| panic!("open {}: {e}", spec.name));
    let open_ms = ms(started.elapsed());
    let db = registry.get(spec.name).expect("mounted database");

    let started = Instant::now();
    db.create_index("id").expect("index id");
    db.create_index("category").expect("index category");
    db.create_btree_index("score").expect("btree score");
    let mut index_names = vec!["id hash".to_string(), "category hash".to_string(), "score btree".to_string()];
    if spec.eq_field != "category" {
        db.create_index(spec.eq_field).expect("index eq field");
        index_names.push(format!("{} hash", spec.eq_field));
    }
    if spec.range_field != "score" {
        db.create_btree_index(spec.range_field).expect("btree range field");
        index_names.push(format!("{} btree", spec.range_field));
    }
    let index_ms = ms(started.elapsed());
    let rss_after = rss_bytes().expect("working-set size is required for the RSS metric");

    let ids = db.get_all_ids();
    assert!(!ids.is_empty(), "{} replayed to zero documents", spec.name);
    // `get_all_ids` is a HashMap walk; sorting freezes the sample, the random
    // lookups and the writer targets across runs.
    let mut ids = ids;
    ids.sort();
    let sample = sampled_docs(&db, &ids, 400);
    let eq_value = most_common_scalar(&sample, spec.eq_field).unwrap_or_else(|| {
        panic!(
            "{}: no scalar value for eq field '{}' in 400 sampled docs",
            spec.name, spec.eq_field
        )
    });
    let bounds = range_bounds(&sample, spec.range_field).unwrap_or_else(|| {
        panic!(
            "{}: fewer than two comparable values for range field '{}' in 400 sampled docs",
            spec.name, spec.range_field
        )
    });
    let scan_value = most_common_scalar(&sample, spec.scan_field).unwrap_or_else(|| {
        panic!(
            "{}: no scalar value for scan field '{}' in 400 sampled docs",
            spec.name, spec.scan_field
        )
    });

    let eq_filter = condition(spec.eq_field, vec![("$eq", eq_value.clone())]);
    let (range_filter, range_label) = match &bounds {
        Bounds::Num(lo, hi) => (
            condition(
                spec.range_field,
                vec![
                    ("$gte", json!(lo)),
                    ("$lte", json!(hi)),
                ],
            ),
            format!("{} in [{lo:.4}, {hi:.4}]", spec.range_field),
        ),
        Bounds::Text(lo, hi) => (
            condition(
                spec.range_field,
                vec![("$gte", json!(lo)), ("$lte", json!(hi))],
            ),
            format!("{} in [{lo}, {hi}]", spec.range_field),
        ),
    };
    let scan_filter = condition(spec.scan_field, vec![("$eq", scan_value.clone())]);

    // Serve the same `Arc<Database>` over HTTP for mget + concurrent writers.
    let mut config = ServerConfig::default();
    config.bind = "127.0.0.1".to_string();
    config.port = 0;
    config.max_connections = 128;
    let server = serve_multi_in_background(Arc::new(registry), config).expect("start ndb serve");
    let addr = server.addr;

    let mut http = must(Http::connect(addr), "bench http connect");
    let (status, body) = must(http.call("GET", &format!("/v2/dbs/{}/stats", spec.name), None), "stats");
    assert!(status == 200, "stats -> {status}: {}", String::from_utf8_lossy(&body));
    let stats: Value = serde_json::from_slice(&body).expect("decode stats");
    assert_eq!(
        stats["documents"].as_u64().map(|n| n as usize),
        Some(db.len()),
        "HTTP doc count disagrees with the local handle"
    );

    let get = time_get(&db, &ids, GET_ROUNDS);
    let mget = time_mget(&mut http, spec.name, &ids, MGET_ROUNDS);

    let eq_opts = QueryOptions::default();
    let (eq, eq_matched, eq_rounds) = time_query(&db, &eq_filter, &eq_opts);
    let (range, range_matched, range_rounds) = time_query(&db, &range_filter, &eq_opts);
    let scan_opts = QueryOptions {
        limit: Some(20),
        offset: None,
        sort_by: Some((spec.sort_field.to_string(), SortDir::Desc)),
    };
    let (scan, scan_matched, scan_rounds) = time_query(&db, &scan_filter, &scan_opts);

    let mut target_ids: Vec<String> = ids.iter().take(WRITE_TARGETS).cloned().collect();
    let mut cursor = 0;
    while target_ids.len() < WRITE_TARGETS {
        target_ids.push(ids[cursor % ids.len()].clone());
        cursor += 1;
    }
    let targets: Vec<(String, Value)> = target_ids
        .into_iter()
        .map(|id| {
            let doc = db.get(&id).expect("writer target doc");
            (id, doc)
        })
        .collect();
    let (ops_per_s, write) = write_load(addr, spec.name, &targets);

    let started = Instant::now();
    db.compact().expect("compact");
    let compact_ms = ms(started.elapsed());
    let file_after = fs::metadata(&journal).expect("stat after compact").len();
    let rss_final = rss_bytes().expect("working-set size is required for the RSS metric");
    server.shutdown();

    let rss_delta_mb = rss_after.saturating_sub(rss_before) as f64 / (1024.0 * 1024.0);
    let rss_final_mb = rss_final as f64 / (1024.0 * 1024.0);

    let row = Row {
        corpus: spec.name.to_string(),
        mode: "v1".to_string(),
        file_mb: mb(file_bytes),
        docs: db.len(),
        open_ms,
        index_ms,
        rss_delta_mb,
        rss_final_mb,
        get,
        mget,
        eq,
        range,
        scan,
        eq_matched,
        range_matched,
        scan_matched,
        eq_rounds,
        range_rounds,
        scan_rounds,
        ops_per_s,
        write,
        compact_ms,
        file_mb_after: mb(file_after),
        compact_load_ms: 0.0,
        write_load_p99: 0.0,
    };
    print_corpus(spec, &row, &index_names, &eq_value, &range_label, &scan_value);
    row
}

fn print_corpus(
    spec: &CorpusSpec,
    row: &Row,
    indexes: &[String],
    eq_value: &Value,
    range_label: &str,
    scan_value: &Value,
) {
    println!("── {} ─────────────────────────────────────────", row.corpus);
    println!(
        "  journal            {:>8.1} MB   docs {:>7}",
        row.file_mb, row.docs
    );
    println!("  cold boot          {:>8.1} ms   (Database::open + journal replay)", row.open_ms);
    println!("  index build        {:>8.1} ms   ({})", row.index_ms, indexes.join(", "));
    println!(
        "  RSS delta          {:>+8.1} MB   (final working set {:.1} MB)",
        row.rss_delta_mb, row.rss_final_mb
    );
    println!(
        "  get                {:>8.1} µs p50 / {:>8.1} µs p95   ({} random lookups)",
        row.get.p50, row.get.p95, GET_ROUNDS
    );
    println!(
        "  mget({MGET_BATCH}) HTTP     {:>8.1} µs p50 / {:>8.1} µs p95   ({MGET_ROUNDS} keep-alive rounds)",
        row.mget.p50, row.mget.p95
    );
    println!(
        "  query $eq          {:>8.1} µs p50 / {:>8.1} µs p95   ({} = {}, n={}, {} rounds)",
        row.eq.p50, row.eq.p95, spec.eq_field, eq_value, row.eq_matched, row.eq_rounds
    );
    println!(
        "  query range        {:>8.1} µs p50 / {:>8.1} µs p95   ({}, n={}, {} rounds)",
        row.range.p50, row.range.p95, range_label, row.range_matched, row.range_rounds
    );
    println!(
        "  scan+sort+limit20  {:>8.3} ms p50 / {:>8.3} ms p95   ({} = {}, sort {} desc, n={}, {} rounds)",
        row.scan.p50 / 1000.0,
        row.scan.p95 / 1000.0,
        spec.scan_field,
        scan_value,
        spec.sort_field,
        row.scan_matched,
        row.scan_rounds
    );
    println!(
        "  writers Immediate  {:>8.1} ops/s   p50 {:.2} ms / p95 {:.2} ms / p99 {:.2} ms   (8 × 25 PUT + 25 PATCH)",
        row.ops_per_s, row.write.p50, row.write.p95, row.write.p99
    );
    println!(
        "  compact            {:>8.1} ms   {:.2} MB → {:.2} MB",
        row.compact_ms, row.file_mb, row.file_mb_after
    );
    println!();
}

// ─── Summary table ──────────────────────────────────────────────────────────

fn print_table(rows: &[Row]) {
    const COLUMNS: usize = 17;
    println!("=== Phase 3 summary (v1 = JSONL, v2 = binary/mmap) ===");
    println!(
        "| corpus | mode | journal MB | docs | open ms | index ms | RSS Δ MB | get p50/p95 µs | mget20 p50/p95 µs | eq p50/p95 µs | range p50/p95 µs | scan p50/p95 ms | writers ops/s | w p50/p95/p99 ms | compact ms | after MB | compact-under-load ms / writer p99 ms |"
    );
    println!("{}", "|---".repeat(COLUMNS) + "|");
    for r in rows {
        println!(
            "| {} | {} | {:.2} | {} | {:.1} | {:.1} | {:+.1} | {:.1} / {:.1} | {:.1} / {:.1} | {:.1} / {:.1} | {:.1} / {:.1} | {:.3} / {:.3} | {:.1} | {:.2} / {:.2} / {:.2} | {:.1} | {:.2} | {} |",
            r.corpus,
            r.mode,
            r.file_mb,
            r.docs,
            r.open_ms,
            r.index_ms,
            r.rss_delta_mb,
            r.get.p50,
            r.get.p95,
            r.mget.p50,
            r.mget.p95,
            r.eq.p50,
            r.eq.p95,
            r.range.p50,
            r.range.p95,
            r.scan.p50 / 1000.0,
            r.scan.p95 / 1000.0,
            r.ops_per_s,
            r.write.p50,
            r.write.p95,
            r.write.p99,
            r.compact_ms,
            r.file_mb_after,
            if r.mode == "v2" {
                format!("{:.1} / {:.2}", r.compact_load_ms, r.write_load_p99)
            } else {
                "—".to_string()
            },
        );
    }
}

/// The Phase 3 exit gates, evaluated per corpus from the paired rows.
fn print_gates(v1: &[Row], v2: &[Row]) {
    const OPEN_GATE: f64 = 5.0;
    const RSS_GATE: f64 = 3.0;
    println!("=== Phase 3 exit gates (v1 → v2) ===");
    println!("| corpus | cold boot ms v1 → v2 | speedup (≥5×) | RSS Δ MB v1 → v2 | reduction (≥3×) | writers ops/s v1 → v2 | eq p95 v1 → v2 | range p95 v1 → v2 | scan p95 v1 → v2 |");
    println!("{}", "|---".repeat(9) + "|");
    let mut all = true;
    for a in v1 {
        let Some(b) = v2.iter().find(|r| r.corpus == a.corpus) else {
            continue;
        };
        let speedup = a.open_ms / b.open_ms;
        let rss_ratio = if b.rss_delta_mb > 0.0 {
            a.rss_delta_mb / b.rss_delta_mb
        } else {
            f64::INFINITY
        };
        let writer_ratio = b.ops_per_s / a.ops_per_s;
        let regress = |x: f64, y: f64| {
            if y <= x * 1.10 {
                "ok"
            } else {
                "REGRESSION"
            }
        };
        let pass = speedup >= OPEN_GATE
            && rss_ratio >= RSS_GATE
            && writer_ratio > 1.0
            && b.eq.p95 <= a.eq.p95 * 1.10
            && b.range.p95 <= a.range.p95 * 1.10
            && b.scan.p95 <= a.scan.p95 * 1.10;
        all &= pass;
        println!(
            "| {} | {:.1} → {:.1} | {:.2}× {} | {:+.1} → {:+.1} | {:.2}× {} | {:.1} → {:.1} ({:.2}×) | {:.1} → {:.1} {} | {:.1} → {:.1} {} | {:.3} → {:.3} {} |",
            a.corpus,
            a.open_ms,
            b.open_ms,
            speedup,
            if speedup >= OPEN_GATE { "PASS" } else { "FAIL" },
            a.rss_delta_mb,
            b.rss_delta_mb,
            rss_ratio,
            if rss_ratio >= RSS_GATE { "PASS" } else { "FAIL" },
            a.ops_per_s,
            b.ops_per_s,
            writer_ratio,
            a.eq.p50,
            b.eq.p50,
            regress(a.eq.p95, b.eq.p95),
            a.range.p50,
            b.range.p50,
            regress(a.range.p95, b.range.p95),
            a.scan.p50 / 1000.0,
            b.scan.p50 / 1000.0,
            regress(a.scan.p95, b.scan.p95),
        );
    }
    println!();
    println!(
        "Overall: {}",
        if all {
            "every Phase 3 gate is met"
        } else {
            "at least one gate failed — see the table"
        }
    );
}

// ─── v2 preparation (its own process, so measurement starts cold) ───────────

fn ndb_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_ndb") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().expect("resolve current executable");
    let sibling = exe
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join(if cfg!(windows) { "ndb.exe" } else { "ndb" }))
        .filter(|path| path.is_file());
    sibling.unwrap_or_else(|| {
        panic!(
            "cannot locate the ndb binary next to {}; run `cargo build --release` first",
            exe.display()
        )
    })
}

fn v2_dir(spec: &CorpusSpec, workdir: &Path) -> PathBuf {
    workdir.join(format!("{}-v2", spec.journal_name()))
}

fn base_and_wal_bytes(dir: &Path) -> u64 {
    let manifest = ndb::read_manifest(dir).expect("read v2 manifest");
    [ndb::base_file_name(manifest.base_gen), ndb::wal_file_name(manifest.wal_gen)]
        .iter()
        .map(|name| fs::metadata(dir.join(name)).map(|m| m.len()).unwrap_or(0))
        .sum()
}

fn create_standard_indexes(db: &Database, spec: &CorpusSpec) -> Vec<String> {
    db.create_index("id").expect("index id");
    db.create_index("category").expect("index category");
    db.create_btree_index("score").expect("btree score");
    let mut names = vec![
        "id hash".to_string(),
        "category hash".to_string(),
        "score btree".to_string(),
    ];
    if spec.eq_field != "category" {
        db.create_index(spec.eq_field).expect("index eq field");
        names.push(format!("{} hash", spec.eq_field));
    }
    if spec.range_field != "score" {
        db.create_btree_index(spec.range_field).expect("btree range field");
        names.push(format!("{} btree", spec.range_field));
    }
    names
}

/// Copy/generate the v1 corpus, `ndb migrate` it, build the standard indexes
/// and compact — so the persisted index sections live in `base.gen`.
fn prepare_v2(spec: &CorpusSpec, workdir: &Path) {
    let dir = v2_dir(spec, workdir);
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("clear a previous v2 corpus");
    }
    fs::create_dir_all(&dir).expect("create v2 corpus dir");
    let journal = dir.join("data.jsonl");
    match &spec.source {
        Some(src) => {
            fs::copy(src, &journal)
                .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", src.display(), journal.display()));
        }
        None => generate_synthetic(&journal),
    };
    let source_bytes = fs::metadata(&journal).expect("stat v1 journal").len();

    println!("── {} (v2 preparation) ──", spec.name);
    let migrate_started = Instant::now();
    let output = std::process::Command::new(ndb_binary())
        .arg("migrate")
        .arg(&dir)
        .output()
        .unwrap_or_else(|e| panic!("spawn ndb migrate: {e}"));
    let migrate_ms = ms(migrate_started.elapsed());
    if !output.status.success() {
        panic!(
            "ndb migrate {} failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            dir.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let db = Database::open_v2_with_options(
        &dir,
        ndb::OpenOptions {
            format: ndb::StorageFormat::V2,
            persistence: ndb::Persistence::Immediate,
        },
    )
    .expect("open migrated v2 corpus");
    let index_started = Instant::now();
    create_standard_indexes(&db, spec);
    let index_ms = ms(index_started.elapsed());
    let compact_started = Instant::now();
    db.compact().expect("compact the prepared v2 corpus");
    let compact_ms = ms(compact_started.elapsed());
    let docs = db.len();
    drop(db);

    let prep = json!({
        "migrate_ms": migrate_ms,
        "index_ms": index_ms,
        "compact_ms": compact_ms,
        "docs": docs,
        "source_mb": mb(source_bytes),
        "v2_mb": mb(base_and_wal_bytes(&dir)),
    });
    fs::write(dir.join("prep.json"), prep.to_string()).expect("write prep.json");
    println!(
        "  prepared: {} docs, v1 {:.2} MB → v2 {:.2} MB   migrate {:.1} ms, index {:.1} ms, compact {:.1} ms",
        docs, mb(source_bytes), mb(base_and_wal_bytes(&dir)), migrate_ms, index_ms, compact_ms
    );
}

// ─── v1 vs v2 head-to-head tail ─────────────────────────────────────────────

/// `compact()` wall clock while four writer threads are mid-flight. The v2
/// compaction is non-blocking: writers keep committing through the mirror WAL,
/// so their p99 during the window is the stall the caller actually feels.
fn compact_under_load(
    db: &Arc<Database>,
    addr: SocketAddr,
    db_name: &str,
    targets: &[(String, Value)],
) -> (f64, Dist) {
    const THREADS: usize = 4;
    const PUTS: usize = 10;
    let barrier = Arc::new(Barrier::new(THREADS + 1));
    let mut handles = Vec::with_capacity(THREADS);
    for t in 0..THREADS {
        let mine: Vec<(String, Value)> = targets
            .iter()
            .skip(t)
            .step_by(THREADS)
            .take(PUTS)
            .cloned()
            .collect();
        assert_eq!(mine.len(), PUTS, "compaction writer {t} has no targets");
        let barrier = Arc::clone(&barrier);
        let db_name = db_name.to_string();
        handles.push(std::thread::spawn(move || -> Vec<f64> {
            let mut http = must(Http::connect(addr), "compaction writer connect");
            let mut latencies = Vec::with_capacity(PUTS);
            barrier.wait();
            for (i, (id, base)) in mine.iter().enumerate() {
                let mut doc = base.clone();
                doc["compact_overlap"] = json!(i);
                let body = serde_json::to_vec(&doc).expect("encode PUT");
                let path = format!("/v2/dbs/{db_name}/doc/{id}");
                let t0 = Instant::now();
                let (status, resp) = must(http.call("PUT", &path, Some(&body)), "PUT under compaction");
                latencies.push(t0.elapsed().as_secs_f64() * 1000.0);
                assert!(status == 200, "PUT -> {status}: {}", String::from_utf8_lossy(&resp));
            }
            latencies
        }));
    }
    barrier.wait();
    std::thread::sleep(Duration::from_millis(5));
    let started = Instant::now();
    db.compact().expect("compact under load");
    let compact_ms = ms(started.elapsed());
    let mut latencies = Vec::with_capacity(THREADS * PUTS);
    for handle in handles {
        latencies.append(&mut handle.join().expect("compaction writer panicked"));
    }
    (compact_ms, dist(latencies))
}

/// Measure one prepared v2 corpus in a fresh process: cold open (persisted
/// indexes, no rebuild), RSS, reads, group-commit writers and compaction.
fn run_corpus_v2(spec: &CorpusSpec, workdir: &Path) -> Row {
    let dir = v2_dir(spec, workdir);
    let prep: Value = serde_json::from_str(
        &fs::read_to_string(dir.join("prep.json")).unwrap_or_else(|e| {
            panic!(
                "{} has no prep.json ({e}); run with --mode both or --mode v2-prep first",
                dir.display()
            )
        }),
    )
    .expect("parse prep.json");
    let file_bytes = base_and_wal_bytes(&dir);
    let source_mb = prep["source_mb"].as_f64().unwrap_or(0.0);

    let rss_before = rss_bytes().expect("working-set size is required for the RSS metric");
    let started = Instant::now();
    let registry = DbRegistry::new(None, PersistenceMode::Immediate, 5_000);
    registry
        .mount_with(spec.name, &dir, PersistenceMode::Immediate)
        .unwrap_or_else(|e| panic!("open v2 {}: {e}", spec.name));
    let open_ms = ms(started.elapsed());
    let db = registry.get(spec.name).expect("mounted v2 database");
    let rss_after = rss_bytes().expect("working-set size is required for the RSS metric");

    assert!(db.is_v2(), "{} is not on the v2 backend", spec.name);
    let ids = db.get_all_ids();
    assert!(!ids.is_empty(), "{} opened to zero documents", spec.name);
    let mut ids = ids;
    ids.sort();
    let sample = sampled_docs(&db, &ids, 400);
    let eq_value = most_common_scalar(&sample, spec.eq_field).unwrap_or_else(|| {
        panic!("{}: no scalar value for eq field '{}'", spec.name, spec.eq_field)
    });
    let bounds = range_bounds(&sample, spec.range_field).unwrap_or_else(|| {
        panic!(
            "{}: fewer than two comparable values for range field '{}'",
            spec.name, spec.range_field
        )
    });
    let scan_value = most_common_scalar(&sample, spec.scan_field).unwrap_or_else(|| {
        panic!("{}: no scalar value for scan field '{}'", spec.name, spec.scan_field)
    });

    let eq_filter = condition(spec.eq_field, vec![("$eq", eq_value.clone())]);
    let (range_filter, range_label) = match &bounds {
        Bounds::Num(lo, hi) => (
            condition(spec.range_field, vec![("$gte", json!(lo)), ("$lte", json!(hi))]),
            format!("{} in [{lo:.4}, {hi:.4}]", spec.range_field),
        ),
        Bounds::Text(lo, hi) => (
            condition(spec.range_field, vec![("$gte", json!(lo)), ("$lte", json!(hi))]),
            format!("{} in [{lo}, {hi}]", spec.range_field),
        ),
    };
    let scan_filter = condition(spec.scan_field, vec![("$eq", scan_value.clone())]);

    let mut config = ServerConfig::default();
    config.bind = "127.0.0.1".to_string();
    config.port = 0;
    config.max_connections = 128;
    let server = serve_multi_in_background(Arc::new(registry), config).expect("start ndb serve");
    let addr = server.addr;

    let mut http = must(Http::connect(addr), "bench http connect");
    let (status, body) = must(
        http.call("GET", &format!("/v2/dbs/{}/stats", spec.name), None),
        "stats",
    );
    assert!(status == 200, "stats -> {status}: {}", String::from_utf8_lossy(&body));

    let get = time_get(&db, &ids, GET_ROUNDS);
    let mget = time_mget(&mut http, spec.name, &ids, MGET_ROUNDS);

    let eq_opts = QueryOptions::default();
    let (eq, eq_matched, eq_rounds) = time_query(&db, &eq_filter, &eq_opts);
    let (range, range_matched, range_rounds) = time_query(&db, &range_filter, &eq_opts);
    let scan_opts = QueryOptions {
        limit: Some(20),
        offset: None,
        sort_by: Some((spec.sort_field.to_string(), SortDir::Desc)),
    };
    let (scan, scan_matched, scan_rounds) = time_query(&db, &scan_filter, &scan_opts);

    let mut target_ids: Vec<String> = ids.iter().take(WRITE_TARGETS).cloned().collect();
    let mut cursor = 0;
    while target_ids.len() < WRITE_TARGETS {
        target_ids.push(ids[cursor % ids.len()].clone());
        cursor += 1;
    }
    let targets: Vec<(String, Value)> = target_ids
        .into_iter()
        .map(|id| {
            let doc = db.get(&id).expect("writer target doc");
            (id, doc)
        })
        .collect();
    let (ops_per_s, write) = write_load(addr, spec.name, &targets);

    let (compact_load_ms, load_write) =
        compact_under_load(&db, addr, spec.name, &targets);

    let started = Instant::now();
    db.compact().expect("compact");
    let compact_ms = ms(started.elapsed());
    let file_after = base_and_wal_bytes(&dir);
    let rss_final = rss_bytes().expect("working-set size is required for the RSS metric");
    server.shutdown();

    let index_names = {
        let mut names = vec![
            "id hash".to_string(),
            "category hash".to_string(),
            "score btree".to_string(),
        ];
        if spec.eq_field != "category" {
            names.push(format!("{} hash", spec.eq_field));
        }
        if spec.range_field != "score" {
            names.push(format!("{} btree", spec.range_field));
        }
        names
    };

    let row = Row {
        corpus: spec.name.to_string(),
        mode: "v2".to_string(),
        file_mb: mb(file_bytes),
        docs: db.len(),
        open_ms,
        // Persisted in `base.gen`: nothing to rebuild on open.
        index_ms: 0.0,
        rss_delta_mb: rss_after.saturating_sub(rss_before) as f64 / (1024.0 * 1024.0),
        rss_final_mb: rss_final as f64 / (1024.0 * 1024.0),
        get,
        mget,
        eq,
        range,
        scan,
        eq_matched,
        range_matched,
        scan_matched,
        eq_rounds,
        range_rounds,
        scan_rounds,
        ops_per_s,
        write,
        compact_ms,
        file_mb_after: mb(file_after),
        compact_load_ms,
        write_load_p99: load_write.p99,
    };

    println!("── {} (v2) ─────────────────────────────────────", row.corpus);
    println!(
        "  v1 corpus          {:>8.2} MB   v2 base+WAL {:.2} MB   docs {:>7}",
        source_mb, row.file_mb, row.docs
    );
    println!(
        "  prepare            migrate {:.1} ms, index {:.1} ms, compact {:.1} ms",
        prep["migrate_ms"].as_f64().unwrap_or(0.0),
        prep["index_ms"].as_f64().unwrap_or(0.0),
        prep["compact_ms"].as_f64().unwrap_or(0.0)
    );
    println!("  cold boot          {:>8.1} ms   (open + mmap base, indexes restored)", row.open_ms);
    println!(
        "  index build        {:>8.1} ms   (persisted in base.gen: {} — no rebuild)",
        row.index_ms,
        index_names.join(", ")
    );
    println!(
        "  RSS delta          {:>+8.1} MB   (final working set {:.1} MB)",
        row.rss_delta_mb, row.rss_final_mb
    );
    println!(
        "  get                {:>8.1} µs p50 / {:>8.1} µs p95   ({} random lookups)",
        row.get.p50, row.get.p95, GET_ROUNDS
    );
    println!(
        "  mget({MGET_BATCH}) HTTP     {:>8.1} µs p50 / {:>8.1} µs p95   ({MGET_ROUNDS} keep-alive rounds)",
        row.mget.p50, row.mget.p95
    );
    println!(
        "  query $eq          {:>8.1} µs p50 / {:>8.1} µs p95   ({} = {}, n={}, {} rounds)",
        row.eq.p50, row.eq.p95, spec.eq_field, eq_value, row.eq_matched, row.eq_rounds
    );
    println!(
        "  query range        {:>8.1} µs p50 / {:>8.1} µs p95   ({}, n={}, {} rounds)",
        row.range.p50, row.range.p95, range_label, row.range_matched, row.range_rounds
    );
    println!(
        "  scan+sort+limit20  {:>8.3} ms p50 / {:>8.3} ms p95   ({} = {}, sort {} desc, n={}, {} rounds)",
        row.scan.p50 / 1000.0,
        row.scan.p95 / 1000.0,
        spec.scan_field,
        scan_value,
        spec.sort_field,
        row.scan_matched,
        row.scan_rounds
    );
    println!(
        "  writers Immediate  {:>8.1} ops/s   p50 {:.2} ms / p95 {:.2} ms / p99 {:.2} ms   (8 × 25 PUT + 25 PATCH, group commit)",
        row.ops_per_s, row.write.p50, row.write.p95, row.write.p99
    );
    println!(
        "  compact under load {:>8.1} ms   writers p99 {:.2} ms   (4 concurrent PUT streams, non-blocking)",
        row.compact_load_ms, row.write_load_p99
    );
    println!(
        "  compact            {:>8.1} ms   {:.2} MB → {:.2} MB",
        row.compact_ms, row.file_mb, row.file_mb_after
    );
    println!();
    row
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .map(|p| args.get(p + 1).cloned().unwrap_or_else(|| panic!("{name} needs a value")))
    };

    if let Some(name) = arg("--corpus") {
        let spec = corpora()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("unknown corpus '{name}'"));
        // The parent owns the work root and deletes it after this process
        // exits, when every journal/bucket handle here is closed.
        let workdir = arg("--workdir").unwrap_or_else(|| panic!("--corpus requires --workdir <path>"));
        let workdir = PathBuf::from(workdir);
        fs::create_dir_all(&workdir).expect("create workdir");
        let mode = arg("--mode").unwrap_or_else(|| "v1".to_string());
        match mode.as_str() {
            "v1" => {
                let row = run_corpus(&spec, &workdir);
                println!("SUMMARY\t{}", row.to_tsv());
            }
            "v2-prep" => {
                prepare_v2(&spec, &workdir);
            }
            "v2" => {
                let row = run_corpus_v2(&spec, &workdir);
                println!("SUMMARY\t{}", row.to_tsv());
            }
            other => panic!("unknown --mode '{other}' (want v1 | v2 | v2-prep | both)"),
        }
        return;
    }

    let mode = arg("--mode").unwrap_or_else(|| "both".to_string());
    let modes: Vec<&str> = match mode.as_str() {
        "both" => vec!["v1", "v2"],
        "v1" => vec!["v1"],
        "v2" => vec!["v2"],
        other => panic!("unknown --mode '{other}' (want both | v1 | v2)"),
    };
    let workdir = tempfile::tempdir().expect("create benchmark workdir");
    let exe = std::env::current_exe().expect("resolve current executable");
    println!("nDB Phase 3 — real-corpus v1 vs v2 benchmark (release)");
    println!("exe: {}", exe.display());
    println!("workdir: {}", workdir.path().display());
    println!("Each corpus/mode runs in a fresh process so the RSS delta is its own.");
    println!();
    let mut v1_rows = Vec::new();
    let mut v2_rows = Vec::new();
    for spec in corpora() {
        println!("═══ {} ═══", spec.name);
        if modes.contains(&"v2") {
            let output = std::process::Command::new(&exe)
                .arg("--corpus")
                .arg(spec.name)
                .arg("--workdir")
                .arg(workdir.path())
                .arg("--mode")
                .arg("v2-prep")
                .output()
                .unwrap_or_else(|e| panic!("spawn v2-prep child for '{}': {e}", spec.name));
            if !output.status.success() {
                panic!(
                    "v2-prep '{}' failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    spec.name,
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            print!("{}", String::from_utf8_lossy(&output.stdout));
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                println!("{}", stderr.trim_end());
            }
        }
        for mode in &modes {
            let output = std::process::Command::new(&exe)
                .arg("--corpus")
                .arg(spec.name)
                .arg("--workdir")
                .arg(workdir.path())
                .arg("--mode")
                .arg(mode)
                .output()
                .unwrap_or_else(|e| panic!("spawn {mode} child for '{}': {e}", spec.name));
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !output.status.success() {
                panic!(
                    "corpus '{}' ({mode}) failed ({})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
                    spec.name, output.status
                );
            }
            for line in stdout.lines() {
                match line.strip_prefix("SUMMARY\t") {
                    Some(tsv) => {
                        let row = Row::from_tsv(tsv);
                        if row.mode == "v2" {
                            v2_rows.push(row);
                        } else {
                            v1_rows.push(row);
                        }
                    }
                    None if !line.trim().is_empty() => println!("{line}"),
                    None => {}
                }
            }
            if !stderr.trim().is_empty() {
                println!("{}", stderr.trim_end());
            }
            println!();
        }
    }
    let mut rows = Vec::new();
    rows.extend(v1_rows.iter().cloned());
    rows.extend(v2_rows.iter().cloned());
    print_table(&rows);
    println!();
    if !v1_rows.is_empty() && !v2_rows.is_empty() {
        print_gates(&v1_rows, &v2_rows);
    }
}
