---
title: Contract — nDB HTTP API (ndb serve)
status: frozen — derived from src/server.rs at 2026-11-27
date: 2026-11-27
scope: docs/contract_http-api.md
complements: docs/contract_ram-first-engine.md, docs/ndb-serve-design.md
---

# Contract — nDB HTTP API

Derived line by line from `src/server.rs`. This is what the server *does*, not
what would be convenient. Where the RAM-first dev plan adds behaviour that does
not exist yet (memory limits, poisoning), the row says so explicitly.

Transport: shared `nhttp` (bare `tokio` + `hyper` 1.x behind a lean facade, no
axum/tower). The core engine stays **100% synchronous** — every handler is a plain
fn dispatched through `nhttp::spawn_blocking`, so a reactor thread never waits on a
lock, an fsync or a compaction. `Content-Length` is always exact, `TCP_NODELAY` is
on, and `Connection: keep-alive` / `close` is always truthful.

Logging: `nlogger` writes structured JSONL to `--logs-dir` (config `logsDir`,
default `./logs`) — the rotated `main-0.log` … `main-9.log` stream plus one
`ndb-<session>.log` per process. A `ServerConfig` built without `logs_dir`
(embedders, tests) logs nothing to disk.

---

## 1. Service-wide rules

| Rule | Value |
|---|---|
| Bind / port | `agents`-free default `127.0.0.1:8323` (CLI/config) |
| Auth | Static bearer token when configured: `Authorization: Bearer <token>`, constant-time compare, checked **before** the body is read |
| Max concurrent connections | `max_connections` (default 1024). A connection over the cap answers `503` immediately — there is no blocking queue |
| I/O timeouts | 30 s read, 30 s idle, 30 s write, enforced by the transport |
| Graceful drain | 10 s for in-flight requests on shutdown, then every mounted database is `flush()`ed |
| Max request body | `max_body` (default 64 MiB) → `413` |
| Query default limit | `default_limit` (default 1000) when `limit` is absent |
| Query hard cap | `max_limit` (default 50000); a larger explicit `limit` is `400`, never truncated |
| Duplicate DB mount | Refused at startup (name or canonical path) |
| DB names | `[a-zA-Z0-9._-]+`, not `.`/`..`, Windows reserved names rejected, case-folded uniqueness |

> **`max_connections` scope.** The cap is settable only through the Rust
> `ndb::server::ServerConfig` API. It is **not** exposed in `--config` JSON or any
> CLI flag yet — the default is 1024.

**`GET /metrics`.** When no ndb route claims the path, the transport serves its own
counter snapshot: `active_connections`, `total_requests`, `rejected_503`,
`status_2xx`, `status_4xx`, `status_5xx`, `uptime_ms`. In `ndb serve` the path is
token-gated like every other path (there are no auth-exempt routes).

### 1.1 Two error envelopes

`/v2/...` (and `{ok}`-shaped legacy stubs) use:

```json
{"ok": false, "error": "<code>", "message": "<human text>"}
```

Legacy routes (`/stats`, `/doc`, `/query`, `/find`, `/facets`, `/index`,
`/compact`, `/file/...`) use the older shape, which has **no `ok` field**:

```json
{"error": "not found: abcd"}
```

Connection-level rejections use:

```json
{"error": "unauthorized"}          // 401
{"error": "payload too large"}     // 413
{"error": "connection cap exceeded"} // 503
```

### 1.2 `/v2` status codes

| Status | `error` code | When |
|---|---|---|
| 200 | — | success |
| 201 | — | document/file/database created |
| 400 | `invalid_argument` | core rejected the request (also used for bad `persistence`/`format`) |
| 400 | `invalid_json`, `empty_body` | body is not JSON / is empty |
| 400 | `invalid_query`, `invalid_filter`, `invalid_since`, `invalid_limit`, `invalid_offset`, `invalid_if_seq`, `invalid_prefix` | malformed query parameters |
| 400 | `invalid_request`, `invalid_name`, `invalid_index_type`, `invalid_bucket`, `invalid_filename`, `field_required`, `value_required`, `fields_required`, `ops_required`, `since_required`, `filename_required` | missing/invalid request fields |
| 403 | `create_disabled` | `POST /v2/dbs` without a configured `--dir` root |
| 404 | `not_found` | document/file missing |
| 404 | `database_not_found` | unknown DB name |
| 404 | `no_route` | unknown method/path under `/v2` |
| 409 | `already_exists` | create a DB that exists |
| 409 | `conflict` | `if_seq`/`If-Match` mismatch — response carries `current_seq` and `doc` |
| 409 | `cursor_expired` | change cursor before `min_available_seq` — carries `requested_seq`, `min_available_seq`, `last_seq` |
| 409 | `cursor_ahead` | change cursor past `last_seq` — carries `requested_seq`, `last_seq` |
| 409 | `file_referenced` | deleting a still-referenced file without `?force=true` |
| 410 | `snapshot_expired` | unknown/expired/foreign `snapshot_id` (300 s TTL) |
| 413 | `payload_too_large` | `mget` with more than 1000 ids |
| 500 | `internal_error` | any other core error (I/O, corruption, serialization) |
| 503 | *(reserved)* `db_poisoned` | **contract, not implemented**: uncertain-fsync poison (see engine contract §3.1) |
| 507 | *(reserved)* `memory_limit_exceeded` | **contract, not implemented**: per-DB/service memory cap (engine contract §5.3) |

> **Note on `cursor_expired`.** The engine contract and the code both answer
> **409** for an expired change cursor; only `snapshot_expired` is 410. Any
> brief that says `410 cursor_expired` is wrong.

### 1.3 Legacy routes (`/stats`, `/doc`, …)

They address the database mounted as `default`. Without a `default` mount every
legacy route answers `404` with a message naming the mistake, body
`{"error": "no database mounted as 'default' …"}`. Unknown method/path is
`404 {"error": "no route: <METHOD> <path>"}`.

**Legacy errors are flat.** `db_error` maps **every** core error — including
`NotFound` and `Conflict` — to `500 {"error": "<text>"}`, with the one exception
that `PUT`/`PATCH` map `Conflict` to `409 {ok:false,error:"conflict",…}` and
`InvalidArgument` to `400 {"error": …}`. A client must not expect `404` from a
legacy route.

| Route | Success body |
|---|---|
| `GET /stats` | `200 {"ok":true,"documents":N}` |
| `POST /query` (no `fields`) | `200 {"ok":true,"total":N,"count":M,"results":[<raw docs>]}` |
| `POST /query` (with `fields`) | `200 {"ok":true,"total":N,"count":M,"results":[…]}` |
| `POST /find` | `200 {"ok":true,"count":N,"results":[…]}` |
| `POST /facets` | `200 {"ok":true,"facets":{field:{term:count}}}` |
| `GET /doc/{id}` (`?fields=a,b`) | `200 {"ok":true,"doc":{…}}` |
| `PUT /doc` | `201 {"ok":true,"id":"…"}` |
| `PUT /doc/{id}` | `200 {"ok":true,"id":"…","seq":N}` (`seq` omitted when absent) |
| `PATCH /doc/{id}` | `200 {"ok":true,"applied":[…],"seq":N}` — **no `doc` field** |
| `DELETE /doc/{id}` | `200 {"ok":true,"id":"…"}` |
| `POST /index` | `201 {"ok":true,"index":"field","type":"hash"\|"btree"}` |
| `POST /compact` | `202 {"ok":true,"accepted":true,"note":"compaction running in background; watch the server log"}` |
| `GET /file/{bucket}/{name}` | `200` raw bytes, `Content-Type` from the extension |
| `PUT /file/{bucket}` | `201 {"ok":true,"ref":"bucket:hash.ext","bucket":"…"}` (stores as `upload.bin`) |

Legacy query/find/facets/compact error bodies are `{"error": "<text>"}` only —
no `ok` key. `POST /compact` returns before compaction finishes (202).

---

## 2. Service routes

### `GET /health` → 200

```json
{
  "ok": true,
  "engine": "ndb",
  "version": "<crate version>",
  "dbs": ["chat_herrbasan", "memories"],
  "allow_create": true
}
```

`allow_create` is true iff a creation root (`--dir`) is configured.

### `GET /v2/dbs` → 200

```json
{"ok": true, "dbs": [<descriptor>, …]}
```

Descriptor:

```json
{
  "name": "memories",
  "format": "v2",              // "v1" | "v2"
  "documents": 5093,
  "source_epoch_uuid": "…",
  "last_seq": 51234,
  "min_available_seq": 1
}
```

### `POST /v2/dbs` → 201

Body (`#[serde(deny_unknown_fields)]` — any extra key is `400 invalid_request`):

```json
{"name": "scratch", "persistence": "immediate", "format": "v2"}
```

- `name` required; `persistence` ∈ `immediate | scheduled | lazy` (default: the
  service mode); `format` ∈ `auto | v1 | v2 | ram` (default: the service create
  format). Bad values → `400 invalid_argument`.
- `201 {"ok":true,"db":<descriptor>}`.
- `403 create_disabled` — no `--dir` root.
- `400 invalid_name` — illegal name.
- `409 already_exists` — name already mounted/created.
- The body may never carry a path; the server derives it from `--dir`.

### `GET /v2/dbs/{name}` → 200

`{"ok":true,"db":<descriptor>}` · `404 database_not_found`.

---

## 3. Per-database routes

All are `/v2/dbs/{name}/...`. `{id}`, `{bucket}`, `{filename}` are single path
components; percent-decoding applies to query strings (`+` = space, malformed
`%` stays literal).

### 3.1 `POST /v2/dbs/{name}/query`

```json
{"filter": {"score": {"$gte": 5}}, "sort": {"field": "score", "dir": "desc"},
 "limit": 20, "offset": 0, "fields": ["title", "score"]}
```

- `filter` defaults to `{}`; it is validated with `validate_query_ast` first —
  unknown operators are `400 invalid_filter`, never a full scan.
- `sort` requires `field`; `dir` ∈ `asc | desc` (default `asc`). Bad values →
  `400 invalid_query`.
- `limit` absent → `default_limit`; `limit > max_limit` → `400 invalid_query`.
  **`offset` is applied after sorting**, with or without a `limit`
  (herrbasan/nDB#15).
- Without `fields`: raw, pre-serialized documents are streamed:
  `{"ok":true,"total":N,"count":M,"results":[…],"took_ms":x,
  "scanned_docs":S,"used_indexes":[…],"next_cursor":null|"…"}`.
- With `fields`: `_id` is always included in each projected document.

#### Phase 4A additions (all optional, strictly additive)

```json
{
  "filter": {"domain": "work"},
  "sort_keys": [
    {"field": "score", "dir": "desc", "missing": "first"},
    {"field": "title", "dir": "asc"}
  ],
  "cursor": "ndbc1:…",
  "text": {"query": "\"rust belt\" caching", "fields": ["title", "body"]},
  "max_scan_docs": 5000,
  "limit": 50
}
```

- **`sort_keys`** is a deterministic multi-key order; a final `_id` **ascending**
  tie-breaker is always appended, so the order is total and pagination is
  stable. `missing` ∈ `first | last` (default `last`) is absolute — it is not
  reversed by `desc`. `sort_keys` supersedes `sort`; `sort` keeps its exact
  legacy behaviour (a missing field sorts as JSON `null`).
- **`cursor`** is the opaque `next_cursor` of the previous page (an
  `ndbc1:`-prefixed hex payload holding the last sort-key tuple and `_id`).
  A cursor and a non-zero `offset` cannot be combined (`400 invalid_query`), and
  a cursor whose key count disagrees with the order is refused. With no
  `sort_keys`/`sort` the order is `_id` ascending.
- **`text`** is a standalone lexical query: whitespace separates terms,
  `"quoted text"` is a phrase, every term **and** every phrase must match
  (case-insensitive) in at least one searched string field.
  `fields` absent → the mapping's `text: true` fields, else every
  string-valued application field (`_id` excluded unless named).
  An unterminated quote or an empty query is `400 invalid_argument`.
- **`max_scan_docs`** is a mandatory scan budget: exceeding it aborts the scan
  and answers `422 scan_budget_exceeded` (`{scanned,max_scan}`), never a
  truncated page.
- **`scanned_docs`** is the number of documents the execution path examined,
  **`used_indexes`** the indexed fields that actually narrowed the scan (empty =
  full scan), **`next_cursor`** the continuation token (`null` when the page is
  the last one).

`{"categories": "rust"}` on a profile-enabled database matches by **array
membership** (the object is tagged `rust`); an array operand still means the
whole array. Every other field keeps exact `$eq` semantics.

Errors: `400 invalid_filter | invalid_query | empty_body | invalid_json`,
`422 scan_budget_exceeded`, `500 internal_error`.

### 3.1a `POST /v2/dbs/{name}/search`

The same body and the same response envelope as `…/query`, with `text`
**required** (`400 text_required` when absent). It is the convenience alias for
lexical search; everything else — `filter`, `sort`/`sort_keys`, `fields`,
`offset`, `limit`, `cursor`, `max_scan_docs` — behaves identically.

### 3.1b `GET`/`PUT /v2/dbs/{name}/schema`

```json
GET  → {"ok":true,"schema":{"classification_profile":false,
        "field_mapping":[{"field":"score","hash":true,"ordered":true,
                          "text":false,"project":false}],
        "indexes":[{"field":"score","kind":"btree"}]}}

PUT  ← {"field_mapping":[{"field":"score","hash":true,"ordered":true},
                         {"field":"title","text":true,"project":true}]}
```

- `hash: true` ensures a Hash index; `ordered: true` ensures a BTree index. A
  field declaring **both** is backed by one BTree, which answers equality and
  ranges. A mapping entry declaring neither drops any index on the field it
  names; fields outside the mapping keep their indexes.
- The mapping persists (manifest + WAL) and drives `text` search defaults.
- `PUT` while the classification profile is enabled must keep `domain` and
  `categories` indexed; dropping either is `400 invalid_argument`.
- Both routes require a v3 (RAM-first) database (`400 invalid_argument`
  otherwise).
- Errors: `400 invalid_request` (bad entry/body), `400 invalid_argument`,
  `500 internal_error`.

### 3.1c `POST /v2/dbs/{name}/profile/classification`

```json
{"enabled": true, "backfill_empty": true}
```

The explicit migration for the standard classification profile (spec §2). Every
live document must carry `domain: string` and `categories: string[]`
(empty = uncategorized; entries are non-empty and deduplicated).

- Inspects every live document **first**: wrong-shaped fields →
  `400 invalid_argument`, nothing mutated.
- Missing fields with `backfill_empty` absent/false → `400 invalid_argument`
  naming how many documents lack which field.
- `backfill_empty: true` durably backfills `domain: ""` / `categories: []` (and
  deduplicates existing `categories`) in one commit, then builds both indexes
  and records both mapping entries.
- `enabled: false` is refused (`400 invalid_request`): the profile is a durable
  contract choice, not a toggle. Enabling an already-enabled profile is
  `400 invalid_argument`.
- Success: `200 {"ok":true,"schema":{…}}` (the shape above).

### 3.1d `POST /v2/dbs/{name}/doc/{id}/peek`

`seq`-pinned partial retrieval (`peek_item` / `read_more`).

```json
{"fields": ["meta.author"], "text_ranges": [{"path": "text", "offset": 0, "limit": 200}],
 "array_slices": [{"path": "messages", "offset": 100, "limit": 50}],
 "expected_seq": 412}
```

`200 {"ok":true,"id":"…","seq":412,"doc":{…},"ranges":{…}}` where each applied
range reports `{"offset","returned","total","has_more","next_offset"}`.

- `text_ranges` offsets count **Unicode scalar values** (`char`s) — never bytes
  and never model tokens; `array_slices` count array elements. A page that
  reports `has_more` continues at `next_offset`.
- Any path named by a range is always included in `doc`, even with `fields`.
  Missing paths simply yield no range entry; a range on the wrong type is
  `400 invalid_argument`.
- `expected_seq` pins the read: a document that moved answers
  `409 conflict` with its `current_seq`, so pages never mix versions.
- Errors: `404 not_found`, `409 conflict`, `400 invalid_argument |
  invalid_request`.

### 3.2 `POST /v2/dbs/{name}/find`

```json
{"field": "kind", "value": "a"}            // equality
{"field": "score", "min": 5, "max": 10}    // range (both required)
```

`200 {"ok":true,"count":N,"results":[…],"took_ms":x}` · `400 field_required` ·
`400 value_required` when neither `value` nor both `min`/`max` are present.

### 3.3 `POST /v2/dbs/{name}/facets`

```json
{"fields": ["kind", "tags"]}
```

`200 {"ok":true,"facets":{"kind":{"a":3},"tags":{"x":7}}}` · `400 fields_required`
when the array is empty. Only top-level string fields and string array elements
are counted.

### 3.4 Documents

| Method · path | Body | Success | Errors |
|---|---|---|---|
| `GET …/doc/{id}` | — (optional `?fields=a,b`) | `200 {"ok":true,"doc":{…}}` | `404 not_found` |
| `PUT …/doc` | document JSON; optional `?prefix=<p>` | `201 {"ok":true,"id":"…","doc":{…},"seq":N}` | `400 invalid_json/empty_body/invalid_prefix`, `400 invalid_argument` (non-object) |
| `PUT …/doc/{id}` | document JSON; optional `?if_seq=N` or `If-Match: N` | `200 {"ok":true,"id":"…","doc":{…},"seq":N}` | `404 not_found`, `409 conflict` (+`current_seq`,`doc`), `400 invalid_if_seq` |
| `PATCH …/doc/{id}` | `{"ops":[{"op":"set","path":"a.b","value":1}, {"op":"remove","path":"c"}, {"op":"array_push","path":"tags","value":"x"}]}`; optional `if_seq`/`If-Match` | `200 {"ok":true,"applied":[true,…],"seq":N,"doc":{…}}` | `400 ops_required` (missing/empty), `400 unknown_op \| path_required \| value_required` (op parsing, nothing applied), `400 invalid_argument` (an op does not resolve → nothing applied), `409 conflict` |
| `DELETE …/doc/{id}` | — | `200 {"ok":true,"deleted":true,"seq":N}` | `404 not_found`, `403`-class policy violations surface as `500 internal_error` |
| `POST …/doc/{id}/restore` | — | `200 {"ok":true,"doc":{…},"seq":N}` | `400 invalid_argument` (not deleted), `404 not_found` (no trash entry) |
| `POST …/docs/mget` | `{"ids":["…"],"fields":["…"]?}` | `200 {"ok":true,"docs":{"id":doc},"missing":["…"]}` | `413 payload_too_large` (>1000 ids), `400 invalid_request` |

`PATCH` op spelling is exactly `set` / `remove` / `array_push` with `path`
(dot notation) and — for `set`/`array_push` — `value`. Batch semantics are
all-or-nothing: an op that does not resolve aborts the whole batch.

### 3.5 Indexes and maintenance

| Method · path | Body | Success | Errors |
|---|---|---|---|
| `POST …/index` | `{"field":"score","type":"hash"\|"btree"}` (`type` default `hash`) | `200 {"ok":true,"field":"score","type":"hash","created":true\|false}` — `created:false` when the index already exists | `400 field_required`, `400 invalid_index_type`, `500 internal_error` |
| `POST …/compact` | — | `200 {"ok":true,"lineage":{…}}` | `500 internal_error` |
| `POST …/flush` | — | `200 {"ok":true}` | `500 internal_error` |
| `GET …/stats` | — | `200 {"ok":true,"documents":N,"lineage":{"source_epoch_uuid":…,"last_seq":N,"min_available_seq":N}}` | — |
| `GET …/lineage` | — | `200 {"ok":true,"source_epoch_uuid":…,"last_seq":N,"min_available_seq":N}` | — |

An index declaration consumes one `seq` in the WAL (a v2 detail the change feed
must account for); it emits no change event.

### 3.6 Sync feed

#### `GET …/changes?since=<u64>&limit=<usize>`

`since` is **required** (`400 since_required`); non-numeric → `400 invalid_since`.
`limit` must be > 0 (`400 invalid_limit`); absent = all.

```json
{
  "ok": true,
  "source_epoch_uuid": "…",
  "from_seq": 100,
  "to_seq": 103,
  "last_seq": 200,
  "has_more": true,
  "changes": [
    {"seq": 101, "id": "…", "kind": "upsert", "doc": {…}},
    {"seq": 102, "id": "…", "kind": "delete"}
  ]
}
```

`to_seq` is the last returned event's seq (or `since` when the page is empty).
Errors: `409 cursor_ahead`, `409 cursor_expired` (each with its numbers).

#### `GET …/snapshot?offset=&limit=&snapshot_id=`

```json
{
  "ok": true,
  "source_epoch_uuid": "…",
  "watermark_seq": 200,
  "snapshot_id": "…" | null,
  "total": 5093,
  "count": 1000,
  "docs": [ {…} ]
}
```

- No pagination parameters → one-shot snapshot, `snapshot_id: null`.
- Any of `offset > 0`, `limit`, or `snapshot_id` → paginated. Without
  `snapshot_id` the server pins a new snapshot (TTL 300 s) and returns its id;
  subsequent pages pass `snapshot_id` to stay on the same frozen state.
- The pin is released as soon as it serves the page that completes the read
  (`offset + count >= total`), including a snapshot that fits in one page: a
  finished read never reserves change-ring history for the rest of the TTL, and
  its `snapshot_id` answers `410` afterwards. An unfinished read stays pinned
  until the TTL reaper releases it.
- Unknown/expired/foreign id → `410 snapshot_expired`.
- `400 invalid_offset` / `400 invalid_limit` for non-numeric values.
- Documents are sorted by `_id`.

### 3.7 Files

| Method · path | Requirements | Success | Errors |
|---|---|---|---|
| `PUT …/files/{bucket}` | raw body; `X-Filename` header or `?filename=`; filename = single component, extension ≤10 ASCII alnum (lowercased) | `201 {"ok":true,"ref":"bucket:hash.ext","hash":"…","ext":"…","size":N}` | `400 invalid_bucket/invalid_filename/filename_required/empty_body` |
| `GET …/files/{bucket}` | — | `200 {"ok":true,"files":["hash.ext",…]}` (sorted) | `400 invalid_bucket` |
| `GET …/files/{bucket}/{name}` | name = `hash.ext` | `200` raw bytes with MIME type | `400 invalid_filename`, `404 not_found` |
| `DELETE …/files/{bucket}/{name}?force=true` | — | `200 {"ok":true,"deleted":true}` | `409 file_referenced` (live or restorable document), `404 not_found`, `400 invalid_filename` |
| `POST …/files/gc` | — | `200 {"ok":true,"moved":N}` | `500 internal_error` |

Bucket and filename validation: non-empty, not `.`/`..`, no control characters,
no `/` or `\`, a plain path component, bounded extension. A bucket read that
misses maps `io::ErrorKind::NotFound` to `404 not_found`.

---

## 4. Behaviour the RAM-first engine must not change

- Every `/v2` error stays `{ok:false,error,message}`; legacy errors stay
  `{error}`. Clients parse both.
- `409 conflict` keeps carrying `current_seq` **and** the current `doc`, so a
  caller can retry without a second round trip.
- `404 not_found` for documents, `404 database_not_found` for databases,
  `404 no_route` for unknown routes — never a generic 400.
- `mget` reports duplicates once and lists only genuinely missing ids in
  `missing`.
- A no-op single-op mutation (`set`/`remove`/`array_push` resolving to `false`)
  is not an error: it writes nothing, consumes no `seq` and emits no change
  event. `PATCH` is deliberately stricter — an op that does not resolve aborts
  the whole batch with `400 invalid_argument`, and `applied` is always all
  `true` or the call fails.
- `GET …/snapshot` without pagination parameters never pins; it cannot leak a
  snapshot.
- `503` on a full connection queue and `413` on an oversized body are answers,
  not dropped sockets.
