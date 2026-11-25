---
title: Contract — nDB HTTP API (ndb serve)
status: frozen — derived from src/server.rs at 2026-11-25
date: 2026-11-25
scope: docs/contract_http-api.md
complements: docs/contract_ram-first-engine.md, docs/ndb-serve-design.md
---

# Contract — nDB HTTP API

Derived line by line from `src/server.rs`. This is what the server *does*, not
what would be convenient. Where the RAM-first dev plan adds behaviour that does
not exist yet (memory limits, poisoning), the row says so explicitly.

Transport: HTTP/1.1 on `std::net` (no async runtime), one `BufReader` per
socket, keep-alive, `TCP_NODELAY`, `Content-Length` always exact, one
`write_all` per response frame.

---

## 1. Service-wide rules

| Rule | Value |
|---|---|
| Bind / port | `agents`-free default `127.0.0.1:8323` (CLI/config) |
| Auth | Static bearer token when configured: `Authorization: Bearer <token>`, constant-time compare, checked **before** the body is read |
| Max concurrent connections | `max_connections` (default 64). A full queue answers `503` on the socket and closes |
| Max request body | `max_body` (default 64 MiB) → `413` |
| Query default limit | `default_limit` (default 1000) when `limit` is absent |
| Query hard cap | `max_limit` (default 50000); a larger explicit `limit` is `400`, never truncated |
| Keep-alive idle timeout | 5 s between requests, 30 s for a live request |
| Duplicate DB mount | Refused at startup (name or canonical path) |
| DB names | `[a-zA-Z0-9._-]+`, not `.`/`..`, Windows reserved names rejected, case-folded uniqueness |

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
  service mode); `format` ∈ `auto | v1 | v2` (default: the service create
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
  **`offset` is applied after sorting.** (Known v2 defect, herrbasan/nDB#15:
  with no `sort` and no `limit`, `offset` is ignored — see
  `tests/refdb_oracle_tests.rs`.)
- Without `fields`: raw, pre-serialized documents are streamed:
  `{"ok":true,"total":N,"count":M,"results":[…],"took_ms":x}`.
- With `fields`: `_id` is always included in each projected document.

Errors: `400 invalid_filter | invalid_query | empty_body | invalid_json`,
`500 internal_error`.

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
