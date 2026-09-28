//! Integration tests for nDB Phase 10: meta.json bucket policies (#10)
//!
//! The policy block (`buckets.<name>.{ onDocumentDelete, ttl_seconds }`) is
//! opt-in: no meta.json, no block, or no entry means exactly the
//! pre-policy behavior. A malformed policy fails open() loudly — an admin
//! config with a typo must never silently degrade to no-policy.

use ndb::{Database, Error, TrashMode};
use serde_json::{json, Value};
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

fn disk_db() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    (db, dir)
}

/// Write meta.json first, then open — policies are loaded at open.
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

// ─── Opt-in activation: absence means today's behavior ──────────────

#[test]
fn no_meta_json_means_no_policy() {
    let (db, _dir) = disk_db();
    let meta = db
        .bucket("avatars")
        .store("a.png", b"avatar", "image/png")
        .unwrap();
    let id = db
        .insert(json!({"avatar": meta._file.to_string_compact()}))
        .unwrap();

    // The pre-policy default: delete cascades, orphaned blob trashed.
    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());
    assert!(!db.bucket("avatars").exists(&meta._file));
}

#[test]
fn unknown_top_level_keys_and_no_buckets_block_mean_no_policy() {
    // The evolution plan's full example shape: `schemas` is a separate,
    // unimplemented item and must be ignored, not rejected.
    let (db, _dir) = db_with_meta(json!({
        "engine": "ndb",
        "version": 1,
        "schemas": { "conversation": { "fields": { "title": { "type": "string" } } } }
    }));
    let id = db.insert(json!({"v": 1})).unwrap();
    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());
}

// ─── onDocumentDelete: "restrict" ────────────────────────────────────

#[test]
fn restrict_refuses_the_delete_and_leaves_everything_untouched() {
    let (db, _dir) = db_with_meta(json!({
        "buckets": { "avatars": { "onDocumentDelete": "restrict" } }
    }));
    let bucket = db.bucket("avatars");
    let meta = bucket.store("a.png", b"avatar", "image/png").unwrap();
    let id = db
        .insert(json!({"avatar": meta._file.to_string_compact()}))
        .unwrap();

    let err = db.delete(&id).unwrap_err();
    assert!(
        matches!(err, Error::PolicyViolation { .. }),
        "expected PolicyViolation, got {err:?}"
    );

    // Nothing moved: document live, blob active, no tombstone.
    assert!(db.get(&id).is_ok());
    assert!(bucket.exists(&meta._file));
    assert!(db.deleted_ids().is_empty());

    // Reassign first (the policy's demand), and the delete proceeds.
    db.update(&id, json!({"avatar": Value::Null})).unwrap();
    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());
}

#[test]
fn restrict_only_applies_to_the_declared_bucket() {
    let (db, _dir) = db_with_meta(json!({
        "buckets": { "avatars": { "onDocumentDelete": "restrict" } }
    }));
    // The document references an *undeclared* bucket — no policy, default.
    let meta = db
        .bucket("attachments")
        .store("f.txt", b"file", "text/plain")
        .unwrap();
    let id = db
        .insert(json!({"file": meta._file.to_string_compact()}))
        .unwrap();

    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());
    assert!(!db.bucket("attachments").exists(&meta._file));
}

#[test]
fn trash_policy_is_the_documented_default() {
    let (db, _dir) = db_with_meta(json!({
        "buckets": { "attachments": { "onDocumentDelete": "trash" } }
    }));
    let bucket = db.bucket("attachments");
    let meta = bucket.store("f.txt", b"file", "text/plain").unwrap();
    let id = db
        .insert(json!({"file": meta._file.to_string_compact()}))
        .unwrap();

    // Declared "trash" behaves exactly like the undeclared default.
    db.delete(&id).unwrap();
    assert!(db.get(&id).is_err());
    assert!(!bucket.exists(&meta._file));
    assert!(bucket.trash_dir().join(meta._file.filename()).exists());
}

// ─── ttl_seconds: per-bucket trash TTL at purge ─────────────────────

#[test]
fn ttl_seconds_overrides_the_database_ttl_at_purge() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("meta.json"),
        serde_json::to_string(&json!({
            "buckets": { "avatars": { "ttl_seconds": 0 } }
        }))
        .unwrap(),
    )
    .unwrap();
    // Database-wide TTL of 30 days: nothing trashed today is purgeable…
    let db = Database::open(dir.path().join("data.jsonl"))
        .unwrap()
        .with_trash_mode(TrashMode::TTL(Duration::from_secs(30 * 24 * 3600)));

    let avatars = db.bucket("avatars");
    let a = avatars.store("a.png", b"a", "image/png").unwrap();
    let attachments = db.bucket("attachments");
    let b = attachments.store("b.txt", b"b", "text/plain").unwrap();
    avatars.delete(&a._file).unwrap();
    attachments.delete(&b._file).unwrap();
    let a_trashed = avatars.trash_dir().join(a._file.filename());
    let b_trashed = attachments.trash_dir().join(b._file.filename());
    assert!(a_trashed.exists() && b_trashed.exists());

    db.purge_trash().unwrap();

    // …except the bucket whose own ttl_seconds (0) overrides it.
    assert!(!a_trashed.exists(), "bucket TTL 0 should purge immediately");
    assert!(b_trashed.exists(), "db-wide 30d TTL should keep fresh trash");
}

// ─── Malformed policy fails open() loudly ────────────────────────────

#[test]
fn malformed_policies_fail_open_loudly() {
    for bad in [
        json!({"buckets": {"avatars": {"onDocumentDelete": "cascade"}}}),
        json!({"buckets": {"avatars": {"ttl_seconds": "30"}}}),
        json!({"buckets": {"avatars": "trash"}}),
        json!({"buckets": [1]}),
    ] {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("meta.json"),
            serde_json::to_string(&bad).unwrap(),
        )
        .unwrap();
        let err = match Database::open(dir.path().join("data.jsonl")) {
            Ok(_) => panic!("meta {bad} opened successfully — malformed policy must fail"),
            Err(e) => e,
        };
        assert!(
            matches!(err, Error::Corruption { .. }),
            "meta {bad} → expected Corruption, got {err:?}"
        );
    }
}

// ─── Legacy CLI bucket arrays open as no-policy (B1 compat) ─────────

#[test]
fn legacy_array_buckets_open_with_default_policies() {
    // `ndb init`/`merge`/`recover` wrote name arrays before #10; the
    // loader must treat them as "names, no policy", not corruption.
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("meta.json"),
        r#"{"version": 1, "buckets": ["avatars", "docs"]}"#,
    )
    .unwrap();
    let db = Database::open(dir.path().join("data.jsonl"))
        .unwrap_or_else(|e| panic!("legacy array meta must open, got {e}"));
    // A declared name parses as a usable bucket with default (no) policy.
    let handle = db.bucket("avatars");
    let stored = handle.store("a.png", b"a", "image/png").unwrap();
    assert_eq!(handle.get(&stored._file).unwrap(), b"a");
}

#[test]
fn meta_json_with_invalid_json_fails_open_loudly() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("meta.json"), b"{ not json").unwrap();
    let err = match Database::open(dir.path().join("data.jsonl")) {
        Ok(_) => panic!("invalid JSON in meta.json opened successfully"),
        Err(e) => e,
    };
    assert!(
        matches!(err, Error::Corruption { .. }),
        "expected Corruption, got {err:?}"
    );
}
