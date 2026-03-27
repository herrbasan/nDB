//! Integration tests for nDB Phase 4: Query Layer
//!
//! Tests single field queries, opt-in indexing, and JSON AST query evaluator.

use ndb::{Database, QueryOptions, SortDir};
use serde_json::json;
use tempfile::TempDir;

fn setup() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("query.jsonl");
    let db = Database::open(&path).unwrap();
    (db, dir)
}

fn populate_db(db: &Database) -> Vec<String> {
    let mut ids = Vec::new();
    ids.push(db.insert(json!({"name": "alice", "age": 30, "status": "active", "score": 150})).unwrap());
    ids.push(db.insert(json!({"name": "bob", "age": 25, "status": "active", "score": 80})).unwrap());
    ids.push(db.insert(json!({"name": "charlie", "age": 35, "status": "inactive", "score": 200})).unwrap());
    ids.push(db.insert(json!({"name": "diana", "age": 28, "status": "active", "score": 95})).unwrap());
    ids.push(db.insert(json!({"name": "eve", "age": 40, "status": "inactive", "score": 300})).unwrap());
    ids
}

// ─── Layer 2: Single Field Queries ──────────────────────────────────

#[test]
fn find_by_field_value() {
    let (db, _dir) = setup();
    populate_db(&db);

    let active = db.find("status", &json!("active"));
    assert_eq!(active.len(), 3);
    assert!(active.iter().all(|d| d["status"] == "active"));
}

#[test]
fn find_by_number() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.find("age", &json!(30));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], "alice");
}

#[test]
fn find_no_results() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.find("name", &json!("nonexistent"));
    assert_eq!(results.len(), 0);
}

#[test]
fn find_where_predicate() {
    let (db, _dir) = setup();
    populate_db(&db);

    let high_scorers = db.find_where("score", |v| v.as_i64().unwrap_or(0) > 100);
    assert_eq!(high_scorers.len(), 3); // alice(150), charlie(200), eve(300)
}

#[test]
fn find_range() {
    let (db, _dir) = setup();
    populate_db(&db);

    let range = db.find_range("age", &json!(26), &json!(35));
    assert_eq!(range.len(), 3); // alice(30), bob(25)->no, charlie(35), diana(28)
    // Actually: 26 <= age <= 35 → alice(30), charlie(35), diana(28) = 3
}

#[test]
fn find_with_hash_index() {
    let (db, _dir) = setup();
    populate_db(&db);

    db.create_index("name").unwrap();
    assert!(db.has_index("name"));

    let results = db.find("name", &json!("alice"));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], "alice");
}

#[test]
fn find_with_btree_index() {
    let (db, _dir) = setup();
    populate_db(&db);

    db.create_btree_index("age").unwrap();

    let results = db.find("age", &json!(30));
    assert_eq!(results.len(), 1);
}

#[test]
fn index_stays_in_sync() {
    let (db, _dir) = setup();
    db.create_index("status").unwrap();

    let id = db.insert(json!({"status": "pending"})).unwrap();
    assert_eq!(db.find("status", &json!("pending")).len(), 1);

    db.update(&id, json!({"status": "done"})).unwrap();
    assert_eq!(db.find("status", &json!("pending")).len(), 0);
    assert_eq!(db.find("status", &json!("done")).len(), 1);

    db.delete(&id).unwrap();
    assert_eq!(db.find("status", &json!("done")).len(), 0);
}

#[test]
fn drop_index() {
    let (db, _dir) = setup();
    db.create_index("name").unwrap();
    assert!(db.has_index("name"));

    db.drop_index("name").unwrap();
    assert!(!db.has_index("name"));
}

// ─── Layer 3: JSON AST Queries ──────────────────────────────────────

#[test]
fn query_eq() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({"name": {"$eq": "alice"}}));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], "alice");
}

#[test]
fn query_implicit_eq() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({"name": "bob"}));
    assert_eq!(results.len(), 1);
}

#[test]
fn query_ne() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({"status": {"$ne": "active"}}));
    assert_eq!(results.len(), 2); // charlie, eve
}

#[test]
fn query_gt_lt() {
    let (db, _dir) = setup();
    populate_db(&db);

    let gt = db.query(json!({"score": {"$gt": 150}}));
    assert_eq!(gt.len(), 2); // charlie(200), eve(300)

    let lt = db.query(json!({"age": {"$lt": 30}}));
    assert_eq!(lt.len(), 2); // bob(25), diana(28)
}

#[test]
fn query_gte_lte() {
    let (db, _dir) = setup();
    populate_db(&db);

    let gte = db.query(json!({"score": {"$gte": 150}}));
    assert_eq!(gte.len(), 3); // alice(150), charlie(200), eve(300)

    let lte = db.query(json!({"age": {"$lte": 30}}));
    assert_eq!(lte.len(), 3); // alice(30), bob(25), diana(28)
}

#[test]
fn query_in() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({"name": {"$in": ["alice", "eve"]}}));
    assert_eq!(results.len(), 2);
}

#[test]
fn query_nin() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({"name": {"$nin": ["alice", "bob"]}}));
    assert_eq!(results.len(), 3); // charlie, diana, eve
}

#[test]
fn query_exists() {
    let (db, _dir) = setup();
    populate_db(&db);

    // All docs have "name"
    let with_name = db.query(json!({"name": {"$exists": true}}));
    assert_eq!(with_name.len(), 5);

    // None have "avatar"
    let with_avatar = db.query(json!({"avatar": {"$exists": true}}));
    assert_eq!(with_avatar.len(), 0);

    let without_avatar = db.query(json!({"avatar": {"$exists": false}}));
    assert_eq!(without_avatar.len(), 5);
}

#[test]
fn query_and() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({
        "$and": [
            {"status": {"$eq": "active"}},
            {"score": {"$gt": 100}}
        ]
    }));
    assert_eq!(results.len(), 1); // alice(150)
    assert_eq!(results[0]["name"], "alice");
}

#[test]
fn query_or() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({
        "$or": [
            {"name": {"$eq": "alice"}},
            {"name": {"$eq": "eve"}}
        ]
    }));
    assert_eq!(results.len(), 2);
}

#[test]
fn query_not() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({
        "$not": {"status": {"$eq": "inactive"}}
    }));
    assert_eq!(results.len(), 3); // active users
}

#[test]
fn query_nested_combinators() {
    let (db, _dir) = setup();
    populate_db(&db);

    // (active AND score > 90) OR name == eve
    let results = db.query(json!({
        "$or": [
            {"$and": [
                {"status": {"$eq": "active"}},
                {"score": {"$gt": 90}}
            ]},
            {"name": {"$eq": "eve"}}
        ]
    }));
    // alice(150, active), diana(95, active), eve
    assert_eq!(results.len(), 3);
}

#[test]
fn query_with_sort_limit_offset() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query_with(
        json!({"status": {"$eq": "active"}}),
        QueryOptions {
            limit: Some(2),
            offset: Some(1),
            sort_by: Some(("score".to_string(), SortDir::Desc)),
        },
    );

    // Active sorted desc: alice(150), diana(95), bob(80)
    // Offset 1, limit 2: diana(95), bob(80)
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["name"], "diana");
    assert_eq!(results[1]["name"], "bob");
}

#[test]
fn query_with_sort_asc() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query_with(
        json!({}),
        QueryOptions {
            limit: None,
            offset: None,
            sort_by: Some(("age".to_string(), SortDir::Asc)),
        },
    );

    assert_eq!(results[0]["name"], "bob"); // 25
    assert_eq!(results[1]["name"], "diana"); // 28
    assert_eq!(results[2]["name"], "alice"); // 30
    assert_eq!(results[3]["name"], "charlie"); // 35
    assert_eq!(results[4]["name"], "eve"); // 40
}

#[test]
fn query_empty_result() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({"name": {"$eq": "nonexistent"}}));
    assert_eq!(results.len(), 0);
}

#[test]
fn query_all_docs() {
    let (db, _dir) = setup();
    populate_db(&db);

    let results = db.query(json!({}));
    assert_eq!(results.len(), 5);
}

#[test]
fn query_multiple_conditions() {
    let (db, _dir) = setup();
    populate_db(&db);

    // Multiple field conditions in one object = implicit AND
    let results = db.query(json!({
        "status": "active",
        "age": {"$gte": 28}
    }));
    // active AND age >= 28: alice(30), diana(28)
    assert_eq!(results.len(), 2);
}
