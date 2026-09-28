// Doc-path perf probe, chat-app shape: insert, conversation arrayPush,
// metadata set, no-op set, get, AST query. Run manually:
//   cargo run --release --example doc_perf
// Numbers printed to stdout as avg µs/op. Compare across versions by
// running the same probe on both trees.

use ndb::{Database, Persistence};
use serde_json::json;
use std::fs;
use std::time::Instant;

fn main() {
    let dir = std::env::temp_dir().join("ndb_doc_perf");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("perf.jsonl");
    let db = Database::open(&path)
        .unwrap()
        .with_persistence(Persistence::Lazy);

    // ── Phase 1: insert 1,000 small docs ─────────────────────────────
    let blob = "x".repeat(200);
    let t0 = Instant::now();
    for i in 0..1_000 {
        db.insert(json!({
            "title": format!("doc-{i}"),
            "status": if i % 2 == 0 { "active" } else { "inactive" },
            "i": i,
            "blob": blob
        }))
        .unwrap();
    }
    println!("insert_1k_docs        {:>10.1} µs/op", t0.elapsed().as_micros() as f64 / 1000.0);

    // ── Phase 2: conversation hot path — 500 arrayPush into one doc ──
    let conv = db.insert(json!({"title": "conversation", "messages": []})).unwrap();
    let t0 = Instant::now();
    for i in 0..500 {
        db.array_push(
            &conv,
            "messages",
            json!({"role": "user", "content": format!("message number {i} with some body text")}),
        )
        .unwrap();
    }
    println!("array_push_500_msgs   {:>10.1} µs/op", t0.elapsed().as_micros() as f64 / 500.0);

    // ── Phase 3: metadata set on 500 docs ────────────────────────────
    let mut ids = Vec::new();
    for i in 0..500 {
        ids.push(db.insert(json!({"n": i, "meta": "old"})).unwrap());
    }
    let t0 = Instant::now();
    for id in &ids {
        db.set(id, "meta", json!("new")).unwrap();
    }
    println!("set_meta_500_docs     {:>10.1} µs/op", t0.elapsed().as_micros() as f64 / 500.0);

    // ── Phase 4: no-op set (unresolvable path) × 500 ─────────────────
    let t0 = Instant::now();
    for id in &ids {
        let _ = db.set(id, "no.such.path.here", json!(1));
    }
    println!("no_op_set_500         {:>10.1} µs/op", t0.elapsed().as_micros() as f64 / 500.0);

    // ── Phase 5: get the 500-message doc × 200 ───────────────────────
    let t0 = Instant::now();
    for _ in 0..200 {
        let doc = db.get(&conv).unwrap();
        assert_eq!(doc["messages"].as_array().unwrap().len(), 500);
    }
    println!("get_500msg_doc        {:>10.1} µs/op", t0.elapsed().as_micros() as f64 / 200.0);

    // ── Phase 6: AST query over 1,501 docs × 200 ─────────────────────
    let t0 = Instant::now();
    for _ in 0..200 {
        let results = db.query(json!({"status": {"$eq": "active"}})).unwrap();
        assert_eq!(results.len(), 500);
    }
    println!("query_eq_200_runs     {:>10.1} µs/op", t0.elapsed().as_micros() as f64 / 200.0);

    db.flush().unwrap();
    drop(db);

    // ── Phase 7: reopen + replay the whole journal ───────────────────
    let t0 = Instant::now();
    let db2 = Database::open(&path).unwrap();
    println!("reopen_replay         {:>10.1} ms     ({} docs)",
        t0.elapsed().as_micros() as f64 / 1000.0, db2.len());

    let _ = fs::remove_dir_all(&dir);
}
