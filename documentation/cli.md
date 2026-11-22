# nDB CLI Reference

> The `ndb` command-line tool. The second interface alongside the Rust crate — used for **maintenance and administration**, plus one long-running mode: `serve`. (The Node.js binding is gone as of v2.0.0; Node consumers use `serve`.)

## Folder model

The CLI uses the same **database-as-a-folder** layout as the library and the primary consumer (LLM-Gateway-Chat). `ndb init` creates:

```
mydb/
├── meta.json      # Engine metadata (version, created, buckets) — written by CLI/migration; the core reads the `buckets` policy block at open
├── data.jsonl     # Document store
├── _files/        # File buckets
└── _trash/        # Trash (the library manages _trash/docs/ on demand)
```

## Usage

```
ndb <command> [args...]
```

Commands:

| Command | Description |
|---------|-------------|
| `init <path> [--buckets a,b]` | Create a new database folder |
| `destroy <path> [--force]` | Permanently delete a database (prompts for confirmation unless `--force`) |
| `info <path>` | Show statistics (doc count, disk usage, fragmentation, buckets) |
| `compact <path>` | Rewrite the journal with only active documents |
| `export <path> <dest> [--consistent]` | Create a portable snapshot |
| `import <src> <path> [--force]` | Restore a snapshot |
| `merge <base> <merge-in> --output <dest>` | Combine two databases (resolves `_modified` collisions) |
| `verify <path>` | Check for corruptions, missing file references, and item-bucket integrity |
| `repair <path>` | Quarantine a corrupt journal (`<file>.corrupt.<ts>.bak`) and truncate it at the last good line |
| `recover <src> --output <dest>` | Salvage surviving rows from a corrupted database |
| `dump <path>` | Print all documents as JSON Lines to stdout |
| `config <get\|set> <key> [value]` | Read/write `meta.json` keys (dot notation) |
| `query <path> <query_ast>` | Run a JSON query (equality / `$eq`) and print matches |
| `serve <path> [options]` | Run as a resident HTTP daemon (blocks; see below) |

## `ndb serve`

Long-running HTTP daemon: loads the database once, keeps it in memory, serves queries until killed. Design and rationale: `docs/ndb-serve-design.md`.

```
ndb serve <path/to/data.jsonl> [--bind 127.0.0.1|0.0.0.0] [--port 8323] [--token <secret>]
       [--threads <n>]
```

- `--threads <n>`: worker threads — the HTTP connection pool size and the index (re)build thread count. `0` (default) = all logical cores (pool floor: 4). The pool pulls from a bounded queue (`--max-connections` deep); a full queue answers `503`.

- The file must already exist (`ndb init` first) — serve fails fast on a missing path.
- `--token <secret>`: clients must send `Authorization: Bearer <secret>`. Required for LAN binds; 401 otherwise.
- `--bind 0.0.0.0` exposes the server on the LAN. Windows Firewall prompts (or needs a `netsh advfirewall` rule) on first run. Never forward the port to the internet.

Routes (all JSON unless noted):

| Route | Description |
|-------|-------------|
| `POST /query` | `{filter, fields, sort: {field, dir}, limit, offset}` — Layer-3 AST, server-side projection, default limit 1000. Response `{ok, total, count, results}` — `total` is the pre-pagination match count |
| `POST /facets` | `{fields: [...]}` → `{ok, facets: {field: {term: count}}}` — array fields count per element, scalars per value, strings only |
| `POST /find` | `{field, value}` or `{field, min, max}` |
| `GET /doc/:id[?fields=a,b]` | Fetch one doc, optional projection |
| `PUT /doc` | Insert |
| `PUT /doc/:id` | Full replace |
| `PATCH /doc/:id` | `{ops: [{op: set\|remove\|array_push, path, value}]}` — delta ops, returns per-op `applied` flags |
| `DELETE /doc/:id` | Soft delete |
| `POST /index` | `{field, type: hash\|btree}` |
| `POST /compact` | 202 Accepted — runs in background, watch the server log |
| `GET /stats` | Document count |
| `PUT /file/:bucket` | Store file (body = raw bytes, `Content-Type` used as MIME) |
| `GET /file/:bucket/:hash.ext` | Fetch file — raw bytes with real Content-Type |

Limits: 64 concurrent connections (503 over), 64 MB request bodies (413), 30 s read timeout, query limit capped at 1000. Unknown query operators are rejected on every path — 400 over HTTP, an error from the library — never silently-match.

### No full-text search (v2.0.0)

`POST /search`, `type: "text"` in `POST /index`, the `text` parameter in `POST /query` and the `--text-index` flag were **removed in v2.0.0** along with `src/search.rs`. Full-text, lexical, vector and hybrid search are owned by **`nVDB`** (`nvdb serve`). nDB answers structured filters, sort, projection and pagination only. A folder holding old `_index_*.fti` files still opens: they are warned about and ignored.

`$in` on array fields matches elements (`{"categories": {"$in": ["A"]}}` finds docs whose `categories` contains `A`).

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | General error |
| `2` | Corruption detected (from `verify`) |
| `3` | Database locked / `.readonly` missing |

## Notes

- **`destroy` checks for a `.lock` marker** and refuses if present (`EXIT_LOCKED`). The library now creates a real OS lock file (`.lock` beside a folder database's `data.jsonl`, `<file>.lock` for a flat journal) while a `Database` is open; `ndb` maintenance commands open the database, so run them against a database no service has mounted.
- **`repair`** is the recovery path for the corruption `Database::open` refuses: it copies the journal to `<file>.corrupt.<timestamp>.bak`, truncates at the last complete record, fsyncs, and prints the preserved record count plus the lost byte/line range. A torn *final* line never needs it — opening truncates that automatically with a warning.
- **`config`** operates on `meta.json` in the *current directory* — run it inside the database folder.
- **`merge`** resolves ID collisions by comparing a `_modified` field on each document; documents without it default to `0`.
- **`verify`** checks for `_file` object references inside documents (the `{bucket, id, ext}` form) against the `_files/` tree, and validates JSON on every journal line. For buckets declared `kind: "items"` in `meta.json` it additionally verifies each live item's original against its committed `size`/`sha256`, that reserved items have their folder, and flags orphan folders.
- The `--consistent` export flag requires a `.readonly` marker file; without it the export is treated as "crash-consistent".
