# nDB Refactor Plan — from open GitHub issues

> Working document. Source: herrbasan/nDB issues #3–#10 (as of 2026-09-27) and local
> `main` @ `63264cb` (6 commits ahead of `origin/main`, clean tree).
> Ordering follows the dependency decisions already recorded in issue #9 ("after #4 and #7,
> alongside #10") and the settled nCMS direction (finish nDB, no rewrite).

---

## Compatibility contract (hard constraint, stated by David 2026-09-27)

**No phase may break the existing `hash` bucket system or any existing API/behavior a
consumer can observe.** Concretely:

1. **Hash buckets are the frozen default.** Layout (`_files/<bucket>/<hash8>.<ext>`),
   content-hash naming, dedup, refcounting (`releaseFile`, `gc_buckets`), nURI
   (`bucket:hash.ext`) and the whole-file `store`/`get` API stay byte- and semantics-identical.
   Item buckets (#9) are strictly additive: `kind` defaults to `"hash"`, so a database without
   the declaration activates nothing.
2. **Journal format is unchanged.** Every phase works within the append-only JSONL shape
   (documents, tombstones, delta ops). #7 reorders *when* state is committed relative to the
   append — it does not change *what* is appended. Old databases load identically; new logs
   replay on old code identically.
3. **API surface only grows.** New: `close()` (already landed), item-bucket calls, `meta.json`
   policy. Changed: nothing removed, no signature changes, no new required options.
4. **Louder ≠ breaking.** The one deliberate behavior change already landed (Phase 0):
   operations that used to *silently succeed while failing* now return `Err` (GC that can't
   enumerate a bucket, `delete()` that can't write its trash record, `insert()` of a
   non-object). Signatures and success-path behavior are unchanged; consumers see errors where
   corruption was previously hidden. This is the fail-fast correction, not a break — but it is
   called out in the changelog/push note so nobody mistakes it for a regression.
5. **Regression oracle, every phase:** the full existing Rust suite (phases 1–9) and the napi
   suite must pass *unmodified* — no test may be edited to accommodate a change. On top of
   that, LLM-Gateway-Chat's attachment flow (the production hash-bucket consumer) is the
   end-to-end oracle before #9 ships, per its acceptance list.
6. **Opt-in activation only.** `meta.json` reading (#10) treats a missing file/block as
   "no policy" = today's behavior; `kind` (#9) defaults to `"hash"`. Nothing activates for an
   existing database by upgrading the binary.

---

## Phase 0 — Land what is already fixed (closes #4, #5, #6, #8)

Six local commits, unpushed:

| commit | fixes |
|---|---|
| `a36d368` | #4 (GC counts/errors), #6 (insert shape guard) |
| `748ccb0` | #6 (poisoned-lock abort at napi boundary) |
| `fc23006` | #4 (tombstone record as delete precondition) |
| `faf071c` | #4 (report ancillary failures, never raise on ordinary outcome) |
| `c9a8e56` | #5 (public `close()`, single-handle `open()`) |
| `63264cb` | #8 (harness awaits async tests; real-type shape checks) |

- [x] Run `cargo test` (phases 1–9 incl. `phase9_failure_discipline_tests.rs`) and `npm test`
      in `napi/` (66 tests + shape guard + harness self-check) against a fresh build.
      **DONE 2026-09-27:** Rust 230/230 green (`EXIT=0`); `npm run build` (release) then all three
      napi test files green.
- [x] Confirm the committed prebuilt `napi/ndb-node.win32-x64-msvc.node` matches HEAD —
      #6 proved a stale binary misreports panic locations.
      **DONE 2026-09-27:** the live binary (the one `index.js` loads) was rebuilt and recommitted
      inside the fix commits (`748ccb0`…`63264cb`); a fresh release build produces no diff.
      ⚠️ `napi/index.win32-x64-msvc.node` (May, 2 MB) is still tracked but never loaded — delete it
      in the push commit so it can't be mistaken for the live artifact (the #6 trap shape).
- [x] Push `main`; close #4, #5, #6, #8 citing commits. (#5 and #6 already carry independent
      verification comments; #4/#8 get closed on the green runs above.) The push note must cover
      the "Louder ≠ breaking" behavior change (contract §4), so consumers don't read new `Err`s
      as regressions.
      **DONE 2026-09-27:** pushed `987c7b1..f637de9` (8 commits: the six fixes, stale-binary
      removal `f0262bb`, this plan `f637de9` with the §4 push note in the commit message).
      #4, #5, #6, #8 closed with verification comments.

**Phase 0 COMPLETE (2026-09-27).** Issue list down to #3, #7, #9, #10.

---

## Phase 1 — Correctness gate: journal write ordering (#7) + deployment fix (#3)

**Phase 1 COMPLETE (2026-09-27).** #7 landed in `b13b0ac` (prepare → journal → commit for
`update` + delta ops; fault-injected tests; docs re-strengthened to the fourth guarantee;
rebuilt binary committed) — commented, left open for independent verification per repo norm.
#3 landed in `5a5f3d1` (root `package.json`, `"type": "commonjs"`; verified against a
simulated ESM parent) — closed. Oracles: full `cargo test` green, napi 66/66 + shape guard +
harness self-check against the fresh binary.

### #7 — Commit path reorder (the only remaining runtime defect) — DONE

Current shape ([src/lib.rs](src/lib.rs)): `handle_ref_delta_and_trash` and the in-memory/index
mutations run *before* the journal append, so a failed append leaves memory and files ahead of
the journal — the probe in #7 shows a failed `update` can trash a live blob while the document
stays old. Delta ops (`set`/`remove`/`array_push`) share the shape.

Target shape, for every mutating op:

1. **Prepare** — compute the full change set (new document, index deltas, ref-counter deltas,
   files to trash) without mutating anything.
2. **Journal** — append. On failure: nothing has moved; return `Err`, operation retryable.
3. **Commit** — apply in-memory state, indexes, ref counters; then perform file cleanup.
   Cleanup failure after commit is under-cleaning: named and reported (`report.rs`), swept by
   GC later — never silent, never implied-rollback.

Notes:
- `delete()` already has the precondition half (tombstone record before any state touch,
  `fc23006`); Phase 1 extends the same discipline to `update` and the delta ops.
- `handle_ref_delta_and_trash` moves from before the append to after the commit.
- Docs were already narrowed (`architecture.md`, `nodejs-api.md`); after the fix lands, the
  all-or-nothing wording can be re-strengthened — verify against the new guarantees, don't
  just restore the old sentence.

Tests (extend `tests/phase9_failure_discipline_tests.rs`): fault-inject journal append
failure (the #7 probe shape: `data.jsonl` replaced by a directory) for `update`, `set`,
`remove`, `array_push`; assert memory, indexes, ref counters and `_files/` are all unchanged
and the operation is retryable.

### #3 — `setup.js` under ESM parents (trivial, independent)

Fix: add a minimal root `package.json` with `{ "type": "commonjs" }` so module-type resolution
stops at the submodule root. Preferred over renaming to `setup.cjs` — no doc reference updates,
and `napi/setup.js` is already shielded by `napi/package.json`. Verify with a throwaway ESM
parent (`"type": "module"`) running `node setup.js` per the submodule deployment doc.

**Exit:** no known way to corrupt state or desync journal/memory/files; nDB deploys into ESM
consumers (mcp_server, LLM-Gateway-Chat). **This phase gates Phase 3.**

---

## Phase 2 — `meta.json` policy wiring (#10)

The engine reads `meta.json` for the first time. Scope per the issue and
`docs/database_evolution_plan.md` §2.3/§2.4.1:

- Parse `buckets.<name>.{ onDocumentDelete, ttl_seconds }` at open; missing file/block =
  today's behavior (no policy), so existing databases are untouched.
- `onDocumentDelete: "trash"` — on document delete, release the files that document referenced
  (same `{bucket, id, ext}` scan `verify` already does), riding the Phase-1 commit ordering.
- `onDocumentDelete: "restrict"` — refuse to delete a document whose files are still
  referenced... semantics pinned to the evolution plan during implementation; if the plan is
  ambiguous, the maintainer question in #10 gets answered in the issue before coding.
- `ttl_seconds` — per-bucket trash TTL feeding the existing purge mechanism.
- Explicitly out of scope: cross-database references (recorded in #10 — a consumer with
  cross-DB refs keeps its own policy on top).
- Tests: `trash` releases only the deleted document's files; `restrict` refuses and names the
  blocker; `ttl_seconds` purges on schedule; and the opt-in oracle — a database with no
  `meta.json`/block behaves byte-identically to today.

Plan assumption: #10's open maintainer question resolves to **implement** — the decided #9
spec already consumes `meta.json` declarations, and the alternative (stripping the documented
shape) contradicts the settled direction. If that assumption is wrong, Phase 2 becomes a doc
edit instead of a feature.

**Exit:** the documented policy block is enforced; consumers can delete documents without
hand-wiring file release for the in-database case.

---

## Phase 3 — Item buckets (#9, spec decided 2026-09-27)

The feature the earlier phases gate. Full spec lives in issue #9; this is the work breakdown.

### 3.1 Declaration & layout
- `meta.json`: `buckets.<name>.kind: "hash" | "items"` (default `"hash"` — zero activation for
  existing databases), `reserved_ttl_seconds` (default 1800).
- Layout: `_files/<bucket>/<itemId>/` — folder-as-unit; engine never parses non-original files
  (variants, caller description file are opaque payload).

### 3.2 Engine API (Rust core)
`createItem` → `{ itemId, path }` (reserved) · `commitItem(itemId, facts)` (one write, facts
incl. caller-computed `sha256`) · `readItem` · `listItems(bucket, state?)` · `deleteItem`
(folder → `_trash/files/<bucket>/`, tombstone) · `restoreItem`.

Key design points from the spec: nDB is never in the ingest path (the app streams to the
handed-out path; no engine buffers, no 64 MB ceiling on this kind); no dedup/refcounting on
item buckets; commit path follows Phase-1 ordering.

### 3.3 Sweeps & verify
- Reserved-TTL sweep (abandoned reservations).
- `ndb verify` item awareness: live item original exists and matches committed `size`/`sha256`;
  orphan folders (no record) flagged sweepable; records with no folder flagged.
- Purge via existing trash TTL / `ndb purge`.

### 3.4 Bindings & surfaces
- napi: full item API on the public wrapper, `.d.ts`, tests in `test-napi.js`.
- CLI: bucket-kind declaration at creation; `verify` output for item buckets.
- Docs: `documentation/file-buckets.md` gains the item kind; `nodejs-api.md`, `cli.md` (`verify`,
  bucket declaration) and `architecture.md` updated in the same commits. This issue exists partly
  because the serve design doc disagreed with the source — the docs are this repo's navigation map.
- `ndb serve`: decide whether item routes exist at all — the spec's consumer (nCMS) streams to
  disk directly; only add routes if a consumer needs them (likely: none for v1).

### 3.5 Acceptance (from the spec)
- [ ] Rust suite mirroring the nCMS pool semantics (reserve/commit/trash/restore/
      reserved-sweep/verify), synthetic oversized-file fixtures.
- [ ] LLM-Gateway-Chat attachment suite passes bit-identical (hash-kind regression oracle).
- [ ] nCMS Phase 4 adoption tracked separately in the nCMS repo: `lib/media.js` on item
      buckets, pool folders migrated, duplicated bookkeeping deleted.

**Exit:** the second bucket kind ships; nCMS's hand-rolled integrity/sweep/refcount logic has
an engine to retire into.

---

## Dependency summary

```
Phase 0 (push landed fixes: #4 #5 #6 #8)
   │
Phase 1 (#7 write ordering ─ gates everything after; #3 trivial, parallel)
   │
Phase 2 (#10 meta.json policy) ── same file touch as ──┐
   │                                                  │
Phase 3 (#9 item buckets) ◄───────────────────────────┘
```

#3 has no dependencies and can land in any phase. #10 and #9 both touch `meta.json` handling —
do them in one stretch (#10 first, it's smaller and teaches the engine to read the file that
#9's declaration then extends).
