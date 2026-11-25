//! The RAM engine's HTTP read contract.
//!
//! `GET …/doc/{id}`, `POST …/docs/mget` and `POST …/query` answer RAM databases
//! from stored bytes (`Response::JsonRaw`) instead of materialising a
//! `serde_json::Value` per hit. These tests pin the wire contract to
//! `docs/contract_http-api.md`: exact document bytes, the `fields` projection,
//! `missing` accounting for `mget`, `total`/`count`/`took_ms` on `query`, and
//! the legacy single-DB routes that share the same selection code.

use ndb::server::{
    serve_multi_in_background, DbRegistry, PersistenceMode, ServerConfig, ServeHandle,
};
use ndb::{Database, RamConfig};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

// ─── Harness ────────────────────────────────────────────────────────

struct TestServer {
    port: u16,
    handle: ServeHandle,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

fn open_ram(dir: &std::path::Path) -> Database {
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

/// A RAM database mounted as `default`, so the legacy routes address it too.
fn start(db: Database) -> TestServer {
    let registry = DbRegistry::new(None, PersistenceMode::Immediate, 5_000);
    registry.add("default", db).expect("mount the RAM database");
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

/// One HTTP/1.1 request on its own connection; returns status and body bytes.
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

// ─── Tests ──────────────────────────────────────────────────────────

#[test]
fn doc_get_streams_exact_bytes_and_honours_projection() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());
    let id = db
        .insert(json!({"kind": "a", "score": 7, "tags": ["x"], "nested": {"k": "v"}}))
        .expect("insert");
    let server = start(db);

    // The raw body must be the document's canonical serialization, wrapped in
    // the contract envelope — no Value round trip on the server side.
    let expected = format!(
        "{{\"doc\":{{\"_id\":\"{id}\",\"kind\":\"a\",\"nested\":{{\"k\":\"v\"}},\"score\":7,\"tags\":[\"x\"]}},\"ok\":true}}"
    );
    let (status, body) = raw(server.port, "GET", &format!("/v2/dbs/default/doc/{id}"), &[]);
    assert_eq!(status, 200);
    assert_eq!(String::from_utf8(body).unwrap(), expected);

    let (status, body) = json_request(
        server.port,
        "GET",
        &format!("/v2/dbs/default/doc/{id}?fields=score"),
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({"ok": true, "doc": {"_id": id, "score": 7}}));

    let (status, body) = json_request(server.port, "GET", "/v2/dbs/default/doc/ghost", None);
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"], json!("not_found"));
}

#[test]
fn mget_streams_docs_and_reports_each_missing_id_once() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());
    let a = db.insert(json!({"kind": "a", "score": 7})).expect("insert a");
    let b = db.insert(json!({"kind": "b", "score": 9})).expect("insert b");
    let server = start(db);

    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/docs/mget",
        Some(&json!({"ids": [b.clone(), a.clone(), a.clone(), "ghost", "ghost"]})),
    );
    assert_eq!(status, 200, "{body}");
    let docs = body["docs"].as_object().expect("docs object");
    assert_eq!(docs.len(), 2, "each id appears once: {body}");
    assert_eq!(docs[&a], json!({"_id": a.clone(), "kind": "a", "score": 7}));
    assert_eq!(docs[&b], json!({"_id": b.clone(), "kind": "b", "score": 9}));
    assert_eq!(body["missing"], json!(["ghost"]));
    assert_eq!(body["ok"], json!(true));

    // A projected mget keeps the projection (it does not take the raw path).
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/docs/mget",
        Some(&json!({"ids": [a.clone()], "fields": ["score"]})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["docs"][&a], json!({"_id": a, "score": 7}));

    // An id carrying a JSON-special character still yields a well-formed body:
    // the raw path escapes it exactly as serde_json would.
    let (status, body) = json_request(
        server.port,
        "PUT",
        "/v2/dbs/default/doc?prefix=ab%22c%5C",
        Some(&json!({"kind": "quoted"})),
    );
    assert_eq!(status, 201, "{body}");
    let weird = body["id"].as_str().expect("id").to_string();
    assert!(weird.starts_with("ab\"c\\"), "prefix applied: {weird}");
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/docs/mget",
        Some(&json!({"ids": [weird.clone(), weird.clone()]})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["docs"][&weird]["kind"], json!("quoted"));
    assert_eq!(body["missing"], json!([]));
}

#[test]
fn query_projects_and_streams_with_the_same_pagination() {
    let dir = TempDir::new().unwrap();
    let db = open_ram(dir.path());
    let a = db.insert(json!({"kind": "a", "score": 7})).expect("insert a");
    let b = db.insert(json!({"kind": "a", "score": 9})).expect("insert b");
    let server = start(db);

    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/query",
        Some(&json!({"filter": {"kind": "a"}, "fields": ["score"], "sort": {"field": "score", "dir": "asc"}})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], json!(2));
    assert_eq!(body["count"], json!(2));
    assert!(
        body["took_ms"].is_number(),
        "the streamed body still carries took_ms: {body}"
    );
    assert_eq!(
        body["results"],
        json!([{"_id": a.clone(), "score": 7}, {"_id": b.clone(), "score": 9}])
    );

    // Unprojected: the results are the full documents.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/v2/dbs/default/query",
        Some(&json!({"filter": {"kind": "a"}, "limit": 1, "sort": {"field": "score", "dir": "asc"}})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], json!(2));
    assert_eq!(body["count"], json!(1));
    assert_eq!(
        body["results"],
        json!([{"_id": a.clone(), "kind": "a", "score": 7}])
    );

    // Legacy single-DB route: same selection, same projection, no took_ms.
    let (status, body) = json_request(
        server.port,
        "POST",
        "/query",
        Some(&json!({"filter": {"kind": "a"}, "fields": ["score"], "sort": {"field": "score", "dir": "desc"}})),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], json!(2));
    assert_eq!(
        body["results"],
        json!([{"_id": b, "score": 9}, {"_id": a, "score": 7}])
    );
    assert!(body.get("took_ms").is_none(), "legacy body has no took_ms");
}
