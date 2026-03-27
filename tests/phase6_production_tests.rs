//! Integration tests for nDB Phase 6: Production Polish
//!
//! Tests corruption recovery, crash simulation, edge cases,
//! concurrent reads, and persistence guarantees.

use ndb::{Database, Persistence, QueryOptions, SortDir};
use serde_json::json;
use std::fs;
use std::io::Write;
use tempfile::TempDir;

fn setup() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("phase6.jsonl");
    let db = Database::open(&path).unwrap();
    (db, dir)
}

// ─── Corruption Recovery ─────────────────────────────────────────────

#[test]
fn open_recovers_from_truncated_lines() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("corrupted.jsonl");

    // Write a file with mixed valid and corrupted lines
    let mut file = fs::File::create(&path).unwrap();
    writeln!(file, r#"{{"_meta":{{"version":1,"created":"0"}}}}"#).unwrap();
    writeln!(file, r#"{{"_id":"good1","v":1}}"#).unwrap();
    writeln!(file, r#"{{"_id":"broken","v":2"#).unwrap(); // truncated
    writeln!(file, r#"{{"_id":"good2","v":3}}"#).unwrap();
    writeln!(file, "not json at all").unwrap();
    writeln!(file, r#"{{"_id":"good3","v":4}}"#).unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 3); // good1, good2, good3
    assert!(db.get("good1").is_ok());
    assert!(db.get("good2").is_ok());
    assert!(db.get("good3").is_ok());
    assert!(db.get("broken").is_err()); // corrupted line skipped
}

#[test]
fn open_handles_empty_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("empty.jsonl");
    // Create truly empty file
    fs::write(&path, "").unwrap();

    // This should fail because there's no meta header — but read_all handles it
    // Actually, Database::open calls init_file only if file doesn't exist.
    // An empty file exists, so it tries to read it. read_all returns empty vec.
    // This is fine — empty db.
    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 0);
}

#[test]
fn open_handles_garbage_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("garbage.jsonl");
    fs::write(&path, "not json\nalso not json\n").unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 0);
}

// ─── Crash Simulation ────────────────────────────────────────────────

#[test]
fn crash_during_write_preserves_previous_data() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("crash.jsonl");

    // Write valid data
    let db = Database::open(&path).unwrap();
    let id1 = db.insert(json!({"important": "data"})).unwrap();
    db.flush().unwrap();

    // Simulate crash: append a partial line directly to the file
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    write!(file, "{}", r#"{"_id":"partial","data":"incomplete"#).unwrap();
    drop(file);

    // Reopen — should recover id1, skip partial
    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.len(), 1);
    let doc = db2.get(&id1).unwrap();
    assert_eq!(doc["important"], "data");
}

#[test]
fn crash_with_multiple_partial_lines() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("multi_crash.jsonl");

    // Build a file with meta + valid + partial + valid + partial
    let mut file = fs::File::create(&path).unwrap();
    writeln!(file, r#"{{"_meta":{{"version":1,"created":"0"}}}}"#).unwrap();
    writeln!(file, r#"{{"_id":"a","v":1}}"#).unwrap();
    writeln!(file, r#"{{"_id":"b"#).unwrap(); // partial
    writeln!(file, r#"{{"_id":"c","v":3}}"#).unwrap();
    write!(file, r#"{{"_id":"d","v":4"#).unwrap(); // partial, no newline

    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 2); // a and c only
    assert!(db.get("a").is_ok());
    assert!(db.get("c").is_ok());
}

// ─── Persistence Guarantees ──────────────────────────────────────────

#[test]
fn immediate_persistence_survives_drop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("imm_persist.jsonl");

    let ids = {
        let db = Database::open(&path)
            .unwrap()
            .with_persistence(Persistence::Immediate);

        let mut ids = Vec::new();
        for i in 0..50 {
            ids.push(db.insert(json!({"i": i})).unwrap());
        }
        ids
    };
    // db dropped here

    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.len(), 50);
    for id in &ids {
        assert!(db2.get(id).is_ok());
    }
}

#[test]
fn lazy_persistence_requires_flush() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("lazy.jsonl");

    let id = {
        let db = Database::open(&path).unwrap(); // Lazy by default
        let id = db.insert(json!({"lazy": true})).unwrap();
        // Don't flush — data may or may not be on disk
        id
    };

    // Reopen — in lazy mode, data might be lost if OS didn't flush
    // But in practice, the OS usually flushes on drop. This test
    // just verifies no crash occurs.
    let db2 = Database::open(&path).unwrap();
    // We can't assert the data is there (OS-dependent), but it shouldn't crash
    let _ = db2.get(&id);
}

#[test]
fn flush_then_drop_preserves_all() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("flushed.jsonl");

    let ids = {
        let db = Database::open(&path).unwrap();
        let mut ids = Vec::new();
        for i in 0..100 {
            ids.push(db.insert(json!({"n": i})).unwrap());
        }
        db.flush().unwrap();
        ids
    };

    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.len(), 100);
    for id in &ids {
        assert!(db2.get(id).is_ok());
    }
}

// ─── Edge Cases ──────────────────────────────────────────────────────

#[test]
fn very_large_document() {
    let db = Database::open_in_memory().unwrap();
    // 1MB string value
    let large_value = "x".repeat(1024 * 1024);
    let id = db.insert(json!({"data": large_value})).unwrap();
    let doc = db.get(&id).unwrap();
    assert_eq!(doc["data"].as_str().unwrap().len(), 1024 * 1024);
}

#[test]
fn deeply_nested_document() {
    let db = Database::open_in_memory().unwrap();
    // 50 levels of nesting
    let mut inner = json!({"leaf": true});
    for _ in 0..50 {
        inner = json!({"nested": inner});
    }
    let id = db.insert(inner).unwrap();
    let doc = db.get(&id).unwrap();
    assert!(doc.get("nested").is_some());
}

#[test]
fn document_with_many_fields() {
    let db = Database::open_in_memory().unwrap();
    let mut obj = serde_json::Map::new();
    for i in 0..1000 {
        obj.insert(format!("field_{}", i), json!(i));
    }
    let id = db.insert(json!(obj)).unwrap();
    let doc = db.get(&id).unwrap();
    assert_eq!(doc.get("field_500").unwrap(), 500);
}

#[test]
fn document_with_unicode_values() {
    let db = Database::open_in_memory().unwrap();
    let id = db.insert(json!({
        "emoji": "🎉🚀💻",
        "cjk": "日本語テスト",
        "arabic": "مرحبا",
        "mixed": "Hello 世界 🌍"
    })).unwrap();
    let doc = db.get(&id).unwrap();
    assert_eq!(doc["emoji"], "🎉🚀💻");
    assert_eq!(doc["cjk"], "日本語テスト");
}

#[test]
fn document_with_null_and_special_values() {
    let db = Database::open_in_memory().unwrap();
    let id = db.insert(json!({
        "null_val": null,
        "bool_true": true,
        "bool_false": false,
        "zero": 0,
        "empty_str": "",
        "empty_arr": [],
        "empty_obj": {}
    })).unwrap();
    let doc = db.get(&id).unwrap();
    assert!(doc["null_val"].is_null());
    assert_eq!(doc["bool_true"], true);
    assert_eq!(doc["bool_false"], false);
    assert_eq!(doc["zero"], 0);
    assert_eq!(doc["empty_str"], "");
}

#[test]
fn insert_update_delete_reinsert_same_data() {
    let (db, _dir) = setup();
    let id = db.insert(json!({"x": 1})).unwrap();
    db.update(&id, json!({"x": 2})).unwrap();
    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());

    // Insert new doc with same data — gets new ID
    let id2 = db.insert(json!({"x": 1})).unwrap();
    assert_ne!(id, id2);
    assert_eq!(db.get(&id2).unwrap()["x"], 1);
}

#[test]
fn rapid_insert_delete_cycle() {
    let db = Database::open_in_memory().unwrap();
    let mut ids = Vec::new();
    for i in 0..100 {
        let id = db.insert(json!({"cycle": i})).unwrap();
        ids.push(id);
        if i % 3 == 0 {
            db.delete(&ids[i / 3]).unwrap();
        }
    }
    // Should have 100 - 34 = 66 docs (deleted indices 0,3,6,...99 → 34 deletions)
    assert_eq!(db.len(), 66);
}

// ─── Concurrent Read Stress ──────────────────────────────────────────

#[test]
fn concurrent_reads_during_writes() {
    use std::sync::Arc;
    use std::thread;

    let db = Arc::new(Database::open_in_memory().unwrap());
    let mut handles = Vec::new();

    // Writer thread
    let writer_db = Arc::clone(&db);
    let writer = thread::spawn(move || {
        for i in 0..200 {
            writer_db.insert(json!({"writer": i})).unwrap();
        }
    });
    handles.push(writer);

    // Reader threads
    for t in 0..4 {
        let reader_db = Arc::clone(&db);
        let reader = thread::spawn(move || {
            for _ in 0..100 {
                let all = reader_db.iter();
                let _len = all.len();
                let _ = reader_db.find("writer", &json!(t));
            }
        });
        handles.push(reader);
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(db.len(), 200);
}

#[test]
fn concurrent_queries_during_inserts() {
    use std::sync::Arc;
    use std::thread;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("concurrent.jsonl");
    let db = Arc::new(
        Database::open(&path)
            .unwrap()
            .with_persistence(Persistence::Immediate),
    );

    // Pre-populate
    for i in 0..50 {
        db.insert(json!({"status": if i % 2 == 0 { "active" } else { "inactive" }, "idx": i}))
            .unwrap();
    }

    let mut handles = Vec::new();

    // Inserter
    let ins_db = Arc::clone(&db);
    handles.push(thread::spawn(move || {
        for i in 50..150 {
            ins_db.insert(json!({"status": "active", "idx": i})).unwrap();
        }
    }));

    // Queriers
    for _ in 0..3 {
        let q_db = Arc::clone(&db);
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                let results = q_db.query(json!({"status": {"$eq": "active"}}));
                assert!(results.len() <= 150);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(db.len(), 150);
}

// ─── Compaction Under Load ───────────────────────────────────────────

#[test]
fn compact_after_many_updates() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("compact_updates.jsonl");
    let db = Database::open(&path).unwrap();

    // Insert then update many times
    let mut ids = Vec::new();
    for _i in 0..50 {
        let id = db.insert(json!({"v": 0})).unwrap();
        ids.push(id);
    }

    // Update each doc 10 times
    for _ in 0..10 {
        for id in &ids {
            db.update(id, json!({"v": 42})).unwrap();
        }
    }

    // Delete half
    for id in &ids[0..25] {
        db.delete(id).unwrap();
    }

    // Compact
    db.compact().unwrap();

    // Reopen and verify
    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.len(), 25);
    for id in &ids[25..50] {
        let doc = db2.get(id).unwrap();
        assert_eq!(doc["v"], 42);
    }
}

#[test]
fn compact_reduces_file_size_significantly() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("size_test.jsonl");
    let db = Database::open(&path).unwrap();

    // Insert 100 docs
    let mut ids = Vec::new();
    for i in 0..100 {
        let id = db.insert(json!({"data": "x".repeat(100), "i": i})).unwrap();
        ids.push(id);
    }

    // Delete 90 of them
    for id in &ids[0..90] {
        db.delete(id).unwrap();
    }

    db.flush().unwrap();
    let size_before = fs::metadata(&path).unwrap().len();

    db.compact().unwrap();
    let size_after = fs::metadata(&path).unwrap().len();

    assert!(size_after < size_before / 2, "compaction should significantly reduce file size");
    assert_eq!(db.len(), 10);
}

// ─── Query Edge Cases ────────────────────────────────────────────────

#[test]
fn query_on_empty_db() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(db.query(json!({"x": 1})).len(), 0);
    assert_eq!(db.find("x", &json!(1)).len(), 0);
    assert_eq!(db.query_with(json!({"x": 1}), QueryOptions {
        limit: Some(10),
        offset: Some(0),
        sort_by: Some(("x".to_string(), SortDir::Asc)),
    }).len(), 0);
}

#[test]
fn query_with_dot_notation() {
    let db = Database::open_in_memory().unwrap();
    db.insert(json!({"user": {"name": "alice", "age": 30}})).unwrap();
    db.insert(json!({"user": {"name": "bob", "age": 25}})).unwrap();

    let results = db.query(json!({"user.name": {"$eq": "alice"}}));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["user"]["name"], "alice");
}

#[test]
fn query_with_nested_and_or() {
    let db = Database::open_in_memory().unwrap();
    db.insert(json!({"a": 1, "b": 10})).unwrap();
    db.insert(json!({"a": 2, "b": 20})).unwrap();
    db.insert(json!({"a": 3, "b": 30})).unwrap();
    db.insert(json!({"a": 1, "b": 30})).unwrap();

    let results = db.query(json!({
        "$and": [
            {"$or": [{"a": {"$eq": 1}}, {"a": {"$eq": 3}}]},
            {"b": {"$gte": 20}}
        ]
    }));
    assert_eq!(results.len(), 2); // (a=3,b=30) and (a=1,b=30)
}

#[test]
fn index_with_many_unique_values() {
    let db = Database::open_in_memory().unwrap();
    for i in 0..1000 {
        db.insert(json!({"email": format!("user{}@test.com", i)})).unwrap();
    }
    db.create_index("email").unwrap();

    let results = db.find("email", &json!("user500@test.com"));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["email"], "user500@test.com");
}

#[test]
fn btree_index_range_query() {
    let db = Database::open_in_memory().unwrap();
    for i in 0..100 {
        db.insert(json!({"score": i})).unwrap();
    }
    db.create_btree_index("score").unwrap();

    let results = db.find_range("score", &json!(20), &json!(80));
    assert_eq!(results.len(), 61); // 20..=80 inclusive
}

// ─── Persistence Across Reopens ──────────────────────────────────────

#[test]
fn full_lifecycle_persist_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("lifecycle.jsonl");

    // Phase 1: Insert
    let mut ids = Vec::new();
    {
        let db = Database::open(&path)
            .unwrap()
            .with_persistence(Persistence::Immediate);
        for i in 0..20 {
            ids.push(db.insert(json!({"val": i})).unwrap());
        }
        // Update some
        db.update(&ids[0], json!({"val": 999})).unwrap();
        db.update(&ids[5], json!({"val": 888})).unwrap();
        // Delete some
        db.delete(&ids[10]).unwrap();
        db.delete(&ids[15]).unwrap();
        db.create_index("val").unwrap();
    }

    // Phase 2: Reopen and verify
    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.len(), 18); // 20 - 2 deleted
    assert_eq!(db2.get(&ids[0]).unwrap()["val"], 999);
    assert_eq!(db2.get(&ids[5]).unwrap()["val"], 888);
    assert!(db2.get(&ids[10]).is_err());
    assert!(db2.get(&ids[15]).is_err());

    // Query
    let results = db2.query(json!({"val": {"$gte": 100}}));
    assert_eq!(results.len(), 2); // 999 and 888

    // Compact
    db2.compact().unwrap();

    // Phase 3: Reopen after compact
    let db3 = Database::open(&path).unwrap();
    assert_eq!(db3.len(), 18);
    assert_eq!(db3.get(&ids[0]).unwrap()["val"], 999);
}

// ─── File Bucket Edge Cases ──────────────────────────────────────────

#[test]
fn bucket_store_empty_file() {
    let (db, _dir) = setup();
    let bucket = db.bucket("empty_files");
    let meta = bucket.store("empty.txt", b"", "text/plain").unwrap();
    assert_eq!(meta.size, 0);

    let data = bucket.get(&meta._file).unwrap();
    assert_eq!(data.len(), 0);
}

#[test]
fn bucket_store_file_with_no_extension() {
    let (db, _dir) = setup();
    let bucket = db.bucket("noext");
    let meta = bucket.store("README", b"readme content", "text/plain").unwrap();
    assert_eq!(meta._file.ext, "");
}

#[test]
fn bucket_store_many_files() {
    let (db, _dir) = setup();
    let bucket = db.bucket("many");

    for i in 0..100 {
        let data = format!("file content {}", i);
        bucket.store(&format!("file_{}.txt", i), data.as_bytes(), "text/plain").unwrap();
    }

    let files = bucket.list().unwrap();
    assert_eq!(files.len(), 100);
}
