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

// ─── Bucket release on delete (#18) ─────────────────────────────────────────

/// Store one file; returns its `hash.ext` filename and its compact nURI ref.
fn store_file(port: u16, bucket: &str, filename: &str, bytes: &[u8]) -> (String, String) {
    let path = format!("/v2/dbs/default/files/{bucket}?filename={filename}");
    let (status, body) = raw(port, "PUT", &path, bytes);
    let body: Value = serde_json::from_slice(&body).expect("store response is JSON");
    assert_eq!(status, 201, "{body}");
    let hash = body["hash"].as_str().expect("hash").to_string();
    let ext = body["ext"].as_str().expect("ext").to_string();
    let reference = body["ref"].as_str().expect("ref").to_string();
    (format!("{hash}.{ext}"), reference)
}

fn insert_doc_with(port: u16, body: &Value) -> String {
    let (status, response) = json_request(port, "PUT", "/v2/dbs/default/doc", Some(body));
    assert_eq!(status, 201, "{response}");
    response["id"].as_str().expect("id").to_string()
}

/// Status of a bucket file read — the body is the file's bytes, not JSON.
fn file_status(port: u16, path: &str) -> u16 {
    raw(port, "GET", path, &[]).0
}

/// Deleting a document on the RAM engine releases the bucket files only it
/// referenced — the exact behaviour the v2 engine has (#18).
///
/// The order is the contract: a file another live document still holds stays,
/// and the delete that removes the last holder moves the orphan out of the
/// bucket in the same call, so the bytes are gone without a manual `POST gc`.
#[test]
fn deleting_a_ram_document_releases_its_unreferenced_bucket_files() {
    let dir = TempDir::new().expect("temp dir");
    let server = start(open_ram(dir.path()));
    let port = server.port;

    let (filename, reference) = store_file(port, "notepad", "note.txt", b"attachment bytes");
    let file_path = format!("/v2/dbs/default/files/notepad/{filename}");

    let first = insert_doc_with(port, &json!({"title": "one", "attach": reference.clone()}));
    let second = insert_doc_with(port, &json!({"title": "two", "attach": reference}));

    let (status, body) = json_request(port, "DELETE", &format!("/v2/dbs/default/doc/{first}"), None);
    assert_eq!(status, 200, "delete of a live document: {body}");
    assert_eq!(
        file_status(port, &file_path),
        200,
        "a file another live document still holds stays"
    );

    let (status, body) = json_request(
        port,
        "DELETE",
        &format!("/v2/dbs/default/files/notepad/{filename}"),
        None,
    );
    assert_eq!(status, 409, "a live reference is still a reference: {body}");
    assert_eq!(body["error"], "file_referenced");

    let (status, body) = json_request(port, "DELETE", &format!("/v2/dbs/default/doc/{second}"), None);
    assert_eq!(status, 200, "delete of the last holding document: {body}");

    assert_eq!(
        file_status(port, &file_path),
        404,
        "the released attachment is no longer served"
    );

    // The orphan left the active bucket, so a global gc has nothing left to do.
    let (status, body) = json_request(port, "POST", "/v2/dbs/default/files/gc", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["moved"], json!(0), "the delete already released it: {body}");
}

/// A RAM update that drops an attachment releases the file, exactly like the
/// v1/v2 update path: the counters are maintained, not only rebuilt on delete.
#[test]
fn a_ram_update_that_drops_a_ref_releases_the_file() {
    let dir = TempDir::new().expect("temp dir");
    let server = start(open_ram(dir.path()));
    let port = server.port;

    let (filename, reference) = store_file(port, "notepad", "note.txt", b"attachment bytes");
    let file_path = format!("/v2/dbs/default/files/notepad/{filename}");
    let id = insert_doc_with(port, &json!({"attach": reference}));

    let (status, body) = json_request(
        port,
        "PUT",
        &format!("/v2/dbs/default/doc/{id}"),
        Some(&json!({"title": "no attachment any more"})),
    );
    assert_eq!(status, 200, "replace the document: {body}");

    assert_eq!(
        file_status(port, &file_path),
        404,
        "the dropped ref released the file"
    );
}

/// `DELETE /files` on a RAM database answers `200` for a file nothing
/// references — no blanket `409 file_referenced` (#18).
#[test]
fn deleting_an_unreferenced_file_on_ram_succeeds() {
    let dir = TempDir::new().expect("temp dir");
    let server = start(open_ram(dir.path()));
    let port = server.port;

    let (filename, _) = store_file(port, "notepad", "orphan.txt", b"nobody claims me");
    let file_path = format!("/v2/dbs/default/files/notepad/{filename}");

    let (status, body) = json_request(port, "DELETE", &file_path, None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], json!(true));

    assert_eq!(file_status(port, &file_path), 404, "the file is gone");
}

/// A pinned snapshot releases its pin as soon as it serves the page that
/// completes the read: a finished read must not reserve change-ring history for
/// the rest of the TTL, and a snapshot that fits in one page must not linger at
/// all.
#[test]
fn a_snapshot_releases_its_pin_once_the_last_page_is_served() {
    let dir = TempDir::new().expect("temp dir");
    let db = open_ram(dir.path());
    for n in 0..3 {
        db.insert(json!({"n": n})).expect("insert");
    }
    let server = start(db);
    let port = server.port;

    // Three docs, page size 2: the first page is not the last, so it stays.
    let (status, first) = json_request(port, "GET", "/v2/dbs/default/snapshot?offset=0&limit=2", None);
    assert_eq!(status, 200, "{first}");
    let snapshot_id = first["snapshot_id"].as_str().expect("pinned").to_string();
    assert_eq!(first["count"], json!(2));

    let (status, second) = json_request(
        port,
        "GET",
        &format!("/v2/dbs/default/snapshot?snapshot_id={snapshot_id}&offset=2&limit=2"),
        None,
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["count"], json!(1), "the final page");

    let (status, body) = json_request(
        port,
        "GET",
        &format!("/v2/dbs/default/snapshot?snapshot_id={snapshot_id}&offset=0&limit=2"),
        None,
    );
    assert_eq!(status, 410, "a finished snapshot is no longer addressable: {body}");
    assert_eq!(body["error"], "snapshot_expired");

    // Every document fits in one page: complete on its first response.
    let (status, single) = json_request(port, "GET", "/v2/dbs/default/snapshot?offset=0&limit=50", None);
    assert_eq!(status, 200, "{single}");
    let single_id = single["snapshot_id"].as_str().expect("pinned").to_string();
    assert_eq!(single["count"], json!(3));
    let (status, body) = json_request(
        port,
        "GET",
        &format!("/v2/dbs/default/snapshot?snapshot_id={single_id}&offset=0&limit=50"),
        None,
    );
    assert_eq!(status, 410, "a single-page snapshot leaves no pin behind: {body}");
}
