//! Integration tests for nDB Phase 9: Failure Discipline
//!
//! Every failure in here is a real one, injected at a real boundary: a
//! directory blocked by a file, a trash destination that cannot be created.
//! The property under test is not "an error happened" but *which* operation's
//! contract survived it — a delete that could not record a restorable copy
//! must not report the document as deleted, and a GC must never count a file
//! it did not move.

use ndb::{Database, Error};
use serde_json::json;
use std::fs;
use tempfile::TempDir;

fn disk_db() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    (db, dir)
}

// ─── Caller-supplied shape must never abort the process (#6) ────────

#[test]
fn insert_rejects_non_object_documents_instead_of_aborting() {
    let db = Database::open_in_memory().unwrap();

    for bad in [
        json!("a string"),
        json!(null),
        json!(7),
        json!([1, 2, 3]),
        json!(true),
    ] {
        let err = db.insert(bad.clone()).unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { .. }),
            "insert({bad}) returned {err:?}"
        );
    }

    // The rejection left the database usable rather than half-written.
    assert_eq!(db.len(), 0);
    db.insert(json!({"ok": true})).unwrap();
    assert_eq!(db.len(), 1);
}

#[test]
fn update_and_insert_with_prefix_reject_non_object_documents() {
    let db = Database::open_in_memory().unwrap();
    let id = db.insert(json!({"v": 1})).unwrap();

    let err = db.update(&id, json!("nope")).unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument { .. }),
        "update returned {err:?}"
    );

    let err = db.insert_with_prefix("conv", json!([1])).unwrap_err();
    assert!(
        matches!(err, Error::InvalidArgument { .. }),
        "insert_with_prefix returned {err:?}"
    );

    // The rejected update did not disturb the stored document.
    assert_eq!(db.get(&id).unwrap()["v"], json!(1));
    assert_eq!(db.len(), 1);
}

#[test]
fn any_json_value_is_still_accepted_as_a_field_value() {
    // The object guard is about *documents*. `set` and `array_push` take
    // arbitrary JSON and must keep accepting scalars, null and containers —
    // this is the test that fails if the guard is drawn too wide.
    let db = Database::open_in_memory().unwrap();
    let id = db.insert(json!({"v": 1})).unwrap();

    db.set(&id, "s", json!("text")).unwrap();
    db.set(&id, "n", json!(null)).unwrap();
    db.set(&id, "num", json!(3.5)).unwrap();
    db.set(&id, "flag", json!(false)).unwrap();
    db.array_push(&id, "list", json!(7)).unwrap();
    db.array_push(&id, "list", json!({"k": "v"})).unwrap();
    db.array_push(&id, "list", json!([1, 2])).unwrap();

    let doc = db.get(&id).unwrap();
    assert_eq!(doc["s"], json!("text"));
    assert_eq!(doc["n"], json!(null));
    assert_eq!(doc["num"], json!(3.5));
    assert_eq!(doc["flag"], json!(false));
    assert_eq!(doc["list"], json!([7, {"k": "v"}, [1, 2]]));
}

// ─── The trash record is the restorability guarantee ────────────────

#[test]
fn delete_fails_rather_than_claiming_an_unrecorded_deletion() {
    let (db, dir) = disk_db();
    let id = db.insert(json!({"v": 1})).unwrap();

    // Block the document-trash directory with a file: no restorable copy can
    // be written, so the delete cannot honestly happen.
    let trash_root = dir.path().join("_trash");
    fs::create_dir_all(&trash_root).unwrap();
    fs::write(trash_root.join("docs"), b"not a directory").unwrap();

    let err = db.delete(&id).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "expected I/O error, got {err:?}");

    // The document is exactly where it was: nothing was half-done.
    assert!(db.get(&id).is_ok(), "document vanished without a trash record");
    assert!(db.deleted_ids().is_empty());
    assert_eq!(db.len(), 1);
}

#[test]
fn a_delete_that_failed_on_the_trash_record_succeeds_on_retry() {
    let (db, dir) = disk_db();
    let id = db.insert(json!({"v": 1})).unwrap();

    let trash_root = dir.path().join("_trash");
    fs::create_dir_all(&trash_root).unwrap();
    fs::write(trash_root.join("docs"), b"not a directory").unwrap();
    assert!(db.delete(&id).is_err());

    // Clear the obstruction — the failure was recoverable, not a lost write.
    fs::remove_file(trash_root.join("docs")).unwrap();

    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());
    assert!(db.deleted_ids().contains(&id));

    // And the retry produced a record that can actually be restored.
    db.restore(&id).unwrap();
    assert_eq!(db.get(&id).unwrap()["v"], json!(1));
}

// ─── GC counts what it moved, and nothing else (#4) ─────────────────

#[test]
fn gc_buckets_moves_unreferenced_files_and_counts_them() {
    let (db, _dir) = disk_db();
    let meta = db
        .bucket("images")
        .store("orphan.png", b"orphan bytes", "image/png")
        .unwrap();
    assert!(db.bucket("images").exists(&meta._file));

    assert_eq!(db.gc_buckets().unwrap(), 1);

    // Moved, not destroyed: visibility is restored from the bucket trash.
    assert!(!db.bucket("images").exists(&meta._file));
    assert!(db
        .bucket("images")
        .trash_dir()
        .join(meta._file.filename())
        .exists());
}

#[test]
fn gc_buckets_counts_only_files_it_actually_moved() {
    let (db, dir) = disk_db();
    let meta = db
        .bucket("images")
        .store("orphan.png", b"orphan bytes", "image/png")
        .unwrap();

    // Block the bucket's trash directory with a file, so the move must fail.
    let trash_files = dir.path().join("_trash").join("files");
    fs::create_dir_all(&trash_files).unwrap();
    fs::write(trash_files.join("images"), b"not a directory").unwrap();

    let count = db.gc_buckets().unwrap();
    assert_eq!(count, 0, "gc counted a file it never moved");

    // Which the caller can now verify against reality.
    assert!(db.bucket("images").exists(&meta._file));
}

#[test]
fn gc_buckets_leaves_referenced_files_alone() {
    let (db, _dir) = disk_db();
    let meta = db
        .bucket("images")
        .store("kept.png", b"kept bytes", "image/png")
        .unwrap();
    let id = db
        .insert(json!({"avatar": meta._file.to_string_compact()}))
        .unwrap();

    assert_eq!(db.gc_buckets().unwrap(), 0);
    assert!(db.bucket("images").exists(&meta._file));

    // Deleting the last referent orphans it, and the delete retires it.
    db.delete(&id).unwrap();
    assert!(!db.bucket("images").exists(&meta._file));
    assert!(db
        .bucket("images")
        .trash_dir()
        .join(meta._file.filename())
        .exists());
}

// ─── Restore survives a file that is no longer in trash ─────────────

#[test]
fn restore_returns_the_document_even_when_a_trashed_file_is_gone() {
    let (db, dir) = disk_db();
    let meta = db
        .bucket("images")
        .store("avatar.png", b"avatar bytes", "image/png")
        .unwrap();
    let id = db
        .insert(json!({"avatar": meta._file.to_string_compact()}))
        .unwrap();
    db.delete(&id).unwrap();

    // The file is in the bucket trash; lose it, then restore the document.
    let trashed = db
        .bucket("images")
        .trash_dir()
        .join(meta._file.filename());
    assert!(trashed.exists());
    fs::remove_file(&trashed).unwrap();

    // The document comes back — the failure to return its file is reported,
    // not converted into a refusal to restore anything at all.
    db.restore(&id).unwrap();
    assert_eq!(
        db.get(&id).unwrap()["avatar"],
        json!(meta._file.to_string_compact())
    );

    let _ = dir;
}

// ─── Journal-first ordering: a failed append moves nothing (#7) ─────
//
// The injection is the #7 probe shape: compact() drops the cached journal
// handle, then the journal path is replaced by a directory so the next
// append-open fails. The contract under test: update / set / array_push /
// remove must leave the document, the secondary indexes, the ref counters,
// the live blobs and the journal itself exactly as they were — and succeed
// whole on retry.

/// Replace the journal with a directory so the next append-open fails.
/// Returns the path the original journal was moved aside to.
fn break_journal(dir: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let journal = dir.path().join("data.jsonl");
    let aside = dir.path().join("data.jsonl.aside");
    fs::rename(&journal, &aside).unwrap();
    fs::create_dir(&journal).unwrap();
    (journal, aside)
}

fn heal_journal(journal: &std::path::Path, aside: &std::path::Path) {
    fs::remove_dir(journal).unwrap();
    fs::rename(aside, journal).unwrap();
}

#[test]
fn a_failed_journal_append_leaves_update_state_untouched() {
    let (db, dir) = disk_db();

    // A live media ref and an indexed field — the state the #7 probe showed
    // being destroyed by a failed write.
    let bucket = db.bucket("images");
    let meta = bucket
        .store("avatar.png", b"avatar bytes", "image/png")
        .unwrap();
    db.create_index("version").unwrap();
    let id = db
        .insert(json!({
            "version": "old",
            "avatar": meta._file.to_string_compact()
        }))
        .unwrap();
    db.compact().unwrap(); // close the journal handle so the open below fails

    let (journal, aside) = break_journal(&dir);
    let err = db.update(&id, json!({"version": "new"})).unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "expected I/O error, got {err:?}");

    // Document, index, ref counters and the blob itself: all as they were.
    let doc = db.get(&id).unwrap();
    assert_eq!(doc["version"], json!("old"));
    assert_eq!(doc["avatar"], json!(meta._file.to_string_compact()));
    assert_eq!(db.find("version", &json!("old")).len(), 1);
    assert!(db.find("version", &json!("new")).is_empty());
    assert!(
        bucket.exists(&meta._file),
        "live media trashed by a failed write — the #7 defect"
    );

    // Retry path: heal the journal and the same update commits whole —
    // including the ref-counted cleanup that the failed attempt skipped.
    heal_journal(&journal, &aside);
    db.update(&id, json!({"version": "new"})).unwrap();
    let doc = db.get(&id).unwrap();
    assert_eq!(doc["version"], json!("new"));
    assert!(doc.get("avatar").is_none());
    assert!(
        !bucket.exists(&meta._file),
        "committed update should trash the orphaned blob"
    );
    assert_eq!(db.find("version", &json!("new")).len(), 1);
}

#[test]
fn failed_delta_ops_leave_the_document_and_journal_untouched() {
    let (db, dir) = disk_db();
    let id = db
        .insert(json!({"v": 1, "list": [1], "keep": true}))
        .unwrap();
    db.compact().unwrap();

    let (journal, aside) = break_journal(&dir);
    assert!(db.set(&id, "v", json!(2)).is_err());
    assert!(db.array_push(&id, "list", json!(2)).is_err());
    assert!(db.remove(&id, "keep").is_err());

    // Nothing moved in memory.
    assert_eq!(
        db.get(&id).unwrap(),
        json!({"_id": id, "v": 1, "list": [1], "keep": true})
    );

    // And nothing reached the journal: replayed from scratch, the database
    // shows the same untouched document.
    drop(db);
    heal_journal(&journal, &aside);
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    assert_eq!(
        db.get(&id).unwrap(),
        json!({"_id": id, "v": 1, "list": [1], "keep": true})
    );

    // The same ops succeed whole on retry.
    db.set(&id, "v", json!(2)).unwrap();
    db.array_push(&id, "list", json!(2)).unwrap();
    db.remove(&id, "keep").unwrap();
    assert_eq!(
        db.get(&id).unwrap(),
        json!({"_id": id, "v": 2, "list": [1, 2]})
    );
}
