//! Integration tests for nDB Phase 11: Item buckets (#9)
//!
//! The second bucket kind: item-keyed folders (`_files/<bucket>/<itemId>/`)
//! where the application streams bytes itself and commits one facts record.
//! Mirrors the nCMS pool semantics the spec was written from:
//! reserve → stream → commit → trash → restore → reserved-sweep → verify.
//!
//! Hash-kind regression is covered by the unmodified phase3/phase9 suites —
//! these tests only cover what the item kind adds.

use ndb::{Database, Error};
use serde_json::{json, Value};
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

/// sha256("hello world") — known vector, so tests never hash through the
/// engine they are checking.
const HELLO_SHA256: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

fn db_with_meta(meta: Value) -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("meta.json"),
        serde_json::to_string(&meta).unwrap(),
    )
    .unwrap();
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    (db, dir)
}

fn items_db() -> (Database, TempDir) {
    db_with_meta(json!({"buckets": {"media": {"kind": "items"}}}))
}

fn hello_facts() -> Value {
    json!({
        "name": "photo.png",
        "size": 11,
        "mime": "image/png",
        "sha256": HELLO_SHA256,
        "custom": {"width": 100}
    })
}

/// create + stream + commit, the full happy path in one helper.
fn committed_item(db: &Database) -> (String, std::path::PathBuf) {
    let (item_id, folder) = db.create_item("media").unwrap();
    fs::write(folder.join("photo.png"), b"hello world").unwrap();
    db.commit_item("media", &item_id, hello_facts()).unwrap();
    (item_id, folder)
}

// ─── Declaration gating (safety by construction) ────────────────────

#[test]
fn item_api_requires_the_kind_declaration() {
    let (db, _dir) = db_with_meta(json!({
        "buckets": { "attachments": { "kind": "hash" } }
    }));
    for bucket in ["attachments", "media"] {
        let err = db.create_item(bucket).unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { .. }),
            "create_item('{bucket}') without declaration returned {err:?}"
        );
    }
}

#[test]
fn malformed_kind_or_reserved_ttl_fails_open_loudly() {
    for bad in [
        json!({"buckets": {"media": {"kind": "boxes"}}}),
        json!({"buckets": {"media": {"kind": "items", "reserved_ttl_seconds": "30"}}}),
    ] {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("meta.json"),
            serde_json::to_string(&bad).unwrap(),
        )
        .unwrap();
        match Database::open(dir.path().join("data.jsonl")) {
            Ok(_) => panic!("meta {bad} opened successfully — malformed policy must fail"),
            Err(e) => assert!(
                matches!(e, Error::Corruption { .. }),
                "meta {bad} → expected Corruption, got {e:?}"
            ),
        }
    }
}

// ─── The lifecycle (nCMS pool semantics) ─────────────────────────────

#[test]
fn reserve_stream_commit_trash_restore() {
    let (db, _dir) = items_db();

    // Reserve: folder + reserved record, path is engine-issued.
    let (item_id, folder) = db.create_item("media").unwrap();
    assert!(folder.is_dir());
    assert!(folder.ends_with(&item_id));
    let rec = db.read_item("media", &item_id).unwrap();
    assert_eq!(rec["state"], json!("reserved"));

    // The application streams bytes itself; variants and the description
    // file are opaque payload the engine never parses.
    fs::write(folder.join("photo.png"), b"hello world").unwrap();
    fs::write(folder.join("photo_720.webp"), b"variant").unwrap();
    fs::write(folder.join("asset.json"), br#"{"title":"x"}"#).unwrap();

    // Commit: one write; caller fields pass through.
    db.commit_item("media", &item_id, hello_facts()).unwrap();
    let rec = db.read_item("media", &item_id).unwrap();
    assert_eq!(rec["state"], json!("live"));
    assert_eq!(rec["facts"]["custom"]["width"], json!(100));
    assert!(rec["committed"].is_u64());
    assert!(db.verify_item_buckets().is_empty());

    // Trash: folder moves whole, payload included; record tombstoned.
    db.delete_item("media", &item_id).unwrap();
    assert!(db.read_item("media", &item_id).is_err());
    assert!(!folder.exists());
    let trashed = _dir.path().join("_trash").join("files").join("media").join(&item_id);
    assert!(trashed.join("photo.png").exists());
    assert!(trashed.join("photo_720.webp").exists());
    assert!(trashed.join("asset.json").exists());

    // Restore: tombstone lifted, folder back whole.
    db.restore_item("media", &item_id).unwrap();
    assert!(folder.join("photo.png").exists());
    assert!(folder.join("asset.json").exists());
    let rec = db.read_item("media", &item_id).unwrap();
    assert_eq!(rec["state"], json!("live"));
    assert!(db.verify_item_buckets().is_empty());
}

#[test]
fn list_items_filters_by_state() {
    let (db, _dir) = items_db();
    let (committed, _) = committed_item(&db);
    let (reserved, _) = db.create_item("media").unwrap();

    let all = db.list_items("media", None).unwrap();
    assert_eq!(all.len(), 2);
    let reserved_only = db.list_items("media", Some("reserved")).unwrap();
    assert_eq!(reserved_only.len(), 1);
    assert_eq!(reserved_only[0]["itemId"], json!(reserved));
    let live_only = db.list_items("media", Some("live")).unwrap();
    assert_eq!(live_only.len(), 1);
    assert_eq!(live_only[0]["itemId"], json!(committed));

    // Tombstoned items are never listed.
    db.delete_item("media", &committed).unwrap();
    assert_eq!(db.list_items("media", None).unwrap().len(), 1);

    let err = db.list_items("media", Some("bogus")).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument { .. }));
}

#[test]
fn commit_requires_a_reserved_item_and_valid_facts() {
    let (db, _dir) = items_db();

    // Unknown id.
    let err = db.commit_item("media", "itm_nope", hello_facts()).unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }));

    // Facts validation: name, size, sha256 are the integrity contract.
    let (item_id, _folder) = db.create_item("media").unwrap();
    for bad in [
        json!({"size": 11, "sha256": HELLO_SHA256}),                 // no name
        json!({"name": "", "size": 11, "sha256": HELLO_SHA256}),     // empty name
        json!({"name": "x", "sha256": HELLO_SHA256}),                // no size
        json!({"name": "x", "size": 11}),                            // no sha256
        json!({"name": "x", "size": 11, "sha256": "nothex"}),        // bad sha256
    ] {
        let err = db.commit_item("media", &item_id, bad.clone()).unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { .. }),
            "facts {bad} returned {err:?}"
        );
    }

    // Double commit refuses.
    let (committed, _) = committed_item(&db);
    let err = db.commit_item("media", &committed, hello_facts()).unwrap_err();
    assert!(matches!(err, Error::InvalidArgument { .. }));
}

// ─── States and sweeps ───────────────────────────────────────────────

#[test]
fn reserved_items_are_swept_by_their_ttl() {
    let (db, _dir) = db_with_meta(json!({
        "buckets": { "media": { "kind": "items", "reserved_ttl_seconds": 0 } }
    }));
    let (live, live_folder) = committed_item(&db);
    let (stale, stale_folder) = db.create_item("media").unwrap();

    // TTL 0: every reserved item is immediately sweepable.
    assert_eq!(db.sweep_reserved_items("media").unwrap(), 1);

    // The abandoned reservation is tombstoned and its folder trashed;
    // the live item is untouched.
    assert!(db.read_item("media", &stale).is_err());
    assert!(!stale_folder.exists());
    assert!(_dir
        .path()
        .join("_trash")
        .join("files")
        .join("media")
        .join(&stale)
        .exists());
    assert!(db.read_item("media", &live).is_ok());
    assert!(live_folder.exists());

    // Sweeping again finds nothing.
    assert_eq!(db.sweep_reserved_items("media").unwrap(), 0);
}

#[test]
fn the_ttl_background_thread_also_sweeps_reserved_items() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("meta.json"),
        serde_json::to_string(&json!({
            "buckets": { "media": { "kind": "items", "reserved_ttl_seconds": 0 } }
        }))
        .unwrap(),
    )
    .unwrap();
    let db = Database::open(dir.path().join("data.jsonl"))
        .unwrap()
        .with_trash_ttl(Duration::from_secs(3600), Duration::from_millis(50));

    let (item_id, folder) = db.create_item("media").unwrap();

    // The sweep runs on the thread's cadence; poll generously instead of
    // sleeping a fixed guess.
    let mut swept = false;
    for _ in 0..60 {
        if !folder.exists() && db.read_item("media", &item_id).is_err() {
            swept = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(swept, "background thread never swept the stale reservation");
}

// ─── Crash windows and verify (#9 states) ────────────────────────────

#[test]
fn verify_flags_orphan_folders_and_missing_or_corrupt_originals() {
    let (db, dir) = items_db();
    assert!(db.verify_item_buckets().is_empty());

    // Crash between folder creation and commit leaves a reserved item —
    // visible, not an anomaly. A folder with NO record is an orphan.
    let orphan = dir.path().join("_files").join("media").join("itm_orphan");
    fs::create_dir_all(&orphan).unwrap();
    // A live record whose folder vanished.
    let (no_folder, folder) = committed_item(&db);
    fs::remove_dir_all(&folder).unwrap();
    // A live item whose original no longer matches its facts.
    let (corrupt, corrupt_folder) = committed_item(&db);
    fs::write(corrupt_folder.join("photo.png"), b"tampered bytes!").unwrap();

    let anomalies = db.verify_item_buckets();
    let text = anomalies.join("\n");
    assert!(
        text.contains("itm_orphan") && text.contains("orphan folder"),
        "orphan not flagged: {text}"
    );
    assert!(
        text.contains(&no_folder) && text.contains("record has no folder"),
        "missing folder not flagged: {text}"
    );
    assert!(
        text.contains(&corrupt) && text.contains("size mismatch"),
        "corruption not flagged: {text}"
    );
}

#[test]
fn a_failed_journal_write_at_create_removes_the_folder() {
    let (db, dir) = items_db();
    // Force the item journal's append-open to fail.
    let journal = dir.path().join("_files").join("media").join("items.jsonl");
    fs::create_dir_all(&journal).unwrap();

    let err = db.create_item("media").unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "expected I/O error, got {err:?}");

    // All-or-nothing from the caller's side: no folder left behind,
    // no record in memory.
    let bucket_dir = dir.path().join("_files").join("media");
    let leftover_folders = fs::read_dir(&bucket_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| e.file_name() != "items.jsonl")
        .count();
    assert_eq!(leftover_folders, 0, "failed create left a folder behind");
    assert_eq!(db.list_items("media", None).unwrap().len(), 0);
}

#[test]
fn a_folder_that_fails_to_move_stays_and_is_flagged() {
    let (db, dir) = items_db();
    let (item_id, folder) = committed_item(&db);

    // Block the bucket's trash path with a file: the folder cannot move.
    let trash_files = dir.path().join("_trash").join("files");
    fs::create_dir_all(&trash_files).unwrap();
    fs::write(trash_files.join("media"), b"not a directory").unwrap();

    // The delete commits anyway — the tombstone is the truth; the
    // stranded folder is under-cleaning, named and reported.
    db.delete_item("media", &item_id).unwrap();
    assert!(db.read_item("media", &item_id).is_err());
    assert!(folder.exists(), "folder should stay when the move fails");

    let anomalies = db.verify_item_buckets().join("\n");
    assert!(
        anomalies.contains(&item_id) && anomalies.contains("tombstoned"),
        "stranded folder not flagged: {anomalies}"
    );
}

// ─── Durability: replay, snapshot, purge ─────────────────────────────

#[test]
fn item_records_replay_across_reopen() {
    let (db, dir) = items_db();
    let (live, _) = committed_item(&db);
    let (reserved, _) = db.create_item("media").unwrap();
    let (deleted, _) = committed_item(&db);
    db.delete_item("media", &deleted).unwrap();
    drop(db);

    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    assert_eq!(db.read_item("media", &live).unwrap()["state"], json!("live"));
    assert_eq!(
        db.read_item("media", &reserved).unwrap()["state"],
        json!("reserved")
    );
    assert!(db.read_item("media", &deleted).is_err());
    assert_eq!(db.list_items("media", None).unwrap().len(), 2);
    assert!(db.verify_item_buckets().is_empty());
}

#[test]
fn export_snapshot_carries_item_folders_and_the_journal() {
    let (db, dir) = items_db();
    let (item_id, _) = committed_item(&db);

    let target = dir.path().join("snapshot");
    db.export_snapshot(&target).unwrap();

    let snapped = target.join("_files").join("media").join(&item_id);
    assert!(snapped.join("photo.png").exists());
    assert!(target
        .join("_files")
        .join("media")
        .join("items.jsonl")
        .exists());
    assert!(target.join("meta.json").exists());
}

#[test]
fn trashed_item_folders_are_purged_whole() {
    let (db, dir) = items_db();
    let (item_id, _) = committed_item(&db);
    db.delete_item("media", &item_id).unwrap();
    let trashed = dir
        .path()
        .join("_trash")
        .join("files")
        .join("media")
        .join(&item_id);
    assert!(trashed.exists());

    // Manual mode, explicit purge: everything in trash goes.
    db.purge_trash().unwrap();
    assert!(!trashed.exists(), "item folder should purge as a unit");
}
