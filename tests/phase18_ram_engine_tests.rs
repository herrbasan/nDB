//! Phase 1 (Slice 1C) — the RAM-first engine, end to end.
//!
//! Three kinds of proof, each aimed at a different failure mode:
//!
//! 1. **Differential oracle** — 10 000+ seeded mutations against the sequential
//!    reference model, comparing every observable: returns, error classes,
//!    documents, sequence numbers, query pages (indexed *and* unindexed, with
//!    offset/limit/sort), facets, snapshots and the change feed.
//! 2. **Fix regressions** — `offset` without `limit` (#15), durable-before-ack
//!    across a reopen (#16), and index DDL not consuming a document seq (#17).
//! 3. **Structure and failure** — op-level WAL growth on a 4 MB document,
//!    concurrent group commit without torn reads, poison-on-uncertain-write,
//!    and the RAM budget.

mod support;

use ndb::{Database, PatchOp, QueryOptions, RamConfig, SortDir};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use support::refdb::{RefDb, RefError, RefOp};

/// A RAM database whose ring is big enough that the oracle never sees eviction:
/// history retention has its own tests below.
fn open_oracle(dir: &Path) -> Database {
    Database::open_ram_with_config(
        dir,
        RamConfig {
            max_memory_bytes: None,
            change_ring_cap_events: 1_000_000,
            change_ring_cap_bytes: 1024 * 1024 * 1024,
            auto_compact_wal_bytes: None,
        },
    )
    .expect("open a RAM-first database")
}

fn ref_error(error: &ndb::Error) -> RefError {
    RefError::from_ndb(error)
}

fn compare_bool(actual: ndb::Result<bool>, expected: Result<bool, RefError>, label: &str) -> bool {
    match (actual, expected) {
        (Ok(actual), Ok(expected)) => {
            assert_eq!(actual, expected, "{label}: applied flag");
            actual
        }
        (Err(actual), Err(expected)) => {
            assert_eq!(ref_error(&actual), expected, "{label}: error class");
            false
        }
        (actual, expected) => {
            panic!("{label}: backend and model disagree on outcome: {actual:?} vs {expected:?}")
        }
    }
}

fn compare_unit(actual: ndb::Result<()>, expected: Result<(), RefError>, label: &str) {
    match (actual, expected) {
        (Ok(()), Ok(())) => {}
        (Err(actual), Err(expected)) => {
            assert_eq!(ref_error(&actual), expected, "{label}: error class")
        }
        (actual, expected) => {
            panic!("{label}: backend and model disagree on outcome: {actual:?} vs {expected:?}")
        }
    }
}

fn compare_doc(actual: ndb::Result<Value>, expected: Result<Value, RefError>, label: &str) {
    match (actual, expected) {
        (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{label}: document"),
        (Err(actual), Err(expected)) => {
            assert_eq!(ref_error(&actual), expected, "{label}: error class")
        }
        (actual, expected) => {
            panic!("{label}: backend and model disagree on outcome: {actual:?} vs {expected:?}")
        }
    }
}

fn assert_in_sync(db: &Database, reference: &RefDb, label: &str) {
    assert_eq!(
        db.lineage().last_seq,
        reference.last_seq(),
        "{label}: last_seq"
    );
    assert_eq!(
        db.lineage().min_available_seq,
        reference.lineage().min_available_seq,
        "{label}: min_available_seq"
    );
    assert_eq!(db.len(), reference.len(), "{label}: live document count");
}

// ─── Random document and path material ─────────────────────────────────────

const KINDS: [&str; 3] = ["a", "b", "c"];
const TITLES: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
const TAGS: [&str; 4] = ["x", "y", "z", "red"];

fn pick<'a, T>(rng: &mut fastrand::Rng, items: &'a [T]) -> &'a T {
    &items[rng.usize(0..items.len())]
}

fn random_doc(rng: &mut fastrand::Rng) -> Value {
    let tag_count = rng.usize(0..4);
    let tags: Vec<Value> = (0..tag_count)
        .map(|_| Value::String((*pick(rng, &TAGS)).to_string()))
        .collect();
    let items: Vec<Value> = (0..rng.usize(0..4)).map(Value::from).collect();
    json!({
        "kind": *pick(rng, &KINDS),
        "score": rng.i64(0..20),
        "rank": rng.i64(-5..5),
        "title": *pick(rng, &TITLES),
        "tags": tags,
        "items": items,
        "nested": {
            "k": rng.i64(0..10),
            "deep": {"flag": rng.bool()},
        },
    })
}

fn random_scalar(rng: &mut fastrand::Rng) -> Value {
    match rng.usize(0..6) {
        0 => json!(rng.i64(-10..30)),
        1 => json!(*pick(rng, &TITLES)),
        2 => json!(rng.bool()),
        3 => json!(null),
        4 => json!([rng.i64(0..5), rng.i64(0..5)]),
        _ => json!({"a": rng.i64(0..5)}),
    }
}

const VALID_PATHS: [&str; 11] = [
    "title",
    "kind",
    "score",
    "nested.k",
    "nested.deep.flag",
    "tags",
    "items",
    "items.0",
    "items.1",
    "nested",
    "fresh",
];

const INVALID_PATHS: [&str; 6] = [
    "absent",
    "absent.deep",
    "items.99",
    "tags.9",
    "title.sub",
    "nested.absent.k",
];

fn random_path(rng: &mut fastrand::Rng) -> String {
    if rng.usize(0..4) == 0 {
        (*pick(rng, &INVALID_PATHS)).to_string()
    } else {
        (*pick(rng, &VALID_PATHS)).to_string()
    }
}

fn random_ops(rng: &mut fastrand::Rng) -> Vec<PatchOp> {
    let count = rng.usize(1..4);
    (0..count)
        .map(|_| match rng.usize(0..3) {
            0 => PatchOp::Set {
                path: random_path(rng),
                value: random_scalar(rng),
            },
            1 => PatchOp::Remove {
                path: random_path(rng),
            },
            _ => PatchOp::ArrayPush {
                path: random_path(rng),
                value: random_scalar(rng),
            },
        })
        .collect()
}

fn as_ref_ops(ops: &[PatchOp]) -> Vec<RefOp> {
    ops.iter()
        .map(|op| match op {
            PatchOp::Set { path, value } => RefOp::Set {
                path: path.clone(),
                value: value.clone(),
            },
            PatchOp::Remove { path } => RefOp::Remove { path: path.clone() },
            PatchOp::ArrayPush { path, value } => RefOp::ArrayPush {
                path: path.clone(),
                value: value.clone(),
            },
        })
        .collect()
}

fn live_ids(db: &Database) -> Vec<String> {
    let mut ids: Vec<String> = db
        .iter()
        .into_iter()
        .filter_map(|doc| doc.get("_id").and_then(Value::as_str).map(str::to_string))
        .collect();
    ids.sort();
    ids
}

// ─── Query batteries ───────────────────────────────────────────────────────

fn query_battery() -> Vec<Value> {
    vec![
        json!({}),
        json!({"kind": "a"}),
        json!({"kind": {"$ne": "b"}}),
        json!({"score": {"$gte": 5, "$lt": 15}}),
        json!({"score": {"$gt": 3}}),
        json!({"score": {"$lte": 10}}),
        json!({"rank": {"$lt": 0}}),
        json!({"tags": {"$in": ["x"]}}),
        json!({"tags": {"$nin": ["y"]}}),
        json!({"items": {"$in": [2]}}),
        json!({"absent": {"$exists": false}}),
        json!({"nested.k": {"$gte": 3}}),
        json!({"nested.deep.flag": true}),
        json!({"$and": [{"kind": {"$in": ["a", "c"]}}, {"score": {"$gt": 5}}]}),
        json!({"$or": [{"kind": "b"}, {"rank": {"$gte": 3}}]}),
        json!({"$not": {"kind": "a"}}),
        json!({"title": {"$gt": "beta"}}),
        json!({"items.0": 1}),
        json!([{"kind": "a"}, {"score": {"$lt": 8}}]),
    ]
}

fn option_battery() -> Vec<QueryOptions> {
    // `offset` without `limit` is included on purpose: the RAM engine honours
    // it (herrbasan/nDB#15), and the reference model always did.
    vec![
        QueryOptions::default(),
        QueryOptions {
            limit: Some(3),
            ..QueryOptions::default()
        },
        QueryOptions {
            offset: Some(2),
            limit: Some(4),
            ..QueryOptions::default()
        },
        QueryOptions {
            offset: Some(2),
            ..QueryOptions::default()
        },
        QueryOptions {
            offset: Some(2),
            sort_by: Some(("score".to_string(), SortDir::Asc)),
            ..QueryOptions::default()
        },
        QueryOptions {
            sort_by: Some(("score".to_string(), SortDir::Asc)),
            ..QueryOptions::default()
        },
        QueryOptions {
            sort_by: Some(("score".to_string(), SortDir::Desc)),
            limit: Some(4),
            ..QueryOptions::default()
        },
        QueryOptions {
            sort_by: Some(("title".to_string(), SortDir::Asc)),
            offset: Some(1),
            limit: Some(3),
            ..QueryOptions::default()
        },
        QueryOptions {
            sort_by: Some(("absent".to_string(), SortDir::Asc)),
            limit: Some(5),
            ..QueryOptions::default()
        },
        QueryOptions {
            sort_by: Some(("nested.k".to_string(), SortDir::Desc)),
            ..QueryOptions::default()
        },
        QueryOptions {
            sort_by: Some(("tags".to_string(), SortDir::Asc)),
            ..QueryOptions::default()
        },
    ]
}

fn doc_ids(docs: &[Value]) -> Vec<String> {
    docs.iter()
        .map(|doc| {
            doc.get("_id")
                .and_then(Value::as_str)
                .unwrap_or("<no _id>")
                .to_string()
        })
        .collect()
}

fn compare_queries(db: &Database, reference: &RefDb, label: &str) -> usize {
    let mut compared = 0;
    for ast in query_battery() {
        for opts in option_battery() {
            let actual = db
                .query_with(ast.clone(), opts.clone())
                .unwrap_or_else(|e| panic!("{label}: query {ast} {opts:?} failed: {e}"));
            let expected = reference
                .query_with(&ast, &opts)
                .unwrap_or_else(|e| panic!("{label}: reference query {ast} {opts:?} failed: {e:?}"));
            if actual != expected {
                panic!(
                    "{label}: query result differs for {ast} with {opts:?}\n  backend ids : {:?}\n  reference ids: {:?}",
                    doc_ids(&actual),
                    doc_ids(&expected)
                );
            }
            compared += 1;
        }
    }
    compared
}

fn compare_facets(db: &Database, reference: &RefDb, label: &str) {
    let fields: Vec<String> = ["kind", "title", "tags", "absent", "score"]
        .iter()
        .map(|f| f.to_string())
        .collect();
    assert_eq!(
        db.facet_counts(&fields),
        reference.facets(&fields),
        "{label}: facet counts"
    );
}

fn compare_changes(db: &Database, reference: &RefDb, since: u64, label: &str) {
    match (db.changes_since(since), reference.changes_since(since)) {
        (Ok(actual), Ok(expected)) => {
            assert_eq!(
                actual.events, expected.events,
                "{label}: change events since {since}"
            );
            assert_eq!(
                actual.lineage.last_seq, expected.lineage.last_seq,
                "{label}: change batch last_seq since {since}"
            );
        }
        (Err(actual), Err(expected)) => assert_eq!(
            ref_error(&actual),
            expected,
            "{label}: change-feed error class since {since}"
        ),
        (actual, expected) => panic!(
            "{label}: change feed disagrees since {since}: {actual:?} vs {expected:?}"
        ),
    }
}

fn compare_snapshot(db: &Database, reference: &RefDb, label: &str) {
    let actual = db.snapshot_at();
    let expected = reference.snapshot_at();
    assert_eq!(actual.docs, expected.docs, "{label}: snapshot documents");
    assert_eq!(
        actual.lineage.last_seq, expected.lineage.last_seq,
        "{label}: snapshot watermark"
    );
}

// ─── The differential run ──────────────────────────────────────────────────

fn run_seed(seed: u64, steps: usize) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_oracle(dir.path());
    // Both index kinds exist from the start, so every mutation below maintains
    // them and every query runs through the planner.
    db.create_index("kind").expect("hash index on kind");
    db.create_btree_index("score").expect("btree index on score");
    let mut reference = RefDb::new();
    let mut rng = fastrand::Rng::with_seed(seed);

    for step in 0..steps {
        let label = format!("seed {seed} step {step}");
        let choice = rng.usize(0..100);

        match choice {
            0..=17 => {
                let doc = random_doc(&mut rng);
                let id = db
                    .insert(doc.clone())
                    .unwrap_or_else(|e| panic!("{label}: insert failed: {e}"));
                reference
                    .insert_with_id(&id, doc)
                    .unwrap_or_else(|e| panic!("{label}: reference insert failed: {e:?}"));
                assert_eq!(db.doc_seq(&id), reference.doc_seq(&id), "{label}: doc_seq");
                compare_doc(db.get(&id), reference.get(&id), &label);
            }
            18..=26 => {
                let ids = live_ids(&db);
                let id = if ids.is_empty() {
                    "absent-0000".to_string()
                } else {
                    ids[rng.usize(0..ids.len())].clone()
                };
                let doc = random_doc(&mut rng);
                let if_seq = if rng.bool() {
                    Some(match reference.doc_seq(&id) {
                        Some(seq) if rng.bool() => seq,
                        Some(seq) => seq + 1,
                        None => 0,
                    })
                } else {
                    None
                };
                compare_unit(
                    db.update_if(&id, doc.clone(), if_seq),
                    reference.update_if(&id, doc, if_seq),
                    &label,
                );
            }
            27..=38 => {
                let ids = live_ids(&db);
                let id = if ids.is_empty() {
                    "absent-0000".to_string()
                } else {
                    ids[rng.usize(0..ids.len())].clone()
                };
                let path = random_path(&mut rng);
                let value = random_scalar(&mut rng);
                compare_bool(
                    db.set(&id, &path, value.clone()),
                    reference.set(&id, &path, value),
                    &label,
                );
            }
            39..=48 => {
                let ids = live_ids(&db);
                let id = if ids.is_empty() {
                    "absent-0000".to_string()
                } else {
                    ids[rng.usize(0..ids.len())].clone()
                };
                let path = random_path(&mut rng);
                compare_bool(db.remove(&id, &path), reference.remove(&id, &path), &label);
            }
            49..=56 => {
                let ids = live_ids(&db);
                let id = if ids.is_empty() {
                    "absent-0000".to_string()
                } else {
                    ids[rng.usize(0..ids.len())].clone()
                };
                let path = random_path(&mut rng);
                let value = random_scalar(&mut rng);
                compare_bool(
                    db.array_push(&id, &path, value.clone()),
                    reference.array_push(&id, &path, value),
                    &label,
                );
            }
            57..=67 => {
                let ids = live_ids(&db);
                let id = if ids.is_empty() {
                    "absent-0000".to_string()
                } else {
                    ids[rng.usize(0..ids.len())].clone()
                };
                let ops = random_ops(&mut rng);
                let if_seq = if rng.bool() {
                    Some(match reference.doc_seq(&id) {
                        Some(seq) if rng.usize(0..4) != 0 => seq,
                        Some(seq) => seq + 3,
                        None => 1,
                    })
                } else {
                    None
                };
                let ref_ops = as_ref_ops(&ops);
                match (
                    db.apply_ops(&id, &ops, if_seq),
                    reference.apply_ops(&id, &ref_ops, if_seq),
                ) {
                    (Ok(actual), Ok((expected_seq, expected_doc))) => {
                        assert_eq!(actual.seq, expected_seq, "{label}: patch seq");
                        assert_eq!(actual.doc, expected_doc, "{label}: patch doc");
                        assert_eq!(actual.applied, vec![true; ops.len()], "{label}: applied");
                    }
                    (Err(actual), Err(expected)) => {
                        assert_eq!(ref_error(&actual), expected, "{label}: patch error class")
                    }
                    (actual, expected) => panic!(
                        "{label}: patch outcome disagrees for {ops:?}: {actual:?} vs {expected:?}"
                    ),
                }
            }
            68..=77 => {
                let ids = live_ids(&db);
                let id = if ids.is_empty() {
                    "absent-0000".to_string()
                } else {
                    ids[rng.usize(0..ids.len())].clone()
                };
                compare_unit(db.delete(&id), reference.delete(&id).map(|_| ()), &label);
                compare_doc(db.get(&id), reference.get(&id), &label);
            }
            78..=85 => {
                let mut candidates = reference.deleted_ids();
                candidates.sort();
                let id = if candidates.is_empty() || rng.usize(0..4) == 0 {
                    let ids = live_ids(&db);
                    if ids.is_empty() {
                        "absent-0000".to_string()
                    } else {
                        ids[rng.usize(0..ids.len())].clone()
                    }
                } else {
                    candidates[rng.usize(0..candidates.len())].clone()
                };
                compare_unit(db.restore(&id), reference.restore(&id).map(|_| ()), &label);
                compare_doc(db.get(&id), reference.get(&id), &label);
            }
            86..=91 => {
                let ids = live_ids(&db);
                let id = if ids.is_empty() {
                    "absent-0000".to_string()
                } else {
                    ids[rng.usize(0..ids.len())].clone()
                };
                compare_doc(db.get(&id), reference.get(&id), &label);
                assert_eq!(db.doc_seq(&id), reference.doc_seq(&id), "{label}: doc_seq");
                assert_eq!(db.contains(&id), reference.contains(&id), "{label}: contains");
                compare_snapshot(&db, &reference, &label);
            }
            _ => {
                let last = reference.last_seq();
                for since in [0, last.saturating_sub(1), last, last + 1] {
                    compare_changes(&db, &reference, since, &label);
                }
            }
        }

        assert_in_sync(&db, &reference, &label);
    }

    compare_queries(&db, &reference, &format!("seed {seed} final"));
    compare_facets(&db, &reference, &format!("seed {seed} final"));
    compare_snapshot(&db, &reference, &format!("seed {seed} final"));
    compare_changes(&db, &reference, 0, &format!("seed {seed} final"));
    assert!(
        db.ram_stats().expect("ram stats").index_bytes > 0,
        "seed {seed}: the indexes must actually hold entries"
    );

    // The op-level WAL must replay into exactly the state the sequential model
    // reached: documents, per-document seqs and the watermark.
    let expected_docs = reference.iter_docs();
    let expected_watermark = reference.last_seq();
    let expected_seqs: Vec<(String, u64)> = expected_docs
        .iter()
        .map(|(id, _)| (id.clone(), reference.doc_seq(id).expect("doc seq")))
        .collect();
    drop(db);

    let reopened = open_oracle(dir.path());
    assert_eq!(
        reopened.lineage().last_seq,
        expected_watermark,
        "seed {seed}: the replayed watermark"
    );
    let mut actual_docs = reopened.all_docs().expect("all_docs");
    actual_docs.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        actual_docs, expected_docs,
        "seed {seed}: replay must reproduce every document"
    );
    for (id, seq) in expected_seqs {
        assert_eq!(
            reopened.doc_seq(&id),
            Some(seq),
            "seed {seed}: replayed doc_seq for {id}"
        );
    }
}

#[test]
fn randomized_mutations_match_the_reference_model() {
    // 5 seeds x 2 000 steps = 10 000 operation batches, plus a full query,
    // facet, snapshot and change-feed comparison after every seed.
    let mut compared = 0;
    for seed in 1..=5 {
        run_seed(seed, 2_000);
        compared += 2_000;
    }
    assert_eq!(compared, 10_000);
}

// ─── Fix regressions ───────────────────────────────────────────────────────

#[test]
fn offset_without_limit_is_honoured() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_oracle(dir.path());
    db.create_btree_index("score").expect("btree index");
    for (index, score) in [10, 20, 30, 40, 50].iter().enumerate() {
        db.insert(json!({"n": index, "score": score}))
            .expect("insert");
    }

    let page = db
        .query_with(
            json!({}),
            QueryOptions {
                sort_by: Some(("score".to_string(), SortDir::Asc)),
                offset: Some(2),
                limit: None,
                ..QueryOptions::default()
            },
        )
        .expect("query");
    let scores: Vec<i64> = page
        .iter()
        .map(|doc| doc["score"].as_i64().expect("score"))
        .collect();
    assert_eq!(
        scores,
        vec![30, 40, 50],
        "offset without limit must skip two rows and return the rest"
    );
}

#[test]
fn index_ddl_consumes_no_document_sequence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_oracle(dir.path());
    let id = db.insert(json!({"kind": "a"})).expect("insert");
    db.insert(json!({"kind": "b"})).expect("insert");

    let before = db.lineage();
    let events_before = db.changes_since(0).expect("changes").events.len();
    db.create_index("kind").expect("create hash index");
    db.create_btree_index("score").expect("create btree index");
    let after_create = db.lineage();
    assert_eq!(
        after_create.last_seq, before.last_seq,
        "index DDL must not consume a document seq"
    );
    assert_eq!(
        db.changes_since(0).expect("changes").events.len(),
        events_before,
        "index DDL must not emit a change event"
    );

    db.drop_index("kind").expect("drop index");
    db.drop_index("score").expect("drop btree index");
    assert_eq!(
        db.lineage().last_seq,
        before.last_seq,
        "index drop must not consume a document seq"
    );

    // The feed is still usable at the watermark a consumer already holds.
    let batch = db
        .changes_since(before.last_seq)
        .expect("changes at last_seq");
    assert!(batch.events.is_empty());
    assert_eq!(db.doc_seq(&id), Some(1));
}

#[test]
fn acknowledged_writes_survive_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let ids: Vec<String> = {
        let db = Database::open_ram(dir.path()).expect("open");
        db.create_btree_index("n").expect("btree index");
        let mut ids = Vec::new();
        for n in 0..50 {
            ids.push(
                db.insert(json!({"n": n, "text": format!("row {n}")}))
                    .expect("insert"),
            );
        }
        db.array_push(&ids[0], "grows", json!(1)).expect("push");
        db.set(&ids[1], "flag", json!(true)).expect("set");
        db.delete(&ids[2]).expect("delete");
        // Dropping the handle closes the process's view; the acknowledged
        // writes must already be durable.
        ids
    };

    let db = Database::open_ram(dir.path()).expect("reopen");
    assert_eq!(db.len(), 49, "the deleted document stays deleted");
    assert_eq!(db.get(&ids[2]).is_err(), true);
    assert_eq!(db.get(&ids[0]).expect("doc")["grows"], json!([1]));
    assert_eq!(db.get(&ids[1]).expect("doc")["flag"], json!(true));
    assert!(db.has_btree_index("n"), "index declarations persist");
    let ranged = db.query_with(
        json!({"n": {"$gte": 10, "$lt": 13}}),
        QueryOptions {
            sort_by: Some(("n".to_string(), SortDir::Asc)),
            ..QueryOptions::default()
        },
    )
    .expect("query");
    assert_eq!(ranged.len(), 3, "the rebuilt index answers a range scan");
}

#[test]
fn auto_detects_a_v3_folder() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let db = Database::open_ram(dir.path()).expect("open");
        db.insert(json!({"x": 1})).expect("insert");
    }
    let db = Database::open(dir.path()).expect("auto-detect open");
    assert!(db.is_ram(), "a manifest with version 3 opens on the RAM engine");
    assert_eq!(db.len(), 1);
}

#[test]
fn compaction_rewrites_the_base_and_keeps_the_sequence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Database::open_ram(dir.path()).expect("open");
    let mut ids = Vec::new();
    for n in 0..20 {
        ids.push(db.insert(json!({"n": n})).expect("insert"));
    }
    let last_before = db.lineage().last_seq;
    db.compact().expect("compact");
    assert_eq!(
        db.lineage().last_seq, last_before,
        "compaction never resets the sequence"
    );
    assert_eq!(
        db.lineage().min_available_seq,
        last_before + 1,
        "the rewritten documents are base state, not events"
    );
    assert_eq!(db.len(), 20);
    // A cursor behind the new floor is expired; at it, the feed is empty.
    assert!(matches!(
        db.changes_since(0),
        Err(ndb::Error::CursorExpired { .. })
    ));
    assert!(db
        .changes_since(last_before)
        .expect("at watermark")
        .events
        .is_empty());
    drop(db);

    let reopened = Database::open_ram(dir.path()).expect("reopen");
    assert_eq!(reopened.len(), 20, "the new base is authoritative");
    assert_eq!(reopened.lineage().last_seq, last_before);
    assert_eq!(reopened.get(&ids[19]).expect("doc")["n"], json!(19));
}

// ─── Structural sharing and op-level WAL ───────────────────────────────────

const MESSAGE_COUNT: usize = 767;
const MESSAGE_BODY_CHARS: usize = 5_000;

fn chat_doc() -> Value {
    let messages: Vec<Value> = (0..MESSAGE_COUNT)
        .map(|index| {
            json!({
                "role": if index % 2 == 0 { "user" } else { "assistant" },
                "content": "y".repeat(MESSAGE_BODY_CHARS),
                "n": index,
            })
        })
        .collect();
    json!({
        "title": "big conversation",
        "messages": messages,
    })
}

fn pushed_message(n: usize) -> Value {
    json!({
        "role": "assistant",
        "content": "z".repeat(MESSAGE_BODY_CHARS),
        "n": MESSAGE_COUNT + n,
    })
}

#[test]
fn structural_sharing_keeps_the_wal_small_on_a_4mb_chat_document() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut reference = RefDb::new();

    let id = {
        let db = Database::open_ram(dir.path()).expect("open");
        let doc = chat_doc();
        let id = db.insert(doc.clone()).expect("insert the chat document");
        reference
            .insert_with_id(&id, doc)
            .expect("reference insert");

        let before = db.ram_stats().expect("ram stats");
        assert!(
            before.live_doc_bytes > 4 * 1024 * 1024,
            "the fixture must really be about 4 MB, got {}",
            before.live_doc_bytes
        );
        let live_after_insert = before.live_doc_bytes;

        for n in 0..20 {
            let value = pushed_message(n);
            assert!(
                db.array_push(&id, "messages", value.clone())
                    .expect("array_push"),
                "push {n} must land"
            );
            assert!(reference
                .array_push(&id, "messages", value)
                .expect("reference push"));
        }

        let after = db.ram_stats().expect("ram stats");
        let growth = after.wal_bytes - before.wal_bytes;
        println!(
            "4 MB chat doc: WAL {} -> {} bytes (growth {}, {} bytes per push); ring {} bytes, live {} bytes",
            before.wal_bytes,
            after.wal_bytes,
            growth,
            growth / 20,
            after.ring_bytes,
            after.live_doc_bytes
        );
        assert!(
            growth < 200 * 1024,
            "20 pushes on a 4 MB document must append kilobytes, not megabytes: grew by {growth} bytes"
        );
        assert!(
            after.live_doc_bytes <= live_after_insert + 256 * 1024,
            "20 pushes must not multiply document memory: {} -> {} bytes",
            live_after_insert,
            after.live_doc_bytes
        );
        assert!(
            after.ring_bytes < 1_000_000,
            "structurally shared pushes must keep the ring under 1 MB, got {} bytes",
            after.ring_bytes
        );
        let retained = db
            .changes_since(1)
            .expect("changes since the insert")
            .events;
        assert_eq!(
            retained.len(),
            20,
            "every push must stay retained in the change ring"
        );
        assert!(
            retained.iter().all(|event| event.seq > 1),
            "only the pushes are expected past the insert"
        );
        assert!(
            db.get(&id).expect("doc") == reference.get(&id).expect("reference doc"),
            "the pushed document must equal the reference model"
        );
        assert_eq!(db.doc_seq(&id).unwrap(), reference.doc_seq(&id).unwrap());
        id
    };

    let db = Database::open_ram(dir.path()).expect("reopen");
    let replayed = db.get(&id).expect("replayed doc");
    assert_eq!(
        replayed,
        reference.get(&id).expect("reference doc"),
        "replaying the op-level WAL must reproduce every push"
    );
    let messages = replayed["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), MESSAGE_COUNT + 20);
    assert_eq!(messages[MESSAGE_COUNT + 19]["n"], json!(MESSAGE_COUNT + 19));
    assert_eq!(db.lineage().last_seq, reference.last_seq());
}

// ─── Group commit concurrency ──────────────────────────────────────────────

#[test]
fn concurrent_writers_and_readers_see_one_visibility_boundary() {
    const WRITERS: usize = 8;
    const PER_WRITER: usize = 100;
    const READERS: usize = 4;

    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(Database::open_ram(dir.path()).expect("open"));
    db.create_index("bucket").expect("hash index on bucket");

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for reader in 0..READERS {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        readers.push(std::thread::spawn(move || {
            let mut observations = 0usize;
            while !stop.load(Ordering::SeqCst) {
                for bucket in 0..WRITERS {
                    let page = db
                        .query_with(
                            json!({"bucket": bucket}),
                            QueryOptions::default(),
                        )
                        .expect("query while writing");
                    let mut seen = std::collections::HashSet::new();
                    for doc in &page {
                        let id = doc["_id"].as_str().expect("_id").to_string();
                        assert!(
                            seen.insert(id.clone()),
                            "reader {reader}: duplicate document {id} in one page"
                        );
                        assert_eq!(
                            doc["bucket"], json!(bucket),
                            "reader {reader}: an indexed query returned a non-matching document"
                        );
                        let stored = db.get(&id).expect("a returned document must be readable");
                        assert_eq!(
                            stored["bucket"],
                            json!(bucket),
                            "reader {reader}: the document changed its indexed value"
                        );
                    }
                    observations += page.len();
                }
                std::thread::yield_now();
            }
            observations
        }));
    }

    let mut writers = Vec::new();
    for writer in 0..WRITERS {
        let db = Arc::clone(&db);
        writers.push(std::thread::spawn(move || {
            for n in 0..PER_WRITER {
                db.insert(json!({"bucket": writer, "n": n}))
                    .expect("concurrent insert");
            }
        }));
    }
    for writer in writers {
        writer.join().expect("writer thread");
    }
    stop.store(true, Ordering::SeqCst);
    let mut observed = 0usize;
    for reader in readers {
        observed += reader.join().expect("reader thread");
    }
    assert!(observed > 0, "the readers must have observed some documents");

    assert_eq!(db.len(), WRITERS * PER_WRITER);
    for bucket in 0..WRITERS {
        let page = db
            .query_with(json!({"bucket": bucket}), QueryOptions::default())
            .expect("query");
        assert_eq!(page.len(), PER_WRITER, "bucket {bucket} is complete");
    }
    let last_seq = db.lineage().last_seq;
    drop(db);

    let reopened = Database::open_ram(dir.path()).expect("reopen");
    assert_eq!(
        reopened.len(),
        WRITERS * PER_WRITER,
        "every acknowledged write survives reopen"
    );
    assert_eq!(reopened.lineage().last_seq, last_seq);
}


// ─── Memory governance ──────────────────────────────���──────────────────────

#[test]
fn memory_budget_evicts_ring_history_then_refuses_the_write() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Database::open_ram_with_config(
        dir.path(),
        RamConfig {
            max_memory_bytes: Some(320 * 1024),
            change_ring_cap_events: 4,
            change_ring_cap_bytes: 64 * 1024 * 1024,
            auto_compact_wal_bytes: None,
        },
    )
    .expect("open with a small budget");

    let doc_id = db
        .insert(json!({"pad": "a".repeat(1_000)}))
        .expect("first insert");
    for n in 0..20 {
        db.set(&doc_id, "n", json!(n)).expect("set");
    }

    let stats = db.ram_stats().expect("ram stats");
    assert!(
        stats.ring_bytes <= 4 * 16 * 1024,
        "the event cap keeps the ring at four bounded patch entries, got {} bytes",
        stats.ring_bytes
    );
    assert!(
        matches!(db.changes_since(0), Err(ndb::Error::CursorExpired { .. })),
        "evicted history must refuse an old cursor"
    );
    assert!(
        db.ram_stats().expect("ram stats").expired_cursors >= 1,
        "the expired cursor is counted"
    );
    assert_eq!(
        db.get(&doc_id).expect("doc")["n"],
        json!(19),
        "evicting history never touches live documents"
    );

    // Fill the live set up to the cap.
    let mut refusal = None;
    for n in 0..2_000 {
        match db.insert(json!({"pad": "b".repeat(1_000), "n": n})) {
            Ok(_) => {}
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }
    let error = refusal.expect("the budget must eventually refuse a write");
    match &error {
        ndb::Error::MemoryLimitExceeded {
            current_bytes,
            limit_bytes,
            needed_bytes,
        } => {
            assert_eq!(*limit_bytes, 320 * 1024);
            assert!(*needed_bytes > 0);
            assert!(*current_bytes > 0);
        }
        other => panic!("expected MemoryLimitExceeded, got {other:?}"),
    }

    // A refused write stages nothing: RAM and the WAL are untouched, and the
    // database keeps serving reads.
    let live_before = db.len();
    let wal_before = db.ram_stats().expect("ram stats").wal_bytes;
    let again = db
        .insert(json!({"pad": "c".repeat(200_000)}))
        .expect_err("an oversized write is refused");
    assert!(matches!(again, ndb::Error::MemoryLimitExceeded { .. }));
    assert_eq!(db.len(), live_before, "a refused write publishes nothing");
    assert_eq!(
        db.ram_stats().expect("ram stats").wal_bytes,
        wal_before,
        "a refused write appends nothing"
    );
    assert!(!db.ram_stats().expect("ram stats").poisoned);
    drop(db);

    let reopened = Database::open_ram(dir.path()).expect("reopen");
    assert_eq!(
        reopened.len(),
        live_before,
        "the durable state matches what RAM held"
    );
}

// ─── Poison on an uncertain durable write ──────────────────────────────────

#[cfg(feature = "fault-io")]
fn poisoned_after_fault(op: ndb::io::IoOp, kind: std::io::ErrorKind) {
    use ndb::io::{FaultAction, FaultIo, FaultPlan};

    let dir = tempfile::tempdir().expect("temp dir");
    let io = Arc::new(FaultIo::new());
    let db = {
        let io_dyn: Arc<dyn ndb::io::Io> = io.clone();
        Database::open_ram_with_io(dir.path(), RamConfig::default(), io_dyn)
            .expect("open over FaultIo")
    };
    let kept = db.insert(json!({"n": 1})).expect("durable insert");

    let call = io.calls(op) + 1;
    io.arm(FaultPlan {
        op,
        trigger_at: call,
        action: FaultAction::Error(kind),
    });

    let error = db
        .insert(json!({"n": 2}))
        .expect_err("the injected failure must surface");
    assert!(
        matches!(error, ndb::Error::Io { .. }),
        "expected the underlying I/O error, got {error:?}"
    );

    let stats = db.ram_stats().expect("ram stats");
    assert!(stats.poisoned, "an uncertain durable tail poisons the database");
    assert_eq!(db.len(), 1, "the failed batch published nothing");
    assert_eq!(
        db.get(&kept).expect("doc")["n"],
        json!(1),
        "reads still work from RAM"
    );

    let refused = db
        .insert(json!({"n": 3}))
        .expect_err("a poisoned database refuses writes");
    match refused {
        ndb::Error::Poisoned { reason } => assert!(!reason.is_empty()),
        other => panic!("expected Poisoned, got {other:?}"),
    }
    // Reads are unaffected by the poison.
    assert_eq!(db.iter().len(), 1);
    drop(db);

    // Reopen recovers from the durable state, unpoisoned. A failed `sync_all`
    // leaves the batch's bytes possibly present: the contract says an
    // unacknowledged write may appear after recovery. What must never happen is
    // a half-applied state or a failure to open.
    let reopened = Database::open_ram(dir.path()).expect("reopen recovers");
    assert!(!reopened.ram_stats().expect("stats").poisoned);
    assert!(
        reopened.len() == 1 || reopened.len() == 2,
        "recovery must land on a whole-batch boundary, got {} documents",
        reopened.len()
    );
    assert_eq!(reopened.get(&kept).expect("doc")["n"], json!(1));
    reopened.insert(json!({"n": 4})).expect("writes work again");
}

#[cfg(feature = "fault-io")]
#[test]
fn injected_write_failure_poisons_and_recovers() {
    poisoned_after_fault(ndb::io::IoOp::WriteAll, std::io::ErrorKind::Other);
}

#[cfg(feature = "fault-io")]
#[test]
fn injected_sync_failure_poisons_and_recovers() {
    poisoned_after_fault(ndb::io::IoOp::SyncAll, std::io::ErrorKind::Other);
}
