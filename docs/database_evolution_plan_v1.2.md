# nDB v1.1 — TTL Trash Purging & Auto File-Trash Plan

## Goal
Implement meaningful soft-delete with automatic orphaned-file trashing, persistent trash storage, and configurable TTL-based purging — exposed through the Node.js NAPI layer.

## Design Decisions

1. **Single persistent trash file per DB**: `_trash/docs/{dbname}.jsonl` — append-only, never compacted. Safety over performance. It will include a `_meta` header for future-proofing.
2. **Trash entry format**: Full document plus `_deleted` (timestamp) and `_trashed_files` (array of nURI strings auto-trashed with this doc).
3. **Auto file-trash on delete (O(1)**: `delete()` avoids full DB scans by using an in-memory **file reference counter**. If a file's count hits 0, it's orphaned and moved to file trash.
4. **Restore brings files back**: `restore()` restores the doc from trash AND restores any `_trashed_files`. It strictly **strips** engine metadata (`_deleted`, `_trashed_files`) before returning it to the active DB.
5. **compact() changes**: No longer archives tombstones to dated trash files. Tombstones are simply removed from `data.jsonl`.
6. **TTL scope & Folder-Led Purging**: Folder-based TTL. File trashes are purged strictly by the filesystem 'modified' timestamp, ignoring historical JSON to ensure no restored file is ever destroyed.
7. **Configuration**: `trashMode` ("manual" / "ttl" / "off"), `trashTtlSeconds`, `trashPurgeIntervalSeconds`. **Default is "manual"** (Infinite trash) for maximum safety out of the box.
8. **Graceful Exit**: The TTL background purger uses a channel-based cancellation (`recv_timeout`) instead of `sleep()` so Node.js can exit safely when the database is dropped.

## Phase 1: Trash File Infrastructure (`src/lib.rs`, `src/storage.rs`)

### `src/storage.rs` — Add trash helpers
- Add `append_doc_trash(trash_path, doc)` — appends a single JSON line to the trash file (no meta header needed).
- Add `read_trash(trash_path)` — reads all lines from trash file, returns `Vec<Value>`. Reuses existing line-parsing logic (skip corrupted lines with warning).

### `src/lib.rs` — Trash path and delete/restore rewrite
- Add `trash_doc_path() -> PathBuf` helper: `base_dir.join("_trash").join("docs").join(filename)` where filename = `path.file_name()` of the main DB file.
- Modify `delete(id)`:
  - Before removing doc from memory, extract file refs via `extract_file_refs`.
  - For each ref, check if any other active doc references it. If orphaned, `bucket.delete(&file_ref)` and collect into `_trashed_files` vec.
  - Build trash entry: full doc + `_deleted` (timestamp) + `_trashed_files` (array of strings, omit if empty).
  - Append trash entry to trash file via `storage::append_doc_trash`.
  - Existing behavior continues: write tombstone to data.jsonl, remove from HashMap, add to `deleted` set.
- Modify `restore(id)`:
  - Instead of scanning `data.jsonl`, read trash file via `storage::read_trash`.
  - Scan backwards to find the most recent entry for this `_id`.
  - Remove `_deleted` and `_trashed_files` fields from the doc copy.
  - Append restored doc to data.jsonl.
  - For each entry in `_trashed_files`, call `bucket.restore(hash, ext)`.
  - Remove ID from `deleted` set, insert doc into HashMap.
  - Return `Error::NotFound` if no trash entry exists.
- Add `purge_trash()` public method:
  - If `trash_mode` is `TTL`, calculate cutoff = now - TTL.
  - Read trash file, filter entries where `_deleted` > cutoff.
  - Rewrite trash file atomically via `storage::rewrite_atomic`.
  - For each removed entry's `_trashed_files`, call `FileBucket::purge_trash` on each bucket with the TTL duration (needs bucket.rs fix in Phase 2).
  - Return count of purged entries.
- Modify `compact()`:
  - Remove the tombstone-archive logic (lines ~1088-1107 in current code). `compact()` now only rewrites active docs. No dated trash archives are created.

### Tests to add/update
- Update `restore_deleted_doc`: same flow works (delete writes trash, restore reads trash).
- Update `compact_removes_tombstones`: verify no dated archives created; deleted doc remains restorable via trash file.
- Add `delete_trashes_orphaned_files`: insert doc with file ref, delete doc, verify file moved to trash.
- Add `restore_brings_files_back`: delete + restore, verify file back in active bucket.
- Add `delete_does_not_trash_shared_file`: two docs share a file ref, delete one, verify file still active.

## Phase 2: Fix FileBucket::purge_trash (`src/bucket.rs`)

- Fix `purge_trash(&self, older_than: Duration)` to actually use the parameter:
  - For each file in trash dir, check `metadata.modified()`.
  - Only delete files where `SystemTime::now() - modified > older_than`.
  - If `Duration::ZERO`, delete all (backward-compatible behavior for `clear_trash`).
- No changes needed to `delete()`, `restore()`, `list()`.

## Phase 3: TTL Background Purger (`src/lib.rs`)

### Database struct changes
- Add fields:
  - `trash_ttl: Option<Duration>`
  - `trash_purge_interval: Option<Duration>`
  - `ttl_thread: Option<std::thread::JoinHandle<()>>`
- `JoinHandle<()>` is `Send + Sync`, so this is safe inside `Arc<Database>`.

### `with_trash_mode` enhancement
- `with_trash_mode(mode: TrashMode)` stays for Rust API.
- Add `with_trash_ttl(ttl: Duration, purge_interval: Duration) -> Self` setter.

### Background thread
- In `Database::open()` / `open_in_memory()`, after construction:
  - If `trash_mode == TrashMode::TTL` and `trash_ttl` is set:
    - Set up a channel: `let (tx, rx) = std::sync::mpsc::channel();`
    - `std::thread::spawn(move || { while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(purge_interval) { purge_trash_static(...); } })`.
    - Store `tx` and handle in `ttl_thread`.
- The thread calls standalone purge logic (does not need `&self` access; works on paths directly).

### `Drop` impl
- Add `impl Drop for Database`:
  - If we have the kill channel `tx`, call `tx.send(())`. This wakes the thread immediately.
  - Call `.join()` on the thread handle to ensure clean exit, preventing Node.js hangs.

## Phase 4: NAPI Exposure (`napi/src/lib.rs`, `napi/index.js`)

### `napi/src/lib.rs` — DatabaseOptions
Extend `DatabaseOptions`:
```rust
#[napi(object)]
pub struct DatabaseOptions {
    pub persistence: Option<String>,
    pub interval: Option<u32>,
    pub trash_mode: Option<String>,        // "manual" | "ttl" | "off"
    pub trash_ttl: Option<u32>,            // seconds
    pub trash_purge_interval: Option<u32>, // seconds
}
```

### `napi/src/lib.rs` — Database::open wiring
In `Database::open`, after persistence setup:
- If `trash_mode` provided, map to `TrashMode` enum.
- If `trash_ttl` provided, set via `with_trash_ttl`.
- Default: `TrashMode::Manual` (no TTL, no auto-purge).

### `napi/src/lib.rs` — Add purge_trash method
```rust
#[napi]
pub fn purge_trash(&self) -> Result<u32> {
    let count = self.inner()?.purge_trash()
        .map_err(|e| Error::from_reason(format!("Purge trash failed: {}", e)))?;
    Ok(count as u32)
}
```

### `napi/index.js` — JS wrapper updates
- `Database.open(path, options)`:
  - Pass through `trashMode`, `trashTtl`, `trashPurgeInterval` to native `open()`.
- Add `purgeTrash()` method:
  ```js
  purgeTrash() {
    return this._native.purgeTrash();
  }
  ```

## Phase 5: Cleanup & Documentation

### `AGENTS.md`
- Update status table:
  - Delta updates: "Complete" (already done, just doc fix)
  - TTL-based trash purging: "Complete"
  - Schema validation: "Intentionally not implemented"
  - nURI link enforcement: "Not implemented"
  - Bucket migration: "Not implemented"

### `documentation/architecture.md` and `documentation/nodejs-api.md`
- Update trash/restore sections to reflect persistent trash file.
- Add TTL configuration examples.
- Document auto file-trash behavior.
- **Note:** All new features and changes (options, default manual mode, file references) MUST be thoroughly documented in the `/documentation` directory when implemented.

## Phase 6: Future Ideas
- **`db.getHistory(id)`**: Since `data.jsonl` is append-only, it naturally holds a version history of every document state before compaction. In the future, we could expose a method to return an array of all states an object has ever been in, acting like a time machine for data. Allows for stepping back in time for as many "steps" as there is data available.

## Files Changed

| File | Changes |
|------|---------|
| `src/lib.rs` | Trash path, delete(), restore(), compact(), purge_trash(), TTL thread, Drop impl |
| `src/storage.rs` | append_doc_trash(), read_trash() |
| `src/bucket.rs` | Fix purge_trash() to respect older_than |
| `napi/src/lib.rs` | DatabaseOptions extension, open() wiring, purge_trash NAPI method |
| `napi/index.js` (Answered & Decided)

1. Should `_trashed_files` be stripped from the restored document? **Decided: Yes.** Engine metadata must never leak into the application layer.
2. Should the trash file get a `_meta` header? **Decided: Yes.** Important for future-proofing and schema changes.
3. How does TTL handle manual restores? **Decided:** Folder-led purging purely uses the filesystem 'modified' timestamp in `_trash/files/`. Because manual restores move files out of this folder, they are 100% safe from TTL background destruction.

1. Should `_trashed_files` be stripped from the restored document, or left in as metadata? (Plan says strip it — clean restore.)
2. Should the trash file get a `_meta` header like data.jsonl? (Plan says no — unnecessary overhead for a secondary file.)
3. If TTL purges a trash entry whose files were already restored manually, should the files still be purged? (Plan says yes — TTL is physical destruction. Manual restore should happen before TTL expires.)

## Risk: Breaking Changes

- `restore()` now requires the trash file. Existing databases that have deleted docs but no trash file (from before this change) will not be restorable. This is acceptable — it was already broken after `compact()`.
- `compact()` no longer creates dated trash archives. Anyone relying on those archives for forensics will lose that. Mitigation: trash file is the new canonical archive.
