# nDB CLI Reference

> The `ndb` command-line tool. The second interface alongside the Rust crate — used for **maintenance and administration**, plus one long-running mode: `serve`. (The Node.js binding is gone as of v2.0.0; Node consumers use `serve`.)

## Folder model

The CLI uses the same **database-as-a-folder** layout as the library and the primary consumer (LLM-Gateway-Chat). `ndb init` creates a **v3 (RAM-first)** folder:

```
mydb/
├── meta.json        # Engine metadata (version, created, buckets) — written by CLI/migration; the core reads the `buckets` policy block at open
├── manifest.json    # v3 generation/lineage authority (`version: 3`)
├── base.gen_1.ndb   # Streamed immutable snapshot (CRC-32 framed records)
├── wal.gen_1.ndb    # Op-level append log, one `Commit` frame per batch
├── _files/          # File buckets
└── _trash/          # Trash (the library manages _trash/docs/ on demand)
```

A v1 folder keeps the same shape with `data.jsonl` in place of the binary
artefacts.

## Usage

```
ndb <command> [args...]
```

Commands:

| Command | Description |
|---------|-------------|
| `init <path> [--buckets a,b] [--ram]` | Create a new folder — **v3 RAM-first by default** (`--ram`/`--v3` accepted; `--v2` is refused) |
| `migrate <path> [--ram \| --v3]` | v1 → v3 RAM-first (the only target) |
| `destroy <path> [--force]` | Permanently delete a database (prompts for confirmation unless `--force`) |
| `info <path>` | Show statistics (doc count, disk usage, fragmentation, buckets) |
| `compact <path>` | Rewrite the journal with only active documents |
| `export <path> <dest> [--consistent]` | Create a portable snapshot |
| `export <path> <out.jsonl> --jsonl` | Export human-readable, id-sorted JSONL (works on v1 and v3) |
| `import <src> <path> [--force]` | Restore a snapshot |
| `merge <base> <merge-in> --output <dest>` | Combine two databases (resolves `_modified` collisions) |
| `verify <path>` | Check for corruptions, missing file references, and item-bucket integrity |
| `fsck <path>` | Full integrity audit — v1 journal or a v3 RAM-first folder |
| `repair <path>` | Quarantine a corrupt v1 journal or v3 WAL generation and truncate it at the last good record |
| `recover <src> --output <dest>` | Salvage surviving rows from a corrupted database |
| `dump <path>` | Print all documents as JSON Lines to stdout |
| `config <get\|set> <key> [value]` | Read/write `meta.json` keys (dot notation) |
| `query <path> <query_ast>` | Run a JSON query (equality / `$eq`) and print matches |
| `serve <path> [options]` | Run as a resident HTTP daemon (blocks; see below) |

## Storage formats

| Format | Layout | Created by |
|--------|--------|------------|
| v1 | `data.jsonl` | `Database::open` on an empty or v1 folder |
| v3 (RAM-first, default) | `manifest.json` (`version: 3`) + `base.gen_*.ndb` + `wal.gen_*.ndb` | `ndb init <path>` (or `--ram`/`--v3`) |

`--v3` is accepted as an alias for `--ram` everywhere it appears.

**v2 (binary/mmap) was retired on 2026-11-25.** `src/v2_engine.rs`,
`src/v2_format.rs`, `src/v2_json_scan.rs` and `src/mmap.rs` were removed (the
code stays in git history, `9118718` and earlier). `--v2`, `format: "v2"` and a
`manifest.json` with `format_version: 2` are all refused loudly: convert the
folder with the preserved v2/transition binary
(`ndb migrate <path> --ram`) first, then reopen it with this build.

## Migration

**`ndb migrate <path>` converts a v1 journal to v3 RAM-first** — that is the
only target:

```
ndb migrate <path>            # v1 → v3 RAM-first
ndb migrate <path> --ram      # same (or --v3)
```

- **v1** (`data.jsonl`) — the journal is retired to `data.jsonl.v1.bak` after the
  v3 folder is published.
- **v2** — refused; see the retirement note above.

The migration stages into `.migrating_ram/`, reopens the staged folder and
verifies **document by document** (count, every `_id`, every JSON value, the
lineage, and every `hash`/`btree` index declaration) before anything live
moves — and then repeats the parity check against the published folder.
`source_epoch_uuid` and `last_seq` are preserved; `min_available_seq` becomes
`last_seq + 1` because every document is folded into `base.gen_1.ndb` and no
event history is carried over (advertising a lower floor would promise replay
that does not exist).
An already-v3 folder is refused (`already a v3 (RAM-first) database`), as is a folder whose `.lock` is held by a running service.

## `ndb serve`

Long-running HTTP daemon: loads the database once, keeps it in memory, serves queries until killed. Design and rationale: `docs/ndb-serve-design.md`. Frozen route contract (incl. the `/v2/dbs/{name}/{search,schema,profile/classification,doc/{id}/peek}` surface): `docs/contract_http-api.md`.

```
ndb serve <path/to/data.jsonl> [--bind 127.0.0.1|0.0.0.0] [--port 8323] [--token <secret>]
       [--threads <n>] [--logs-dir <dir>] [--persistence immediate|scheduled|lazy]
       [--scheduled-ms <n>] [--max-limit <n>] [--default-limit <n>] [--format auto|v1|ram]

ndb serve --db <name>=<path> ...       # mount named databases (repeatable)
ndb serve --dir <root>                 # mount every child DB folder; enables POST /v2/dbs
ndb serve --config <file.json>         # bind/port/token/persistence/scheduled_ms/dir/
                                       # max_limit/default_limit/logsDir/dbs; CLI flags win
```

- `--threads <n>`: tokio runtime worker threads (`0`, the default, means one per core). The transport's connection cap (`max_connections`, default 1024) is separate and settable **only** through the Rust `ServerConfig` API — it is not a CLI flag.
- `--logs-dir <dir>`: `nlogger` JSONL output — the rotated `main-0.log` … `main-9.log` stream plus one `ndb-<session>.log` per process (default `./logs`).
- `--persistence immediate|scheduled|lazy` (default `immediate`); `--scheduled-ms <n>` sets the flush interval for `scheduled` (default 5000).
- `--max-limit <n>` (default 50000) is the hard cap for an explicit query `limit`; `--default-limit <n>` (default 1000) is the page size when `limit` is omitted.
- `--format auto|v1|ram` (default `ram`) selects the format for databases created via `POST /v2/dbs` or a `--db` mount of a not-yet-existing path.

- The positional path must already exist (`ndb init` first) — serve fails fast on a missing path.
- `--token <secret>`: clients must send `Authorization: Bearer <secret>`. Required for LAN binds; 401 otherwise.
- `--bind 0.0.0.0` exposes the server on the LAN. Windows Firewall prompts (or needs a `netsh advfirewall` rule) on first run. Never forward the port to the internet.

Routes (all JSON unless noted):

| Route | Description |
|-------|-------------|
| `POST /query` | `{filter, fields, sort: {field, dir}, limit, offset}` — Layer-3 AST, server-side projection, default limit 1000. Response `{ok, total, count, results}` — `total` is the pre-pagination match count. Also accepts the standalone-contract options (`sort_keys`, `cursor`, `text`, `max_scan_docs`); pages report `scanned_docs`, `used_indexes`, `next_cursor` |
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

Limits: 1024 concurrent connections (503 over, no queue), 64 MB request bodies (413), 30 s read/idle/write bounds, a 10 s graceful drain that flushes every mounted database, and a query limit capped at `--max-limit` (default 50000). Unknown query operators are rejected on every path — 400 over HTTP, an error from the library — never silently-match.

This table is the **legacy single-DB** surface (`default`). The `/v2/dbs/{name}` surface adds the same routes per database plus `POST …/search` (lexical, `text` required), `GET|PUT …/schema`, `POST …/profile/classification`, `POST …/doc/{id}/peek` and `GET …/snapshot`. Frozen shapes: `docs/contract_http-api.md`.

### Lexical search (standalone contract, Phase 4A)

The native inverted index (`src/search.rs`, `type: "text"` in `POST /index`, the `--text-index` flag) was **removed in v2.0.0** (zero production callers). The standalone contract adds back a **basic, scan-based lexical** `POST /v2/dbs/{name}/search` and the `text` option on `POST …/query`: AND-of-terms plus `"quoted phrases"`, case-insensitive, over `text`-mapped fields or every string field. There is no ranking, stemming, vector or hybrid scoring — those are **`nVDB`'s** job (`nvdb serve`), which `mcp_server`'s query facade (`src/lib/ndb-query-facade.js`) prefers when it is online and fresh. A folder holding old `_index_*.fti` files still opens: they are warned about and ignored.

`$in` on array fields matches elements (`{"categories": {"$in": ["A"]}}` finds docs whose `categories` contains `A`).

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | General error |
| `2` | Corruption detected (`verify`, `fsck`) |
| `3` | Database locked / `.readonly` missing |

## Notes

- **`destroy` checks for a `.lock` marker** and refuses if present (`EXIT_LOCKED`). The library now creates a real OS lock file (`.lock` beside a folder database's `data.jsonl`, `<file>.lock` for a flat journal) while a `Database` is open; `ndb` maintenance commands open the database, so run them against a database no service has mounted.
- **`repair`** is the recovery path for the corruption `Database::open` refuses: it copies the journal to `<file>.corrupt.<timestamp>.bak`, truncates at the last complete record, fsyncs, and prints the preserved record count plus the lost byte/line range. A torn *final* line never needs it — opening truncates that automatically with a warning. On a **v3** folder it repairs the WAL generations (`wal.gen_*`, and `pending_wal_gen` when a stalled compaction left one) under the maintenance lock, printing the quarantined copy and the sequence the lineage now ends at. Note that `manifest.json`’s `last_seq` is published by compaction, so it may lag the WAL — `fsck` reports that as a warning, not corruption.
- **`config`** operates on `meta.json` in the *current directory* — run it inside the database folder.
- **`merge`** resolves ID collisions by comparing a `_modified` field on each document; documents without it default to `0`.
- **`verify`** checks every file reference a document holds against the `_files/` tree — the nested `_file` object form (`{bucket, id, ext}`) **and** every string form the engine's mark phase recognizes (compact `bucket:hash.ext`, `/api/buckets/…`, `/file/…`, `/files/…`, `/v2/dbs/<db>/files/…`), using the same normalizer as `gc_buckets`. It also validates JSON on every journal line. For buckets declared `kind: "items"` in `meta.json` it additionally verifies each live item's original against its committed `size`/`sha256`, that reserved items have their folder, and flags orphan folders. On a v3 folder `verify` delegates to `fsck`.
- The `--consistent` export flag requires a `.readonly` marker file; without it the export is treated as "crash-consistent".
