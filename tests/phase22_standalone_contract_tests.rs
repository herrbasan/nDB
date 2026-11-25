//! Phase 4A — the standalone object and query contract
//! (`spec_ndb-standalone-contract.md` §6 items 1, 2, 3, 4 and 8).
//!
//! What is pinned here:
//!
//! - the classification profile is opt-in, enforced on **every** write path,
//!   migratable only explicitly, and durable across a restart;
//! - category membership queries agree between the index and a scan;
//! - the shared field mapping persists and drives indexes and text search;
//! - multi-key sorting plus an opaque cursor is a total order with no dropped
//!   or duplicated rows, identical with and without indexes;
//! - standalone lexical search is AND-of-terms with phrases, honest about its
//!   scan, and refuses to exceed a scan budget;
//! - `peek`/`read_more` returns projections, character ranges and array
//!   slices at one pinned sequence, over UTF-8 that is not ASCII.

use ndb::server::{serve_multi_in_background, DbRegistry, PersistenceMode, ServerConfig, ServeHandle};
use ndb::{
    ArraySliceSpec, Database, Error, FieldMapping, MissingPlacement, PatchOp, PeekRequest,
    QueryOptions, RamConfig, SortDir, SortKey, TextQuery, TextRangeSpec,
};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

// ─── Harness ────────────────────────────────────────────────────────

fn open_ram(dir: &Path) -> Database {
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

struct TestServer {
    port: u16,
    handle: ServeHandle,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

fn start(db: Database) -> TestServer {
    let registry = DbRegistry::new(None, PersistenceMode::Immediate, 5_000);
    registry.add("default", db).expect("mount the database");
    let handle = serve_multi_in_background(
        Arc::new(registry),
        ServerConfig {
            port: 0,
            ..Default::default()
        },
    )
    .expect("serve");
    TestServer {
        port: handle.addr.port(),
        handle,
    }
}

fn raw(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
        method,
        path,
        body.len()
    );
    if !body.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body).unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has a header/body separator");
    let head = String::from_utf8_lossy(&response[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, response[split + 4..].to_vec())
}

fn json_request(port: u16, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
    let (status, body) = raw(port, method, path, &bytes);
    (
        status,
        serde_json::from_slice(&body).expect("response body is JSON"),
    )
}

// ─── Helpers ────────────────────────────────────────────────────────

fn page_ids(rows: &[std::sync::Arc<[u8]>]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let doc: Value = serde_json::from_slice(row).expect("row is JSON");
            doc["_id"].as_str().expect("_id").to_string()
        })
        .collect()
}

fn page_n(rows: &[std::sync::Arc<[u8]>]) -> Vec<i64> {
    rows.iter()
        .map(|row| {
            let doc: Value = serde_json::from_slice(row).expect("row is JSON");
            doc["n"].as_i64().expect("n")
        })
        .collect()
}

fn texts(docs: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = docs
        .iter()
        .map(|doc| doc["text"].as_str().expect("text").to_string())
        .collect();
    out.sort();
    out
}

fn is_invalid_argument(error: &Error) -> bool {
    matches!(error, Error::InvalidArgument { .. })
}

/// A stable, generated-id-independent key for cross-database comparisons.
fn titles(docs: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = docs
        .iter()
        .map(|doc| doc["title"].as_str().expect("title").to_string())
        .collect();
    out.sort();
    out
}

/// The `n` column in result order — the deterministic order oracle.
fn ordered_n(docs: &[Value]) -> Vec<i64> {
    docs.iter()
        .map(|doc| doc["n"].as_i64().expect("n"))
        .collect()
}

// ─── 1. Opt-in profile ──────────────────────────────────────────────

#[test]
fn classification_profile_is_opt_in_and_off_by_default() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());

    let schema = db.get_schema().expect("schema");
    assert!(!schema.classification_profile, "off until requested");
    assert!(schema.field_mapping.is_empty());
    assert!(schema.indexes.is_empty());

    // Arbitrary documents are the default contract.
    let id = db.insert(json!({"title": "anything", "n": 1, "tags": ["a"]})).unwrap();
    db.update(&id, json!({"title": "still anything", "tags": ["a"]})).unwrap();
    db.set(&id, "extra", json!(true)).unwrap();
    db.remove(&id, "extra").unwrap();
    db.array_push(&id, "tags", json!("b")).unwrap();
    db.apply_ops(
        &id,
        &[PatchOp::Remove {
            path: "tags".to_string(),
        }],
        None,
    )
    .unwrap();
    assert_eq!(db.get(&id).unwrap()["title"], "still anything");
}

// ─── 2. Enforcement on every write path ─────────────────────────────

#[test]
fn classification_profile_enforces_every_write_path() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());

    let pre_profile = db.insert(json!({"n": 1})).unwrap();
    let doomed = db.insert(json!({"legacy": true})).unwrap();
    db.delete(&doomed).unwrap();
    db.enable_classification_profile(true)
        .expect("backfills the one live document");

    assert_eq!(db.get(&pre_profile).unwrap()["domain"], json!(""));
    assert_eq!(db.get(&pre_profile).unwrap()["categories"], json!([]));

    // insert / insert_with_prefix
    let error = db.insert(json!({"categories": []})).unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    let error = db.insert(json!({"domain": "d"})).unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    let error = db
        .insert_with_prefix("doc", json!({"domain": 1, "categories": []}))
        .unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");

    // The defined uncategorized values are legal.
    let uncategorized = db
        .insert(json!({"domain": "", "categories": []}))
        .unwrap();
    // Empty strings inside `categories` are not.
    let error = db
        .insert(json!({"domain": "d", "categories": ["x", ""]}))
        .unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    let error = db
        .insert(json!({"domain": "d", "categories": ["x", 7]}))
        .unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");

    // Deduplication preserves first-seen order before storage.
    let tagged = db
        .insert(json!({"domain": "d", "categories": ["b", "a", "b", "c", "a"]}))
        .unwrap();
    assert_eq!(db.get(&tagged).unwrap()["categories"], json!(["b", "a", "c"]));

    // update (replace): a wrong shape fails and mutates nothing.
    let before = db.get(&uncategorized).unwrap();
    let error = db.update(&uncategorized, json!({"domain": "d"})).unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    assert_eq!(db.get(&uncategorized).unwrap(), before);
    db.update(&uncategorized, json!({"domain": "d", "categories": ["z", "z"]}))
        .unwrap();
    assert_eq!(
        db.get(&uncategorized).unwrap()["categories"],
        json!(["z"]),
        "a replace deduplicates too"
    );

    // set: type changes and removals fail loudly.
    assert!(db.set(&uncategorized, "domain", json!(42)).is_err());
    assert_eq!(db.get(&uncategorized).unwrap()["domain"], json!("d"));
    assert!(db.set(&uncategorized, "categories", json!("nope")).is_err());
    assert!(db.set(&uncategorized, "categories", json!(["ok", ""])).is_err());
    assert!(db.set(&uncategorized, "domain", json!("work")).unwrap());
    assert_eq!(db.get(&uncategorized).unwrap()["domain"], json!("work"));

    // remove: the required fields cannot be removed.
    assert!(db.remove(&uncategorized, "domain").is_err());
    assert!(db.remove(&uncategorized, "categories").is_err());
    assert_eq!(db.get(&uncategorized).unwrap()["domain"], json!("work"));

    // array_push on `categories`.
    assert!(db.array_push(&tagged, "categories", json!("")).is_err());
    let seq = db.doc_seq(&tagged).unwrap();
    assert_eq!(
        db.array_push(&tagged, "categories", json!("a")).unwrap(),
        false,
        "pushing an existing category changes nothing"
    );
    assert_eq!(db.doc_seq(&tagged).unwrap(), seq, "and consumes no sequence");
    assert!(db.array_push(&tagged, "categories", json!("new")).unwrap());
    assert_eq!(
        db.get(&tagged).unwrap()["categories"],
        json!(["b", "a", "c", "new"])
    );

    // apply_ops: the whole batch fails when the result is invalid.
    let error = db
        .apply_ops(
            &tagged,
            &[PatchOp::Remove {
                path: "domain".to_string(),
            }],
            None,
        )
        .unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    assert_eq!(db.get(&tagged).unwrap()["domain"], json!("d"));
    db.apply_ops(
        &tagged,
        &[PatchOp::Set {
            path: "domain".to_string(),
            value: json!("ops"),
        }],
        None,
    )
    .unwrap();

    // restore: a trashed document from before the profile is refused loudly.
    let error = db.restore(&doomed).unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    assert!(db.get(&doomed).is_err(), "and stays deleted");
}

// ─── 3. Explicit migration + restart ────────────────────────────────

#[test]
fn classification_migration_fails_loud_backfills_and_survives_restart() {
    let dir = TempDir::new().unwrap();
    let first_seq;
    {
        let db = open_ram(dir.path());
        db.insert(json!({"n": 1})).unwrap();
        db.insert(json!({"n": 2, "domain": "x"})).unwrap();
        db.insert(json!({"n": 3, "categories": ["a"]})).unwrap();
        first_seq = db.doc_seq(&db.get_all_ids()[0]).unwrap();

        // Missing fields without the backfill flag: loud, nothing mutated.
        let error = db.enable_classification_profile(false).unwrap_err();
        assert!(is_invalid_argument(&error), "{error}");
        assert!(
            error.to_string().contains("lack 'domain'"),
            "the error names what is missing: {error}"
        );
        assert!(!db.get_schema().unwrap().classification_profile);
        assert!(db.get_schema().unwrap().indexes.is_empty());
        assert_eq!(db.doc_seq(&db.get_all_ids()[0]).unwrap(), first_seq);

        // A wrong-shaped field fails the whole migration.
        let bad = db
            .insert(json!({"n": 4, "domain": 42, "categories": []}))
            .unwrap();
        let error = db.enable_classification_profile(true).unwrap_err();
        assert!(is_invalid_argument(&error), "{error}");
        assert!(error.to_string().contains("domain"), "{error}");
        assert!(!db.get_schema().unwrap().classification_profile);
        db.delete(&bad).unwrap();

        // With the backfill the migration succeeds and builds both indexes.
        db.enable_classification_profile(true).unwrap();
        let schema = db.get_schema().unwrap();
        assert!(schema.classification_profile);
        assert!(
            schema.indexes.iter().any(|spec| spec.field == "domain"),
            "{schema:?}"
        );
        assert!(
            schema
                .indexes
                .iter()
                .any(|spec| spec.field == "categories"),
            "{schema:?}"
        );
        assert!(
            schema
                .field_mapping
                .iter()
                .any(|entry| entry.field == "domain" && entry.hash),
            "{schema:?}"
        );
        for id in db.get_all_ids() {
            let doc = db.get(&id).unwrap();
            assert!(doc["domain"].is_string(), "{doc}");
            assert!(doc["categories"].is_array(), "{doc}");
        }
    }

    // Restart: the profile, the mapping and the indexes are durable.
    let db = open_ram(dir.path());
    let schema = db.get_schema().unwrap();
    assert!(schema.classification_profile);
    assert!(db.has_index("domain"));
    assert!(db.has_index("categories"));
    assert!(db.insert(json!({"n": 9})).is_err(), "enforcement survives");
    db.insert(json!({"n": 9, "domain": "", "categories": []}))
        .unwrap();
}

// ─── 4. Category membership, indexed vs scan ────────────────────────

#[test]
fn category_membership_queries_match_indexed_and_scan() {
    let indexed_dir = TempDir::new().unwrap();
    let scan_dir = TempDir::new().unwrap();
    let indexed = open_ram(indexed_dir.path());
    let scan = open_ram(scan_dir.path());
    indexed
        .enable_classification_profile(true)
        .expect("empty database migrates trivially");

    let corpus = [
        json!({"title": "one", "domain": "work", "categories": ["rust", "db"]}),
        json!({"title": "two", "domain": "work", "categories": ["rust"]}),
        json!({"title": "three", "domain": "home", "categories": ["rust", "garden"]}),
        json!({"title": "four", "domain": "", "categories": []}),
        json!({"title": "five", "domain": "home", "categories": ["garden"]}),
    ];
    for doc in corpus {
        indexed.insert(doc.clone()).expect("indexed insert");
        scan.insert(doc).expect("scan insert");
    }
    // The scan side gains its arrays through a patch, exercising the same
    // predicate on a different write path.
    let extra = scan.insert(json!({"title": "six"})).unwrap();
    scan.array_push(&extra, "categories", json!("rust")).unwrap();
    let extra_indexed = indexed
        .insert(json!({"title": "six", "domain": "", "categories": []}))
        .unwrap();
    indexed
        .array_push(&extra_indexed, "categories", json!("rust"))
        .unwrap();

    let filters = [
        json!({"categories": "rust"}),
        json!({"categories": {"$eq": "garden"}}),
        json!({"categories": {"$in": ["rust", "garden"]}}),
        json!({"$and": [{"domain": "work"}, {"categories": "rust"}]}),
        json!({"$and": [{"domain": ""}, {"categories": []}]}),
    ];
    for filter in filters {
        let with_index = titles(&indexed.query(filter.clone()).unwrap());
        let without_index = titles(&scan.query(filter.clone()).unwrap());
        assert_eq!(
            with_index, without_index,
            "indexed and scan agree on {filter}"
        );
        assert!(!with_index.is_empty(), "{filter} matched nothing");
    }

    // The membership query is planned through the index, and says so.
    let (meta, rows) = indexed
        .query_raw_page(
            json!({"categories": "rust"}),
            QueryOptions::default(),
            None,
        )
        .unwrap();
    assert_eq!(meta.total, 4);
    assert_eq!(meta.scanned_docs, 4, "the index narrowed the candidates");
    assert!(
        meta.used_indexes.contains(&"categories".to_string()),
        "{:?}",
        meta.used_indexes
    );
    assert_eq!(page_ids(&rows).len(), 4);
}

// ─── 5. Shared field mapping ────────────────────────────────────────

#[test]
fn field_mapping_over_http_declares_indexes_and_text_fields() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());
    db.insert(json!({"title": "Rust basics", "body": "katze", "score": 5}))
        .unwrap();
    db.insert(json!({"title": "Garden", "body": "the rust belt", "score": 9}))
        .unwrap();
    let server = start(db);

    let (status, body) = json_request(server.port, "GET", "/v2/dbs/default/schema", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["schema"]["classification_profile"], false);

    let (status, body) = json_request(
        server.port,
        "PUT",
        "/v2/dbs/default/schema",
        Some(&json!({
            "field_mapping": [
                {"field": "score", "hash": true, "ordered": true},
                {"field": "title", "text": true, "project": true},
                {"field": "body", "text": true},
                {"field": "unindexed", "project": true},
            ]
        })),
    );
    assert_eq!(status, 200, "{body}");
    let mapping = body["schema"]["field_mapping"].as_array().unwrap();
    assert_eq!(mapping.len(), 4);
    assert_eq!(mapping[0]["hash"], true);
    assert_eq!(mapping[0]["ordered"], true);
    assert_eq!(mapping[1]["project"], true);
    assert_eq!(mapping[3]["hash"], false);
    assert!(
        body["schema"]["indexes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|spec| spec["field"] == "score" && spec["kind"] == "btree"),
        "hash + ordered is backed by one BTree: {body}"
    );

    // The declared capabilities answer as promised through the same service.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/search",
        Some(&json!({"text": "rust"})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 2, "{body}");

    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/query",
        Some(&json!({"filter": {"score": {"$gte": 6}}})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 1, "the ordered capability answers a range");
    assert_eq!(server.port, server.handle.addr.port());
}

#[test]
fn field_mapping_persists_across_restart() {
    let dir = TempDir::new().unwrap();
    {
        let db = open_ram(dir.path());
        db.insert(json!({"title": "Rust basics", "body": "katze", "score": 5}))
            .unwrap();
        db.insert(json!({"title": "Garden", "body": "the rust belt", "score": 9}))
            .unwrap();
        db.set_field_mapping(vec![
            FieldMapping {
                field: "score".to_string(),
                hash: true,
                ordered: true,
                ..FieldMapping::default()
            },
            FieldMapping {
                field: "title".to_string(),
                text: true,
                project: true,
                ..FieldMapping::default()
            },
            FieldMapping {
                field: "body".to_string(),
                text: true,
                ..FieldMapping::default()
            },
        ])
        .expect("set the mapping");
        assert!(db.has_btree_index("score"));
    }

    let db = open_ram(dir.path());
    let schema = db.get_schema().unwrap();
    assert_eq!(schema.field_mapping.len(), 3);
    assert_eq!(schema.field_mapping[0].field, "score");
    assert!(schema.field_mapping[0].hash && schema.field_mapping[0].ordered);
    assert!(schema.field_mapping[1].project);
    assert!(db.has_btree_index("score"));
    assert_eq!(
        db.query(json!({"score": {"$gte": 6}})).unwrap().len(),
        1,
        "the persisted ordered index answers a range"
    );

    // `text: true` fields are the default search target.
    let only_title = db
        .query_with(
            json!({}),
            QueryOptions {
                text: Some(TextQuery {
                    query: "rust".to_string(),
                    fields: None,
                }),
                ..QueryOptions::default()
            },
        )
        .unwrap();
    assert_eq!(
        only_title.len(),
        2,
        "both documents mention rust in a `text` field"
    );
}

// ─── 6. Multi-key sort + cursor pagination ──────────────────────────

#[test]
fn multi_key_sort_and_cursor_pagination_is_total() {
    let indexed_dir = TempDir::new().unwrap();
    let scan_dir = TempDir::new().unwrap();
    let indexed = open_ram(indexed_dir.path());
    let scan = open_ram(scan_dir.path());
    indexed.create_btree_index("a").unwrap();
    indexed.create_btree_index("b").unwrap();

    let a_values = [
        Value::Null,
        json!(1),
        json!("x"),
        json!(true),
        json!([1, 2]),
    ];
    for index in 0..40 {
        let mut doc = serde_json::Map::new();
        if index % 4 != 0 {
            doc.insert(
                "a".to_string(),
                a_values[index % a_values.len()].clone(),
            );
        }
        if index % 3 != 0 {
            doc.insert("b".to_string(), json!(index % 3));
        }
        doc.insert("n".to_string(), json!(index));
        let doc = Value::Object(doc);
        indexed.insert(doc.clone()).unwrap();
        scan.insert(doc).unwrap();
    }

    let order = vec![
        SortKey {
            field: "a".to_string(),
            dir: SortDir::Desc,
            missing: MissingPlacement::First,
        },
        SortKey {
            field: "b".to_string(),
            dir: SortDir::Asc,
            missing: MissingPlacement::Last,
        },
    ];
    // The same order plus a deterministic tail, so two databases that generate
    // different `_id`s produce comparable sequences. Ties on `(a, b)` are
    // resolved by `_id` — stable within one database, which the pagination
    // check below relies on.
    let mut order_with_n = order.clone();
    order_with_n.push(SortKey::asc("n"));

    let full_indexed = ordered_n(
        &indexed
            .query_with(
                json!({}),
                QueryOptions {
                    sort_keys: order_with_n.clone(),
                    limit: None,
                    ..QueryOptions::default()
                },
            )
            .unwrap(),
    );
    let full_scan = ordered_n(
        &scan.query_with(
            json!({}),
            QueryOptions {
                sort_keys: order_with_n,
                limit: None,
                ..QueryOptions::default()
            },
        )
        .unwrap(),
    );
    assert_eq!(
        full_indexed, full_scan,
        "indexed and scan orders are identical"
    );
    assert_eq!(full_indexed.len(), 40);

    // The `_id` tie-breaker makes even the two-key order a total order within
    // one database.
    let paged_order = ordered_n(
        &indexed
            .query_with(
                json!({}),
                QueryOptions {
                    sort_keys: order.clone(),
                    limit: None,
                    ..QueryOptions::default()
                },
            )
            .unwrap(),
    );
    assert_eq!(paged_order.len(), 40);
    assert_eq!(
        paged_order,
        ordered_n(
            &indexed
                .query_with(
                    json!({}),
                    QueryOptions {
                        sort_keys: order.clone(),
                        limit: None,
                        ..QueryOptions::default()
                    },
                )
                .unwrap()
        ),
        "the same order twice is identical"
    );

    // Paginate through the indexed database, 7 rows at a time.
    let mut paged: Vec<i64> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (meta, rows) = indexed
            .query_raw_page(
                json!({}),
                QueryOptions {
                    sort_keys: order.clone(),
                    limit: Some(7),
                    cursor: cursor.clone(),
                    ..QueryOptions::default()
                },
                None,
            )
            .unwrap();
        paged.extend(page_n(&rows));
        match meta.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
        assert!(paged.len() <= 40, "pagination must terminate");
    }
    assert_eq!(paged, paged_order, "no drops, no duplicates, same order");
    let unique: HashSet<i64> = paged.iter().copied().collect();
    assert_eq!(unique.len(), 40);

    // An out-of-spec cursor combination fails loudly.
    let error = indexed
        .query_raw_page(
            json!({}),
            QueryOptions {
                sort_keys: order,
                cursor: Some("ndbc1:00".to_string()),
                ..QueryOptions::default()
            },
            None,
        )
        .expect_err("a malformed cursor is refused");
    let Error::InvalidArgument { .. } = error else {
        panic!("expected invalid_argument, got {error}");
    };
}

// ─── 7. Standalone text search ──────────────────────────────────────

#[test]
fn standalone_text_search_is_deterministic_and_honest() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());
    db.insert(json!({"title": "Über Katzen", "body": "Ein kurzer Text über Katzen"}))
        .unwrap();
    db.insert(json!({"title": "Hunde", "body": "Ein langer Text über Hunde und Katzen"}))
        .unwrap();
    db.insert(json!({"title": "Nichts", "body": "Ein Text ohne Tiere"}))
        .unwrap();
    let server = start(db);

    // Term AND + case-insensitivity, across all string fields by default.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/search",
        Some(&json!({"text": "KATZEN text"})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 2, "{body}");

    // Quoted phrase.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/search",
        Some(&json!({"text": "\"ohne tiere\""})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["results"][0]["title"], "Nichts");

    // Explicit fields restrict the search.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/search",
        Some(&json!({"text": {"query": "katzen", "fields": ["title"]}})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 1, "{body}");

    // Combined with a structured filter and a deterministic sort.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/search",
        Some(&json!({
            "text": "katzen",
            "filter": {"title": {"$ne": "Nichts"}},
            "sort_keys": [{"field": "title", "dir": "asc"}],
            "limit": 10
        })),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 2, "{body}");
    assert_eq!(body["results"][0]["title"], "Hunde");
    assert_eq!(body["scanned_docs"], 3, "an unindexed scan examined all");
    assert_eq!(body["used_indexes"], json!([]));
    assert_eq!(body["next_cursor"], Value::Null);

    // Fail loud on malformed queries.
    for bad in [
        json!({"text": "\"unterminated"}),
        json!({"text": "   "}),
        json!({"text": {"fields": ["title"]}}),
    ] {
        let (status, body) = json_request(
            server.port,
            "POST",
            "/v2/dbs/default/search",
            Some(&bad),
        );
        assert_eq!(status, 400, "{bad} -> {body}");
        assert_eq!(body["ok"], false);
    }

    // A scan budget that cannot be met aborts loudly with 422.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/search",
        Some(&json!({"text": "text", "max_scan_docs": 1})),
    );
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "scan_budget_exceeded");
    assert_eq!(body["max_scan"], 1);
    assert!(body["scanned"].as_u64().unwrap() > 1);
}

// ─── 8. Partial retrieval ───────────────────────────────────────────

#[test]
fn peek_returns_projections_ranges_and_slices_at_one_sequence() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());

    let mut messages = Vec::new();
    for index in 0..300 {
        messages.push(json!({
            "role": if index % 2 == 0 { "user" } else { "assistant" },
            "content": format!("Nachricht {index}"),
        }));
    }
    let text = "Grüße aus München — schöne Überraschung 🎉 mit Emoji!";
    let id = db
        .insert(json!({
            "meta": {"author": "dave", "tags": ["a", "b"]},
            "text": text,
            "messages": messages,
            "blob": 42,
        }))
        .unwrap();
    let seq = db.doc_seq(&id).unwrap();

    // Whole-document peek needs no ranges.
    let response = db.peek_item(&id, &PeekRequest::default()).unwrap();
    assert_eq!(response.seq, seq);
    assert_eq!(response.id, id);
    assert_eq!(response.doc["_id"], json!(id));
    assert!(response.ranges.is_empty());

    // Nested projection.
    let response = db
        .peek_item(
            &id,
            &PeekRequest {
                fields: Some(vec!["meta.author".to_string()]),
                ..PeekRequest::default()
            },
        )
        .unwrap();
    assert_eq!(response.doc["meta"]["author"], "dave");
    assert!(response.doc.get("text").is_none());
    assert_eq!(response.doc["_id"], json!(id));

    // Character ranges over multibyte UTF-8, in exact Unicode scalar counts.
    let total_chars = text.chars().count();
    let first = db
        .peek_item(
            &id,
            &PeekRequest {
                text_ranges: vec![TextRangeSpec {
                    path: "text".to_string(),
                    offset: 0,
                    limit: 10,
                }],
                ..PeekRequest::default()
            },
        )
        .unwrap();
    let meta = &first.ranges["text"];
    assert_eq!(meta.offset, 0);
    assert_eq!(meta.returned, 10);
    assert_eq!(meta.total, total_chars);
    assert!(meta.has_more);
    assert_eq!(meta.next_offset, 10);
    assert_eq!(
        first.doc["text"],
        json!(text.chars().take(10).collect::<String>())
    );

    // Multi-page read_more, pinned to the same sequence.
    let mut collected = String::new();
    let mut offset = 0usize;
    loop {
        let page = db
            .peek_item(
                &id,
                &PeekRequest {
                    text_ranges: vec![TextRangeSpec {
                        path: "text".to_string(),
                        offset,
                        limit: 7,
                    }],
                    expected_seq: Some(seq),
                    ..PeekRequest::default()
                },
            )
            .unwrap();
        assert_eq!(page.seq, seq);
        let meta = &page.ranges["text"];
        collected.push_str(page.doc["text"].as_str().expect("a sliced string"));
        assert_eq!(meta.offset, offset);
        assert_eq!(meta.returned, page.doc["text"].as_str().unwrap().chars().count());
        if !meta.has_more {
            assert_eq!(meta.next_offset, total_chars);
            break;
        }
        offset = meta.next_offset;
    }
    assert_eq!(collected, text, "the pages reassemble the field exactly");

    // Array slices over a multi-hundred-message conversation.
    let page = db
        .peek_item(
            &id,
            &PeekRequest {
                array_slices: vec![ArraySliceSpec {
                    path: "messages".to_string(),
                    offset: 100,
                    limit: 5,
                }],
                expected_seq: Some(seq),
                ..PeekRequest::default()
            },
        )
        .unwrap();
    let slice = page.doc["messages"].as_array().unwrap();
    assert_eq!(slice.len(), 5);
    assert_eq!(slice[0]["content"], "Nachricht 100");
    let meta = &page.ranges["messages"];
    assert_eq!(meta.total, 300);
    assert_eq!(meta.returned, 5);
    assert!(meta.has_more);
    assert_eq!(meta.next_offset, 105);
    assert!(page.doc.get("text").is_none(), "only requested paths return");

    // A slice past the end is an empty, honest range.
    let page = db
        .peek_item(
            &id,
            &PeekRequest {
                array_slices: vec![ArraySliceSpec {
                    path: "messages".to_string(),
                    offset: 400,
                    limit: 5,
                }],
                ..PeekRequest::default()
            },
        )
        .unwrap();
    assert_eq!(page.doc["messages"], json!([]));
    assert_eq!(page.ranges["messages"].returned, 0);
    assert!(!page.ranges["messages"].has_more);

    // Wrong field types, missing documents and stale sequences fail loudly.
    let error = db
        .peek_item(
            &id,
            &PeekRequest {
                text_ranges: vec![TextRangeSpec {
                    path: "blob".to_string(),
                    offset: 0,
                    limit: 1,
                }],
                ..PeekRequest::default()
            },
        )
        .unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    let error = db
        .peek_item(
            &id,
            &PeekRequest {
                array_slices: vec![ArraySliceSpec {
                    path: "text".to_string(),
                    offset: 0,
                    limit: 1,
                }],
                ..PeekRequest::default()
            },
        )
        .unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    assert!(matches!(
        db.peek_item("missing", &PeekRequest::default()),
        Err(Error::NotFound { .. })
    ));

    // The document moves mid-read → the pinned read reports a conflict.
    db.set(&id, "meta.author", json!("someone else")).unwrap();
    let error = db
        .peek_item(
            &id,
            &PeekRequest {
                text_ranges: vec![TextRangeSpec {
                    path: "text".to_string(),
                    offset: 0,
                    limit: 5,
                }],
                expected_seq: Some(seq),
                ..PeekRequest::default()
            },
        )
        .unwrap_err();
    match error {
        Error::Conflict { current_seq, .. } => {
            assert_eq!(current_seq, db.doc_seq(&id).unwrap());
            assert_ne!(current_seq, seq);
        }
        other => panic!("expected a conflict, got {other}"),
    }
}

#[test]
fn peek_over_http_returns_the_contract_envelope() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());
    let id = db
        .insert(json!({
            "text": "Grüße 🎉",
            "items": [1, 2, 3, 4],
        }))
        .unwrap();
    let seq = db.doc_seq(&id).unwrap();
    let server = start(db);
    let path = format!("/v2/dbs/default/doc/{id}/peek");

    let (status, body) = json_request(
        server.port,
        "POST",
        &path,
        Some(&json!({
            "text_ranges": [{"path": "text", "offset": 0, "limit": 6}],
            "array_slices": [{"path": "items", "offset": 1, "limit": 2}],
            "expected_seq": seq,
        })),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["id"], json!(id));
    assert_eq!(body["seq"], seq);
    assert_eq!(body["doc"]["text"], "Grüße ");
    assert_eq!(body["doc"]["items"], json!([2, 3]));
    assert_eq!(body["ranges"]["text"]["total"], 7);
    assert_eq!(body["ranges"]["text"]["returned"], 6);
    assert_eq!(body["ranges"]["text"]["has_more"], true);
    assert_eq!(body["ranges"]["items"]["total"], 4);
    assert_eq!(body["ranges"]["items"]["next_offset"], 3);

    // 409 on a stale sequence, with the current one named.
    let (status, body) = json_request(
        server.port,
        "POST",
        &path,
        Some(&json!({"expected_seq": seq + 1})),
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "conflict");
    assert_eq!(body["current_seq"], seq);

    // 404 for a missing document.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/doc/nope/peek",
        Some(&json!({})),
    );
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"], "not_found");

    // 400 for a range on the wrong type.
    let (status, body) = json_request(
        server.port,
        "POST",
        &path,
        Some(&json!({"text_ranges": [{"path": "items", "offset": 0, "limit": 1}]})),
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "invalid_argument");
}

// ─── 2 (continued). Indexed-vs-scan oracle on the enhanced query ─────

#[test]
fn enhanced_queries_are_index_agnostic() {
    let plain_dir = TempDir::new().unwrap();
    let indexed_dir = TempDir::new().unwrap();
    let plain = open_ram(plain_dir.path());
    let indexed = open_ram(indexed_dir.path());
    plain.create_index("kind").unwrap();
    indexed.create_btree_index("kind").unwrap();
    indexed.create_index("rank").unwrap();

    for index in 0..25 {
        let mut doc = serde_json::Map::new();
        doc.insert(
            "kind".to_string(),
            json!(if index % 3 == 0 { "a" } else { "b" }),
        );
        doc.insert("rank".to_string(), json!(index % 7));
        doc.insert("text".to_string(), json!(format!("row {index} shared")));
        let doc = Value::Object(doc);
        plain.insert(doc.clone()).unwrap();
        indexed.insert(doc).unwrap();
    }

    let probes = [
        json!({"kind": "a"}),
        json!({"rank": {"$gte": 3}, "kind": {"$in": ["a", "b"]}}),
        json!({"$or": [{"kind": "a"}, {"rank": 5}]}),
    ];
    for filter in probes {
        let opts = QueryOptions {
            sort_keys: vec![
                SortKey::desc("rank"),
                SortKey {
                    field: "kind".to_string(),
                    dir: SortDir::Asc,
                    missing: MissingPlacement::Last,
                },
            ],
            text: Some(TextQuery {
                query: "SHARED".to_string(),
                fields: None,
            }),
            ..QueryOptions::default()
        };
        let left = texts(&plain.query_with(filter.clone(), opts.clone()).unwrap());
        let right = texts(&indexed.query_with(filter, opts).unwrap());
        assert_eq!(left, right, "indexed == scan for the probe");
    }
}

// ─── 5 (continued). Schema rejects dropping the profile's indexes ───

#[test]
fn schema_rejects_dropping_the_profile_indexes() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());
    db.enable_classification_profile(true).unwrap();

    let error = db
        .set_field_mapping(vec![FieldMapping {
            field: "domain".to_string(),
            hash: true,
            ..FieldMapping::default()
        }])
        .unwrap_err();
    assert!(is_invalid_argument(&error), "{error}");
    assert!(error.to_string().contains("categories"), "{error}");
    assert!(db.has_index("categories"), "nothing was dropped");

    // Dropping a profile index directly is refused too.
    let error = db.drop_index("domain").unwrap_err();
    assert!(matches!(error, Error::IndexError { .. }), "{error}");
    assert!(db.has_index("domain"));
}
