//! Phase 13 - multi-DB `ndb serve` (v2 service rework, step 1).
//!
//! One process, many databases: named mounts from arbitrary filesystem paths,
//! a filesystem-creation API scoped by an operator-supplied root, the full
//! Phase 0 sync lifecycle over HTTP, and bucket/CRUD/query parity with the
//! legacy single-DB routes. Everything is spoken as raw HTTP/1.1 over TCP -
//! the same wire a Node client uses.

use ndb::server::{
    serve_multi_in_background, DbRegistry, PersistenceMode, ServeHandle, ServerConfig, ServiceConfig,
};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

// --- Harness -------------------------------------------------------

struct TestServer {
    port: u16,
    handle: ServeHandle,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

fn server_config(token: Option<&str>) -> ServerConfig {
    ServerConfig {
        port: 0,
        token: token.map(str::to_string),
        ..Default::default()
    }
}

fn start(registry: DbRegistry, config: ServerConfig) -> TestServer {
    let handle = serve_multi_in_background(Arc::new(registry), config).unwrap();
    TestServer {
        port: handle.addr.port(),
        handle,
    }
}

/// One HTTP/1.1 request on its own connection; returns status, headers, body.
fn raw(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> (u16, String, Vec<u8>) {
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
    for (k, v) in headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
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
    (status, head, response[split + 4..].to_vec())
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> (u16, Value) {
    let bytes = body
        .map(|b| b.to_string().into_bytes())
        .unwrap_or_default();
    let auth = token.map(|t| format!("Bearer {}", t));
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if body.is_some() {
        headers.push(("Content-Type", "application/json"));
    }
    if let Some(auth) = &auth {
        headers.push(("Authorization", auth.as_str()));
    }
    let (status, _, raw_body) = raw(port, method, path, &bytes, &headers);
    (
        status,
        serde_json::from_slice(&raw_body).unwrap_or(Value::Null),
    )
}

fn get(port: u16, path: &str) -> (u16, Value) {
    request(port, "GET", path, None, None)
}

fn post(port: u16, path: &str, body: &Value) -> (u16, Value) {
    request(port, "POST", path, Some(body), None)
}

fn put(port: u16, path: &str, body: &Value) -> (u16, Value) {
    request(port, "PUT", path, Some(body), None)
}

fn insert(port: u16, db: &str, doc: &Value) -> String {
    let (status, body) = put(port, &format!("/v2/dbs/{}/doc", db), doc);
    assert_eq!(status, 201, "insert into {}: {}", db, body);
    body["id"].as_str().unwrap().to_string()
}

fn registry(dirs: &[(&str, std::path::PathBuf)]) -> DbRegistry {
    let registry = DbRegistry::new(None, PersistenceMode::Immediate, 5000);
    for (name, path) in dirs {
        registry.mount(name, path).unwrap();
    }
    registry
}

// --- Multi-DB mounting & isolation ---------------------------------

#[test]
fn mounts_independent_databases_from_separate_paths() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let path_a = dir_a.path().join("store-a").join("data.jsonl");
    let path_b = dir_b.path().join("store-b.jsonl");

    let registry = registry(&[("alpha", path_a.clone()), ("beta", path_b.clone())]);
    let server = start(registry, server_config(None));

    let a_id = insert(server.port, "alpha", &json!({"kind": "a", "n": 1}));
    let b_id = insert(server.port, "beta", &json!({"kind": "b", "n": 2}));

    // Each route reads only its own database.
    let (status, body) = get(server.port, &format!("/v2/dbs/alpha/doc/{}", a_id));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["doc"]["kind"], "a");

    let (status, body) = get(server.port, &format!("/v2/dbs/beta/doc/{}", b_id));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["doc"]["kind"], "b");

    // alpha cannot see beta's document and vice versa.
    let (status, _) = get(server.port, &format!("/v2/dbs/alpha/doc/{}", b_id));
    assert_eq!(status, 404, "documents must not leak across databases");

    let (_, body) = get(server.port, "/v2/dbs/alpha/stats");
    assert_eq!(body["documents"], 1);
    assert_eq!(body["lineage"]["last_seq"], 1);

    let (_, body) = get(server.port, "/v2/dbs/beta/stats");
    assert_eq!(body["documents"], 1);
    assert_eq!(body["lineage"]["last_seq"], 1);

    // Full query on alpha finds only alpha's row.
    let (status, body) = post(server.port, "/v2/dbs/alpha/query", &json!({}));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["total"], 1);
    assert_eq!(body["results"][0]["kind"], "a");

    // Service views.
    let (status, body) = get(server.port, "/health");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["ok"], true);
    assert_eq!(body["engine"], "ndb");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["dbs"], json!(["alpha", "beta"]), "names must be sorted");
    assert_eq!(body["allow_create"], false);

    let (status, body) = get(server.port, "/v2/dbs");
    assert_eq!(status, 200, "{}", body);
    let dbs = body["dbs"].as_array().unwrap();
    assert_eq!(dbs.len(), 2);
    assert_eq!(dbs[0]["name"], "alpha");
    assert_eq!(dbs[1]["name"], "beta");
    for descriptor in dbs {
        assert!(
            descriptor.get("path").is_none(),
            "filesystem paths must not be exposed"
        );
        assert!(descriptor.get("documents").is_some());
        assert!(descriptor.get("source_epoch_uuid").is_some());
        assert!(descriptor.get("last_seq").is_some());
        assert!(descriptor.get("min_available_seq").is_some());
    }

    // The journals really are separate files on disk.
    assert!(path_a.exists());
    assert!(path_b.exists());
    let a_text = std::fs::read_to_string(&path_a).unwrap();
    assert!(a_text.contains("\"kind\":\"a\""));
    assert!(!a_text.contains("\"kind\":\"b\""));
}

#[test]
fn token_auth_applies_to_all_routes() {
    let dir = TempDir::new().unwrap();
    let registry = registry(&[("alpha", dir.path().join("db.jsonl"))]);
    let server = start(registry, server_config(Some("secret123")));

    let (status, _) = get(server.port, "/health");
    assert_eq!(status, 401, "/health must require the token");
    let (status, _) = get(server.port, "/v2/dbs");
    assert_eq!(status, 401);
    let (status, _) = get(server.port, "/v2/dbs/alpha/stats");
    assert_eq!(status, 401);

    let (status, body) = request(server.port, "GET", "/v2/dbs", None, Some("secret123"));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["ok"], true);
}

// --- Startup rejection ---------------------------------------------

#[test]
fn duplicate_names_and_paths_are_rejected_at_mount() {
    let dir = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    let path = dir.path().join("data.jsonl");

    let registry = DbRegistry::new(None, PersistenceMode::Immediate, 5000);
    registry.mount("alpha", &path).unwrap();

    let err = registry
        .mount("alpha", other.path().join("x.jsonl"))
        .unwrap_err();
    assert!(
        err.to_string().contains("duplicate database name"),
        "same name must be refused: {err}"
    );

    // Non-normalized aliases of the mounted path.
    for alias in [
        dir.path().join(".").join("data.jsonl"),
        dir.path().join("nested").join("..").join("data.jsonl"),
        // The folder form resolves to the same journal file.
        dir.path().to_path_buf(),
    ] {
        let err = registry.mount("beta", &alias).unwrap_err();
        assert!(
            err.to_string().contains("duplicate database path"),
            "alias {} must be refused, got: {err}",
            alias.display()
        );
    }
}

#[test]
fn invalid_database_names_are_rejected() {
    for name in ["", ".", "..", "a/b", "a\\b", "a b", "naive\u{0308}", "a:b", "a*b"] {
        assert!(
            DbRegistry::validate_name(name).is_err(),
            "'{}' must be an invalid database name",
            name
        );
    }
    for name in ["a", "db-1", "db_1", "v1.5.0", "UPPER", "0"] {
        assert!(
            DbRegistry::validate_name(name).is_ok(),
            "'{}' must be a valid database name",
            name
        );
    }

    let dir = TempDir::new().unwrap();
    let registry = DbRegistry::new(None, PersistenceMode::Immediate, 5000);
    let err = registry.mount("..", dir.path().join("x.jsonl")).unwrap_err();
    assert!(err.to_string().contains("path component"), "{err}");
}

// --- POST /v2/dbs --------------------------------------------------

#[test]
fn create_database_under_root_dir() {
    let root = TempDir::new().unwrap();
    let registry = DbRegistry::new(
        Some(root.path().to_path_buf()),
        PersistenceMode::Immediate,
        5000,
    );
    let server = start(registry, server_config(None));

    let (status, body) = get(server.port, "/health");
    assert_eq!(status, 200);
    assert_eq!(body["allow_create"], true);

    let (status, body) = post(server.port, "/v2/dbs", &json!({"name": "fresh"}));
    assert_eq!(status, 201, "create: {}", body);
    assert_eq!(body["ok"], true);
    assert_eq!(body["db"]["name"], "fresh");
    assert_eq!(body["db"]["documents"], 0);
    assert!(body["db"].get("path").is_none());

    // The folder exists under the operator's root as a v3 (RAM-first) database,
    // and the mount is live.
    let manifest = root.path().join("fresh").join("manifest.json");
    assert!(manifest.is_file(), "POST /v2/dbs defaults to the RAM format");
    let text = fs::read_to_string(&manifest).unwrap();
    assert!(text.contains("\"version\": 3"), "{text}");
    assert_eq!(body["db"]["format"], "ram");
    insert(server.port, "fresh", &json!({"v": 1}));
    let (status, body) = get(server.port, "/v2/dbs/fresh/stats");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["documents"], 1);

    // Duplicate name -> 409.
    let (status, body) = post(server.port, "/v2/dbs", &json!({"name": "fresh"}));
    assert_eq!(status, 409, "{}", body);
    assert_eq!(body["error"], "already_exists");

    // A path field is not part of the contract - reject, never ignore.
    let (status, body) = post(
        server.port,
        "/v2/dbs",
        &json!({"name": "elsewhere", "path": "C:\\outside"}),
    );
    assert_eq!(status, 400, "path field must be rejected: {}", body);

    // Invalid names -> 400.
    for bad in ["bad name", "a/b", "..", ""] {
        let (status, body) = post(server.port, "/v2/dbs", &json!({"name": bad}));
        assert_eq!(status, 400, "name {:?} must be 400: {}", bad, body);
    }

    // Unknown persistence value -> 400.
    let (status, _) = post(
        server.port,
        "/v2/dbs",
        &json!({"name": "odd", "persistence": "sometimes"}),
    );
    assert_eq!(status, 400);
}

#[test]
fn create_is_disabled_without_root_dir() {
    let dir = TempDir::new().unwrap();
    let registry = registry(&[("alpha", dir.path().join("data.jsonl"))]);
    let server = start(registry, server_config(None));

    let (status, body) = post(server.port, "/v2/dbs", &json!({"name": "nope"}));
    assert_eq!(status, 403, "{}", body);
    assert_eq!(body["error"], "create_disabled");
    assert_eq!(body["ok"], false);
}

// --- CRUD, mget, patch, restore, query, find, index -----------------

#[test]
fn crud_prefix_mget_patch_restore_and_query() {
    let dir = TempDir::new().unwrap();
    let registry = registry(&[("workshop", dir.path().join("db.jsonl"))]);
    let server = start(registry, server_config(None));
    let p = server.port;

    // Insert (server-assigned id) and insert with a prefix.
    let (status, body) = put(
        p,
        "/v2/dbs/workshop/doc",
        &json!({"name": "alice", "score": 3, "tags": ["a"]}),
    );
    assert_eq!(status, 201, "{}", body);
    let alice = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["doc"]["_id"], json!(alice));

    let (status, body) = put(
        p,
        "/v2/dbs/workshop/doc?prefix=mem",
        &json!({"name": "bob", "score": 10, "tags": []}),
    );
    assert_eq!(status, 201, "{}", body);
    let bob = body["id"].as_str().unwrap().to_string();
    assert!(bob.starts_with("mem_"), "prefix must be honored: {bob}");

    // Full replace / upsert.
    let (status, body) = put(
        p,
        &format!("/v2/dbs/workshop/doc/{}", alice),
        &json!({"name": "alice", "score": 7, "tags": ["a"]}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["doc"]["score"], 7);

    // Projection.
    let (status, body) = get(
        p,
        &format!("/v2/dbs/workshop/doc/{}?fields=name,score", alice),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["doc"]["name"], "alice");
    assert_eq!(body["doc"]["score"], 7);
    assert!(body["doc"].get("tags").is_none());
    assert_eq!(body["doc"]["_id"], json!(alice));

    // Missing document -> 404 with the machine-readable code.
    let (status, body) = get(p, "/v2/dbs/workshop/doc/nope");
    assert_eq!(status, 404, "{}", body);
    assert_eq!(body["error"], "not_found");

    // Unknown database -> 404 database_not_found.
    let (status, body) = get(p, "/v2/dbs/absent/stats");
    assert_eq!(status, 404, "{}", body);
    assert_eq!(body["error"], "database_not_found");

    // Multi-op patch: set + array_push + remove.
    let (status, body) = request(
        p,
        "PATCH",
        &format!("/v2/dbs/workshop/doc/{}", alice),
        Some(&json!({"ops": [
            {"op": "set", "path": "score", "value": 9},
            {"op": "array_push", "path": "tags", "value": "b"},
            {"op": "remove", "path": "name"}
        ]})),
        None,
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["applied"], json!([true, true, true]));

    let (_, body) = get(p, &format!("/v2/dbs/workshop/doc/{}", alice));
    assert_eq!(body["doc"]["score"], 9);
    assert_eq!(body["doc"]["tags"], json!(["a", "b"]));
    assert!(body["doc"].get("name").is_none());

    // mget: existing ids in a map, missing ids listed - never an error.
    let (status, body) = post(
        p,
        "/v2/dbs/workshop/docs/mget",
        &json!({"ids": [alice, bob, "ghost"], "fields": ["score"]}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["docs"][&alice]["score"], 9);
    assert_eq!(body["docs"][&bob]["score"], 10);
    assert_eq!(body["missing"], json!(["ghost"]));
    assert!(
        body["docs"][&alice].get("tags").is_none(),
        "projection applies to mget"
    );

    // Delete -> 404 afterwards -> restore brings it back with its state.
    let (status, body) = request(
        p,
        "DELETE",
        &format!("/v2/dbs/workshop/doc/{}", alice),
        None,
        None,
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["deleted"], true);

    let (status, _) = get(p, &format!("/v2/dbs/workshop/doc/{}", alice));
    assert_eq!(status, 404);

    let (status, body) = post(
        p,
        &format!("/v2/dbs/workshop/doc/{}/restore", alice),
        &json!({}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["doc"]["score"], 9);
    assert_eq!(body["doc"]["tags"], json!(["a", "b"]));

    let (status, body) = request(p, "DELETE", "/v2/dbs/workshop/doc/ghost", None, None);
    assert_eq!(status, 404, "deleting a missing doc must say so: {}", body);

    // Query: filter + sort + projection + pagination.
    for (name, score) in [("carol", 1), ("dave", 5), ("erin", 8)] {
        insert(
            p,
            "workshop",
            &json!({"name": name, "score": score, "tags": []}),
        );
    }
    let (status, body) = post(
        p,
        "/v2/dbs/workshop/query",
        &json!({
            "filter": {"score": {"$gte": 5}},
            "sort": {"field": "score", "dir": "desc"},
            "fields": ["name", "score"],
            "limit": 2,
            "offset": 0
        }),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["total"], 4, "total is pre-pagination (10, 9, 8, 5)");
    assert_eq!(body["count"], 2, "count is the page size");
    assert_eq!(body["results"][0]["score"], 10);
    assert_eq!(body["results"][1]["score"], 9);
    assert!(body["results"][0].get("tags").is_none());
    assert!(
        body["took_ms"].as_f64().is_some(),
        "took_ms must be numeric"
    );

    // A limit above the default page size is allowed up to max_limit.
    let (status, body) = post(p, "/v2/dbs/workshop/query", &json!({"limit": 20000}));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["total"], 5);
    assert_eq!(body["count"], 5);

    // Above max_limit -> 400, never a silent truncation.
    let (status, body) = post(p, "/v2/dbs/workshop/query", &json!({"limit": 60000}));
    assert_eq!(status, 400, "{}", body);
    assert_eq!(body["error"], "invalid_query");

    // Invalid filter operator still fails loud.
    let (status, _) = post(
        p,
        "/v2/dbs/workshop/query",
        &json!({"filter": {"score": {"$eqq": 1}}}),
    );
    assert_eq!(status, 400);

    // Find: equality and range.
    let (status, body) = post(
        p,
        "/v2/dbs/workshop/find",
        &json!({"field": "name", "value": "dave"}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["count"], 1);
    assert!(body["took_ms"].as_f64().is_some());

    let (status, body) = post(
        p,
        "/v2/dbs/workshop/find",
        &json!({"field": "score", "min": 5, "max": 9}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["count"], 3); // 5, 8, 9

    // Facets.
    let (status, body) = post(p, "/v2/dbs/workshop/facets", &json!({"fields": ["name"]}));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["facets"]["name"]["dave"], 1);

    // Idempotent index creation.
    let (status, body) = post(
        p,
        "/v2/dbs/workshop/index",
        &json!({"field": "score", "type": "hash"}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["created"], true);
    assert_eq!(body["type"], "hash");

    let (status, body) = post(
        p,
        "/v2/dbs/workshop/index",
        &json!({"field": "score", "type": "hash"}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["created"], false, "re-creating an index must not error");

    let (status, body) = post(
        p,
        "/v2/dbs/workshop/index",
        &json!({"field": "name", "type": "btree"}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["created"], true);
    assert_eq!(body["type"], "btree");
    let (status, body) = post(
        p,
        "/v2/dbs/workshop/index",
        &json!({"field": "name", "type": "btree"}),
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["created"], false);

    let (status, _) = post(
        p,
        "/v2/dbs/workshop/index",
        &json!({"field": "x", "type": "magic"}),
    );
    assert_eq!(status, 400);

    // Full-text belongs to nVDB: neither the index type nor the route exists.
    let (status, body) = post(
        p,
        "/v2/dbs/workshop/index",
        &json!({"field": "name", "type": "text"}),
    );
    assert_eq!(status, 400, "{}", body);
    assert_eq!(body["error"], "invalid_index_type");
    let (status, _) = post(
        p,
        "/v2/dbs/workshop/search",
        &json!({"field": "name", "queries": [{"type": "term", "value": "dave"}]}),
    );
    assert_eq!(status, 404, "/search is gone; nVDB owns full-text");

    // flush is a synchronous no-drama call.
    let (status, body) = post(p, "/v2/dbs/workshop/flush", &json!({}));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["ok"], true);
}

// --- Phase 0 sync lifecycle over HTTP ------------------------------

#[test]
fn sync_lifecycle_lineage_snapshot_changes_and_compact() {
    let dir = TempDir::new().unwrap();
    let registry = registry(&[("sync", dir.path().join("data.jsonl"))]);
    let server = start(registry, server_config(None));
    let p = server.port;

    let a = insert(p, "sync", &json!({"n": 1}));
    insert(p, "sync", &json!({"n": 2}));
    let c = insert(p, "sync", &json!({"n": 3}));

    let (status, body) = get(p, "/v2/dbs/sync/lineage");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["ok"], true);
    assert_eq!(body["last_seq"], 3);
    assert_eq!(body["min_available_seq"], 1);
    assert_eq!(body["source_epoch_uuid"].as_str().unwrap().len(), 36);
    let epoch = body["source_epoch_uuid"].as_str().unwrap().to_string();

    // Snapshot: consistent full set at a watermark, paginatable.
    let (status, body) = get(p, "/v2/dbs/sync/snapshot");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["watermark_seq"], 3);
    assert_eq!(body["total"], 3);
    assert_eq!(body["count"], 3);
    assert_eq!(body["docs"].as_array().unwrap().len(), 3);
    assert_eq!(body["source_epoch_uuid"], json!(epoch));
    let ids: Vec<&str> = body["docs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["_id"].as_str().unwrap())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "snapshot docs are sorted by id");
    assert!(ids.contains(&a.as_str()));

    let (status, body) = get(p, "/v2/dbs/sync/snapshot?offset=1&limit=1");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["total"], 3);
    assert_eq!(body["count"], 1);
    assert_eq!(body["docs"].as_array().unwrap().len(), 1);

    // Changes: full feed, then a limited page, then the tail.
    let (status, body) = get(p, "/v2/dbs/sync/changes?since=0");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["from_seq"], 0);
    assert_eq!(body["to_seq"], 3);
    assert_eq!(body["last_seq"], 3);
    assert_eq!(body["has_more"], false);
    assert_eq!(body["changes"].as_array().unwrap().len(), 3);
    assert_eq!(body["changes"][0]["seq"], 1);
    assert_eq!(body["changes"][0]["kind"], "upsert");
    assert_eq!(body["changes"][0]["id"], json!(a));
    assert_eq!(body["changes"][0]["doc"]["n"], 1);

    let (status, body) = get(p, "/v2/dbs/sync/changes?since=0&limit=2");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["to_seq"], 2);
    assert_eq!(body["has_more"], true, "a short page must announce the rest");
    assert_eq!(body["changes"].as_array().unwrap().len(), 2);

    let (status, body) = get(p, "/v2/dbs/sync/changes?since=2&limit=5");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["changes"].as_array().unwrap().len(), 1);
    assert_eq!(body["has_more"], false);

    // Deletes ride the same feed.
    request(p, "DELETE", &format!("/v2/dbs/sync/doc/{}", c), None, None);
    let (_, body) = get(p, "/v2/dbs/sync/changes?since=3");
    assert_eq!(body["changes"].as_array().unwrap().len(), 1);
    assert_eq!(body["changes"][0]["kind"], "delete");
    assert_eq!(body["changes"][0]["id"], json!(c));

    // Cursor ahead: a seq this journal never issued.
    let (status, body) = get(p, "/v2/dbs/sync/changes?since=99");
    assert_eq!(status, 409, "{}", body);
    assert_eq!(body["error"], "cursor_ahead");
    assert_eq!(body["requested_seq"], 99);

    // Malformed cursors are caller errors.
    let (status, _) = get(p, "/v2/dbs/sync/changes");
    assert_eq!(status, 400, "since is required");
    let (status, _) = get(p, "/v2/dbs/sync/changes?since=abc");
    assert_eq!(status, 400);
    let (status, _) = get(p, "/v2/dbs/sync/changes?since=0&limit=0");
    assert_eq!(status, 400, "limit 0 would be a silent no-op");

    // Compaction: synchronous, identity preserved, window advanced.
    let (status, body) = post(p, "/v2/dbs/sync/compact", &json!({}));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["lineage"]["source_epoch_uuid"], json!(epoch));
    assert_eq!(body["lineage"]["last_seq"], 4);
    assert_eq!(body["lineage"]["min_available_seq"], 5);

    let (status, body) = get(p, "/v2/dbs/sync/changes?since=0");
    assert_eq!(status, 409, "{}", body);
    assert_eq!(body["error"], "cursor_expired");
    assert_eq!(body["min_available_seq"], 5);
    assert_eq!(body["last_seq"], 4);

    // The cursor at the watermark is valid and empty.
    let (status, body) = get(p, "/v2/dbs/sync/changes?since=4");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["changes"].as_array().unwrap().len(), 0);

    // Post-compact writes continue the same sequence.
    insert(p, "sync", &json!({"n": 4}));
    let (_, body) = get(p, "/v2/dbs/sync/changes?since=4");
    assert_eq!(body["changes"][0]["seq"], 5);
    assert_eq!(body["last_seq"], 5);
    assert_eq!(body["source_epoch_uuid"], json!(epoch));
}

// --- File buckets --------------------------------------------------

#[test]
fn file_bucket_put_get_list_delete_gc() {
    let dir = TempDir::new().unwrap();
    let registry = registry(&[("buckets", dir.path().join("data.jsonl"))]);
    let server = start(registry, server_config(None));
    let p = server.port;
    let files = "/v2/dbs/buckets/files";

    let payload = b"\x89PNG\r\n\x1a\nnot-really-a-png";
    let (status, _, raw_body) = raw(
        p,
        "PUT",
        &format!("{}/notepad", files),
        payload,
        &[("X-Filename", "photo.png")],
    );
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&raw_body));
    let meta: Value = serde_json::from_slice(&raw_body).unwrap();
    assert_eq!(meta["ext"], "png", "the real extension is preserved: {meta}");
    assert_eq!(meta["size"], payload.len());
    assert_eq!(meta["ok"], true);
    let hash = meta["hash"].as_str().unwrap().to_string();
    assert_eq!(meta["ref"], json!(format!("notepad:{}.png", hash)));

    // Raw bytes back, with a real content type.
    let (status, head, body) = raw(
        p,
        "GET",
        &format!("{}/notepad/{}.png", files, hash),
        &[],
        &[],
    );
    assert_eq!(status, 200);
    assert!(
        head.to_ascii_lowercase().contains("content-type: image/png"),
        "{head}"
    );
    assert_eq!(body, payload);

    // Listing shows the stored filename.
    let (status, body) = get(p, &format!("{}/notepad", files));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["files"], json!([format!("{}.png", hash)]));

    // Missing file -> 404.
    let (status, _, _) = raw(p, "GET", &format!("{}/notepad/deadbeef.png", files), &[], &[]);
    assert_eq!(status, 404);

    // Filename validation: traversal never reaches the filesystem.
    let (status, _, raw_body) = raw(
        p,
        "PUT",
        &format!("{}/notepad", files),
        b"x",
        &[("X-Filename", "../evil.png")],
    );
    assert_eq!(status, 400);
    let body: Value = serde_json::from_slice(&raw_body).unwrap();
    assert_eq!(body["error"], "invalid_filename");

    let (status, _, raw_body) = raw(p, "PUT", &format!("{}/notepad", files), b"x", &[]);
    assert_eq!(status, 400, "a filename is required");
    let body: Value = serde_json::from_slice(&raw_body).unwrap();
    assert_eq!(body["error"], "filename_required");

    // The query-parameter form works too, including percent-decoding.
    let (status, _, raw_body) = raw(
        p,
        "PUT",
        &format!("{}/notepad?filename=my%20notes.txt", files),
        b"plain text",
        &[],
    );
    assert_eq!(status, 201);
    let meta: Value = serde_json::from_slice(&raw_body).unwrap();
    assert_eq!(meta["ext"], "txt");

    // Delete releases the file (unreferenced -> moved to trash).
    let (status, body) = request(
        p,
        "DELETE",
        &format!("{}/notepad/{}.png", files, hash),
        None,
        None,
    );
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["ok"], true);
    assert_eq!(body["deleted"], true);

    let (status, _, _) = raw(
        p,
        "GET",
        &format!("{}/notepad/{}.png", files, hash),
        &[],
        &[],
    );
    assert_eq!(status, 404);

    let (status, _) = request(
        p,
        "DELETE",
        &format!("{}/notepad/{}.png", files, hash),
        None,
        None,
    );
    assert_eq!(status, 404, "deleting a missing file must say so");

    // GC moves the remaining (unreferenced) file to trash.
    let (status, body) = post(p, &format!("{}/gc", files), &json!({}));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["moved"], 1);

    let (status, body) = get(p, &format!("{}/notepad", files));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["files"], json!([]));
}

// --- Config file loader --------------------------------------------

#[test]
fn config_file_supports_shorthand_and_object_forms() {
    let text = r#"{
        "bind": "0.0.0.0",
        "port": 9000,
        "token": "t0ken",
        "persistence": "lazy",
        "scheduled_ms": 1500,
        "dir": "dbs",
        "max_limit": 1234,
        "default_limit": 10,
        "dbs": {
            "one": "a/one.jsonl",
            "two": { "path": "b/two", "persistence": "scheduled", "scheduled_ms": 250 }
        }
    }"#;
    let cfg = ServiceConfig::from_json(text).unwrap();
    assert_eq!(cfg.persistence().unwrap(), Some(PersistenceMode::Lazy));

    let mut server = ServerConfig::default();
    let dbs = cfg.apply_to(&mut server).unwrap();
    assert_eq!(server.bind, "0.0.0.0");
    assert_eq!(server.port, 9000);
    assert_eq!(server.token.as_deref(), Some("t0ken"));
    assert_eq!(server.max_limit, 1234);
    assert_eq!(server.default_limit, 10);

    assert_eq!(dbs.len(), 2, "both forms must load");
    assert_eq!(dbs[0].0, "one");
    assert_eq!(dbs[0].1, "a/one.jsonl");
    assert_eq!(dbs[0].2, None);
    assert_eq!(dbs[1].0, "two");
    assert_eq!(dbs[1].1, "b/two");
    assert_eq!(dbs[1].2, Some(PersistenceMode::Scheduled));
    assert_eq!(dbs[1].3, Some(250));

    // Unknown top-level fields are errors, not ignored.
    assert!(ServiceConfig::from_json(r#"{"bind": "x", "wut": 1}"#).is_err());
    // Unknown fields inside a database object are errors too.
    assert!(
        ServiceConfig::from_json(r#"{"dbs": {"a": {"path": "p", "colour": "red"}}}"#).is_err()
    );
    // A bad persistence spelling is refused, not defaulted.
    let bad = ServiceConfig::from_json(r#"{"persistence": "sometimes"}"#).unwrap();
    assert!(bad.persistence().is_err());
    let bad_db =
        ServiceConfig::from_json(r#"{"dbs": {"a": {"path": "p", "persistence": "sometimes"}}}"#)
            .unwrap();
    assert!(bad_db.apply_to(&mut ServerConfig::default()).is_err());
}

#[test]
fn legacy_positional_database_still_serves_single_db_routes() {
    let dir = TempDir::new().unwrap();
    let db = ndb::Database::open(dir.path().join("legacy.jsonl")).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = ServerConfig {
        port: 0,
        ..Default::default()
    };
    std::thread::spawn(move || {
        ndb::server::serve_listener(listener, db, config).unwrap();
    });

    // The request itself would fail loudly (connection refused) if the
    // listener were not up; a short pause keeps the race out of the test.
    std::thread::sleep(Duration::from_millis(50));

    let (status, body) = put(port, "/doc", &json!({"legacy": true}));
    assert_eq!(status, 201, "{}", body);
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = get(port, &format!("/doc/{}", id));
    assert_eq!(status, 200, "legacy routes must keep working: {}", body);
    assert_eq!(body["doc"]["legacy"], true);

    // /health reports the implicit default mount.
    let (status, body) = get(port, "/health");
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["dbs"], json!(["default"]));
    assert_eq!(body["allow_create"], false);

    // The same database is reachable through the v2 namespace.
    let (status, body) = get(port, &format!("/v2/dbs/default/doc/{}", id));
    assert_eq!(status, 200, "{}", body);
    assert_eq!(body["doc"]["legacy"], true);
}
