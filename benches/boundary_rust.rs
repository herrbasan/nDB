// Boundary-tax probe, Rust side: builds a test DB once, then times hot
// in-process loops (zero napi crossings). The Node-side twin
// (napi/test/boundary-node.cjs) opens the SAME folder and runs the SAME
// loops through the binding. The delta per shape = the napi boundary tax.
// Run manually:  cargo run --release --example boundary_rust

use ndb::{Database, Persistence};
use serde_json::json;
use std::fs;
use std::time::Instant;

fn main() {
    let dir = std::env::temp_dir().join("ndb_boundary");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.jsonl");
    let db = Database::open(&path)
        .unwrap()
        .with_persistence(Persistence::Lazy);

    // 5,000 docs, ~600 B each (~3 MB total). 1-in-10 active.
    let body = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do \
                eiusmod tempor incididunt ut labore et dolore magna aliqua ut \
                enim ad minim veniam quis nostrud. "
        .repeat(4); // ~400 chars
    println!("building 5000-doc test db in {} ...", path.display());
    let t0 = Instant::now();
    let mut first_active = String::new();
    for i in 0..5_000 {
        let id = db
            .insert(json!({
                "i": i,
                "title": format!("doc-{i}"),
                "status": if i % 10 == 0 { "active" } else { "inactive" },
                "kind": format!("kind-{}", i % 7),
                "body": body,
            }))
            .unwrap();
        if first_active.is_empty() {
            first_active = id;
        }
    }
    db.flush().unwrap();
    println!("built in {:?}", t0.elapsed());

    // ── 1-hit query (boundary overhead floor) ────────────────────────
    let t = Instant::now();
    let n = 200;
    for _ in 0..n {
        let r = db.query(json!({"i": {"$eq": 4999}})).unwrap();
        assert_eq!(r.len(), 1);
    }
    println!("query_1hit        {:>10.1} µs/op", t.elapsed().as_micros() as f64 / n as f64);

    // ── 500-hit query (~300 KB of results per op) ────────────────────
    let t = Instant::now();
    let n = 50;
    for _ in 0..n {
        let r = db.query(json!({"status": {"$eq": "active"}})).unwrap();
        assert_eq!(r.len(), 500);
    }
    println!("query_500hits     {:>10.1} µs/op", t.elapsed().as_micros() as f64 / n as f64);

    // ── get one doc (~600 B per op) ──────────────────────────────────
    let t = Instant::now();
    let n = 2_000;
    for _ in 0..n {
        let doc = db.get(&first_active).unwrap();
        assert!(doc["body"].is_string());
    }
    println!("get_1doc          {:>10.1} µs/op", t.elapsed().as_micros() as f64 / n as f64);

    // ── iter: whole DB (~3 MB per op) ────────────────────────────────
    let t = Instant::now();
    let n = 5;
    for _ in 0..n {
        assert_eq!(db.iter().len(), 5_000);
    }
    println!("iter_5000docs     {:>10.1} µs/op", t.elapsed().as_micros() as f64 / n as f64);

    println!("db left in place for the Node twin: {}", dir.display());
}
