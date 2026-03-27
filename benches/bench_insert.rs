//! Benchmarks for nDB.
//!
//! Measures throughput and latency across all layers:
//! - Insert (in-memory, lazy, immediate)
//! - Get by ID (Layer 1)
//! - Find by field (Layer 2)
//! - Query with JSON AST (Layer 3)
//! - Indexed vs non-indexed queries
//! - Compaction
//! - Bulk operations

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use ndb::{Database, Persistence};
use serde_json::json;
use tempfile::TempDir;

// ─── Insert Throughput ───────────────────────────────────────────────

fn bench_insert_in_memory(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    group.bench_function("in_memory", |b| {
        let db = Database::open_in_memory().unwrap();
        b.iter(|| {
            let id = db.insert(json!({"index": 0, "value": "test"})).unwrap();
            black_box(id);
        });
    });
    group.finish();
}

fn bench_insert_lazy(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    group.bench_function("lazy_persist", |b| {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bench.jsonl");
        let db = Database::open(&path).unwrap();
        b.iter(|| {
            let id = db.insert(json!({"index": 0, "value": "test"})).unwrap();
            black_box(id);
        });
    });
    group.finish();
}

fn bench_insert_immediate(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    group.bench_function("immediate_persist", |b| {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bench.jsonl");
        let db = Database::open(&path).unwrap().with_persistence(Persistence::Immediate);
        b.iter(|| {
            let id = db.insert(json!({"index": 0, "value": "test"})).unwrap();
            black_box(id);
        });
    });
    group.finish();
}

fn bench_insert_bulk(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_bulk");
    for size in [100, 1000, 10000] {
        group.bench_with_input(BenchmarkId::new("in_memory", size), &size, |b, &size| {
            b.iter(|| {
                let db = Database::open_in_memory().unwrap();
                for i in 0..size {
                    db.insert(json!({"i": i, "data": "benchmark"})).unwrap();
                }
                black_box(db.len());
            });
        });
    }
    group.finish();
}

// ─── Layer 1: Get by ID ──────────────────────────────────────────────

fn bench_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("get");
    group.bench_function("by_id", |b| {
        let db = Database::open_in_memory().unwrap();
        let id = db.insert(json!({"value": "test"})).unwrap();
        b.iter(|| {
            let doc = db.get(&id).unwrap();
            black_box(doc);
        });
    });
    group.finish();
}

fn bench_get_large_db(c: &mut Criterion) {
    let mut group = c.benchmark_group("get");
    group.bench_function("by_id_10k_docs", |b| {
        let db = Database::open_in_memory().unwrap();
        let mut target_id = String::new();
        for i in 0..10000 {
            let id = db.insert(json!({"i": i, "data": "x".repeat(50)})).unwrap();
            if i == 5000 {
                target_id = id;
            }
        }
        b.iter(|| {
            let doc = db.get(&target_id).unwrap();
            black_box(doc);
        });
    });
    group.finish();
}

// ─── Layer 2: Field Queries ──────────────────────────────────────────

fn bench_find_no_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("find");
    group.bench_function("no_index_10k", |b| {
        let db = Database::open_in_memory().unwrap();
        for i in 0..10000 {
            db.insert(json!({"status": if i % 100 == 0 { "target" } else { "other" }, "i": i})).unwrap();
        }
        b.iter(|| {
            let results = db.find("status", &json!("target"));
            black_box(results.len());
        });
    });
    group.finish();
}

fn bench_find_with_hash_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("find");
    group.bench_function("hash_index_10k", |b| {
        let db = Database::open_in_memory().unwrap();
        for i in 0..10000 {
            db.insert(json!({"status": if i % 100 == 0 { "target" } else { "other" }, "i": i})).unwrap();
        }
        db.create_index("status").unwrap();
        b.iter(|| {
            let results = db.find("status", &json!("target"));
            black_box(results.len());
        });
    });
    group.finish();
}

fn bench_find_with_btree_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("find");
    group.bench_function("btree_index_10k", |b| {
        let db = Database::open_in_memory().unwrap();
        for i in 0..10000 {
            db.insert(json!({"score": i})).unwrap();
        }
        db.create_btree_index("score").unwrap();
        b.iter(|| {
            let results = db.find_range("score", &json!(4000), &json!(6000));
            black_box(results.len());
        });
    });
    group.finish();
}

// ─── Layer 3: JSON AST Queries ───────────────────────────────────────

fn bench_query_simple_eq(c: &mut Criterion) {
    let mut group = c.benchmark_group("query");
    group.bench_function("simple_eq_10k", |b| {
        let db = Database::open_in_memory().unwrap();
        for i in 0..10000 {
            db.insert(json!({"status": if i % 2 == 0 { "active" } else { "inactive" }, "i": i})).unwrap();
        }
        b.iter(|| {
            let results = db.query(json!({"status": {"$eq": "active"}}));
            black_box(results);
        });
    });
    group.finish();
}

fn bench_query_and_or(c: &mut Criterion) {
    let mut group = c.benchmark_group("query");
    group.bench_function("and_or_10k", |b| {
        let db = Database::open_in_memory().unwrap();
        for i in 0..10000 {
            db.insert(json!({"a": i % 10, "b": i % 5, "i": i})).unwrap();
        }
        b.iter(|| {
            let results = db.query(json!({
                "$and": [
                    {"a": {"$eq": 3}},
                    {"$or": [{"b": {"$eq": 1}}, {"b": {"$eq": 2}}]}
                ]
            }));
            black_box(results);
        });
    });
    group.finish();
}

fn bench_query_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("query");
    group.bench_function("comparison_10k", |b| {
        let db = Database::open_in_memory().unwrap();
        for i in 0..10000 {
            db.insert(json!({"value": i})).unwrap();
        }
        b.iter(|| {
            let results = db.query(json!({"value": {"$gte": 5000}}));
            black_box(results);
        });
    });
    group.finish();
}

// ─── Iteration ───────────────────────────────────────────────────────

fn bench_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("iter");
    group.bench_function("10k_docs", |b| {
        let db = Database::open_in_memory().unwrap();
        for i in 0..10000 {
            db.insert(json!({"i": i})).unwrap();
        }
        b.iter(|| {
            let all = db.iter();
            black_box(all.len());
        });
    });
    group.finish();
}

// ─── Update ──────────────────────────────────────────────────────────

fn bench_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("update");
    group.bench_function("in_memory", |b| {
        let db = Database::open_in_memory().unwrap();
        let id = db.insert(json!({"v": 0})).unwrap();
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            db.update(&id, json!({"v": counter})).unwrap();
            black_box(counter);
        });
    });
    group.finish();
}

// ─── Delete ──────────────────────────────────────────────────────────

fn bench_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("delete");
    group.bench_function("in_memory", |b| {
        b.iter(|| {
            let db = Database::open_in_memory().unwrap();
            let mut ids = Vec::new();
            for i in 0..100 {
                ids.push(db.insert(json!({"i": i})).unwrap());
            }
            for id in &ids {
                db.delete(id).unwrap();
            }
            black_box(db.len());
        });
    });
    group.finish();
}

// ─── Compaction ──────────────────────────────────────────────────────

fn bench_compact(c: &mut Criterion) {
    let mut group = c.benchmark_group("compact");
    for size in [100, 1000, 5000] {
        group.bench_with_input(BenchmarkId::new("with_deletions", size), &size, |b, &size| {
            b.iter(|| {
                let dir = TempDir::new().unwrap();
                let path = dir.path().join("compact.jsonl");
                let db = Database::open(&path).unwrap();
                let mut ids = Vec::new();
                for i in 0..size {
                    ids.push(db.insert(json!({"i": i, "data": "x".repeat(50)})).unwrap());
                }
                // Delete half
                for id in &ids[0..size / 2] {
                    db.delete(id).unwrap();
                }
                db.compact().unwrap();
                black_box(db.len());
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_insert_in_memory,
    bench_insert_lazy,
    bench_insert_immediate,
    bench_insert_bulk,
    bench_get,
    bench_get_large_db,
    bench_find_no_index,
    bench_find_with_hash_index,
    bench_find_with_btree_index,
    bench_query_simple_eq,
    bench_query_and_or,
    bench_query_comparison,
    bench_iter,
    bench_update,
    bench_delete,
    bench_compact,
);
criterion_main!(benches);
