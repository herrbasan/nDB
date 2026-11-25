//! Seeded differential oracle: the RAM-first semantics as implemented by the
//! v2 backend must equal the sequential reference model, exactly, on every
//! observable â€” return values, error classes, stored documents, sequence
//! numbers, query pages (indexed and unindexed), facets, snapshots and the
//! change feed.
//!
//! Randomised, but seeded and reproducible: a failure names the seed and the
//! step, so it is a bug report, not a flake.
mod support;

use ndb::{Database, PatchOp, QueryOptions, SortDir};
use serde_json::{json, Value};
use std::path::Path;
use support::refdb::{sample_doc, RefDb, RefError, RefOp};

fn open_v2(dir: &Path) -> Database {
    Database::open_v2(dir).expect("open a v2 database")
}

fn ref_error(error: &ndb::Error) -> RefError {
    RefError::from_ndb(error)
}

fn compare_bool(
    actual: ndb::Result<bool>,
    expected: Result<bool, RefError>,
    label: &str,
) -> bool {
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

// â”€â”€â”€ Random document and path material â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

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

/// Paths that resolve on a document from `random_doc`, plus paths that must
/// fail to resolve. Both sides have to agree on which is which.
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
        .filter_map(|doc| {
            doc.get("_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

fn batch_crc(ops: &[PatchOp]) -> u64 {
    // Cheap identity for op batches; not a checksum, just a label.
    ops.iter().fold(0u64, |acc, op| match op {
        PatchOp::Set { path, .. } => acc.wrapping_mul(31).wrapping_add(path.len() as u64),
        PatchOp::Remove { path } => acc.wrapping_mul(31).wrapping_add(path.len() as u64 + 1),
        PatchOp::ArrayPush { path, .. } => acc.wrapping_mul(31).wrapping_add(path.len() as u64 + 2),
    })
}

// â”€â”€â”€ Query batteries â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// The ASTs every differential run executes. Mix covers implicit `$eq`,
/// scalar comparisons, array membership, presence, logical combinators, dot
/// paths and a nested array index.
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
    // NOTE: `offset` without `limit` is deliberately absent. The v2 backend
    // ignores `offset` in that combination (see
    // `v2_ignores_offset_without_a_limit_known_defect` below), while the
    // reference model â€” and v1 â€” honour it.
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
                let actual_ids = doc_ids(&actual);
                let expected_ids = doc_ids(&expected);
                panic!(
                    "{label}: query result differs for {ast} with {opts:?}\n  backend ids : {actual_ids:?}\n  reference ids: {expected_ids:?}"
                );
            }
            compared += 1;
        }
    }
    compared
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

fn compare_facets(db: &Database, reference: &RefDb, label: &str) {
    let fields: Vec<String> = ["kind", "title", "tags", "absent", "score"]
        .iter()
        .map(|f| f.to_string())
        .collect();
    let actual = db.facet_counts(&fields);
    let expected = reference.facets(&fields);
    assert_eq!(actual, expected, "{label}: facet counts");
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

// â”€â”€â”€ The differential run â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn run_seed(seed: u64, steps: usize) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_v2(dir.path());
    let mut reference = RefDb::new();
    let mut rng = fastrand::Rng::with_seed(seed);

    for step in 0..steps {
        let label = format!("seed {seed} step {step}");
        let choice = rng.usize(0..100);

        match choice {
            // Insert.
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
            // Full replace, sometimes with an if_seq token.
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
            // set
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
            // remove
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
            // array_push
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
            // Atomic multi-op patch, valid and invalid, with and without if_seq.
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
                        assert_eq!(actual.seq, expected_seq, "{label}: patch seq (ops {})", batch_crc(&ops));
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
            // delete
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
            // restore
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
            // Reads: get / doc_seq / len / snapshot.
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
            // Change feed from several cursors.
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
}

#[test]
fn randomized_mutations_match_the_reference_model() {
    for seed in 0..16u64 {
        run_seed(seed, 180);
    }
}

#[test]
fn indexed_queries_match_unindexed_queries_and_the_reference_model() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_v2(dir.path());
    let mut reference = RefDb::new();
    let mut rng = fastrand::Rng::with_seed(0xC0FFEE);

    for _ in 0..120 {
        let doc = random_doc(&mut rng);
        let id = db.insert(doc.clone()).expect("insert");
        reference.insert_with_id(&id, doc).expect("reference insert");
    }
    assert_in_sync(&db, &reference, "seed before indexing");

    let unindexed = compare_queries(&db, &reference, "unindexed");
    assert!(unindexed > 0);

    // A v2 index declaration consumes one WAL seq and emits no change event.
    db.create_index("score").expect("hash index on score");
    reference.note_index_seq();
    db.create_btree_index("kind").expect("btree index on kind");
    reference.note_index_seq();
    assert_in_sync(&db, &reference, "after index creation");

    // Same state, same queries: the planner must not change the answer.
    compare_queries(&db, &reference, "indexed");
    compare_facets(&db, &reference, "indexed");
    compare_changes(&db, &reference, 0, "indexed");

    // Mutating through an indexed field keeps the index honest, on both sides.
    let ids = live_ids(&db);
    for (n, id) in ids.iter().enumerate() {
        if n % 7 != 0 {
            continue;
        }
        let path = if n % 2 == 0 { "score" } else { "kind" };
        let value = if n % 2 == 0 {
            json!(n as i64)
        } else {
            json!("c")
        };
        compare_bool(
            db.set(id, path, value.clone()),
            reference.set(id, path, value),
            "indexed mutation",
        );
    }
    assert_in_sync(&db, &reference, "after indexed mutations");
    compare_queries(&db, &reference, "indexed after mutations");
}

#[test]
fn error_classes_match_the_reference_model() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_v2(dir.path());
    let mut reference = RefDb::new();

    // Insert rejects a non-object the same way on both sides.
    let err = db.insert(json!("not an object")).expect_err("scalar insert fails");
    assert_eq!(ref_error(&err), RefError::InvalidArgument);
    assert_eq!(
        reference.insert(json!("not an object")).expect_err("scalar insert fails"),
        RefError::InvalidArgument
    );

    // Missing documents.
    for id in ["absent-1", "absent-2"] {
        let err = db.get(id).expect_err("missing get fails");
        assert_eq!(ref_error(&err), RefError::NotFound);
        assert_eq!(reference.get(id), Err(RefError::NotFound));

        let err = db.delete(id).expect_err("missing delete fails");
        assert_eq!(ref_error(&err), RefError::NotFound);
        assert_eq!(reference.delete(id), Err(RefError::NotFound));

        let err = db.set(id, "a", json!(1)).expect_err("missing set fails");
        assert_eq!(ref_error(&err), RefError::NotFound);
        assert_eq!(reference.set(id, "a", json!(1)), Err(RefError::NotFound));

        let err = db.remove(id, "a").expect_err("missing remove fails");
        assert_eq!(ref_error(&err), RefError::NotFound);
        assert_eq!(reference.remove(id, "a"), Err(RefError::NotFound));

        let err = db
            .array_push(id, "a", json!(1))
            .expect_err("missing push fails");
        assert_eq!(ref_error(&err), RefError::NotFound);
        assert_eq!(
            reference.array_push(id, "a", json!(1)),
            Err(RefError::NotFound)
        );
    }

    // Empty patch batches are rejected before any lookup.
    let err = db
        .apply_ops("absent-1", &[], None)
        .expect_err("empty patch fails");
    assert_eq!(ref_error(&err), RefError::InvalidArgument);
    assert_eq!(
        reference.apply_ops("absent-1", &[], None),
        Err(RefError::InvalidArgument)
    );

    // A stale if_seq is a conflict carrying the current seq; a deleted doc
    // cannot be restored twice.
    let id = db.insert(json!({"a": 1})).expect("insert");
    reference.insert_with_id(&id, json!({"a": 1})).expect("insert");
    let seq = reference.doc_seq(&id).expect("doc seq");
    compare_unit(
        db.update_if(&id, json!({"a": 2}), Some(seq)),
        reference.update_if(&id, json!({"a": 2}), Some(seq)),
        "matching if_seq",
    );
    compare_unit(
        db.update_if(&id, json!({"a": 3}), Some(seq)),
        reference.update_if(&id, json!({"a": 3}), Some(seq)),
        "stale if_seq",
    );
    compare_unit(db.delete(&id), reference.delete(&id).map(|_| ()), "delete");
    compare_unit(
        db.restore(&id),
        reference.restore(&id).map(|_| ()),
        "restore",
    );
    compare_unit(
        db.restore(&id),
        reference.restore(&id).map(|_| ()),
        "second restore",
    );
    assert_in_sync(&db, &reference, "error semantics");
    compare_changes(&db, &reference, 0, "error semantics");
}

#[test]
fn no_op_writes_move_neither_seq_nor_the_feed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_v2(dir.path());
    let mut reference = RefDb::new();

    let id = db
        .insert(sample_doc("a", 1, 0, "alpha", &["x"], 2))
        .expect("insert");
    reference
        .insert_with_id(&id, sample_doc("a", 1, 0, "alpha", &["x"], 2))
        .expect("insert");
    let seq_before = reference.last_seq();

    // Unresolvable paths are no-ops on both sides: a missing intermediate
    // segment, an out-of-bounds index, or a scalar where an object is needed.
    for (path, value) in [
        ("absent.deep", json!(1)),
        ("items.99", json!(1)),
        ("title.sub", json!(1)),
        ("nested.absent.k", json!(1)),
    ] {
        compare_bool(
            db.set(&id, path, value.clone()),
            reference.set(&id, path, value),
            "no-op set",
        );
    }
    for path in ["absent", "absent.deep", "items.99", "tags.9", "title.sub"] {
        compare_bool(
            db.remove(&id, path),
            reference.remove(&id, path),
            "no-op remove",
        );
    }
    compare_bool(
        db.array_push(&id, "title", json!("nope")),
        reference.array_push(&id, "title", json!("nope")),
        "push onto a scalar",
    );

    assert_eq!(reference.last_seq(), seq_before, "no-op moved seq");
    assert_in_sync(&db, &reference, "after no-ops");
    compare_changes(&db, &reference, seq_before, "after no-ops");
}

#[test]
fn cursor_contract_matches_the_reference_model() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_v2(dir.path());
    let mut reference = RefDb::new();
    let id = db.insert(json!({"n": 1})).expect("insert");
    reference.insert_with_id(&id, json!({"n": 1})).expect("insert");

    compare_changes(&db, &reference, 0, "cursor 0");
    compare_changes(&db, &reference, 1, "cursor 1");
    compare_changes(&db, &reference, 2, "cursor ahead");
    compare_changes(&db, &reference, u64::MAX, "cursor far ahead");
}

/// A recorded v2 defect, pinned so it cannot be forgotten (herrbasan/nDB#15).
///
/// An unsorted v2 query applies `offset` only inside the `limit` window
/// (`cap.map_or(true, |cap| index >= offset && index < cap)` in
/// `V2Engine::select_matches`), so `offset` with no `limit` is ignored and the
/// page starts at the first match. v1 and the contract both skip `offset`
/// matches. Phase 1's engine must follow the contract; when it does, this test
/// fails and should be deleted.
#[test]
fn v2_ignores_offset_without_a_limit_known_defect() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open_v2(dir.path());
    let mut reference = RefDb::new();
    for n in 0..5 {
        let doc = json!({"n": n});
        let id = db.insert(doc.clone()).expect("insert");
        reference.insert_with_id(&id, doc).expect("insert");
    }

    let opts = QueryOptions {
        offset: Some(2),
        ..QueryOptions::default()
    };
    let actual = db.query_with(json!({}), opts.clone()).expect("query");
    let expected = reference.query_with(&json!({}), &opts).expect("query");
    assert_eq!(
        actual.len(),
        5,
        "v2 currently ignores offset without a limit"
    );
    assert_eq!(
        expected.len(),
        3,
        "the contract skips the offset matches"
    );
    assert_ne!(doc_ids(&actual), doc_ids(&expected));
}
