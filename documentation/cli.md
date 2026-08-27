# nDB CLI Reference

> The `ndb` command-line tool. A third interface alongside the Rust crate and the Node.js bindings — used for **maintenance and administration**, not for live serving. Every invocation is a one-shot process: it loads the whole database, does one thing, and exits.

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
