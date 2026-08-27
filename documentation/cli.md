# nDB CLI Reference

> The `ndb` command-line tool. A third interface alongside the Rust crate and the Node.js bindings — used for **maintenance and administration**, not for live serving. Every invocation is a one-shot process: it loads the whole database, does one thing, and exits.

## Folder model

The CLI uses its own **database-as-a-folder** convention, created by `ndb init`:

```
mydb/
├── meta.json      # Engine metadata (version, created, buckets)
├── db.jsonl       # Document store
├── trash.jsonl    # Trash journal
└── buckets/       # File buckets (note: the CLI uses `buckets/`, the library uses `_files/`)
```

> ⚠️ **Known inconsistency:** the CLI's file names differ from the library and from the main consumer. The library creates `_files/` (buckets) and `_trash/docs/` (trash) as siblings of `data.jsonl`; the CLI expects `buckets/` and `trash.jsonl`. Run CLI maintenance against a folder created by `ndb init` — don't point CLI commands at a library-managed folder and expect them to see the same buckets/trash.

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

## Notes & known bugs

- **`init` deletes the database it just created** (copy-paste from `destroy`): `ndb init` creates the folder, then removes it and prints "Destroyed database". This is a real bug — do not rely on `init` producing a usable database until it is fixed.
- **`destroy` checks for a `.lock` marker** and refuses if present (`EXIT_LOCKED`). The library does not create `.lock`/`.readonly` markers, so these checks are effectively advisory for CLI-created databases.
- **`config`** operates on `meta.json` in the *current directory* — run it inside the database folder.
- **`merge`** resolves ID collisions by comparing a `_modified` field on each document; documents without it default to `0`.
- **`verify`** checks for `_file` object references inside documents (the `{bucket, id, ext}` form) against the `buckets/` tree, and validates JSON on every journal line.
- The `--consistent` export flag requires a `.readonly` marker file; without it the export is treated as "crash-consistent".
