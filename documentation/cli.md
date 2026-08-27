# nDB CLI Reference

> The `ndb` command-line tool. A third interface alongside the Rust crate and the Node.js bindings — used for **maintenance and administration**, plus one long-running mode: `serve`.

## Folder model

The CLI uses the same **database-as-a-folder** layout as the library and the primary consumer (LLM-Gateway-Chat). `ndb init` creates:

```
mydb/
├── meta.json      # Engine metadata (version, created, buckets) — CLI/migration only, core ignores it
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
| `verify <path>` | Check for corruptions and missing file references |
| `recover <src> --output <dest>` | Salvage surviving rows from a corrupted database |
| `dump <path>` | Print all documents as JSON Lines to stdout |
| `config <get\|set> <key> [value]` | Read/write `meta.json` keys (dot notation) |
| `query <path> <query_ast>` | Run a JSON query (equality / `$eq`) and print matches |
| `serve <path> [options]` | Run as a resident HTTP daemon (blocks; see below) |

## `ndb serve`

Long-running HTTP daemon: loads the database once, keeps it in memory, serves queries until killed. Design and rationale: `docs/ndb-serve-design.md`.

```
ndb serve <path/to/data.jsonl> [--bind 127.0.0.1|0.0.0.0] [--port 8323] [--token <secret>]
```

- The file must already exist (`ndb init` first) — serve fails fast on a missing path.
- `--token <secret>`: clients must send `Authorization: Bearer <secret>`. Required for LAN binds; 401 otherwise.
- `--bind 0.0.0.0` exposes the server on the LAN. Windows Firewall prompts (or needs a `netsh advfirewall` rule) on first run. Never forward the port to the internet.

Routes (all JSON unless noted):

| Route | Description |
|-------|-------------|
| `POST /query` | `{filter, fields, sort: {field, dir}, limit, offset}` — Layer-3 AST, server-side projection, default limit 1000 |
| `POST /search` | Full-text search — see below |
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

Limits: 64 concurrent connections (503 over), 64 MB request bodies (413), 30 s read timeout, query limit capped at 1000. Unknown query operators are 400, never silently-match.

### `POST /search`

Full-text search over a text-indexed field. Create the index first:

```
curl -X POST .../index -d '{"field": "content", "type": "text"}'
```

Then query:

```json
{
  "field": "content",
  "mode": "and",
  "case_sensitive": false,
  "queries": [
    {"type": "phrase", "value": "The hare was tired"},
    {"type": "phrase", "value": "at the end of the race"},
    {"type": "term",   "value": "paul"},
    {"type": "term",   "value": "yesterday"},
    {"type": "term",   "value": "rabbit", "exclude": true}
  ],
  "fields": ["title"],
  "limit": 50,
  "offset": 0
}
```

- `mode`: `"and"` (default) — every positive query must match; `"or"` — any.
- `queries[].type`: `term` (whole token) \| `phrase` (contiguous words) \|
  `prefix` (`"yester"` → `"yesterday"`). `exclude: true` negates a query.
- `case_sensitive`: default false.
- `fields` / `limit` / `offset`: response projection and paging
  (`total` in the response is the full match count).
- Response: `{ok, count, total, results: [...]}`.
- Requires a text index on the field (`POST /index` with `type: "text"`);
  otherwise the request fails loud with `no text index on '<field>'`.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | General error |
| `2` | Corruption detected (from `verify`) |
| `3` | Database locked / `.readonly` missing |

## Notes

- **`destroy` checks for a `.lock` marker** and refuses if present (`EXIT_LOCKED`). The library does not create `.lock`/`.readonly` markers, so these checks are effectively advisory for CLI-created databases.
- **`config`** operates on `meta.json` in the *current directory* — run it inside the database folder.
- **`merge`** resolves ID collisions by comparing a `_modified` field on each document; documents without it default to `0`.
- **`verify`** checks for `_file` object references inside documents (the `{bucket, id, ext}` form) against the `_files/` tree, and validates JSON on every journal line.
- The `--consistent` export flag requires a `.readonly` marker file; without it the export is treated as "crash-consistent".
