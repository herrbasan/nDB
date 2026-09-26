# nDB Common Patterns

> Idioms for working with nDB that aren't obvious from the API signatures.
> These cover the three traps from the original docs gap: `get()` throwing, the
> singleton/config-record pattern, and soft-delete visibility.

---

## 1. `get()` throws on missing keys

`db.get(id)` **throws** if the ID doesn't exist or the document is soft-deleted. It does **not** return `null`. This is the single most surprising behavior in the API.

To read a key that may or may not exist:

```js
function readOptional(id) {
  try { return db.get(id); } catch { return null; }
}
```

To avoid the exception entirely when you only need existence, use `contains(id)`:

```js
if (db.contains(id)) {
  const doc = db.get(id);
}
```

---

## 2. Singleton / config-record pattern

nDB generates `_id` values on `insert()` — you cannot choose a well-known key like `__app_settings__`. `insertWithPrefix(prefix, doc)` only adds a prefix to a generated ID; it does not let you set a fixed one.

The idiomatic approach for a single app-settings document: use a marker field, find it on startup with `iter()`, and cache the auto-generated ID in a module variable. Subsequent writes use `update(cachedId, …)`.

```js
const SETTINGS_TYPE = 'app_settings';
let settingsId = null;
let settingsCache = {};

// Find the singleton on startup (iter() only returns active docs)
for (const doc of db.iter()) {
  if (doc && doc._type === SETTINGS_TYPE) {
    settingsId = doc._id;
    settingsCache = doc;
    break;
  }
}

function saveSettings(partial) {
  const merged = { ...settingsCache, ...partial, _type: SETTINGS_TYPE, updatedAt: Date.now() };
  if (settingsId) {
    db.update(settingsId, merged);
  } else {
    settingsId = db.insert(merged);
  }
  settingsCache = merged;
}
```

The same "find by marker on startup, cache the ID" approach generalizes to any well-known record.

---

## 3. Soft-deleted records are invisible to the API

`db.delete(id)` writes a tombstone to the journal. From the caller's point of view it looks like a hard delete:

- `iter()` skips soft-deleted documents.
- `get(id)` throws for them.
- `len()` / `isEmpty()` / `contains()` count only active documents.
- `deletedIds()` lists their IDs.

The deleted document is preserved (with a `_deleted` timestamp) in the persistent trash file (`_trash/docs/data.jsonl`) until compaction or TTL purging, and can be brought back with `restore(id)`.

**If you read the JSONL file directly** (e.g. with `fs.readFileSync`), you *will* see records of the form:

```json
{"_id":"...","_deleted":1700000000000}
```

These are tombstones — they take up journal space but are invisible through the API. Do not treat their presence as corruption.

> ⚠️ **Never bypass the API to edit the file.** Reading the JSONL to inspect data is fine, but writing to it directly while an nDB instance holds it open corrupts the append-only log (the server may also be writing concurrently). Use `insert()` / `update()` / `set()` / `remove()` as intended.

---

## 4. Writing large documents: delta patches vs full replacement

For large documents (e.g. a multi-megabyte conversation object), replacing the whole document on every change is O(N²) journal bloat. Prefer the delta operations:

| Operation | Writes to journal |
|-----------|-------------------|
| `db.update(id, fullDoc)` | Full document (append) |
| `db.arrayPush(id, field, value)` | Tiny patch |
| `db.set(id, path, value)` | Tiny patch |
| `db.remove(id, path)` | Tiny patch |

Deltas are replayed in memory on load and baked into a fresh base document on `compact()`.

---

## 5. Reading large corpora

`iter()` marshals the entire database across the napi boundary as one string and can exceed napi's ~2 GB string limit on very large databases. Page bulk reads with `queryWith` instead:

```js
const page = await db.queryWith(
  {},
  { limit: 500, offset: 0, sortBy: '_id', sortDir: 'asc' }
);
```
