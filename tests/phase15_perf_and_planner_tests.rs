//! Phase 1b / Slice 2 — hot-path performance, real query planner, HTTP
//! keep-alive.
//!
//! Three gates:
//!
//! 1. **Planner equivalence oracle.** The same query suite runs against an
//!    unindexed database and a hash+BTree indexed one; both must return the
//!    identical document set. Indexing is a performance decision, never a
//!    semantic one.
//! 2. **Indexed speed sanity.** 20k documents, exact indexed results, and an
//!    insert path that does not regress to O(n²).
//! 3. **HTTP keep-alive.** 20 sequential requests over ONE TCP socket, then a
//!    clean `Connection: close`.

use ndb::server::{serve_multi_in_background, DbRegistry, PersistenceMode, ServerConfig};
use ndb::{Database, QueryOptions, SortDir};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ─── Shared corpus ─────────────────────────────────────────────────

/// 500 deterministic documents covering every value shape the planner has to
/// reason about: ints (including 9 vs 10 and -5 vs 2), floats, numeric-looking
/// strings, booleans, nulls, arrays, missing fields and a nested dot path.
fn corpus_doc(i: usize) -> Value {
    let n = i as i64 - 250; // -250 ..= 249
    let s = ["a", "b", "c", "9", "10"][i % 5];
    let kind = ["alpha", "beta", "gamma"][i % 3];
    let tags = match i % 5 {
        0 => json!(["a", "b"]),
        1 => json!(["b"]),
        2 => json!(["a", "c", "d"]),
        3 => json!(["e"]),
        _ => json!([]),
    };
    let mut doc = json!({
        "n": n,
        "f": (i as f64) * 0.5 - 125.0,   // -125.0 ..= 124.5
        "s": s,
        "flag": i % 2 == 0,
        "meta": {
            "score": if i % 4 == 0 { n - 100 } else { n },
            "kind": kind,
        },
        "tags": tags,
    });
    // Every 6th document has an explicit null; every 11th omits the field.
    if i % 6 == 0 {
        doc.as_object_mut().unwrap().insert("nil".into(), Value::Null);
    } else if i % 11 != 0 {
        doc.as_object_mut()
            .unwrap()
            .insert("nil".into(), json!(i % 4));
    }
    doc
}

fn seeded(kind: &str, dir: &TempDir) -> Database {
    let path = dir.path().join(format!("{kind}.jsonl"));
    let db = Database::open(&path).unwrap();
    for i in 0..500 {
        db.insert(corpus_doc(i)).unwrap();
    }
    db
}

/// Canonical comparison form: documents ordered by `_id`.
fn canonical(mut docs: Vec<Value>) -> Vec<Value> {
    docs.sort_by(|a, b| {
        a["_id"]
            .as_str()
            .unwrap_or_default()
            .cmp(b["_id"].as_str().unwrap_or_default())
    });
    docs
}

/// Cross-database comparison form: two independently seeded databases have
/// different generated `_id`s, so results are compared by content, with the
/// content strings sorted (ties in any sort key are broken deterministically,
/// which makes the comparison stable for equal-valued documents).
fn fingerprint(docs: Vec<Value>) -> Vec<String> {
    let mut out: Vec<String> = docs
        .iter()
        .map(|doc| {
            let mut copy = doc.clone();
            if let Some(obj) = copy.as_object_mut() {
                obj.remove("_id");
            }
            copy.to_string()
        })
        .collect();
    out.sort();
    out
}

/// One probe of the oracle: a filter plus the query options and projection.
struct Probe {
    label: &'static str,
    filter: Value,
    opts: QueryOptions,
    fields: Option<Vec<String>>,
}

fn probe(label: &'static str, filter: Value) -> Probe {
    Probe {
        label,
        filter,
        opts: QueryOptions::default(),
        fields: None,
    }
}

fn planner_probes() -> Vec<Probe> {
    let mut probes = vec![
        // Equality: implicit, explicit, string-typed and numeric-typed.
        probe("implicit eq int 9", json!({"n": 9})),
        probe("explicit eq int 10", json!({"n": {"$eq": 10}})),
        probe("implicit eq string '9'", json!({"s": "9"})),
        probe("implicit eq bool", json!({"flag": true})),
        probe("implicit eq null", json!({"nil": null})),
        // Negation: not indexable, must not change the result.
        probe("ne int", json!({"n": {"$ne": 9}})),
        probe("nin strings", json!({"s": {"$nin": ["a", "b"]}})),
        // $in over a plain field and over an array field.
        probe("in strings", json!({"s": {"$in": ["a", "9"]}})),
        probe("in array field", json!({"tags": {"$in": ["a", "c"]}})),
        probe("eq scalar on array field", json!({"tags": {"$eq": "b"}})),
        // Range: numeric order, never string order.
        probe("gt 9", json!({"n": {"$gt": 9}})),
        probe("gte -5 and lt 2", json!({"n": {"$gte": -5, "$lt": 2}})),
        probe("lte float", json!({"f": {"$lte": 1.5}})),
        probe("gt and lte numeric strings", json!({"s": {"$gt": "8"}})),
        // Nested dot paths.
        probe("nested gte", json!({"meta.score": {"$gte": 5}})),
        probe("nested lt", json!({"meta.score": {"$lt": 0}})),
        probe("nested eq", json!({"meta.kind": "beta"})),
        // Presence.
        probe("exists false", json!({"nil": {"$exists": false}})),
        probe("exists true", json!({"nil": {"$exists": true}})),
        // Multi-field conjunction.
        probe("two fields", json!({"flag": true, "n": {"$gt": 0}})),
        probe(
            "eq plus range same field",
            json!({"n": {"$eq": 9, "$gte": 0}}),
        ),
        // Combinators.
        probe(
            "and",
            json!({"$and": [{"n": {"$gte": 0}}, {"s": "c"}]}),
        ),
        probe(
            "nested and",
            json!({"$and": [{"$and": [{"n": {"$lt": 10}}]}, {"flag": true}]}),
        ),
        probe("or", json!({"$or": [{"n": 9}, {"s": "b"}]})),
        probe("not", json!({"$not": {"n": {"$gt": 100}}})),
        probe("top array = implicit and", json!([{"n": {"$gt": 0}}, {"flag": false}])),
    ];

    probes.push(Probe {
        label: "sort desc offset limit",
        filter: json!({"n": {"$gte": 0}}),
        opts: QueryOptions {
            sort_by: Some(("n".to_string(), SortDir::Desc)),
            offset: Some(10),
            limit: Some(25),
        },
        fields: None,
    });
    probes.push(Probe {
        label: "sort nested asc projected",
        filter: json!({"meta.score": {"$gte": -20}}),
        opts: QueryOptions {
            sort_by: Some(("n".to_string(), SortDir::Asc)),
            offset: Some(5),
            limit: Some(20),
        },
        fields: Some(vec![
            "n".to_string(),
            "meta.score".to_string(),
            "tags".to_string(),
        ]),
    });
    probes
}

#[test]
fn planner_equals_full_scan_on_every_probe() {
    let plain_dir = TempDir::new().unwrap();
    let plain = seeded("plain", &plain_dir);

    let indexed_dir = TempDir::new().unwrap();
    let indexed = seeded("indexed", &indexed_dir);
    // Both kinds on the same fields, plus a hash-only and a btree-only field.
    for field in ["n", "s", "flag", "nil", "meta.score", "meta.kind"] {
        indexed.create_index(field).unwrap();
        indexed.create_btree_index(field).unwrap();
    }
    indexed.create_index("tags").unwrap();
    indexed.create_btree_index("f").unwrap();
    assert!(indexed.has_index("n") && indexed.has_index("meta.score"));

    for p in planner_probes() {
        let plain_hits = plain
            .query_projected(p.filter.clone(), p.opts.clone(), p.fields.as_deref())
            .unwrap();
        let indexed_hits = indexed
            .query_projected(p.filter.clone(), p.opts.clone(), p.fields.as_deref())
            .unwrap();

        assert_eq!(
            plain_hits.0, indexed_hits.0,
            "probe '{}': total mismatch",
            p.label
        );
        assert_eq!(
            fingerprint(plain_hits.1),
            fingerprint(indexed_hits.1),
            "probe '{}': result set mismatch",
            p.label
        );

        // Same oracle through the public `query` / `query_with` paths.
        let plain_docs = plain.query_with(p.filter.clone(), p.opts.clone()).unwrap();
        let indexed_docs = indexed
            .query_with(p.filter.clone(), p.opts.clone())
            .unwrap();
        assert_eq!(
            fingerprint(plain_docs),
            fingerprint(indexed_docs),
            "probe '{}': query_with mismatch",
            p.label
        );
    }

    // find_range uses the BTree index, and the index must not change the set.
    let plain_range = plain.find_range("n", &json!(-5), &json!(10));
    let indexed_range = indexed.find_range("n", &json!(-5), &json!(10));
    assert_eq!(plain_range.len(), 16, "-5..=10 is 16 integers");
    assert_eq!(fingerprint(plain_range), fingerprint(indexed_range));
}

#[test]
fn planner_keeps_indexes_in_sync_through_mutations() {
    let plain_dir = TempDir::new().unwrap();
    let plain = seeded("plain2", &plain_dir);
    let indexed_dir = TempDir::new().unwrap();
    let indexed = seeded("indexed2", &indexed_dir);
    indexed.create_index("n").unwrap();
    indexed.create_btree_index("n").unwrap();
    indexed.create_index("tags").unwrap();

    let extra_plain = plain.insert(json!({"n": 100_000, "tags": ["z"]})).unwrap();
    let extra_indexed = indexed.insert(json!({"n": 100_000, "tags": ["z"]})).unwrap();
    assert_eq!(plain.query(json!({"n": 100_000})).unwrap().len(), 1);
    assert_eq!(indexed.query(json!({"n": 100_000})).unwrap().len(), 1);

    plain.set(&extra_plain, "n", json!(7)).unwrap();
    indexed.set(&extra_indexed, "n", json!(7)).unwrap();
    plain.array_push(&extra_plain, "tags", json!("y")).unwrap();
    indexed.array_push(&extra_indexed, "tags", json!("y")).unwrap();

    for filter in [
        json!({"n": 7}),
        json!({"n": 100_000}),
        json!({"tags": {"$in": ["z"]}}),
        json!({"tags": {"$in": ["y"]}}),
        json!({"n": {"$gte": 7, "$lte": 7}}),
    ] {
        assert_eq!(
            fingerprint(plain.query(filter.clone()).unwrap()),
            fingerprint(indexed.query(filter.clone()).unwrap()),
            "post-mutation probe {filter}"
        );
    }

    plain.delete(&extra_plain).unwrap();
    indexed.delete(&extra_indexed).unwrap();
    assert_eq!(
        fingerprint(plain.query(json!({"tags": {"$in": ["z"]}})).unwrap()),
        fingerprint(indexed.query(json!({"tags": {"$in": ["z"]}})).unwrap())
    );
}

#[test]
fn planner_equals_full_scan_on_a_compacted_ram_database() {
    use std::fs;

    // The v1 reference: the same corpus with the same in-memory indexes.
    let reference_dir = TempDir::new().unwrap();
    let reference = seeded("ram-reference", &reference_dir);
    for field in ["n", "s", "flag", "nil", "meta.score", "meta.kind"] {
        reference.create_index(field).unwrap();
        reference.create_btree_index(field).unwrap();
    }
    reference.create_index("tags").unwrap();
    reference.create_btree_index("f").unwrap();

    // The RAM-first leg: 500 documents, both index kinds, then compaction.
    let ram_root = TempDir::new().unwrap();
    let ram_dir = ram_root.path().join("db");
    fs::create_dir_all(&ram_dir).unwrap();
    let ram = Database::open_ram(&ram_dir).unwrap();
    for i in 0..500 {
        ram.insert(corpus_doc(i)).unwrap();
    }
    // One index kind per field: BTree where the probes exercise a range, Hash
    // where they exercise equality/$in.
    for field in ["n", "f", "s", "meta.score"] {
        ram.create_btree_index(field).unwrap();
    }
    for field in ["flag", "nil", "meta.kind", "tags"] {
        ram.create_index(field).unwrap();
    }
    ram.compact().unwrap();
    assert!(ram.is_ram());
    assert!(ram.has_index("n") && ram.has_index("meta.score") && ram.has_index("f"));

    for p in planner_probes() {
        let expected = reference
            .query_projected(p.filter.clone(), p.opts.clone(), p.fields.as_deref())
            .unwrap();
        let actual = ram
            .query_projected(p.filter.clone(), p.opts.clone(), p.fields.as_deref())
            .unwrap();
        assert_eq!(expected.0, actual.0, "probe '{}': total mismatch", p.label);
        assert_eq!(
            fingerprint(expected.1),
            fingerprint(actual.1.clone()),
            "probe '{}': result set mismatch",
            p.label
        );

        let reference_docs = reference
            .query_with(p.filter.clone(), p.opts.clone())
            .unwrap();
        let ram_docs = ram.query_with(p.filter.clone(), p.opts.clone()).unwrap();
        assert_eq!(
            fingerprint(reference_docs),
            fingerprint(ram_docs),
            "probe '{}': query_with mismatch",
            p.label
        );

        // The HTTP raw fast path must select exactly the same page as the
        // unprojected query (projection is a separate path).
        let (raw_total, raw_docs) = ram
            .query_raw(p.filter.clone(), p.opts.clone())
            .unwrap();
        let unprojected = ram
            .query_projected(p.filter.clone(), p.opts.clone(), None)
            .unwrap();
        assert_eq!(raw_total, unprojected.0, "probe '{}': query_raw total", p.label);
        let raw_values: Vec<Value> = raw_docs
            .iter()
            .map(|bytes| serde_json::from_slice(bytes).expect("raw hit is valid JSON"))
            .collect();
        assert_eq!(
            fingerprint(raw_values),
            fingerprint(unprojected.1),
            "probe '{}': query_raw page mismatch",
            p.label
        );
    }

    // Indexed range + equality probes directly against the built indexes.
    let plain_range = reference.find_range("n", &json!(-5), &json!(10));
    let ram_range = ram.find_range("n", &json!(-5), &json!(10));
    assert_eq!(plain_range.len(), 16, "-5..=10 is 16 integers");
    assert_eq!(fingerprint(plain_range), fingerprint(ram_range));
    assert_eq!(
        fingerprint(reference.find("meta.kind", &json!("beta"))),
        fingerprint(ram.find("meta.kind", &json!("beta")))
    );

    // Writes after the compaction land in the WAL and still resolve through
    // the rebuilt indexes.
    let id = ram.insert(json!({"n": 10_000, "s": "z"})).unwrap();
    assert_eq!(ram.find("n", &json!(10_000)).len(), 1);
    assert_eq!(ram.find_range("n", &json!(9_999), &json!(10_001)).len(), 1);
    ram.delete(&id).unwrap();
    assert_eq!(ram.find("n", &json!(10_000)).len(), 0);
    assert!(ram.deleted_ids().contains(&id));
}

// ─── Indexed speed sanity ──────────────────────────────────────────

#[test]
fn indexed_queries_and_bulk_insert_stay_fast() {
    const DOCS: usize = 20_000;
    let db = Database::open_in_memory().unwrap();
    db.create_index("key").unwrap();
    db.create_btree_index("score").unwrap();

    // Insert throughput: the old insert cloned every live id into a HashSet
    // per write (O(n²) overall). Compare the last batch with the first — a
    // linear-cost insert path keeps them in the same range.
    let mut first_batch = Duration::ZERO;
    let mut last_batch = Duration::ZERO;
    let total_start = Instant::now();
    let mut batch_start = Instant::now();
    for i in 0..DOCS {
        db.insert(json!({"key": format!("k{i:05}"), "score": i, "n": i}))
            .unwrap();
        if (i + 1) % 1_000 == 0 {
            let elapsed = batch_start.elapsed();
            if i + 1 == 1_000 {
                first_batch = elapsed;
            }
            if i + 1 == DOCS {
                last_batch = elapsed;
            }
            batch_start = Instant::now();
        }
    }
    let total_insert = total_start.elapsed();
    assert_eq!(db.len(), DOCS);
    println!(
        "insert: {DOCS} docs in {:?} ({} docs/s); first 1k {:?}, last 1k {:?}",
        total_insert,
        (DOCS as f64 / total_insert.as_secs_f64()) as u64,
        first_batch,
        last_batch
    );
    assert!(
        total_insert < Duration::from_secs(20),
        "20k inserts took {total_insert:?} — the insert path regressed (O(n²)?)"
    );
    assert!(
        last_batch < first_batch * 6 + Duration::from_millis(200),
        "insert cost grows with database size: first 1k {:?}, last 1k {:?}",
        first_batch,
        last_batch
    );

    // Indexed equality: one candidate document.
    let t = Instant::now();
    let eq = db.query(json!({"key": "k12345"})).unwrap();
    let eq_elapsed = t.elapsed();
    assert_eq!(eq.len(), 1);
    assert_eq!(eq[0]["score"], json!(12345));

    // Same question against an unindexed field: the full-scan baseline.
    let t = Instant::now();
    let scan = db.query(json!({"n": 12345})).unwrap();
    let scan_elapsed = t.elapsed();
    assert_eq!(scan.len(), 1);
    assert!(
        eq_elapsed < scan_elapsed.mul_f64(1.5),
        "indexed eq {:?} is not faster than the full scan {:?}",
        eq_elapsed,
        scan_elapsed
    );

    // Indexed range: [100, 200] is exactly 101 documents.
    let t = Instant::now();
    let range = db.query(json!({"score": {"$gte": 100, "$lte": 200}})).unwrap();
    let range_elapsed = t.elapsed();
    assert_eq!(range.len(), 101);
    assert!(range
        .iter()
        .all(|d| (100..=200).contains(&d["score"].as_i64().unwrap())));

    // find_range through the BTree index returns the same exact set.
    let t = Instant::now();
    let found = db.find_range("score", &json!(100), &json!(200));
    let find_elapsed = t.elapsed();
    assert_eq!(found.len(), 101);
    assert_eq!(canonical(found), canonical(range));

    // Unbounded-side ranges are index scans too.
    let tail = db.query(json!({"score": {"$gt": 19_990}})).unwrap();
    assert_eq!(tail.len(), 9);

    println!(
        "indexed latency: eq {:?} vs full scan {:?}; range {:?}; find_range {:?} (debug build)",
        eq_elapsed, scan_elapsed, range_elapsed, find_elapsed
    );
    for (label, elapsed) in [
        ("eq", eq_elapsed),
        ("range", range_elapsed),
        ("find_range", find_elapsed),
    ] {
        assert!(
            elapsed < Duration::from_millis(500),
            "{label} took {elapsed:?} — index not used?"
        );
    }
}

#[test]
fn stats_is_constant_time_not_a_full_clone() {
    let db = Database::open_in_memory().unwrap();
    for i in 0..5_000 {
        db.insert(json!({"n": i, "pad": "x".repeat(64)})).unwrap();
    }
    let t = Instant::now();
    for _ in 0..1_000 {
        assert_eq!(db.len(), 5_000);
    }
    let elapsed = t.elapsed();
    println!("len() x1000 over 5k docs: {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(200),
        "len() is not O(1): {elapsed:?}"
    );
}

// ─── HTTP keep-alive ───────────────────────────────────────────────

struct Conn {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Conn {
    fn new(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Conn { stream, reader }
    }

    /// One request/response exchange on the persistent socket.
    fn request(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        close: bool,
    ) -> (u16, HashMap<String, String>, Vec<u8>) {
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let head = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
            method,
            path,
            payload.len(),
            if close { "close" } else { "keep-alive" }
        );
        self.stream.write_all(head.as_bytes()).unwrap();
        self.stream.write_all(payload.as_bytes()).unwrap();
        self.stream.flush().unwrap();

        let mut status_line = String::new();
        self.reader.read_line(&mut status_line).unwrap();
        assert!(
            status_line.starts_with("HTTP/1.1"),
            "bad status line: {status_line:?}"
        );
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();

        let mut headers = HashMap::new();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (k, v) = line.split_once(':').unwrap();
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }

        let len: usize = headers["content-length"].parse().unwrap();
        let mut body_bytes = vec![0u8; len];
        self.reader.read_exact(&mut body_bytes).unwrap();
        (status, headers, body_bytes)
    }
}

#[test]
fn http_keep_alive_serves_twenty_requests_on_one_socket() {
    let dir = TempDir::new().unwrap();
    let registry = DbRegistry::new(None, PersistenceMode::Lazy, 5_000);
    registry
        .mount("default", dir.path().join("data.jsonl"))
        .unwrap();
    let handle = serve_multi_in_background(
        Arc::new(registry),
        ServerConfig {
            port: 0,
            ..Default::default()
        },
    )
    .unwrap();
    let port = handle.addr.port();

    let mut conn = Conn::new(port);
    let mut ids = Vec::new();

    // Five full cycles: PUT, GET, POST /query, PATCH, GET /stats — 20
    // exchanges, each on the SAME socket.
    for cycle in 0..4 {
        let (status, headers, body) =
            conn.request("PUT", "/doc", Some(&json!({"n": cycle, "tag": "c"})), false);
        assert_eq!(status, 201, "PUT cycle {cycle}: {:?}", String::from_utf8_lossy(&body));
        assert_eq!(
            headers.get("connection").map(String::as_str),
            Some("keep-alive"),
            "PUT must keep the connection alive"
        );
        let id = serde_json::from_slice::<Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        ids.push(id.clone());

        let (status, headers, body) = conn.request("GET", &format!("/doc/{id}"), None, false);
        assert_eq!(status, 200);
        assert_eq!(headers.get("connection").map(String::as_str), Some("keep-alive"));
        let doc: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(doc["doc"]["n"], json!(cycle));

        let (status, headers, body) =
            conn.request("POST", "/query", Some(&json!({"filter": {"n": cycle}})), false);
        assert_eq!(status, 200);
        assert_eq!(headers.get("connection").map(String::as_str), Some("keep-alive"));
        let res: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(res["count"], json!(1), "query cycle {cycle}");

        let (status, headers, body) = conn.request(
            "PATCH",
            &format!("/doc/{id}"),
            Some(&json!({"ops": [{"op": "set", "path": "tag", "value": "patched"}]})),
            false,
        );
        assert_eq!(status, 200, "PATCH cycle {cycle}");
        assert_eq!(headers.get("connection").map(String::as_str), Some("keep-alive"));
        let patched: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(patched["applied"], json!([true]));

        let (status, headers, body) = conn.request("GET", "/stats", None, false);
        assert_eq!(status, 200);
        assert_eq!(headers.get("connection").map(String::as_str), Some("keep-alive"));
        let stats: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stats["documents"], json!(cycle + 1), "stats cycle {cycle}");
    }

    // The 21st request asks to close: the header must say so and the server
    // must close the socket cleanly (EOF, no trailing bytes).
    let (status, headers, body) = conn.request("GET", "/stats", None, true);
    assert_eq!(status, 200);
    assert_eq!(
        headers.get("connection").map(String::as_str),
        Some("close"),
        "a closing response must announce Connection: close"
    );
    let stats: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(stats["documents"], json!(4));

    let mut leftover = Vec::new();
    conn.reader.read_to_end(&mut leftover).unwrap();
    assert!(
        leftover.is_empty(),
        "socket must end cleanly after Connection: close, got {:?}",
        String::from_utf8_lossy(&leftover)
    );

    // The 20 ids are all readable afterwards (nothing was lost mid-stream).
    drop(conn);
    let mut check = Conn::new(port);
    for (cycle, id) in ids.iter().enumerate() {
        let (status, _, body) = check.request("GET", &format!("/doc/{id}"), None, false);
        assert_eq!(status, 200);
        let doc: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(doc["doc"]["tag"], json!("patched"), "id {cycle}");
    }
    handle.shutdown();
}
