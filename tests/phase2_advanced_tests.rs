//! Integration tests for nDB Phase 2: Advanced Core
//!
//! Tests update, iteration, compaction, trash, and persistence modes.

use ndb::{Database, Persistence, TrashMode};
use serde_json::json;
use tempfile::TempDir;

fn setup() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("phase2.jsonl");
    let db = Database::open(&path).unwrap();
    (db, dir)
}

#[test]
fn update_preserves_id() {
    let (db, _dir) = setup();

    let id = db.insert(json!({"v": 1})).unwrap();
    db.update(&id, json!({"v": 2})).unwrap();

    let doc = db.get(&id).unwrap();
    assert_eq!(doc["_id"], id);
    assert_eq!(doc["v"], 2);
}

#[test]
fn update_file_append() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("update_append.jsonl");
    let db = Database::open(&path).unwrap();

    let id = db.insert(json!({"v": 1})).unwrap();
    db.update(&id, json!({"v": 2})).unwrap();
    db.flush().unwrap();
    drop(db);

    // File should have both versions (last-write-wins on reload)
    let content = std::fs::read_to_string(&path).unwrap();
    let doc_lines: Vec<&str> = content.lines().filter(|l| !l.contains("_meta")).collect();
    assert_eq!(doc_lines.len(), 2); // original + update

    // Reload should have latest version
    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.get(&id).unwrap()["v"], 2);
}

#[test]
fn iter_returns_active_only() {
    let (db, _dir) = setup();

    let _id1 = db.insert(json!({"keep": true})).unwrap();
    let _id2 = db.insert(json!({"keep": true})).unwrap();
    let id3 = db.insert(json!({"delete": true})).unwrap();

    db.delete(&id3).unwrap();

    let all = db.iter();
    assert_eq!(all.len(), 2);
    assert!(all.iter().all(|d| d.get("keep").is_some()));
}

#[test]
fn compact_reduces_file_size() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("compact.jsonl");
    let db = Database::open(&path).unwrap();

    // Insert and delete many docs
    let mut ids = Vec::new();
    for i in 0..100 {
        let id = db.insert(json!({"i": i})).unwrap();
        ids.push(id);
    }
    // Delete half
    for id in ids.iter().take(50) {
        db.delete(id).unwrap();
    }
    db.flush().unwrap();

    let size_before = std::fs::metadata(&path).unwrap().len();

    db.compact().unwrap();

    let size_after = std::fs::metadata(&path).unwrap().len();
    assert!(size_after < size_before);
    assert_eq!(db.len(), 50);
}

#[test]
fn compact_preserves_data() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("preserve.jsonl");
    let db = Database::open(&path).unwrap();

    let id1 = db.insert(json!({"name": "alice"})).unwrap();
    let id2 = db.insert(json!({"name": "bob"})).unwrap();
    let id3 = db.insert(json!({"name": "charlie"})).unwrap();
    db.delete(&id2).unwrap();

    db.compact().unwrap();

    assert_eq!(db.len(), 2);
    assert_eq!(db.get(&id1).unwrap()["name"], "alice");
    assert_eq!(db.get(&id3).unwrap()["name"], "charlie");
    assert!(db.get(&id2).is_err());
}

#[test]
fn compact_creates_trash_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("trash.jsonl");
    let db = Database::open(&path).unwrap();

    let id = db.insert(json!({"deleted_doc": true})).unwrap();
    db.delete(&id).unwrap();
    db.compact().unwrap();

    let trash_dir = dir.path().join("_trash").join("docs");
    assert!(trash_dir.exists());
    let trash_files: Vec<_> = std::fs::read_dir(&trash_dir).unwrap().collect();
    assert_eq!(trash_files.len(), 1);
}

#[test]
fn restore_deleted_document() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("restore.jsonl");
    let db = Database::open(&path).unwrap();

    let id = db.insert(json!({"restore_me": true, "val": 42})).unwrap();
    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());

    db.restore(&id).unwrap();
    let doc = db.get(&id).unwrap();
    assert_eq!(doc["restore_me"], true);
    assert_eq!(doc["val"], 42);
}

#[test]
fn deleted_ids_tracking() {
    let (db, _dir) = setup();

    let id1 = db.insert(json!({"a": 1})).unwrap();
    let _id2 = db.insert(json!({"b": 2})).unwrap();
    db.delete(&id1).unwrap();

    let deleted = db.deleted_ids();
    assert_eq!(deleted.len(), 1);
    assert!(deleted.contains(&id1));
}

#[test]
fn lazy_persistence_manual_flush() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("lazy.jsonl");
    let db = Database::open(&path).unwrap(); // Lazy by default

    let id = db.insert(json!({"lazy": true})).unwrap();
    // Without flush, data may not be on disk
    db.flush().unwrap();
    drop(db);

    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.get(&id).unwrap()["lazy"], true);
}

#[test]
fn scheduled_persistence_mode() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("scheduled.jsonl");
    let db = Database::open(&path)
        .unwrap()
        .with_persistence(Persistence::Scheduled(std::time::Duration::from_secs(60)));

    let id = db.insert(json!({"scheduled": true})).unwrap();
    db.flush().unwrap(); // Manual flush still works
    drop(db);

    let db2 = Database::open(&path).unwrap();
    assert_eq!(db2.get(&id).unwrap()["scheduled"], true);
}

#[test]
fn trash_mode_off() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("no_trash.jsonl");
    let db = Database::open(&path)
        .unwrap()
        .with_trash_mode(TrashMode::Off);

    let id = db.insert(json!({"x": 1})).unwrap();
    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());
    // With TrashMode::Off, no trash archiving on compact
    db.compact().unwrap();
    assert_eq!(db.len(), 0);
}
