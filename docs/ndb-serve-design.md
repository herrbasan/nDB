# ndb serve — Design Document

> Status: DESIGN SETTLED, not yet implemented. Tracks issue #2.
> Decisions agreed 2026-08-26 (herrbasan + Copilot session). Reviewed 2026-08-27 (Kimi) — see "Core prerequisites".
> Change via this file, not memory.

## Motivation

nDB has two interfaces: napi (in-process Node) and CLI (one-shot). Neither fits the case of a **large shared database (4–10 GB) queried by Node apps that must not hold the data in their own heap**.

- napi: every Node process embeds its own full copy of the DB in the Node heap. N apps = N copies. At 10 GB this is untenable, and napi's ~2 GB string marshaling limit caps bulk reads (`iter()`) outright.
- CLI: process spawn per operation = full JSONL replay each time. Useless for queries.

`ndb serve` is the third interface: a **resident Rust daemon** that owns the database in memory. Data and computation stay in Rust; only results cross to the Node process. One process holds the 10 GB; N clients share it.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Transport | Hand-rolled HTTP/1.1 on `std::net::TcpListener` | Zero new crates. ~400 lines. curl + fetch + every language for free. Sync I/O is sufficient for a handful of local/LAN clients. |
| Concurrency | Thread-per-connection, small cap | Reads already concurrent (`RwLock`); writes serialize on the write lock — maps 1:1 onto nDB's single-writer model. No async runtime. |
| API shape | Query-oriented, not CRUD-mirrored | The point is computation in Rust. One powerful `/query` beats mirroring 20 methods. |
| Framing | `Content-Length` only, keep-alive | We control both ends; no chunked encoding needed. |
| Process model | Resident daemon, loads JSONL once at boot, never exits on its own | Boot replay is a one-time cost. The server IS the database process (redis-server model). |
| Network | Bind `0.0.0.0` (configurable to `127.0.0.1`), static bearer token, no TLS | Trusted LAN only, no internet exposure, no user management. Token is a door, not access control: holder = trusted. |
| Dependencies | Zero new crates | Longevity rule. tokio/hyper/axum explicitly rejected. |

## API

All responses JSON. Errors as `{"error": "..."}` with appropriate 4xx/5xx.

### Reads

- `POST /query` — **the workhorse.** Body:
  ```json
  {
    "filter": { "status": { "$in": ["active"] }, "meta.views": { "$gte": 10 } },
    "fields": ["title", "messages.0.role"],
    "sort": [{ "field": "meta.views", "dir": "desc" }],
    "limit": 100,
    "offset": 0
  }
  ```
  - `filter` = Layer-3 AST verbatim (`$eq $ne $gt $gte $lt $lte $in $nin $exists $and $or $not`, dot-notation).
  - `fields` = server-side projection. **Mandatory at multi-GB scale**: full-doc responses on a broad query can ship hundreds of MB. Responses must be proportional to what was asked, not what is stored.
  - `limit` **defaults to a server-side cap** (e.g. 1000) so a missing filter never streams the whole DB.
- `GET /doc/:id` (+ optional `?fields=`) — O(1) HashMap path.
- `POST /find` — simple `{"field": "...", "value": ...}` and range form, for when a full AST is overkill.

### Writes

- `PUT /doc` — insert (full document).
- `PATCH /doc/:id` — delta ops: `set`, `remove`, `array_push`. **Critical at multi-GB scale**: full-doc updates of huge documents (3 MB conversation objects) are the catastrophic-I/O case deltas were built for.
- `DELETE /doc/:id` — soft delete (tombstone), as in the core API.

### Ops

- `GET /stats` — doc count, index list, log size.
- `POST /compact` — rewrite JSONL to active docs only. At 10 GB with `array_push` deltas the log grows fast; compaction temporarily doubles disk usage.
- `POST /index` — create hash/BTree index (`{"field": "...", "type": "hash"|"btree"}`).
- Bucket file endpoints — mirror `store_file` / `get_file` / `release_file` / `gc_buckets`.

## Security model (explicitly minimal)

- One static token: `--token <secret>` flag or env var. Clients send `Authorization: Bearer <token>`. Wrong/missing → 401, connection dropped.
- No TLS (LAN traffic, accepted threat model — same as other gateway services).
- No users, no per-route restrictions, no rate limiting. Token holder = trusted. Build more only when a real need appears.
- Router must not forward the port. Windows Firewall will prompt on first `0.0.0.0` bind — document the one-time `netsh advfirewall` rule in README so LAN clients don't mysteriously time out.

## CLI surface

Extends the existing subcommand pattern (`src/bin/ndb.rs`, restructured in 01d7029):

```
ndb serve <path/to/data.jsonl> [--bind 0.0.0.0] [--port 8323] [--token <secret>]
```

## Client pattern (Node side)

The daemon runs standalone (service / scheduled task / auto-restart wrapper) — **not** as a child of an app process, so app restarts don't reload 10 GB and app crashes don't kill the DB. First-needing app may spawn it detached if not up; all apps retry-connect until the socket answers.

## Core prerequisites (fix BEFORE writing src/server.rs)

Review 2026-08-27 (Kimi, verified against code) found the design assumed behavior the core doesn't have. These must land in the core first — otherwise the server bakes the bugs into a network API.

1. **Sort silently no-ops on nested fields.** `lib.rs:1198` sorts via `a.get(field)` — top-level only. The doc's own example (`sort: meta.views`) compares all-`Null` and no-ops. Fix: reuse dot-notation resolution for sort keys. Also: `QueryOptions` supports one `sort_by`; the API's multi-key sort array needs new code regardless.
2. **Projection paths can't traverse arrays.** `field_get` (lib.rs:270) uses `Value::get(&str)` — no numeric segments, so `messages.0.role` fails. Meanwhile `apply_path_set`'s walker *does* handle them. Two path engines with different capabilities — extract one shared array-aware `path_get` from the walker, use it for projection, sort, and `GET /doc/:id?fields=` (one code path).
3. **Delta ops don't maintain secondary indexes** — `set()`/`remove()`/`array_push()` mutate the doc but skip index updates (only `insert`/`update`/`delete` do). Known production bug (memory #1233: false pending-embed flags). PATCH makes stale indexes the *normal* state; `POST /find` (indexed) and `POST /query` would disagree after every PATCH. **Fix this first — live bug independent of the server.**
4. **Unknown query operators silently match everything** (`lib.rs:287` `_ => true`). A client typo (`$eqq`) becomes a full-DB scan at 10 GB. The server must AST-validate at the boundary and return 400 on unknown operators. Hand-written client JSON is exactly where boundary tolerance stops.
5. **Query deep-clones every match under the read lock** (`lib.rs:1184-1189` `.cloned().collect()`), then sorts/limits after. Broad query + `limit: 10` still clones GBs and stalls all writers behind `docs.read()`. Fix: under the lock extract only `(id, sort-key values, projected fields)` tuples; release; sort/paginate/materialize the page outside. Projection pushdown before clone.
6. **`array_push` is top-level-only** (`lib.rs:853` `obj.get_mut(field)`), while `set`/`remove` take dot paths. `PATCH array_push` on `threads.2.messages` would silently create a key literally named `"threads.2.messages"`. Unify on the walker.

Minor core-adjacent: `set`/`remove` silently skip unresolvable paths — over HTTP, return whether each op applied (see Writes below).

## Implementation notes

- New module in the main crate (e.g. `src/server.rs`), wired as the `serve` subcommand in `src/bin/ndb.rs`. Library users can also call `server::serve(db, config)`.
- HTTP parsing: request line + headers, read `Content-Length` body, route, respond, keep-alive loop. Reject anything malformed with 400 — fail loud, no lenient recovery.
- **`Expect: 100-continue`:** curl sends it for bodies over ~1 KB; a server that doesn't answer `100 Continue` stalls every curl PUT for a second. Must handle — "curl for free" is a stated rationale.
- **Check the token BEFORE reading the request body.**
- **Read timeout** on every connection (`set_read_timeout`) — kills the slow-loris class and reclaims threads from crashed clients holding half-open connections out of the small pool.
- **Body cap** (generous, e.g. 64 MB) on `PUT /doc` / `PATCH` — fail loud 413 on excess `Content-Length` rather than OOM on a client bug.
- **Thread-cap exhaustion → 503**, fail loud, no queueing.
- **`POST /compact` and `POST /index` hold the writer lock for minutes at 10 GB** — every write hangs until client timeouts fire. Return `202 Accepted` + expose completion via `/stats`, and document required client timeouts.
- **Bucket endpoints use raw bytes, not base64-in-JSON** (33% overhead + full buffering defeats the purpose): `PUT /file/:bucket` with file body, `GET /file/:bucket/:hash.ext` with real `Content-Type`.
- **Writes:** add `PUT /doc/:id` (full replace — maps to existing core `update()`) alongside insert + deltas. `PATCH` response includes per-op `applied: true/false` so wrong-path writes surface immediately.
- SHA-1 not needed (no WebSocket in v1). WebSocket/push (watch/subscribe) is a later layer on the same listener if a real need appears.
- Tests: spin up server on ephemeral port in-process; round-trip query/projection/delta/limit; token 401 path; concurrent readers during a write; 400 on unknown operator; 413 on oversized body; 503 on thread exhaustion.
