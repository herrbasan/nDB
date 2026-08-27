//! ndb serve — resident HTTP daemon (third interface alongside CLI and napi).
//!
//! Design: docs/ndb-serve-design.md (issue #2).
//! Hand-rolled HTTP/1.1 on std::net — zero new crates. Thread-per-connection
//! with a cap, keep-alive, read timeout, body cap, static bearer token.
//! The daemon owns the database in memory; clients receive only results.

use crate::{Database, Error, QueryOptions, SortDir, TextMode, TextQuery, TextSearch};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ─── Config ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Bind address. "127.0.0.1" for local-only, "0.0.0.0" for LAN.
    pub bind: String,
    pub port: u16,
    /// Static bearer token. None = no auth (local-only setups).
    pub token: Option<String>,
    /// Max concurrent connections; excess gets 503.
    pub max_connections: usize,
    /// Max request body size (bytes). Excess gets 413.
    pub max_body: usize,
    /// Default + hard cap for query limit.
    pub default_limit: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 8323,
            token: None,
            max_connections: 64,
            max_body: 64 * 1024 * 1024,
            default_limit: 1000,
        }
    }
}

// ─── Entry point ────────────────────────────────────────────────────

/// Run the server. Blocks forever; each connection handled on its own thread.
pub fn serve(db: Database, config: ServerConfig) -> Result<(), Error> {
    let listener = TcpListener::bind((config.bind.as_str(), config.port))
        .map_err(|e| Error::invalid_arg(format!("bind {}:{} failed: {}", config.bind, config.port, e)))?;
    serve_listener(listener, db, config)
}

/// Serve on an already-bound listener (tests bind port 0 for an ephemeral port).
pub fn serve_listener(listener: TcpListener, db: Database, config: ServerConfig) -> Result<(), Error> {
    let db = Arc::new(db);
    let config = Arc::new(config);
    let active = Arc::new(AtomicUsize::new(0));

    eprintln!("ndb serve listening on {} ({} connections max)",
        listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
        config.max_connections);

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {}", e);
                continue;
            }
        };
        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
        if count > config.max_connections {
            // Over cap: 503 and close. Fail loud, no queueing.
            let mut stream = stream;
            let _ = write_response(&mut stream, &Response::Json(503, json!({"error": "connection cap exceeded"})));
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let db = Arc::clone(&db);
        let config = Arc::clone(&config);
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            handle_connection(stream, &db, &config);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

// ─── Connection handling ────────────────────────────────────────────

fn handle_connection(stream: TcpStream, db: &Arc<Database>, config: &ServerConfig) {
    // Read timeout: kills slow-loris and reclaims threads from dead clients.
    if stream.set_read_timeout(Some(Duration::from_secs(30))).is_err() {
        return;
    }
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut stream = stream;

    loop {
        let req = match read_request(&mut reader, &mut stream, config) {
            Ok(Some(r)) => r,
            Ok(None) => return, // clean EOF between requests
            Err(e) => {
                // Malformed request or rejected (auth/size): respond, then close.
                // read_request signals auth/size rejections as "401"/"413".
                let status = match e.as_str() {
                    "401" => 401,
                    "413" => 413,
                    _ => 400,
                };
                let body = if status == 401 {
                    json!({"error": "unauthorized"})
                } else if status == 413 {
                    json!({"error": "payload too large"})
                } else {
                    json!({"error": e})
                };
                let _ = write_response(&mut stream, &Response::Json(status, body));
                return;
            }
        };

        let resp = route(&Arc::clone(db), config, &req);
        let keep_alive = req.keep_alive;
        let ok = write_response(&mut stream, &resp).is_ok();
        if !ok || !keep_alive {
            return;
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    keep_alive: bool,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Read one HTTP/1.1 request. Returns Ok(None) on clean EOF before a request.
fn read_request(
    reader: &mut BufReader<TcpStream>,
    stream: &mut TcpStream,
    config: &ServerConfig,
) -> Result<Option<Request>, String> {
    // Request line
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(_) => return Ok(None), // timeout / reset between requests
    }
    let line = line.trim_end();
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing path")?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.1");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    // Headers
    let mut headers = Vec::new();
    loop {
        let mut hline = String::new();
        reader
            .read_line(&mut hline)
            .map_err(|e| format!("header read error: {}", e))?;
        let hline = hline.trim_end();
        if hline.is_empty() {
            break;
        }
        if let Some((k, v)) = hline.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let mut req = Request {
        method,
        path,
        query,
        headers,
        body: Vec::new(),
        keep_alive: !version.eq_ignore_ascii_case("HTTP/1.0"),
    };
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("connection") {
            if v.eq_ignore_ascii_case("close") {
                req.keep_alive = false;
            } else if v.eq_ignore_ascii_case("keep-alive") {
                req.keep_alive = true;
            }
        }
    }

    // Auth BEFORE reading the body — bad tokens never cost us the transfer.
    if let Some(expected) = &config.token {
        let authorized = req
            .header("authorization")
            .and_then(|got| got.strip_prefix("Bearer "))
            .map(|bearer| constant_time_eq(bearer.as_bytes(), expected.as_bytes()))
            .unwrap_or(false);
        if !authorized {
            return Err("401".to_string());
        }
    }

    let content_length: usize = req
        .header("content-length")
        .map(|v| v.trim().parse::<usize>())
        .transpose()
        .map_err(|e| format!("invalid content-length: {}", e))?
        .unwrap_or(0);

    if content_length > config.max_body {
        return Err("413".to_string());
    }

    // curl sends this for bodies > ~1KB; not answering stalls the client.
    if let Some(expect) = req.header("expect") {
        if expect.eq_ignore_ascii_case("100-continue") {
            stream
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .map_err(|e| format!("continue write error: {}", e))?;
        }
    }

    if content_length > 0 {
        req.body.resize(content_length, 0);
        reader
            .read_exact(&mut req.body)
            .map_err(|e| format!("body read error: {}", e))?;
    }

    Ok(Some(req))
}

/// Length-independent comparison so token checks don't leak timing info.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── Routing ────────────────────────────────────────────────────────

enum Response {
    Json(u16, Value),
    /// Raw bytes with a real content type — bucket files never ride in JSON.
    Binary(u16, String, Vec<u8>),
}

fn route(db: &Arc<Database>, config: &ServerConfig, req: &Request) -> Response {
    let segments: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();

    // Binary route: raw bytes with a real content type, never JSON-wrapped.
    if let ("GET", ["file", bucket, name]) = (req.method.as_str(), segments.as_slice()) {
        return handle_get_file(db, bucket, name);
    }

    let (status, body) = match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["stats"]) => handle_stats(db),

        ("POST", ["query"]) => handle_query(db, config, req),
        ("POST", ["find"]) => handle_find(db, req),
        ("POST", ["search"]) => handle_search(db, config, req),

        ("GET", ["doc", id]) => handle_get_doc(db, req, id),
        ("PUT", ["doc"]) => handle_insert(db, req),
        ("PUT", ["doc", id]) => handle_replace(db, req, id),
        ("PATCH", ["doc", id]) => handle_patch(db, req, id),
        ("DELETE", ["doc", id]) => handle_delete(db, id),

        ("POST", ["index"]) => handle_create_index(db, req),
        ("POST", ["compact"]) => handle_compact(db),

        ("PUT", ["file", bucket]) => handle_store_file(db, req, bucket),

        // Unknown route: 404, fail loud.
        _ => (404, json!({"error": format!("no route: {} {}", req.method, req.path)})),
    };
    Response::Json(status, body)
}

fn parse_body(req: &Request) -> Result<Value, (u16, Value)> {
    if req.body.is_empty() {
        return Err((400, json!({"error": "empty body"})));
    }
    serde_json::from_slice(&req.body).map_err(|e| (400, json!({"error": format!("invalid JSON body: {}", e)})))
}

fn db_error(e: Error) -> (u16, Value) {
    (500, json!({"error": e.to_string()}))
}

fn handle_stats(db: &Database) -> (u16, Value) {
    let count = db.iter().len();
    (
        200,
        json!({
            "ok": true,
            "documents": count,
        }),
    )
}

fn handle_query(db: &Database, config: &ServerConfig, req: &Request) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };

    let filter = body.get("filter").cloned().unwrap_or_else(|| json!({}));
    if let Err(e) = crate::validate_query_ast(&filter) {
        return (400, json!({"error": format!("invalid filter: {}", e)}));
    }

    let opts = match parse_query_options(&body, config) {
        Ok(o) => o,
        Err(e) => return (400, json!({"error": e})),
    };

    let fields: Option<Vec<String>> = body
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect());

    let results = db.query_projected(filter, opts, fields.as_deref());
    (
        200,
        json!({
            "ok": true,
            "count": results.len(),
            "results": results,
        }),
    )
}

fn parse_query_options(body: &Value, config: &ServerConfig) -> Result<QueryOptions, String> {
    let mut opts = QueryOptions::default();

    if let Some(sort) = body.get("sort") {
        let field = sort
            .get("field")
            .and_then(|v| v.as_str())
            .ok_or("sort.field required when sort is present")?;
        let dir = match sort.get("dir").and_then(|v| v.as_str()).unwrap_or("asc") {
            "asc" => SortDir::Asc,
            "desc" => SortDir::Desc,
            other => return Err(format!("sort.dir must be 'asc' or 'desc', got '{}'", other)),
        };
        opts.sort_by = Some((field.to_string(), dir));
    }

    if let Some(offset) = body.get("offset") {
        opts.offset = Some(offset.as_u64().ok_or("offset must be a non-negative integer")? as usize);
    }

    if let Some(limit) = body.get("limit") {
        let limit = limit.as_u64().ok_or("limit must be a non-negative integer")? as usize;
        if limit > config.default_limit {
            return Err(format!("limit {} exceeds cap {}", limit, config.default_limit));
        }
        opts.limit = Some(limit);
    } else {
        // Mandatory default: an unfiltered query must never stream the whole DB.
        opts.limit = Some(config.default_limit);
    }

    Ok(opts)
}

/// POST /search — full-text search over a text-indexed field.
/// Body: {field, mode: "and"|"or", case_sensitive?, queries: [{type, value, exclude?}],

/// POST /search — full-text search over a text-indexed field.
/// Body: {field, mode: "and"|"or", case_sensitive?, queries: [{type, value, exclude?}],
///         fields?, limit?, offset?}
fn handle_search(db: &Database, config: &ServerConfig, req: &Request) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };

    let field = match body.get("field").and_then(|v| v.as_str()) {
        Some(f) => f.to_string(),
        None => return (400, json!({"error": "field required"})),
    };

    let mode = match body.get("mode").and_then(|v| v.as_str()).unwrap_or("and") {
        "and" => TextMode::And,
        "or" => TextMode::Or,
        other => {
            return (
                400,
                json!({"error": format!("mode must be 'and' or 'or', got '{}'", other)}),
            )
        }
    };

    let case_sensitive = body.get("case_sensitive").and_then(|v| v.as_bool()).unwrap_or(false);

    let raw_queries = match body.get("queries").and_then(|q| q.as_array()) {
        Some(q) if !q.is_empty() => q,
        _ => return (400, json!({"error": "queries: non-empty array required"})),
    };

    let mut queries = Vec::with_capacity(raw_queries.len());
    for q in raw_queries {
        let qtype = q.get("type").and_then(|v| v.as_str()).unwrap_or("term");
        let value = match q.get("value").and_then(|v| v.as_str()) {
            Some(v) if !v.trim().is_empty() => v.to_string(),
            _ => return (400, json!({"error": "each query needs a non-empty value"})),
        };
        let inner = match qtype {
            "term" => TextQuery::Term(value),
            "phrase" => TextQuery::Phrase(value),
            "prefix" => TextQuery::Prefix(value),
            other => {
                return (
                    400,
                    json!({"error": format!("query type must be term|phrase|prefix, got '{}'", other)}),
                )
            }
        };
        if q.get("exclude").and_then(|v| v.as_bool()).unwrap_or(false) {
            queries.push(TextQuery::Exclude(Box::new(inner)));
        } else {
            queries.push(inner);
        }
    }

    let search = TextSearch { mode, case_sensitive, queries };
    if let Err(e) = search.validate() {
        return (400, json!({"error": e.to_string()}));
    }

    let ids = match db.text_search(&field, &search) {
        Ok(ids) => ids,
        Err(e) => {
            let (status, body) = db_error(e);
            return (status, body);
        }
    };
    let total = ids.len();

    // Hydrate results with optional projection, honoring limit/offset.
    let limit = body
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(config.default_limit)
        .min(config.default_limit);
    let offset = body.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

    let fields: Option<Vec<String>> = body
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect());

    let page: Vec<Value> = ids
        .into_iter()
        .skip(offset)
        .take(limit)
        .filter_map(|id| db.get(&id).ok())
        .map(|doc| match &fields {
            Some(fs) => {
                let mut out = serde_json::Map::new();
                for f in fs {
                    if let Some(v) = crate::field_get(&doc, f) {
                        out.insert(f.clone(), v.clone());
                    }
                }
                out.insert("_id".to_string(), json!(doc["_id"]));
                Value::Object(out)
            }
            None => doc,
        })
        .collect();

    (
        200,
        json!({
            "ok": true,
            "count": page.len(),
            "total": total,
            "results": page,
        }),
    )
}

fn handle_find(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let field = match body.get("field").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return (400, json!({"error": "field required"})),
    };

    if let (Some(min), Some(max)) = (body.get("min"), body.get("max")) {
        let results = db.find_range(field, min, max);
        return (200, json!({"ok": true, "count": results.len(), "results": results}));
    }

    match body.get("value") {
        Some(value) => {
            let results = db.find(field, value);
            (200, json!({"ok": true, "count": results.len(), "results": results}))
        }
        None => (400, json!({"error": "value or min/max required"})),
    }
}

fn projection_from_query(req: &Request) -> Option<Vec<String>> {
    req.query
        .split('&')
        .find(|p| p.starts_with("fields="))
        .and_then(|p| p.strip_prefix("fields="))
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

fn handle_get_doc(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let doc = match db.get(id) {
        Ok(d) => d,
        Err(e) => return db_error(e),
    };
    match projection_from_query(req) {
        Some(fields) if !fields.is_empty() => {
            let mut out = serde_json::Map::new();
            for f in &fields {
                // Dot-notation projection (same path engine as /query).
                if let Some(v) = crate::field_get(&doc, f) {
                    out.insert(f.clone(), v.clone());
                }
            }
            out.insert("_id".to_string(), json!(id));
            (200, json!({"ok": true, "doc": Value::Object(out)}))
        }
        _ => (200, json!({"ok": true, "doc": doc})),
    }
}

fn handle_insert(db: &Database, req: &Request) -> (u16, Value) {
    let doc = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    match db.insert(doc) {
        Ok(id) => (201, json!({"ok": true, "id": id})),
        Err(e) => db_error(e),
    }
}

fn handle_replace(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let doc = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    match db.update(id, doc) {
        Ok(()) => (200, json!({"ok": true, "id": id})),
        Err(e) => db_error(e),
    }
}

fn handle_patch(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let ops = match body.get("ops").and_then(|o| o.as_array()) {
        Some(o) if !o.is_empty() => o,
        _ => return (400, json!({"error": "ops: non-empty array required"})),
    };

    let mut applied = Vec::with_capacity(ops.len());
    for op in ops {
        let before = db.get(id).ok();
        let result = match op.get("op").and_then(|v| v.as_str()) {
            Some("set") => db.set(
                id,
                op.get("path").and_then(|v| v.as_str()).ok_or_else(|| Error::invalid_arg("set: path required")).unwrap(),
                op.get("value").cloned().ok_or_else(|| Error::invalid_arg("set: value required")).unwrap(),
            ),
            Some("remove") => db.remove(
                id,
                op.get("path").and_then(|v| v.as_str()).ok_or_else(|| Error::invalid_arg("remove: path required")).unwrap(),
            ),
            Some("array_push") => db.array_push(
                id,
                op.get("path").and_then(|v| v.as_str()).ok_or_else(|| Error::invalid_arg("array_push: path required")).unwrap(),
                op.get("value").cloned().ok_or_else(|| Error::invalid_arg("array_push: value required")).unwrap(),
            ),
            other => {
                return (
                    400,
                    json!({"error": format!("unknown op {:?} (expected set | remove | array_push)", other)}),
                )
            }
        };
        if let Err(e) = result {
            return db_error(e);
        }
        // set/remove silently skip unresolvable paths; surface it per-op.
        let after = db.get(id).ok();
        applied.push(match (&before, &after) {
            (Some(b), Some(a)) => b != a,
            _ => true,
        });
    }

    (200, json!({"ok": true, "applied": applied}))
}

fn handle_delete(db: &Database, id: &str) -> (u16, Value) {
    match db.delete(id) {
        Ok(()) => (200, json!({"ok": true, "id": id})),
        Err(e) => db_error(e),
    }
}

fn handle_create_index(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let field = match body.get("field").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return (400, json!({"error": "field required"})),
    };
    let kind = body.get("type").and_then(|v| v.as_str()).unwrap_or("hash");
    let result = match kind {
        "hash" => db.create_index(field),
        "btree" => db.create_btree_index(field),
        other => return (400, json!({"error": format!("type must be hash or btree, got '{}'", other)})),
    };
    match result {
        Ok(()) => (201, json!({"ok": true, "index": field, "type": kind})),
        Err(e) => db_error(e),
    }
}

/// Compaction holds the writer lock and can take minutes at multi-GB scale.
/// 202 + background thread; completion observable via the log.
fn handle_compact(db: &Arc<Database>) -> (u16, Value) {
    let db = Arc::clone(db);
    std::thread::spawn(move || match db.compact() {
        Ok(()) => eprintln!("compact: done"),
        Err(e) => eprintln!("compact: FAILED: {}", e),
    });
    (202, json!({"ok": true, "accepted": true, "note": "compaction running in background; watch the server log"}))
}

fn handle_store_file(db: &Database, req: &Request, bucket: &str) -> (u16, Value) {
    if req.body.is_empty() {
        return (400, json!({"error": "empty body"}));
    }
    let mime = req.header("content-type").unwrap_or("application/octet-stream");
    match db.bucket(bucket).store("upload.bin", &req.body, mime) {
        Ok(meta) => (201, json!({"ok": true, "ref": meta._file.to_string_compact(), "bucket": bucket})),
        Err(e) => db_error(e),
    }
}

fn handle_get_file(db: &Database, bucket: &str, name: &str) -> Response {
    // name = hash.ext
    let (hash, ext) = match name.rsplit_once('.') {
        Some((h, e)) => (h, e),
        None => return Response::Json(400, json!({"error": "file name must be hash.ext"})),
    };
    match db.bucket(bucket).get_by_hash(hash, ext) {
        Ok(data) => Response::Binary(200, content_type_for(ext), data),
        Err(e) => {
            let (status, body) = db_error(e);
            Response::Json(status, body)
        }
    }
}

fn content_type_for(ext: &str) -> String {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        _ => "application/octet-stream",
    }
    .to_string()
}

// ─── HTTP response ──────────────────────────────────────────────────

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

fn write_response(stream: &mut TcpStream, resp: &Response) -> std::io::Result<()> {
    let (status, content_type, payload): (u16, &str, Vec<u8>) = match resp {
        Response::Json(status, body) => {
            let payload = serde_json::to_vec(body)
                .unwrap_or_else(|_| b"{\"error\":\"serialization failed\"}".to_vec());
            (*status, "application/json", payload)
        }
        Response::Binary(status, ctype, data) => (*status, ctype, data.clone()),
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        status,
        reason(status),
        content_type,
        payload.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&payload)?;
    stream.flush()
}
