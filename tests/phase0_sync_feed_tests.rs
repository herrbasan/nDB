//! Phase 0 — durable lineage, monotonic `seq`, and the sync feed
//! (`lineage`, `snapshot_at`, `changes_since`).
//!
//! The contract under test: every committed document mutation takes the next
//! `seq` and becomes one event whose payload is the post-mutation document
//! (or a delete), the sequence never resets — not across compaction, not
//! across a restart — the engine sequence never leaks into documents, and a
//! watermark plus the events after it reconstructs the exact live state.

use ndb::{ChangeKind, Database, Error};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use tempfile::TempDir;

fn disk_db(dir: &TempDir) -> Database {
    Database::open(dir.path().join("data.jsonl")).unwrap()
}

fn live_state(db: &Database) -> BTreeMap<String, Value> {
    db.iter()
        .into_iter()
        .map(|doc| {
            let id = doc["_id"]
                .as_str()
                .expect("every doc has an _id")
                .to_string();
            (id, doc)
        })
        .collect()
}

#[test]
fn seq_is_monotonic_across_every_mutation() {
    let dir = TempDir::new().unwrap();
    let db = disk_db(&dir);

    let fresh = db.lineage();
    assert_eq!(fresh.last_seq, 0);
    assert_eq!(fresh.min_available_seq, 1);
    assert_eq!(fresh.source_epoch_uuid.len(), 36);

    let a = db.insert(json!({"v": 1})).unwrap();
    assert_eq!(db.lineage().last_seq, 1);
    let b = db.insert(json!({"list": [1], "keep": true})).unwrap();
    assert_eq!(db.lineage().last_seq, 2);
    db.update(&a, json!({"v": 2})).unwrap();
    assert_eq!(db.lineage().last_seq, 3);
    assert!(db.set(&b, "keep", json!(false)).unwrap());
    assert_eq!(db.lineage().last_seq, 4);
    assert!(db.array_push(&b, "list", json!(2)).unwrap());
    assert_eq!(db.lineage().last_seq, 5);
    assert!(db.remove(&b, "keep").unwrap());
    assert_eq!(db.lineage().last_seq, 6);
    db.delete(&a).unwrap();
    assert_eq!(db.lineage().last_seq, 7);
    db.restore(&a).unwrap();
    assert_eq!(db.lineage().last_seq, 8);

    let batch = db.changes_since(0).unwrap();
    assert_eq!(batch.lineage.last_seq, 8);
    let seqs: Vec<u64> = batch.events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=8).collect::<Vec<u64>>());
    assert!(
        batch.events.windows(2).all(|w| w[0].seq < w[1].seq),
        "events must be strictly ascending"
    );

    let ids: Vec<&str> = batch.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            a.as_str(),
            b.as_str(),
            a.as_str(),
            b.as_str(),
            b.as_str(),
            b.as_str(),
            a.as_str(),
            a.as_str()
        ]
    );

    // The full post-mutation document, not the journal line.
    assert_eq!(
        batch.events[0].kind,
        ChangeKind::Upsert(json!({"_id": a, "v": 1}))
    );
    assert_eq!(
        batch.events[3].kind,
        ChangeKind::Upsert(json!({"_id": b, "list": [1], "keep": false}))
    );
    assert_eq!(
        batch.events[4].kind,
        ChangeKind::Upsert(json!({"_id": b, "list": [1, 2], "keep": false}))
    );
    assert_eq!(
        batch.events[5].kind,
        ChangeKind::Upsert(json!({"_id": b, "list": [1, 2]}))
    );
    assert_eq!(batch.events[6].kind, ChangeKind::Delete);
    assert_eq!(
        batch.events[7].kind,
        ChangeKind::Upsert(json!({"_id": a, "v": 2})),
        "restore carries the restored document"
    );

    // A tail cursor returns exactly the tail.
    let tail = db.changes_since(6).unwrap();
    assert_eq!(tail.events.len(), 2);
    assert_eq!(tail.events[0].seq, 7);
}

#[test]
fn no_op_writes_do_not_commit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.jsonl");
    let db = Database::open(&path).unwrap();

    let id = db.insert(json!({"list": [1], "keep": true})).unwrap();
    db.flush().unwrap();

    let before = db.lineage().last_seq;
    let before_len = std::fs::metadata(&path).unwrap().len();
    let before_events = db.changes_since(0).unwrap().events.len();

    assert!(!db.set(&id, "missing.deep", json!(1)).unwrap());
    assert!(!db.remove(&id, "nope").unwrap());
    assert!(!db.array_push(&id, "keep", json!(1)).unwrap());
    db.flush().unwrap();

    assert_eq!(db.lineage().last_seq, before, "a no-op must not take a seq");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        before_len,
        "a no-op must not write a journal line"
    );
    assert_eq!(
        db.changes_since(0).unwrap().events.len(),
        before_events,
        "a no-op must not appear in the feed"
    );
    assert_eq!(
        db.get(&id).unwrap(),
        json!({"_id": id, "list": [1], "keep": true})
    );
}

#[test]
fn seq_never_leaks_into_documents() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.jsonl");
    let db = Database::open(&path).unwrap();

    let id = db.insert(json!({"v": 1})).unwrap();
    db.update(&id, json!({"v": 2})).unwrap();
    assert!(db.set(&id, "v", json!(3)).unwrap());

    assert!(db.get(&id).unwrap().get("_seq").is_none());
    assert!(db.iter().iter().all(|d| d.get("_seq").is_none()));
    assert!(db
        .query(json!({"v": 3}))
        .unwrap()
        .iter()
        .all(|d| d.get("_seq").is_none()));

    let snapshot = db.snapshot_at();
    for (_, doc) in &snapshot.docs {
        assert!(doc.get("_seq").is_none(), "snapshot leaked _seq: {doc}");
    }
    for event in &db.changes_since(0).unwrap().events {
        if let ChangeKind::Upsert(doc) = &event.kind {
            assert!(doc.get("_seq").is_none(), "event leaked _seq: {doc}");
        }
    }

    // The journal itself is where the seq lives.
    db.flush().unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    let mut lines = raw.lines();
    let header = lines.next().unwrap();
    assert!(header.contains("\"source_epoch_uuid\""));
    assert!(header.contains("\"last_seq\""));
    assert!(header.contains("\"min_available_seq\""));
    let doc_lines: Vec<&str> = lines.collect();
    assert_eq!(doc_lines.len(), 3);
    for (i, line) in doc_lines.iter().enumerate() {
        let entry: Value = serde_json::from_str(line).unwrap();
        assert_eq!(entry["_seq"], json!((i + 1) as u64));
    }
}

#[test]
fn compact_preserves_identity_advances_window() {
    let dir = TempDir::new().unwrap();
    let db = disk_db(&dir);

    let a = db.insert(json!({"n": 1})).unwrap();
    let b = db.insert(json!({"n": 2})).unwrap();
    db.delete(&b).unwrap();
    let before = db.lineage();
    assert_eq!(before.last_seq, 3);

    db.compact().unwrap();
    let after = db.lineage();
    assert_eq!(after.source_epoch_uuid, before.source_epoch_uuid);
    assert_eq!(
        after.last_seq, before.last_seq,
        "compaction must never reset seq"
    );
    assert_eq!(after.min_available_seq, before.last_seq + 1);

    assert!(db.changes_since(after.last_seq).unwrap().events.is_empty());
    match db.changes_since(after.last_seq - 1) {
        Err(Error::CursorExpired {
            requested_seq,
            min_available_seq,
            last_seq,
        }) => {
            assert_eq!(requested_seq, after.last_seq - 1);
            assert_eq!(min_available_seq, after.last_seq + 1);
            assert_eq!(last_seq, after.last_seq);
        }
        other => panic!("expected CursorExpired, got {other:?}"),
    }

    // Post-compact writes continue the same sequence.
    assert!(db.set(&a, "n", json!(10)).unwrap());
    assert_eq!(db.lineage().last_seq, before.last_seq + 1);
    let batch = db.changes_since(before.last_seq).unwrap();
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].seq, before.last_seq + 1);
    assert_eq!(
        batch.events[0].kind,
        ChangeKind::Upsert(json!({"_id": a, "n": 10}))
    );
}

#[test]
fn lineage_survives_reopen_compaction_and_later_writes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.jsonl");

    let (epoch, mid_seq) = {
        let db = Database::open(&path).unwrap();
        let id = db.insert(json!({"v": 1})).unwrap();
        db.update(&id, json!({"v": 2})).unwrap();
        (db.lineage().source_epoch_uuid, db.lineage().last_seq)
    };
    assert_eq!(mid_seq, 2);

    // Reopen after plain writes: the journal replays into the same feed.
    {
        let db = Database::open(&path).unwrap();
        let lineage = db.lineage();
        assert_eq!(lineage.source_epoch_uuid, epoch);
        assert_eq!(lineage.last_seq, mid_seq);
        assert_eq!(lineage.min_available_seq, 1);
        let batch = db.changes_since(0).unwrap();
        assert_eq!(batch.events.len() as u64, mid_seq);
        assert_eq!(batch.events.last().unwrap().seq, mid_seq);
        assert_eq!(db.len(), 1);
        db.compact().unwrap();
    }

    // Reopen after compaction: identity and seq survive, the old window does not.
    {
        let db = Database::open(&path).unwrap();
        let lineage = db.lineage();
        assert_eq!(lineage.source_epoch_uuid, epoch);
        assert_eq!(lineage.last_seq, mid_seq);
        assert_eq!(lineage.min_available_seq, mid_seq + 1);
        assert!(db.changes_since(mid_seq).unwrap().events.is_empty());
        assert!(matches!(
            db.changes_since(mid_seq - 1),
            Err(Error::CursorExpired { .. })
        ));
        db.insert(json!({"v": 3})).unwrap();
        db.insert(json!({"v": 4})).unwrap();
    }

    // Reopen after compact + more writes: only the post-compact tail is replayable.
    {
        let db = Database::open(&path).unwrap();
        let lineage = db.lineage();
        assert_eq!(lineage.source_epoch_uuid, epoch);
        assert_eq!(lineage.last_seq, mid_seq + 2);
        assert_eq!(lineage.min_available_seq, mid_seq + 1);
        let batch = db.changes_since(mid_seq).unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0].seq, mid_seq + 1);
        assert_eq!(batch.events[1].seq, mid_seq + 2);
        assert!(matches!(
            batch.events[0].kind,
            ChangeKind::Upsert(ref doc) if doc["v"] == json!(3)
        ));
        assert_eq!(db.len(), 3);
    }
}

#[test]
fn truncated_tail_is_skipped_and_seq_matches_last_commit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.jsonl");

    {
        let db = Database::open(&path).unwrap();
        let id = db.insert(json!({"v": 1})).unwrap();
        db.update(&id, json!({"v": 2})).unwrap();
        db.flush().unwrap();
        assert_eq!(db.lineage().last_seq, 2);
    }

    // Torn write: a line that would have carried seq 3 never completed. It is
    // the FINAL line, so it is a crash artifact and is truncated away; the
    // records before it are untouched.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    write!(file, "{}", r#"{"_id":"ghost","_seq":3,"v":9"#).unwrap();
    drop(file);

    let db = Database::open(&path).unwrap();
    assert_eq!(db.lineage().last_seq, 2, "an uncommitted line holds no seq");
    assert_eq!(db.len(), 1);
    assert!(db.get("ghost").is_err());
    assert_eq!(db.changes_since(0).unwrap().events.len(), 2);

    // The next commit takes the seq the torn line never got, and it survives
    // a second reopen — the truncated tail cannot swallow it.
    let next = db.insert(json!({"v": 3})).unwrap();
    assert_eq!(db.lineage().last_seq, 3);
    let batch = db.changes_since(2).unwrap();
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].seq, 3);
    assert_eq!(batch.events[0].id, next);
    drop(db);

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.len(), 2);
    assert!(reopened.get(&next).is_ok());
}

#[test]
fn cursor_ahead_of_journal_is_rejected() {
    let db = Database::open_in_memory().unwrap();
    assert!(matches!(
        db.changes_since(1),
        Err(Error::CursorAhead {
            requested_seq: 1,
            last_seq: 0
        })
    ));

    db.insert(json!({"v": 1})).unwrap();
    match db.changes_since(2) {
        Err(Error::CursorAhead {
            requested_seq,
            last_seq,
        }) => {
            assert_eq!(requested_seq, 2);
            assert_eq!(last_seq, 1);
        }
        other => panic!("expected CursorAhead, got {other:?}"),
    }
}

#[test]
fn in_memory_feed_works_like_disk() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(db.lineage().last_seq, 0);
    assert_eq!(db.lineage().min_available_seq, 1);

    let a = db.insert(json!({"v": 1})).unwrap();
    let b = db.insert_with_prefix("conv_", json!({"v": 2})).unwrap();
    assert!(b.starts_with("conv_"));
    assert_eq!(db.lineage().last_seq, 2);
    assert!(db.set(&a, "v", json!(3)).unwrap());
    assert!(db.array_push(&a, "list", json!(1)).unwrap());
    db.delete(&b).unwrap();
    assert_eq!(db.lineage().last_seq, 5);

    let snapshot = db.snapshot_at();
    assert_eq!(snapshot.lineage.last_seq, 5);
    assert_eq!(
        snapshot.lineage.source_epoch_uuid,
        db.lineage().source_epoch_uuid
    );
    assert_eq!(snapshot.docs.len(), 1);
    assert_eq!(snapshot.docs[0].0, a);
    assert_eq!(snapshot.docs[0].1, json!({"_id": a, "v": 3, "list": [1]}));

    let batch = db.changes_since(0).unwrap();
    assert_eq!(batch.events.len(), 5);
    assert_eq!(batch.events[4].kind, ChangeKind::Delete);
    assert!(db.changes_since(5).unwrap().events.is_empty());
}

#[test]
fn snapshot_plus_changes_reconstructs_concurrent_state() {
    let dir = TempDir::new().unwrap();
    let db = std::sync::Arc::new(disk_db(&dir));

    let mut ids = Vec::new();
    for i in 0..8 {
        ids.push(db.insert(json!({"n": i, "list": []})).unwrap());
    }

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let mut handles = Vec::new();
    for thread in 0..4 {
        let db = std::sync::Arc::clone(&db);
        let owned = vec![ids[thread * 2].clone(), ids[thread * 2 + 1].clone()];
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let mut present = [true, true];
            for round in 0..30u64 {
                let slot = (round % 2) as usize;
                if !present[slot] {
                    continue;
                }
                let id = &owned[slot];
                match round % 5 {
                    0 => {
                        db.set(id, "n", json!(round)).unwrap();
                    }
                    1 => {
                        db.array_push(id, "list", json!(round)).unwrap();
                    }
                    2 => {
                        db.update(id, json!({"n": round, "list": []})).unwrap();
                    }
                    3 => {
                        db.delete(id).unwrap();
                        present[slot] = false;
                    }
                    _ => {
                        db.insert(json!({"n": round, "thread": thread})).unwrap();
                    }
                }
            }
        }));
    }

    // The watermark is taken while the writers run: it must name a state that
    // actually existed, and the events after it must account for every write.
    let watermark = db.snapshot_at();

    for handle in handles {
        handle.join().unwrap();
    }

    let batch = db.changes_since(watermark.lineage.last_seq).unwrap();
    assert_eq!(batch.lineage.last_seq, db.lineage().last_seq);
    assert!(
        batch.events.windows(2).all(|w| w[0].seq < w[1].seq),
        "feed must stay in commit order"
    );
    assert!(batch
        .events
        .iter()
        .all(|e| e.seq > watermark.lineage.last_seq));
    assert_eq!(
        batch.events.len() as u64,
        batch.lineage.last_seq - watermark.lineage.last_seq,
        "every commit after the watermark is one event"
    );

    let mut rebuilt: BTreeMap<String, Value> = watermark.docs.into_iter().collect();
    for event in &batch.events {
        match &event.kind {
            ChangeKind::Upsert(doc) => {
                rebuilt.insert(event.id.clone(), doc.clone());
            }
            ChangeKind::Delete => {
                rebuilt.remove(&event.id);
            }
        }
    }
    assert_eq!(rebuilt, live_state(&db));
}

#[test]
fn legacy_journal_gets_a_deterministic_epoch() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("legacy.jsonl");

    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(
        file,
        r#"{{"_meta":{{"version":1,"created":"1700000000"}}}}"#
    )
    .unwrap();
    writeln!(file, r#"{{"_id":"a","v":1}}"#).unwrap();
    writeln!(file, r#"{{"_id":"b","v":2}}"#).unwrap();
    writeln!(file, r#"{{"_id":"a","_op":"set","path":"v","value":9}}"#).unwrap();
    drop(file);

    let epoch = {
        let db = Database::open(&path).unwrap();
        let lineage = db.lineage();
        assert_eq!(lineage.last_seq, 3, "each legacy line was a commit");
        assert_eq!(lineage.min_available_seq, 1);
        assert_eq!(lineage.source_epoch_uuid.len(), 36);

        let batch = db.changes_since(0).unwrap();
        assert_eq!(batch.events.len(), 3);
        assert_eq!(
            batch.events[2].kind,
            ChangeKind::Upsert(json!({"_id": "a", "v": 9}))
        );

        db.insert(json!({"v": 3})).unwrap();
        assert_eq!(db.lineage().last_seq, 4);
        lineage.source_epoch_uuid
    };

    let db = Database::open(&path).unwrap();
    assert_eq!(
        db.lineage().source_epoch_uuid,
        epoch,
        "a legacy file must keep one identity across reopens"
    );
    assert_eq!(db.lineage().last_seq, 4);
    assert_eq!(db.changes_since(3).unwrap().events.len(), 1);
}

#[test]
fn export_preserves_lineage_and_resets_window() {
    let dir = TempDir::new().unwrap();
    let db = disk_db(&dir);
    db.insert(json!({"v": 1})).unwrap();
    db.insert(json!({"v": 2})).unwrap();
    let before = db.lineage();

    let export_dir = dir.path().join("snapshot");
    db.export_snapshot(&export_dir).unwrap();

    let exported = Database::open(export_dir.join("data.jsonl")).unwrap();
    let after = exported.lineage();
    assert_eq!(after.source_epoch_uuid, before.source_epoch_uuid);
    assert_eq!(after.last_seq, before.last_seq);
    assert_eq!(after.min_available_seq, before.last_seq + 1);
    assert_eq!(exported.len(), 2);
    assert!(exported
        .changes_since(after.last_seq)
        .unwrap()
        .events
        .is_empty());
    assert!(matches!(
        exported.changes_since(after.last_seq - 1),
        Err(Error::CursorExpired { .. })
    ));
}
