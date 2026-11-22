//! Phase 3 — Slice 3B: the v2 binary/mmap backend wired into `Database`.
//!
//! These tests exercise the public API only: format detection, group commit
//! under `Immediate`, the compact slot store, the bounded change ring's WAL
//! fallback, non-blocking compaction, and persistence of declared indexes.

use ndb::{Database, Error, OpenOptions, Persistence, StorageFormat};
use serde_json::{json, Value};
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;

fn open_v2(dir: &std::path::Path) -> Database {
    Database::open_v2_with_options(
        dir,
        OpenOptions {
            format: StorageFormat::V2,
            persistence: Persistence::Immediate,
        },
    )
    .expect("open v2")
}

fn assert_v2_dir(dir: &std::path::Path) {
    assert!(dir.join("manifest.json").exists(), "manifest.json must exist");
    assert!(
        dir.join("base.gen_1.ndb").exists()
            || fs::read_dir(dir)
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("base.gen_")),
        "a base generation must exist"
    );
    assert!(!dir.join("data.jsonl").exists(), "v2 must not write a v1 journal");
}

#[test]
fn v2_initialises_and_round_trips_through_reopen() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());

    let id = db.insert(json!({"kind": "note", "n": 1})).unwrap();
    assert_eq!(db.get(&id).unwrap()["kind"], json!("note"));
    assert_eq!(db.len(), 1);
    drop(db);

    assert_v2_dir(dir.path());

    // Auto-detection: a folder with manifest.json opens on v2 by itself.
    let reopened = Database::open(dir.path()).expect("auto-detect v2");
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened.get(&id).unwrap()["n"], json!(1));
    assert_eq!(reopened.lineage().last_seq, 1);
}

#[test]
fn v2_auto_detect_prefers_the_manifest_and_ignores_a_leftover_v1_journal() {
    let dir = TempDir::new().unwrap();
    {
        let db = open_v2(dir.path());
        db.insert(json!({"v": 1})).unwrap();
    }
    // A stale v1 journal must be ignored, not merged.
    fs::write(
        dir.path().join("data.jsonl"),
        "{\"_meta\":{\"version\":1,\"created\":\"0\"}}\n{\"_id\":\"stale\",\"v\":99}\n",
    )
    .unwrap();

    let db = Database::open(dir.path()).unwrap();
    assert_eq!(db.len(), 1, "only the v2 document is live");
    assert!(db.get("stale").is_err());
}

#[test]
fn v2_mutations_persist_across_reopen() {
    let dir = TempDir::new().unwrap();
    let id = {
        let db = open_v2(dir.path());
        let id = db.insert(json!({"count": 1, "tags": ["a"]})).unwrap();
        db.set(&id, "nested.value", json!(7)).unwrap();
        db.array_push(&id, "tags", json!("b")).unwrap();
        assert!(db.remove(&id, "count").unwrap());
        db.update(&id, json!({"tags": ["a", "b"], "nested": {"value": 7}}))
            .unwrap();
        id
    };

    let db = open_v2(dir.path());
    let doc = db.get(&id).unwrap();
    assert_eq!(doc["nested"]["value"], json!(7));
    assert_eq!(doc["tags"], json!(["a", "b"]));
    assert!(doc.get("count").is_none(), "removed field stays removed");
}

#[test]
fn v2_apply_ops_is_atomic_and_persists() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    let id = db.insert(json!({"a": 1, "list": [1, 2]})).unwrap();

    let result = db
        .apply_ops(
            &id,
            &[
                db_patch_set("a", json!(2)),
                db_patch_push("list", json!(3)),
            ],
            None,
        )
        .unwrap();
    assert_eq!(result.seq, 2);
    assert_eq!(db.get(&id).unwrap()["list"], json!([1, 2, 3]));

    // An unresolvable batch applies nothing and writes nothing.
    let seq_before = db.lineage().last_seq;
    let err = db
        .apply_ops(
            &id,
            &[db_patch_set("a", json!(9)), db_patch_set("nope.deep", json!(1))],
            None,
        )
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument { .. }));
    assert_eq!(db.get(&id).unwrap()["a"], json!(2));
    assert_eq!(db.lineage().last_seq, seq_before);
}

fn db_patch_set(path: &str, value: Value) -> ndb::PatchOp {
    ndb::PatchOp::Set {
        path: path.to_string(),
        value,
    }
}

fn db_patch_push(path: &str, value: Value) -> ndb::PatchOp {
    ndb::PatchOp::ArrayPush {
        path: path.to_string(),
        value,
    }
}

#[test]
fn v2_optimistic_concurrency_token_is_per_document() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    let a = db.insert(json!({"v": 1})).unwrap();
    let b = db.insert(json!({"v": 1})).unwrap();

    let seq_a = db.doc_seq(&a).unwrap();
    db.update(&b, json!({"v": 2})).unwrap();

    // b moved; a's token is still valid.
    db.update_if(&a, json!({"v": 3}), Some(seq_a)).unwrap();
    let err = db.update_if(&a, json!({"v": 4}), Some(seq_a)).unwrap_err();
    assert!(matches!(err, Error::Conflict { .. }));
}

#[test]
fn v2_group_commit_coalesces_concurrent_immediate_writers() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_v2(dir.path()));

    let mut handles = Vec::new();
    for thread in 0..8 {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for i in 0..25 {
                let id = db
                    .insert(json!({"thread": thread, "i": i}))
                    .expect("concurrent insert");
                db.set(&id, "touched", json!(true)).expect("concurrent set");
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(db.len(), 200);
    // 200 inserts + 200 sets, all committed, no gaps and no duplicates.
    assert_eq!(db.lineage().last_seq, 400);
    let ids = db.get_all_ids();
    assert_eq!(ids.len(), 200);
    let docs = db.query(json!({"touched": true})).unwrap();
    assert_eq!(docs.len(), 200);

    let batch = db.changes_since(0).unwrap();
    assert_eq!(batch.events.len(), 400);
    for window in batch.events.windows(2) {
        assert_eq!(window[1].seq, window[0].seq + 1, "seqs are contiguous");
    }

    drop(db);
    let reopened = open_v2(dir.path());
    assert_eq!(reopened.len(), 200);
    assert_eq!(reopened.lineage().last_seq, 400);
}

#[test]
fn v2_indexes_are_declared_persisted_and_used() {
    let dir = TempDir::new().unwrap();
    {
        let db = open_v2(dir.path());
        for n in 0..50 {
            db.insert(json!({"bucket": format!("k{}", n % 5), "score": n}))
                .unwrap();
        }
        db.create_index("bucket").unwrap();
        db.create_btree_index("score").unwrap();
        assert!(db.has_index("bucket"));
        assert!(db.has_index("score"));
        assert_eq!(db.find("bucket", &json!("k3")).len(), 10);
        assert_eq!(db.find_range("score", &json!(10), &json!(19)).len(), 10);
    }

    // Reopen: the indexes come back from the WAL frames, not a rebuild.
    let db = open_v2(dir.path());
    assert!(db.has_index("bucket"), "hash index survives reopen");
    assert!(db.has_index("score"), "btree index survives reopen");
    assert_eq!(db.find("bucket", &json!("k3")).len(), 10);
    assert_eq!(db.find_range("score", &json!(40), &json!(49)).len(), 10);

    // Drop is journaled too.
    db.drop_index("bucket").unwrap();
    drop(db);
    let db = open_v2(dir.path());
    assert!(!db.has_index("bucket"));
    assert!(db.has_index("score"));
}

#[test]
fn v2_query_layers_match_the_v1_semantics() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    for (n, kind) in [(1, "a"), (2, "b"), (3, "a"), (10, "b")] {
        db.insert(json!({"kind": kind, "n": n, "meta": {"rank": n * 2}}))
            .unwrap();
    }
    db.create_index("kind").unwrap();
    db.create_btree_index("n").unwrap();

    assert_eq!(db.find("kind", &json!("a")).len(), 2);
    assert_eq!(db.find_where("n", |v| v.as_u64().unwrap() > 2).len(), 2);
    assert_eq!(db.find_range("n", &json!(2), &json!(3)).len(), 2);

    let eq = db.query(json!({"meta.rank": 20})).unwrap();
    assert_eq!(eq.len(), 1);
    let range = db
        .query(json!({"$and": [{"n": {"$gte": 3}}, {"kind": "a"}]}))
        .unwrap();
    assert_eq!(range.len(), 1);

    let opts = ndb::QueryOptions {
        limit: Some(2),
        offset: Some(1),
        sort_by: Some(("n".to_string(), ndb::SortDir::Desc)),
    };
    let (total, page) = db
        .query_projected(
            json!({"kind": {"$in": ["a", "b"]}}),
            opts,
            Some(&["n".to_string()]),
            None,
        )
        .unwrap();
    assert_eq!(total, 4);
    assert_eq!(page.len(), 2);
    assert_eq!(page[0]["n"], json!(3));
    assert_eq!(page[1]["n"], json!(2));

    let facets = db.facet_counts(&["kind".to_string()]);
    assert_eq!(facets[0].1.get("a"), Some(&2));
    assert_eq!(facets[0].1.get("b"), Some(&2));
}

#[test]
fn v2_change_ring_falls_back_to_the_wal_on_disk() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    // Comfortably more than the 1024-event ring.
    for i in 0..1100 {
        db.insert(json!({ "i": i })).unwrap();
    }
    assert!(ndb::V2_CHANGE_RING_CAP < 1100);

    // A cursor older than the ring must still be answered — from the WAL.
    let batch = db.changes_since(0).unwrap();
    assert_eq!(batch.events.len(), 1100);
    assert_eq!(batch.events[0].seq, 1);
    assert_eq!(batch.events[1099].seq, 1100);

    let tail = db.changes_since(1090).unwrap();
    assert_eq!(tail.events.len(), 10);

    let ahead = db.changes_since(5000).unwrap_err();
    assert!(matches!(ahead, Error::CursorAhead { .. }));
}

#[test]
fn v2_compaction_rebases_documents_and_keeps_writes_made_meanwhile() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    db.create_index("kind").unwrap();
    db.create_btree_index("n").unwrap();

    let mut ids = Vec::new();
    for i in 0..100 {
        ids.push(db.insert(json!({"kind": format!("k{}", i % 4), "n": i})).unwrap());
    }
    let seq_before = db.lineage().last_seq;
    db.compact().unwrap();

    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(dir.path().join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["base_gen"], json!(2));
    assert_eq!(manifest["wal_gen"], json!(2));
    assert_eq!(manifest["base_seq"], json!(seq_before));
    assert_eq!(manifest["doc_count"], json!(100));
    assert!(
        !dir.path().join("base.gen_1.ndb").exists(),
        "the superseded base is unlinked"
    );
    assert!(
        !dir.path().join("wal.gen_1.ndb").exists(),
        "the superseded WAL is unlinked"
    );

    // The compaction watermark makes old feed cursors expire.
    assert!(matches!(
        db.changes_since(0).unwrap_err(),
        Error::CursorExpired { .. }
    ));
    assert_eq!(db.len(), 100);
    assert_eq!(db.find("kind", &json!("k2")).len(), 25);

    // The declared indexes were written into the new base generation.
    let report = ndb::fsck_v2_dir(dir.path()).unwrap();
    assert_eq!(report.index_count, 2);

    // Writes after compaction land in the new WAL and survive a reopen.
    let new_id = db.insert(json!({"kind": "k2", "n": 1000})).unwrap();
    drop(db);
    let db = open_v2(dir.path());
    assert_eq!(db.len(), 101);
    assert_eq!(db.get(&new_id).unwrap()["n"], json!(1000));
    assert_eq!(db.find("kind", &json!("k2")).len(), 26);
    assert_eq!(db.find_range("n", &json!(0), &json!(99)).len(), 100);
}

#[test]
fn v2_compaction_races_writers_without_losing_any() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_v2(dir.path()));
    for i in 0..200 {
        db.insert(json!({"seed": i})).unwrap();
    }

    let writer = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            for i in 0..200 {
                db.insert(json!({"late": i})).unwrap();
            }
        })
    };
    // Compact while the writer is running; the mirror WAL must capture
    // everything that lands after the freeze point.
    db.compact().unwrap();
    writer.join().unwrap();

    assert_eq!(db.len(), 400);
    drop(db);
    let reopened = open_v2(dir.path());
    assert_eq!(reopened.len(), 400, "no write lost across compaction");
}

#[test]
fn v2_delete_writes_a_restorable_trash_record_and_tombstone() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    let id = db.insert(json!({"kind": "temp", "n": 1})).unwrap();
    db.delete(&id).unwrap();

    assert!(db.get(&id).is_err());
    assert_eq!(db.len(), 0);
    assert!(db.deleted_ids().contains(&id));
    assert!(dir
        .path()
        .join("_trash")
        .join("docs")
        .join("data.jsonl")
        .exists());

    db.restore(&id).unwrap();
    assert_eq!(db.get(&id).unwrap()["kind"], json!("temp"));
    assert!(!db.deleted_ids().contains(&id));

    // The tombstone and the restore both survive a reopen.
    db.delete(&id).unwrap();
    drop(db);
    let db = open_v2(dir.path());
    assert!(!db.contains(&id));
    db.restore(&id).unwrap();
    assert!(db.contains(&id));
}

#[test]
fn v2_full_text_index_works_and_tracks_writes() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    db.insert(json!({"body": "the quick brown fox"})).unwrap();
    db.insert(json!({"body": "lazy dog sleeps"})).unwrap();
    db.create_text_index("body").unwrap();

    let query = ndb::TextSearch::and(vec![ndb::TextQuery::Term("quick".to_string())]);
    assert_eq!(db.text_search("body", &query).unwrap().len(), 1);

    let id = db.insert(json!({"body": "quick silver"})).unwrap();
    assert_eq!(db.text_search("body", &query).unwrap().len(), 2);
    db.delete(&id).unwrap();
    assert_eq!(db.text_search("body", &query).unwrap().len(), 1);
}

#[test]
fn v2_bucket_files_follow_delete_and_restore() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    let meta = db.store_file("media", b"binary-bytes", "clip.webm").unwrap();
    let reference = meta._file.to_string_compact();

    let id = db
        .insert(json!({"name": "clip", "file": reference.clone()}))
        .unwrap();
    assert!(db.find("file", &json!(reference)).len() == 1);
    assert_eq!(db.list_files("media").unwrap().len(), 1);

    // Deleting the referencing document moves the orphaned blob to trash…
    db.delete(&id).unwrap();
    assert_eq!(db.list_files("media").unwrap().len(), 0);
    // …and restoring it brings the file back.
    db.restore(&id).unwrap();
    assert_eq!(db.list_files("media").unwrap().len(), 1);
    assert_eq!(db.get(&id).unwrap()["file"], json!(reference));
}

#[test]
fn v2_export_snapshot_is_openable_as_a_v1_database() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    for n in 0..10 {
        db.insert(json!({"n": n})).unwrap();
    }

    let out = TempDir::new().unwrap();
    db.export_snapshot(out.path()).unwrap();
    let snapshot = Database::open(out.path().join("data.jsonl")).unwrap();
    assert_eq!(snapshot.len(), 10);
    assert_eq!(snapshot.lineage().source_epoch_uuid, db.lineage().source_epoch_uuid);
}

#[test]
fn v2_jsonl_export_preserves_lineage_and_documents() {
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());
    for n in 0..5 {
        db.insert(json!({"n": n})).unwrap();
    }
    let out = dir.path().join("export.jsonl");
    assert_eq!(db.export_jsonl(&out, true).unwrap(), 5);

    let text = fs::read_to_string(&out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 6, "one _meta header plus five documents");
    let meta: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(
        meta["_meta"]["source_epoch_uuid"],
        json!(db.lineage().source_epoch_uuid)
    );
    for line in &lines[1..] {
        serde_json::from_str::<Value>(line).expect("every export line is valid JSON");
    }

    // The export opens as a normal v1 database.
    let imported = Database::open(&out).unwrap();
    assert_eq!(imported.len(), 5);
}

#[test]
fn registry_create_honours_the_requested_format() {    use ndb::server::{DbRegistry, PersistenceMode};

    let root = TempDir::new().unwrap();
    let registry = DbRegistry::new(
        Some(root.path().to_path_buf()),
        PersistenceMode::Immediate,
        5000,
    );

    // Default creation stays v1 — existing behaviour.
    registry.create("legacy", None).unwrap();
    assert!(root.path().join("legacy").join("data.jsonl").exists());
    assert!(!root.path().join("legacy").join("manifest.json").exists());

    // `format: v2` creates a binary folder database directly.
    let db = registry
        .create_with_format("binary", None, StorageFormat::V2)
        .unwrap();
    db.insert(json!({"v": 1})).unwrap();
    assert!(root.path().join("binary").join("manifest.json").exists());
    assert!(!root.path().join("binary").join("data.jsonl").exists());

    // A service config may declare the default format too.
    let config = ndb::server::ServiceConfig::from_json(r#"{"format": "v2"}"#).unwrap();
    assert_eq!(config.format().unwrap(), Some(StorageFormat::V2));
    let bad = ndb::server::ServiceConfig::from_json(r#"{"format": "v9"}"#).unwrap();
    assert!(bad.format().is_err());
}

#[test]
fn http_create_v2_and_use_it_over_the_wire() {
    use ndb::server::{serve_multi_in_background, DbRegistry, PersistenceMode, ServerConfig};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    fn raw(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("response headers");
        let head = String::from_utf8_lossy(&response[..split]).to_string();
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, head, response[split + 4..].to_vec())
    }

    let root = TempDir::new().unwrap();
    let registry = DbRegistry::new(
        Some(root.path().to_path_buf()),
        PersistenceMode::Immediate,
        5000,
    );
    let handle = serve_multi_in_background(
        Arc::new(registry),
        ServerConfig {
            bind: "127.0.0.1".to_string(),
            port: 0,
            ..ServerConfig::default()
        },
    )
    .unwrap();
    let port = handle.addr.port();

    let (status, _, body) = raw(
        port,
        "POST",
        "/v2/dbs",
        br#"{"name":"remote","format":"v2"}"#,
    );
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let created: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(created["db"]["format"], json!("v2"));
    assert!(root.path().join("remote").join("manifest.json").exists());

    let (status, _, body) = raw(port, "PUT", "/v2/dbs/remote/doc", br#"{"a":1}"#);
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let inserted: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(inserted["doc"]["a"], json!(1));

    let id = inserted["id"].as_str().unwrap();
    let (status, _, body) = raw(
        port,
        "GET",
        &format!("/v2/dbs/remote/doc/{id}"),
        b"",
    );
    assert_eq!(status, 200);
    let fetched: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(fetched["doc"]["_id"], json!(id));

    // Compaction over HTTP too.
    let (status, _, _) = raw(port, "POST", "/v2/dbs/remote/compact", b"");
    assert_eq!(status, 200);
    handle.shutdown();
}

#[test]
fn v2_empty_database_and_generation_sweep_on_open() {
    let dir = TempDir::new().unwrap();
    {
        let db = open_v2(dir.path());
        assert!(db.is_empty());
        db.insert(json!({"v": 1})).unwrap();
        db.compact().unwrap();
        db.insert(json!({"v": 2})).unwrap();
        db.compact().unwrap();
    }
    // A stale generation left behind by an interrupted run is swept.
    fs::write(dir.path().join("base.gen_99.ndb"), b"garbage").unwrap();
    fs::write(dir.path().join("wal.gen_99.ndb"), b"garbage").unwrap();

    let db = open_v2(dir.path());
    assert_eq!(db.len(), 2);
    assert!(!dir.path().join("base.gen_99.ndb").exists());
    assert!(!dir.path().join("wal.gen_99.ndb").exists());
}
