//! Integration tests for nDB Phase 1: Storage Core
//!
//! Tests basic CRUD operations, NanoID generation, JSON Lines I/O,
//! and in-memory document store.

use ndb::{Database, Error, Persistence};
use serde_json::json;
use tempfile::TempDir;

fn setup() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("integration.jsonl");
    let db = Database::open(&path).unwrap();
    (db, dir)
}

#[test]
fn full_crud_lifecycle() {
    let (db, _dir) = setup();

    // Insert
    let id = db.insert(json!({"title": "Test Doc", "value": 42})).unwrap();
    assert_eq!(id.len(), 16);

    // Get
    let doc = db.get(&id).unwrap();
    assert_eq!(doc["title"], "Test Doc");
    assert_eq!(doc["value"], 42);
    assert_eq!(doc["_id"], id);

    // Update
    db.update(&id, json!({"title": "Updated", "value": 100})).unwrap();
    let updated = db.get(&id).unwrap();
    assert_eq!(updated["title"], "Updated");
    assert_eq!(updated["value"], 100);

    // Delete
    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());
    assert_eq!(db.len(), 0);
}

#[test]
fn prefixed_ids() {
    let (db, _dir) = setup();

    let conv_id = db.insert_with_prefix("conv", json!({"msg": "hello"})).unwrap();
    assert!(conv_id.starts_with("conv_"));

    let user_id = db.insert_with_prefix("user", json!({"name": "alice"})).unwrap();
    assert!(user_id.starts_with("user_"));

    // Both should be retrievable
    assert_eq!(db.get(&conv_id).unwrap()["msg"], "hello");
    assert_eq!(db.get(&user_id).unwrap()["name"], "alice");
}

#[test]
fn persistence_across_reopens() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("persist.jsonl");

    let id = {
        let db = Database::open(&path).unwrap();
        let id = db.insert(json!({"persistent": true})).unwrap();
        db.flush().unwrap();
        id
    };

    // Reopen and verify
    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.len(), 1);
    let doc = db2.get(&id).unwrap();
    assert_eq!(doc["persistent"], true);
}

#[test]
fn immediate_persistence() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("immediate.jsonl");

    let id = {
        let db = Database::open(&path)
            .unwrap()
            .with_persistence(Persistence::Immediate);
        let id = db.insert(json!({"safe": true})).unwrap();
        // No explicit flush needed
        id
    };

    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.get(&id).unwrap()["safe"], true);
}

#[test]
fn in_memory_database() {
    let db = Database::open_in_memory().unwrap();

    let id1 = db.insert(json!({"a": 1})).unwrap();
    let id2 = db.insert(json!({"b": 2})).unwrap();

    assert_eq!(db.len(), 2);
    assert_eq!(db.get(&id1).unwrap()["a"], 1);
    assert_eq!(db.get(&id2).unwrap()["b"], 2);

    db.delete(&id1).unwrap();
    assert_eq!(db.len(), 1);
}

#[test]
fn many_documents() {
    let (db, _dir) = setup();

    let mut ids = Vec::new();
    for i in 0..1000 {
        let id = db.insert(json!({"index": i})).unwrap();
        ids.push(id);
    }

    assert_eq!(db.len(), 1000);

    // Verify random access
    for (i, id) in ids.iter().enumerate() {
        let doc = db.get(id).unwrap();
        assert_eq!(doc["index"], i);
    }

    // Delete half
    for id in ids.iter().take(500) {
        db.delete(id).unwrap();
    }
    assert_eq!(db.len(), 500);
}

#[test]
fn error_cases() {
    let (db, _dir) = setup();

    // Get nonexistent
    assert!(matches!(db.get("nonexistent"), Err(Error::NotFound { .. })));

    // Delete nonexistent
    assert!(matches!(db.delete("nonexistent"), Err(Error::NotFound { .. })));

    // Update nonexistent
    assert!(matches!(
        db.update("nonexistent", json!({"x": 1})),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn jsonl_file_format() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("format.jsonl");
    let db = Database::open(&path).unwrap();

    db.insert(json!({"hello": "world"})).unwrap();
    db.flush().unwrap();
    drop(db);

    // Read raw file and verify format
    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();

    // First line should be _meta header
    assert!(lines[0].contains("\"_meta\""));

    // Second line should be the document
    let doc: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(doc["hello"], "world");
    assert!(doc["_id"].is_string());
}
