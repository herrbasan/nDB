//! Phase 14 — durability & zero-data-loss hardening (Phase 1b, slice 1).
//!
//! One test per audit finding, each proving the *end state* the operator
//! cares about:
//!
//! - C1  torn journal tail → truncated at open, next write survives a reopen
//! - M1  mid-journal corruption → open fails loud, `ndb repair` recovers
//! - C2  one OS writer lock per journal
//! - C3  transactional `apply_ops` + optimistic concurrency
//! - C4  every bucket ref form counts; a referenced file is never deleted
//! - H3  `store_file` serializes against GC
//! - H4  full SHA-256 ids, verified on dedup
//! - C5  the doc trash record is durable before the tombstone
//! - Windows names, mget cap, pinned snapshot pagination

use ndb::server::{serve_multi_in_background, DbRegistry, PersistenceMode, ServeHandle, ServerConfig};
use ndb::{Database, Error};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
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

fn registry(dirs: &[(&str, std::path::PathBuf)]) -> DbRegistry {
    let reg = DbRegistry::new(None, PersistenceMode::Immediate, 5000);
    for (name, path) in dirs {
        reg.mount(name, path).unwrap();
    }
    reg
}

fn start(registry: DbRegistry) -> TestServer {
    let config = ServerConfig {
        port: 0,
        ..Default::default()
    };
    let handle = serve_multi_in_background(Arc::new(registry), config).unwrap();
    TestServer {
        port: handle.addr.port(),
        handle,
    }
}

fn raw(
    port: u16,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> (u16, Vec<u8>) {
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

    // Read the response to its Content-Length boundary instead of to EOF: a
    // loaded Windows loopback occasionally reports the peer's close as a reset
    // *after* the complete response has arrived, and `read_to_end` would turn
    // that into a spurious failure. An incomplete response is still fatal.
    let mut response = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(end) = complete_response(&response) {
            return (end.0, end.1);
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                if let Some(end) = complete_response(&response) {
                    return end;
                }
                panic!("{method} {path}: response cut short by a reset: {e}");
            }
            Err(e) => panic!("{method} {path}: {e}"),
        }
    }
    complete_response(&response).unwrap_or_else(|| panic!("{method} {path}: no complete response"))
}

/// `(status, body)` once the header block and the full Content-Length body are
/// present, `None` while the response is still incomplete.
fn complete_response(bytes: &[u8]) -> Option<(u16, Vec<u8>)> {
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&bytes[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse().ok()
            } else {
                None
            }
        })?;
    let body = &bytes[split + 4..];
    if body.len() < content_length {
        return None;
    }
    Some((status, body[..content_length].to_vec()))
}

fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(&str, &str)],
) -> (u16, Value) {
    let bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
    let mut all: Vec<(&str, &str)> = Vec::new();
    if body.is_some() {
        all.push(("Content-Type", "application/json"));
    }
    all.extend_from_slice(headers);
    let (status, raw_body) = raw(port, method, path, &bytes, &all);
    (status, serde_json::from_slice(&raw_body).unwrap_or(Value::Null))
}

fn get(port: u16, path: &str) -> (u16, Value) {
    request(port, "GET", path, None, &[])
}

fn post(port: u16, path: &str, body: &Value) -> (u16, Value) {
    request(port, "POST", path, Some(body), &[])
}

fn put(port: u16, path: &str, body: &Value) -> (u16, Value) {
    request(port, "PUT", path, Some(body), &[])
}

fn insert(port: u16, db: &str, doc: &Value) -> (String, u64) {
    let (status, body) = put(port, &format!("/v2/dbs/{db}/doc"), doc);
    assert_eq!(status, 201, "insert: {body}");
    (
        body["id"].as_str().unwrap().to_string(),
        body["seq"].as_u64().unwrap(),
    )
}

// ─── C1: torn tail ──────────────────────────────────────────────────

#[test]
fn c1_torn_tail_is_truncated_and_the_next_insert_survives_a_second_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.jsonl");

    let first = {
        let db = Database::open(&path).unwrap();
        db.insert(json!({"v": 1})).unwrap()
    };

    // Crash mid-write: a final line with no terminator and no valid JSON.
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    write!(file, "{}", r#"{"_id":"torn","v":999"#).unwrap();
    drop(file);

    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 1);
    assert!(db.get("torn").is_err());

    // The corrupt bytes are gone, so this append lands on a clean boundary…
    let second = db.insert(json!({"v": 2})).unwrap();
    drop(db);

    // …and a second reopen still sees both records (C1's actual failure mode
    // was the next line being glued onto the torn bytes and lost).
    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.len(), 2);
    assert_eq!(reopened.get(&first).unwrap()["v"], 1);
    assert_eq!(reopened.get(&second).unwrap()["v"], 2);
}

// ─── M1: mid-journal corruption + ndb repair ────────────────────────

#[test]
fn m1_mid_journal_corruption_fails_loud_and_repair_recovers() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.jsonl");
    {
        let db = Database::open(&path).unwrap();
        db.insert(json!({"v": 1})).unwrap();
        db.insert(json!({"v": 2})).unwrap();
    }

    // A hole in the middle, with valid records after it.
    let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, r#"{{"_id":"broken","v":3"#).unwrap();
    writeln!(file, r#"{{"_id":"after","v":4}}"#).unwrap();
    drop(file);

    match Database::open(&path).err().expect("open must refuse") {
        Error::Corruption { message, .. } => {
            assert!(message.contains("ndb repair"), "{message}");
        }
        other => panic!("expected Corruption, got {other:?}"),
    }

    let out = Command::new(env!("CARGO_BIN_EXE_ndb"))
        .args(["repair", dir.path().to_str().unwrap()])
        .output()
        .expect("spawn ndb repair");
    assert!(
        out.status.success(),
        "repair failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(printed.contains("Preserved: 2 record(s)"), "{printed}");

    let backups: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".corrupt."))
        .collect();
    assert_eq!(backups.len(), 1, "repair must quarantine the original bytes");

    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 2, "repaired journal keeps every complete record");
    let id = db.insert(json!({"v": 5})).unwrap();
    drop(db);
    let reopened = Database::open(&path).unwrap();
    assert!(reopened.get(&id).is_ok());
}

// ─── C2: exclusive writer lock ──────────────────────────────────────

#[test]
fn c2_a_second_open_fails_while_the_first_holds_the_lock() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("locked.jsonl");

    let db = Database::open(&path).unwrap();
    match Database::open(&path).err().expect("second open must fail") {
        Error::DatabaseLocked { .. } => {}
        other => panic!("expected DatabaseLocked, got {other:?}"),
    }

    // Dropping the owner releases the OS lock immediately — no stale file.
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.len(), 0);
}

#[test]
fn c2_in_memory_databases_take_no_file_lock() {
    let db = Database::open_in_memory().unwrap();
    let other = Database::open_in_memory().unwrap();
    db.insert(json!({"a": 1})).unwrap();
    other.insert(json!({"b": 2})).unwrap();
    assert_eq!(db.len(), 1);
    assert_eq!(other.len(), 1);
}

// ─── C3: transactional apply_ops + optimistic concurrency ────────────

#[test]
fn c3_patch_is_all_or_nothing_and_if_seq_guards_concurrent_edits() {
    let dir = TempDir::new().unwrap();
    // Mounted as `default` so both the v1 and v2 routes address it.
    let server = start(registry(&[("default", dir.path().join("data.jsonl"))]));
    let p = server.port;
    let db = "default";

    let (id, seq) = insert(p, db, &json!({"status": "pending", "tags": ["a"], "n": 1}));

    // Op 3 is malformed: the whole batch is a 400 and ops 1–2 never landed.
    let (status, body) = request(
        p,
        "PATCH",
        &format!("/v2/dbs/{db}/doc/{id}"),
        Some(&json!({"ops": [
            {"op": "set", "path": "status", "value": "done"},
            {"op": "array_push", "path": "tags", "value": "b"},
            {"op": "bogus", "path": "n"}
        ]})),
        &[],
    );
    assert_eq!(status, 400, "{body}");
    let (_, body) = get(p, &format!("/v2/dbs/{db}/doc/{id}"));
    assert_eq!(body["doc"]["status"], "pending", "op 1 must not be durable");
    assert_eq!(body["doc"]["tags"], json!(["a"]), "op 2 must not be durable");

    // A stale token conflicts and changes nothing.
    let (status, body) = request(
        p,
        "PATCH",
        &format!("/v2/dbs/{db}/doc/{id}"),
        Some(&json!({"ops": [{"op": "set", "path": "status", "value": "done"}]})),
        &[("If-Match", &(seq - 1).to_string())],
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "conflict");
    assert_eq!(body["current_seq"], json!(seq));
    assert_eq!(body["doc"]["status"], json!("pending"));

    // The current token succeeds and reports the new seq.
    let (status, body) = request(
        p,
        "PATCH",
        &format!("/v2/dbs/{db}/doc/{id}"),
        Some(&json!({"ops": [
            {"op": "set", "path": "status", "value": "done"},
            {"op": "array_push", "path": "tags", "value": "b"}
        ]})),
        &[("If-Match", &seq.to_string())],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["applied"], json!([true, true]));
    let new_seq = body["seq"].as_u64().unwrap();
    assert!(new_seq > seq);
    assert_eq!(body["doc"]["status"], "done");
    assert_eq!(body["doc"]["tags"], json!(["a", "b"]));

    // `?if_seq=` is equivalent, and a PUT carries the same guarantee.
    let (status, body) = put(p, &format!("/v2/dbs/{db}/doc/{id}?if_seq={new_seq}"), &json!({"n": 2}));
    assert_eq!(status, 200, "{body}");
    assert!(body["seq"].as_u64().unwrap() > new_seq);
    let (status, body) = put(p, &format!("/v2/dbs/{db}/doc/{id}?if_seq={new_seq}"), &json!({"n": 3}));
    assert_eq!(status, 409, "{body}");

    // v1 PATCH uses the same engine.
    let (status, body) = request(
        p,
        "PATCH",
        &format!("/doc/{id}"),
        Some(&json!({"ops": [{"op": "set", "path": "n", "value": 9}]})),
        &[],
    );
    assert_eq!(status, 200, "v1 patch: {body}");
    assert!(body["seq"].as_u64().is_some());
}

// ─── C4 + H3 + H4: bucket refs, GC, dedup verification ──────────────

#[test]
fn c4_gc_and_delete_protect_a_file_referenced_by_v2_url_form() {
    let dir = TempDir::new().unwrap();
    let server = start(registry(&[("b", dir.path().join("data.jsonl"))]));
    let p = server.port;
    let payload = b"referenced bytes";

    let (status, raw_body) = raw(
        p,
        "PUT",
        "/v2/dbs/b/files/images",
        payload,
        &[("X-Filename", "pic.txt")],
    );
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&raw_body));
    let meta: Value = serde_json::from_slice(&raw_body).unwrap();
    let hash = meta["hash"].as_str().unwrap().to_string();
    assert_eq!(hash.len(), 64, "content ids are full SHA-256 (H4)");

    // The document stores the v2 URL form, not the compact nURI — the form the
    // old `normalize_file_ref` ignored (C4).
    let url = format!("/v2/dbs/b/files/images/{hash}.txt");
    insert(p, "b", &json!({"attachment": url}));

    // H3: GC and store serialize on the writer lock; GC sees the reference.
    let (status, body) = post(p, "/v2/dbs/b/files/gc", &json!({}));
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["moved"], 0, "a live reference must survive GC");
    let (status, _) = get(p, &format!("/v2/dbs/b/files/images/{hash}.txt"));
    assert_eq!(status, 200, "file must still be readable");

    // Deleting a referenced file is refused with the machine-readable code.
    let (status, body) = request(
        p,
        "DELETE",
        &format!("/v2/dbs/b/files/images/{hash}.txt"),
        None,
        &[],
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "file_referenced");

    // Force is the caller's explicit override.
    let (status, body) = request(
        p,
        "DELETE",
        &format!("/v2/dbs/b/files/images/{hash}.txt?force=true"),
        None,
        &[],
    );
    assert_eq!(status, 200, "{body}");
}

#[test]
fn h4_a_damaged_stored_file_is_detected_on_re_store() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    let payload = b"the real content";

    let meta = db.store_file("images", payload, "note.txt").unwrap();
    let stored = dir
        .path()
        .join("_files")
        .join("images")
        .join(meta._file.filename());
    assert!(stored.exists());
    assert_eq!(meta._file.id.len(), 64);

    // Power loss truncates the blob.
    fs::write(&stored, b"the rea").unwrap();

    // A dedup hit must never hand back the damaged bytes.
    match db.store_file("images", payload, "note.txt").err() {
        Some(Error::Corruption { message, .. }) => {
            assert!(message.contains("damaged"), "{message}");
        }
        other => panic!("expected Corruption, got {other:?}"),
    }
}

// ─── C5: durable document trash ─────────────────────────────────────

#[test]
fn c5_the_restorable_trash_record_is_on_disk_after_delete() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("data.jsonl");
    let db = Database::open(&path).unwrap();

    let id = db.insert(json!({"keep": "me"})).unwrap();
    db.delete(&id).unwrap();

    let trash = dir.path().join("_trash").join("docs").join("data.jsonl");
    assert!(trash.exists(), "delete writes a restorable record");
    let content = fs::read_to_string(&trash).unwrap();
    assert!(
        content
            .lines()
            .any(|l| l.contains(&id) && l.contains("\"keep\":\"me\"")),
        "the record must carry the document: {content}"
    );

    // …and restore still finds it.
    db.restore(&id).unwrap();
    assert_eq!(db.get(&id).unwrap()["keep"], "me");
}

// ─── Windows mount guardrails ───────────────────────────────────────

#[test]
fn windows_reserved_and_case_folded_database_names_are_refused() {
    let root = TempDir::new().unwrap();
    let reg = DbRegistry::new(
        Some(root.path().to_path_buf()),
        PersistenceMode::Immediate,
        5000,
    );
    reg.create("Mem", None).unwrap();

    let server = start(reg);
    let p = server.port;

    for bad in ["CON", "nul", "COM1", "lpt9", "con.txt", "aux.log"] {
        let (status, body) = post(p, "/v2/dbs", &json!({"name": bad}));
        assert_eq!(status, 400, "reserved name {bad} must be 400: {body}");
    }
    for bad in ["trailing.", "trailing "] {
        let (status, body) = post(p, "/v2/dbs", &json!({"name": bad}));
        assert_eq!(status, 400, "name {bad:?} must be 400: {body}");
    }

    // Case-folded collision: one folder, one journal, one writer.
    let (status, body) = post(p, "/v2/dbs", &json!({"name": "mem"}));
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"], "already_exists");
}

// ─── mget cap + dedup ───────────────────────────────────────────────

#[test]
fn mget_is_capped_and_deduplicated() {
    let dir = TempDir::new().unwrap();
    let server = start(registry(&[("m", dir.path().join("data.jsonl"))]));
    let p = server.port;

    let (a, _) = insert(p, "m", &json!({"v": 1}));

    let ids: Vec<String> = (0..1_001).map(|i| format!("id-{i}")).collect();
    let (status, body) = post(p, "/v2/dbs/m/docs/mget", &json!({"ids": ids}));
    assert_eq!(status, 413, "{body}");
    assert_eq!(body["error"], "payload_too_large");

    let (status, body) = post(
        p,
        "/v2/dbs/m/docs/mget",
        &json!({"ids": [a, a, "ghost", "ghost"]}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["docs"].as_object().unwrap().len(), 1);
    assert_eq!(body["missing"], json!(["ghost"]), "each id is reported once");
}

// ─── Pinned snapshot pagination ─────────────────────────────────────

#[test]
fn snapshot_pagination_is_pinned_across_concurrent_writes() {
    let dir = TempDir::new().unwrap();
    let server = start(registry(&[("s", dir.path().join("data.jsonl"))]));
    let p = server.port;

    for n in 0..3 {
        insert(p, "s", &json!({"n": n}));
    }

    let (status, first) = get(p, "/v2/dbs/s/snapshot?offset=0&limit=2");
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["total"], 3);
    assert_eq!(first["count"], 2);
    let snapshot_id = first["snapshot_id"]
        .as_str()
        .expect("paginated read is pinned")
        .to_string();
    let watermark = first["watermark_seq"].clone();

    // Writes land between pages; the pinned read must not drift.
    insert(p, "s", &json!({"n": 100}));
    insert(p, "s", &json!({"n": 101}));

    let (status, second) = get(
        p,
        &format!("/v2/dbs/s/snapshot?snapshot_id={snapshot_id}&offset=2&limit=2"),
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["total"], 3, "the pinned snapshot is frozen");
    assert_eq!(second["count"], 1);
    assert_eq!(second["watermark_seq"], watermark);

    // Unknown / foreign ids are a loud 410, never a silent fresh snapshot.
    let (status, body) = get(p, "/v2/dbs/s/snapshot?snapshot_id=not-a-real-id");
    assert_eq!(status, 410, "{body}");
    assert_eq!(body["error"], "snapshot_expired");
}

// ─── Upload filename guardrails ─────────────────────────────────────

#[test]
fn upload_filenames_and_extensions_are_validated() {
    let dir = TempDir::new().unwrap();
    let server = start(registry(&[("f", dir.path().join("data.jsonl"))]));
    let p = server.port;

    for bad in ["../../evil.sh", "no/dirs.txt", "payload.superlongext", "a.b-c"] {
        let (status, raw_body) = raw(
            p,
            "PUT",
            "/v2/dbs/f/files/images",
            b"x",
            &[("X-Filename", bad)],
        );
        assert_eq!(
            status,
            400,
            "filename {bad:?} must be 400: {}",
            String::from_utf8_lossy(&raw_body)
        );
    }

    // A normal name (and a bare name with no extension) still works.
    for ok in ["notes.txt", "README"] {
        let (status, raw_body) = raw(
            p,
            "PUT",
            "/v2/dbs/f/files/images",
            b"payload",
            &[("X-Filename", ok)],
        );
        assert_eq!(status, 201, "filename {ok:?}: {}", String::from_utf8_lossy(&raw_body));
    }
}

// ─── H2: Scheduled persistence is a real flusher ────────────────────

#[test]
fn scheduled_persistence_keeps_every_write_and_flushes_on_drop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("scheduled.jsonl");

    let ids = {
        let db = Database::open(&path)
            .unwrap()
            .with_persistence(ndb::Persistence::Scheduled(Duration::from_millis(20)));
        let mut ids = Vec::new();
        for n in 0..20 {
            ids.push(db.insert(json!({"n": n})).unwrap());
        }
        // The background thread runs while the handle lives; Drop must flush
        // whatever is still dirty.
        std::thread::sleep(Duration::from_millis(80));
        ids
    };

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.len(), 20);
    for id in ids {
        assert!(reopened.get(&id).is_ok());
    }
}
