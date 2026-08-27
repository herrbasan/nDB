//! Integration tests for nDB Phase 7: HTTP server (`ndb serve`)
//!
//! Each test binds an ephemeral port, spawns the server on a background
//! thread, and speaks raw HTTP/1.1 over TCP — the same wire a Node client
//! would use. Design: docs/ndb-serve-design.md.

use ndb::server::{serve_listener, ServerConfig};
use ndb::Database;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use tempfile::TempDir;

fn start_server(token: Option<String>) -> (u16, Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("serve.jsonl")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = ServerConfig { token, ..Default::default() };
    std::thread::spawn(move || {
        serve_listener(listener, db, cfg).unwrap();
    });
    (port, Database::open(dir.path().join("serve.jsonl")).unwrap(), dir)
    // NOTE: second open is a separate handle; tests that assert persisted state
    // use it read-only after writes go through the server.
}

/// Minimal HTTP/1.1 client: one request per connection.
fn request(port: u16, method: &str, path: &str, body: Option<&Value>, token: Option<&str>) -> (u16, Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let body_bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
        method, path, body_bytes.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {}\r\n", t));
    }
    if !body_bytes.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(&body_bytes).unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body_start = text.find("\r\n\r\n").unwrap() + 4;
    let body: Value = serde_json::from_str(&text[body_start..]).unwrap_or(Value::Null);
    (status, body)
}

// ─── Auth ───────────────────────────────────────────────────────────

#[test]
fn token_required_when_configured() {
    let (port, _db, _dir) = start_server(Some("secret123".to_string()));

    let (status, _) = request(port, "GET", "/stats", None, None);
    assert_eq!(status, 401, "no token must be rejected");

    let (status, _) = request(port, "GET", "/stats", None, Some("wrong"));
    assert_eq!(status, 401, "wrong token must be rejected");

    let (status, body) = request(port, "GET", "/stats", None, Some("secret123"));
    assert_eq!(status, 200);
    assert_eq!(body["ok"], json!(true));
}

#[test]
fn no_token_needed_when_unset() {
    let (port, _db, _dir) = start_server(None);
    let (status, _) = request(port, "GET", "/stats", None, None);
    assert_eq!(status, 200);
}

// ─── CRUD ───────────────────────────────────────────────────────────

#[test]
fn insert_get_delete_roundtrip() {
    let (port, _db, _dir) = start_server(None);

    let (status, body) = request(port, "PUT", "/doc", Some(&json!({"name": "alice", "age": 30})), None);
    assert_eq!(status, 201, "insert: {}", body);
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = request(port, "GET", &format!("/doc/{}", id), None, None);
    assert_eq!(status, 200);
    assert_eq!(body["doc"]["name"], "alice");

    // get with projection
    let (status, body) = request(port, "GET", &format!("/doc/{}?fields=name", id), None, None);
    assert_eq!(status, 200);
    assert_eq!(body["doc"]["name"], "alice");
    assert!(body["doc"].get("age").is_none(), "age must not be projected");
    assert_eq!(body["doc"]["_id"], json!(id));

    // missing doc: fail loud
    let (status, _) = request(port, "GET", "/doc/nonexistent", None, None);
    assert_eq!(status, 500, "get on missing id must surface the core error");

    let (status, body) = request(port, "DELETE", &format!("/doc/{}", id), None, None);
    assert_eq!(status, 200, "delete: {}", body);

    let (status, _) = request(port, "GET", &format!("/doc/{}", id), None, None);
    assert_eq!(status, 500, "deleted doc must not be retrievable");
}

#[test]
fn patch_delta_ops() {
    let (port, _db, _dir) = start_server(None);
    let (_, body) = request(port, "PUT", "/doc", Some(&json!({"status": "pending", "tags": ["a"]})), None);
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = request(
        port,
        "PATCH",
        &format!("/doc/{}", id),
        Some(&json!({"ops": [
            {"op": "set", "path": "status", "value": "done"},
            {"op": "array_push", "path": "tags", "value": "b"},
            {"op": "remove", "path": "status"}
        ]})),
        None,
    );
    assert_eq!(status, 200, "patch: {}", body);
    assert_eq!(body["applied"], json!([true, true, true]), "all three ops must apply");

    let (_, body) = request(port, "GET", &format!("/doc/{}", id), None, None);
    assert_eq!(body["doc"]["tags"], json!(["a", "b"]));
    assert!(body["doc"].get("status").is_none());

    // Unresolvable path surfaces as applied=false — silent no-ops are visible.
    let (status, body) = request(
        port,
        "PATCH",
        &format!("/doc/{}", id),
        Some(&json!({"ops": [{"op": "set", "path": "no.such.path.here", "value": 1}]})),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(body["applied"], json!([false]), "unresolvable path must report applied=false");

    // Unknown op name: 400
    let (status, _) = request(
        port,
        "PATCH",
        &format!("/doc/{}", id),
        Some(&json!({"ops": [{"op": "increment", "path": "n"}]})),
        None,
    );
    assert_eq!(status, 400);
}

// ─── Query ──────────────────────────────────────────────────────────

#[test]
fn query_with_projection_sort_limit() {
    let (port, _db, _dir) = start_server(None);
    for (name, score) in [("a", 3), ("b", 10), ("c", 1)] {
        request(port, "PUT", "/doc", Some(&json!({"name": name, "score": score, "meta": {"views": score * 2}})), None);
    }

    let (status, body) = request(
        port,
        "POST",
        "/query",
        Some(&json!({
            "filter": {"meta.views": {"$gte": 2}},
            "fields": ["name", "meta.views"],
            "sort": {"field": "meta.views", "dir": "desc"}
        })),
        None,
    );
    assert_eq!(status, 200, "query: {}", body);
    assert_eq!(body["count"], 3);
    assert_eq!(body["results"][0]["name"], "b", "nested sort key must work over HTTP");
    assert!(body["results"][0].get("score").is_none(), "unprojected field must be absent");

    // Default limit: unfiltered query never streams the whole DB.
    let (_, body) = request(port, "POST", "/query", Some(&json!({})), None);
    assert_eq!(body["count"], 3);

    // Explicit limit + offset
    let (_, body) = request(
        port,
        "POST",
        "/query",
        Some(&json!({"sort": {"field": "score", "dir": "asc"}, "limit": 1, "offset": 1})),
        None,
    );
    assert_eq!(body["count"], 1);
    assert_eq!(body["results"][0]["name"], "a");
}

#[test]
fn query_rejects_unknown_operator() {
    let (port, _db, _dir) = start_server(None);
    let (status, body) = request(
        port,
        "POST",
        "/query",
        Some(&json!({"filter": {"status": {"$eqq": "typo"}}})),
        None,
    );
    assert_eq!(status, 400, "unknown operator must 400: {}", body);
}

#[test]
fn find_endpoint() {
    let (port, _db, _dir) = start_server(None);
    request(port, "PUT", "/doc", Some(&json!({"status": "active", "n": 5})), None);
    request(port, "PUT", "/doc", Some(&json!({"status": "active", "n": 9})), None);

    let (status, body) = request(port, "POST", "/find", Some(&json!({"field": "status", "value": "active"})), None);
    assert_eq!(status, 200);
    assert_eq!(body["count"], 2);

    let (status, body) = request(port, "POST", "/find", Some(&json!({"field": "n", "min": 6, "max": 10})), None);
    assert_eq!(status, 200);
    assert_eq!(body["count"], 1);
}

// ─── Ops & errors ───────────────────────────────────────────────────

#[test]
fn index_and_compact_accepted() {
    let (port, _db, _dir) = start_server(None);
    request(port, "PUT", "/doc", Some(&json!({"status": "x"})), None);

    let (status, body) = request(port, "POST", "/index", Some(&json!({"field": "status", "type": "hash"})), None);
    assert_eq!(status, 201, "index: {}", body);

    let (status, body) = request(port, "POST", "/compact", None, None);
    assert_eq!(status, 202, "compact must be accepted-async: {}", body);
}

#[test]
fn unknown_route_404() {
    let (port, _db, _dir) = start_server(None);
    let (status, body) = request(port, "GET", "/nope", None, None);
    assert_eq!(status, 404);
    assert!(body["error"].is_string());
}

#[test]
fn invalid_json_body_400() {
    let (port, _db, _dir) = start_server(None);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let payload = "{not json";
    let req = format!(
        "POST /query HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(), payload
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert_eq!(status, 400);
}

/// Concurrent readers while writing — the RwLock model over HTTP.
#[test]
fn concurrent_readers_during_writes() {
    let (port, _db, _dir) = start_server(None);
    for i in 0..20 {
        request(port, "PUT", "/doc", Some(&json!({"i": i, "status": "warm"})), None);
    }

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let p = port;
            std::thread::spawn(move || {
                for _ in 0..20 {
                    let (status, _) = request(p, "POST", "/query", Some(&json!({"filter": {"status": "warm"}})), None);
                    assert_eq!(status, 200);
                }
            })
        })
        .collect();
    let writer = std::thread::spawn(move || {
        for i in 20..40 {
            request(port, "PUT", "/doc", Some(&json!({"i": i, "status": "warm"})), None);
        }
    });

    for r in readers {
        r.join().unwrap();
    }
    writer.join().unwrap();

    let (_, body) = request_on_new(port, "POST", "/query", &json!({}));
    assert_eq!(body["count"], 40, "all writes must land");
}

fn request_on_new(port: u16, method: &str, path: &str, body: &Value) -> (u16, Value) {
    request(port, method, path, Some(body), None)
}
