/**
 * nDB N-API Integration Tests
 *
 * Comprehensive tests for the nDB Node.js native bindings.
 * Tests all layers: Core CRUD, Field Queries, JSON AST Queries,
 * Indexes, Compaction, Trash, and File Buckets.
 *
 * Run: node test/test-napi.js
 */

const { Database } = require('../index.js');
const { existsSync, mkdirSync, renameSync, rmSync } = require('fs');
const { join } = require('path');
const os = require('os');

// ─── Test Harness ────────────────────────────────────────────────────

let passed = 0;
let failed = 0;
let errors = [];

function assert(condition, message) {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
}

function assertEqual(actual, expected, message) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(
      `${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`
    );
  }
}

// Tests are registered, then run in order by run(). Awaiting each callback is
// the point: a test returning a promise used to be counted as passed the moment
// it was called, so its rejection surfaced later — after process.exit had
// already reported success.
const plan = [];

function test(name, fn) {
  plan.push({ kind: 'test', name, fn });
}

function section(title) {
  plan.push({ kind: 'section', title });
}

async function run() {
  for (const item of plan) {
    if (item.kind === 'section') {
      console.log(`\n── ${item.title} ${'─'.repeat(Math.max(0, 60 - item.title.length))}`);
      continue;
    }
    try {
      await item.fn();
      passed++;
      console.log(`  ✓ ${item.name}`);
    } catch (e) {
      failed++;
      errors.push({ name: item.name, error: e.message });
      console.log(`  ✗ ${item.name}: ${e.message}`);
    }
  }
}

function createTempDir() {
  const dir = join(os.tmpdir(), `ndb-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
  mkdirSync(dir, { recursive: true });
  return dir;
}

// ─── Tests ───────────────────────────────────────────────────────────

(async () => {
console.log('nDB N-API Integration Tests');
console.log('='.repeat(70));

// ─── Phase 1: Core CRUD ─────────────────────────────────────────────

section('Phase 1: Core CRUD');

test('open creates database file', async () => {
  const dir = createTempDir();
  const path = join(dir, 'test.jsonl');
  const db = new Database(path);
  assert(existsSync(path), 'File should exist');
  assert(db.isEmpty(), 'New database should be empty');
  rmSync(dir, { recursive: true, force: true });
});

test('openInMemory creates in-memory database', async () => {
  const db = Database.openInMemory();
  assert(db.isEmpty(), 'In-memory database should be empty');
  assertEqual(db.len(), 0, 'Length should be 0');
});

test('insert returns NanoID', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ title: 'Hello' });
  assertEqual(id.length, 16, 'ID should be 16 chars');
  assert(/^[a-zA-Z0-9]+$/.test(id), 'ID should be base62');
});

test('insert with prefix returns prefixed NanoID', async () => {
  const db = Database.openInMemory();
  const id = db.insertWithPrefix('conv', { msg: 'hi' });
  assert(id.startsWith('conv_'), 'ID should start with prefix');
  assertEqual(id.length, 21, 'Prefixed ID should be 21 chars (prefix_ + 16)');
});

test('get by ID returns document', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ title: 'Test', value: 42 });
  const doc = db.get(id);
  assertEqual(doc.title, 'Test', 'Title should match');
  assertEqual(doc.value, 42, 'Value should match');
  assertEqual(doc._id, id, '_id should match');
});

test('get throws for nonexistent ID', async () => {
  const db = Database.openInMemory();
  let threw = false;
  try {
    db.get('nonexistent');
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw for nonexistent ID');
});

test('update replaces document', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ v: 1 });
  db.update(id, { v: 2 });
  const doc = db.get(id);
  assertEqual(doc.v, 2, 'Value should be updated');
  assertEqual(doc._id, id, '_id should be preserved');
});

test('update throws for nonexistent ID', async () => {
  const db = Database.openInMemory();
  let threw = false;
  try {
    db.update('nonexistent', { v: 1 });
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw for nonexistent ID');
});

test('delete soft-deletes document', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ x: 1 });
  assertEqual(db.len(), 1, 'Should have 1 doc');
  db.delete(id);
  assertEqual(db.len(), 0, 'Should have 0 docs after delete');
  let threw = false;
  try {
    db.get(id);
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw when getting deleted doc');
});

test('delete throws for nonexistent ID', async () => {
  const db = Database.openInMemory();
  let threw = false;
  try {
    db.delete('nonexistent');
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw for nonexistent ID');
});

test('iter returns all documents', async () => {
  const db = Database.openInMemory();
  db.insert({ a: 1 });
  db.insert({ b: 2 });
  db.insert({ c: 3 });
  const docs = db.iter();
  assertEqual(docs.length, 3, 'Should return 3 docs');
});

test('contains checks existence', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ x: 1 });
  assert(db.contains(id), 'Should contain inserted ID');
  assert(!db.contains('nonexistent'), 'Should not contain random ID');
});

test('len returns correct count', async () => {
  const db = Database.openInMemory();
  assertEqual(db.len(), 0, 'Empty db');
  db.insert({ a: 1 });
  assertEqual(db.len(), 1, '1 doc');
  db.insert({ b: 2 });
  assertEqual(db.len(), 2, '2 docs');
});

test('isEmpty works correctly', async () => {
  const db = Database.openInMemory();
  assert(db.isEmpty(), 'Should be empty');
  db.insert({ a: 1 });
  assert(!db.isEmpty(), 'Should not be empty');
});

// ─── Phase 2: Persistence & Reload ──────────────────────────────────

section('Phase 2: Persistence & Reload');

test('data persists across database reopen', async () => {
  const dir = createTempDir();
  const path = join(dir, 'persist.jsonl');

  const id = (() => {
    const db = new Database(path);
    const id = db.insert({ name: 'Alice', score: 100 });
    db.flush();
    return id;
  })();

  // Reopen
  const db2 = new Database(path);
  assertEqual(db2.len(), 1, 'Should have 1 doc after reload');
  const doc = db2.get(id);
  assertEqual(doc.name, 'Alice', 'Name should persist');
  assertEqual(doc.score, 100, 'Score should persist');

  rmSync(dir, { recursive: true, force: true });
});

test('update persists across reopen', async () => {
  const dir = createTempDir();
  const path = join(dir, 'update.jsonl');

  const id = (() => {
    const db = new Database(path);
    const id = db.insert({ v: 1 });
    db.update(id, { v: 42 });
    db.flush();
    return id;
  })();

  const db2 = new Database(path);
  const doc = db2.get(id);
  assertEqual(doc.v, 42, 'Updated value should persist');

  rmSync(dir, { recursive: true, force: true });
});

test('delete persists across reopen', async () => {
  const dir = createTempDir();
  const path = join(dir, 'delete.jsonl');

  const id = (() => {
    const db = new Database(path);
    const id = db.insert({ x: 1 });
    db.delete(id);
    db.flush();
    return id;
  })();

  const db2 = new Database(path);
  assertEqual(db2.len(), 0, 'Deleted doc should not appear');
  const deletedIds = db2.deletedIds();
  assert(deletedIds.includes(id), 'ID should be in deleted list');

  rmSync(dir, { recursive: true, force: true });
});

test('deletedIds returns soft-deleted IDs', async () => {
  const db = Database.openInMemory();
  const id1 = db.insert({ x: 1 });
  const id2 = db.insert({ x: 2 });
  db.delete(id1);
  const deleted = db.deletedIds();
  assert(deleted.includes(id1), 'id1 should be in deleted');
  assert(!deleted.includes(id2), 'id2 should not be in deleted');
});

// ─── Phase 3: Field Queries ─────────────────────────────────────────

section('Phase 3: Field Queries (Layer 2)');

test('find by field equality', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice', age: 30 });
  db.insert({ name: 'Bob', age: 25 });
  db.insert({ name: 'Alice', age: 35 });

  const results = db.find('name', 'Alice');
  assertEqual(results.length, 2, 'Should find 2 Alices');
});

test('find by numeric value', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice', age: 30 });
  db.insert({ name: 'Bob', age: 25 });

  const results = db.find('age', 25);
  assertEqual(results.length, 1, 'Should find 1 doc with age 25');
  assertEqual(results[0].name, 'Bob', 'Should be Bob');
});

test('find returns empty for no match', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice' });
  const results = db.find('name', 'Charlie');
  assertEqual(results.length, 0, 'Should find 0 docs');
});

test('findRange returns documents in range', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 50 });
  db.insert({ name: 'C', score: 90 });
  db.insert({ name: 'D', score: 100 });

  const results = db.findRange('score', 20, 95);
  assertEqual(results.length, 2, 'Should find 2 docs in range');
});

// ─── Phase 4: JSON AST Queries ──────────────────────────────────────

section('Phase 4: JSON AST Queries (Layer 3)');

test('query rejects unknown operators instead of matching everything', async () => {
  // G4: a typo'd operator must reject — it used to silently resolve to
  // match-everything, returning the whole database as if the query worked.
  const db = Database.openInMemory();
  db.insert({ status: 'active', name: 'A' });
  db.insert({ status: 'deleted', name: 'B' });

  let threw = false;
  try { await db.query({ status: { $eqq: 'typo' } }); } catch (e) { threw = true; }
  assert(threw, 'unknown operator $eqq must throw');

  threw = false;
  try { await db.query({ $where: 'x' }); } catch (e) { threw = true; }
  assert(threw, 'unknown top-level operator must throw');

  threw = false;
  try { await db.queryWith({ a: { $gtee: 1 } }, { limit: 5 }); } catch (e) { threw = true; }
  assert(threw, 'queryWith must validate too');

  // Valid queries unaffected.
  const results = await db.query({ status: { $eq: 'active' } });
  assertEqual(results.length, 1, 'Valid query after rejections still works');
});

test('query with $eq', async () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active', name: 'A' });
  db.insert({ status: 'deleted', name: 'B' });
  db.insert({ status: 'active', name: 'C' });

  const results = await db.query({ status: { $eq: 'active' } });
  assertEqual(results.length, 2, 'Should find 2 active');
});

test('query with $gt', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 50 });
  db.insert({ name: 'C', score: 90 });

  const results = await db.query({ score: { $gt: 40 } });
  assertEqual(results.length, 2, 'Should find 2 with score > 40');
});

test('query with $and', async () => {
  const db = Database.openInMemory();
  db.insert({ user: 'alice', status: 'active', score: 100 });
  db.insert({ user: 'bob', status: 'active', score: 50 });
  db.insert({ user: 'alice', status: 'deleted', score: 200 });

  const results = await db.query({
    $and: [
      { user: { $eq: 'alice' } },
      { status: { $eq: 'active' } }
    ]
  });
  assertEqual(results.length, 1, 'Should find 1 matching $and');
  assertEqual(results[0].score, 100, 'Score should be 100');
});

test('query with $or', async () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active' });
  db.insert({ status: 'pending' });
  db.insert({ status: 'deleted' });

  const results = await db.query({
    $or: [
      { status: { $eq: 'active' } },
      { status: { $eq: 'pending' } }
    ]
  });
  assertEqual(results.length, 2, 'Should find 2 matching $or');
});

test('query with $not', async () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active' });
  db.insert({ status: 'deleted' });

  const results = await db.query({
    $not: { status: { $eq: 'deleted' } }
  });
  assertEqual(results.length, 1, 'Should find 1 not deleted');
});

test('query with $in', async () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active' });
  db.insert({ status: 'pending' });
  db.insert({ status: 'deleted' });

  const results = await db.query({ status: { $in: ['active', 'pending'] } });
  assertEqual(results.length, 2, 'Should find 2 in array');
});

test('query with $exists', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', avatar: 'yes' });
  db.insert({ name: 'B' });

  const results = await db.query({ avatar: { $exists: true } });
  assertEqual(results.length, 1, 'Should find 1 with avatar');
});

test('queryWith with limit and sort', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'C', score: 30 });
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 20 });

  const results = await db.queryWith(
    {},
    { limit: 2, sortBy: 'score', sortDir: 'asc' }
  );
  assertEqual(results.length, 2, 'Should limit to 2');
  assertEqual(results[0].name, 'A', 'First should be A (lowest score)');
  assertEqual(results[1].name, 'B', 'Second should be B');
});

test('queryWith with offset', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 20 });
  db.insert({ name: 'C', score: 30 });

  const results = await db.queryWith(
    {},
    { sortBy: 'score', sortDir: 'asc', offset: 1 }
  );
  assertEqual(results.length, 2, 'Should skip 1');
  assertEqual(results[0].name, 'B', 'First should be B (offset 1)');
});

test('queryWith with desc sort', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 20 });
  db.insert({ name: 'C', score: 30 });

  const results = await db.queryWith(
    {},
    { sortBy: 'score', sortDir: 'desc' }
  );
  assertEqual(results[0].name, 'C', 'First should be C (highest)');
  assertEqual(results[2].name, 'A', 'Last should be A (lowest)');
});

// ─── Phase 5: Index Management ──────────────────────────────────────

section('Phase 5: Index Management');

test('createIndex and hasIndex', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice', age: 30 });
  db.createIndex('name');
  assert(db.hasIndex('name'), 'Should have name index');
  assert(!db.hasIndex('age'), 'Should not have age index');
});

test('find uses index', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice', age: 30 });
  db.insert({ name: 'Bob', age: 25 });
  db.createIndex('name');

  const results = db.find('name', 'Alice');
  assertEqual(results.length, 1, 'Should find via index');
  assertEqual(results[0].name, 'Alice', 'Name should match');
});

test('dropIndex removes index', async () => {
  const db = Database.openInMemory();
  db.createIndex('name');
  assert(db.hasIndex('name'), 'Should exist');
  db.dropIndex('name');
  assert(!db.hasIndex('name'), 'Should be gone');
});

test('createBTreeIndex works', async () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 50 });
  db.createBTreeIndex('score');
  assert(db.hasIndex('score'), 'Should have score BTree index');
});

// ─── Phase 6: Compaction & Trash ────────────────────────────────────

section('Phase 6: Compaction & Trash');

test('compact removes deleted docs from file', async () => {
  const dir = createTempDir();
  const path = join(dir, 'compact.jsonl');

  const id = await (async () => {
    const db = new Database(path);
    const id = db.insert({ keep: true });
    const delId = db.insert({ delete: true });
    db.delete(delId);
    db.flush();
    await db.compact();
    return id;
  })();

  const db2 = new Database(path);
  assertEqual(db2.len(), 1, 'Should have 1 doc after compact');
  const doc = db2.get(id);
  assertEqual(doc.keep, true, 'Kept doc should survive compact');

  rmSync(dir, { recursive: true, force: true });
});

test('restore recovers deleted document', async () => {
  const dir = createTempDir();
  const path = join(dir, 'restore.jsonl');

  const id = (() => {
    const db = new Database(path);
    const id = db.insert({ name: 'recover-me' });
    db.delete(id);
    db.flush();
    db.restore(id);
    return id;
  })();

  const db2 = new Database(path);
  assertEqual(db2.len(), 1, 'Should have 1 doc after restore');
  const doc = db2.get(id);
  assertEqual(doc.name, 'recover-me', 'Name should be restored');

  rmSync(dir, { recursive: true, force: true });
});

// ─── Phase 7: File Buckets ──────────────────────────────────────────

section('Phase 7: File Buckets');

test('storeFile and getFile round-trip', async () => {
  const dir = createTempDir();
  const path = join(dir, 'bucket.jsonl');
  const db = new Database(path);

  const testData = Buffer.from('Hello, nDB file storage!');
  const meta = db.storeFile('test', 'hello.txt', testData, 'text/plain');

  // FileMeta has _file: {bucket, id, ext}, name, size, type, created
  assert(meta._file, 'Should have _file ref');
  assert(meta._file.id, 'Should have hash id');
  assertEqual(meta.name, 'hello.txt', 'Name should match');
  assertEqual(meta.type, 'text/plain', 'MIME type should match');
  assertEqual(meta.size, testData.length, 'Size should match');
  assertEqual(meta._file.bucket, 'test', 'Bucket should match');

  const retrieved = db.getFile('test', meta._file.id, meta._file.ext);
  assertEqual(retrieved.toString(), testData.toString(), 'Content should match');

  rmSync(dir, { recursive: true, force: true });
});

test('listFiles returns stored files', async () => {
  const dir = createTempDir();
  const path = join(dir, 'list.jsonl');
  const db = new Database(path);

  db.storeFile('docs', 'a.txt', Buffer.from('aaa'), 'text/plain');
  db.storeFile('docs', 'b.txt', Buffer.from('bbb'), 'text/plain');

  const files = db.listFiles('docs');
  assertEqual(files.length, 2, 'Should list 2 files');

  rmSync(dir, { recursive: true, force: true });
});

test('deleteFile removes file', async () => {
  const dir = createTempDir();
  const path = join(dir, 'del.jsonl');
  const db = new Database(path);

  const meta = db.storeFile('temp', 'del.txt', Buffer.from('delete me'), 'text/plain');
  db.deleteFile('temp', meta._file.id, meta._file.ext);

  let threw = false;
  try {
    db.getFile('temp', meta._file.id, meta._file.ext);
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw after file deleted');

  rmSync(dir, { recursive: true, force: true });
});

test('file deduplication by content hash', async () => {
  const dir = createTempDir();
  const path = join(dir, 'dedup.jsonl');
  const db = new Database(path);

  const content = Buffer.from('same content');
  const meta1 = db.storeFile('files', 'original.txt', content, 'text/plain');
  const meta2 = db.storeFile('files', 'copy.txt', content, 'text/plain');

  // Same content = same hash = same id
  assertEqual(meta1.id, meta2.id, 'Same content should produce same hash');

  const files = db.listFiles('files');
  assertEqual(files.length, 1, 'Deduplication should result in 1 file');

  rmSync(dir, { recursive: true, force: true });
});

// ─── Phase 8: Complex Scenarios ─────────────────────────────────────

section('Phase 8: Complex Scenarios');

test('full lifecycle: insert, query, update, delete, compact', async () => {
  const dir = createTempDir();
  const path = join(dir, 'lifecycle.jsonl');
  const db = new Database(path);

  // Insert batch
  const ids = [];
  for (let i = 0; i < 10; i++) {
    ids.push(db.insert({ user: i < 5 ? 'alice' : 'bob', score: i * 10 }));
  }
  assertEqual(db.len(), 10, 'Should have 10 docs');

  // Query
  const aliceDocs = await db.query({ user: { $eq: 'alice' } });
  assertEqual(aliceDocs.length, 5, 'Should find 5 alice docs');

  // Update
  db.update(ids[0], { user: 'alice', score: 999 });
  const updated = db.get(ids[0]);
  assertEqual(updated.score, 999, 'Score should be updated');

  // Delete some
  db.delete(ids[0]);
  db.delete(ids[5]);
  assertEqual(db.len(), 8, 'Should have 8 after 2 deletes');

  // Compact
  await db.compact();

  // Reopen and verify
  db.flush();
  const db2 = new Database(path);
  assertEqual(db2.len(), 8, 'Should have 8 after compact + reload');

  rmSync(dir, { recursive: true, force: true });
});

test('nested document fields with dot notation in queries', async () => {
  const db = Database.openInMemory();
  db.insert({ user: { name: 'Alice', address: { city: 'Berlin' } } });
  db.insert({ user: { name: 'Bob', address: { city: 'Tokyo' } } });

  const results = await db.query({ 'user.name': { $eq: 'Alice' } });
  assertEqual(results.length, 1, 'Should find 1 with dot notation');
  assertEqual(results[0].user.address.city, 'Berlin', 'Nested field should work');
});

test('query with $ne, $gte, $lte, $nin', async () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active', score: 10 });
  db.insert({ status: 'pending', score: 50 });
  db.insert({ status: 'deleted', score: 90 });

  // $ne
  const notDeleted = await db.query({ status: { $ne: 'deleted' } });
  assertEqual(notDeleted.length, 2, '$ne should find 2');

  // $gte
  const gte = await db.query({ score: { $gte: 50 } });
  assertEqual(gte.length, 2, '$gte should find 2');

  // $lte
  const lte = await db.query({ score: { $lte: 50 } });
  assertEqual(lte.length, 2, '$lte should find 2');

  // $nin
  const nin = await db.query({ status: { $nin: ['deleted', 'pending'] } });
  assertEqual(nin.length, 1, '$nin should find 1');
});

test('Database.open with persistence option', async () => {
  const dir = createTempDir();
  const path = join(dir, 'opts.jsonl');
  const db = Database.open(path, { persistence: 'lazy' });
  db.insert({ x: 1 });
  db.flush();
  assertEqual(db.len(), 1, 'Should work with lazy persistence');
  rmSync(dir, { recursive: true, force: true });
});

test('concurrent operations sequence', async () => {
  const db = Database.openInMemory();
  
  // Rapid insert/update/delete cycle
  for (let i = 0; i < 100; i++) {
    const id = db.insert({ idx: i });
    if (i % 3 === 0) {
      db.update(id, { idx: i, updated: true });
    }
    if (i % 5 === 0) {
      db.delete(id);
    }
  }

  // Verify count: 100 inserts - 20 deletes (every 5th) = 80
  assertEqual(db.len(), 80, 'Should have 80 docs after mixed ops');
});

// ─── Phase 9: Atomic set / remove / arrayPush ───────────────────────

section('Phase 9: Atomic set / remove / arrayPush');

test('set updates top-level field', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ title: 'old', count: 0 });
  db.set(id, 'title', 'new');
  const doc = db.get(id);
  assertEqual(doc.title, 'new', 'Title should be updated');
  assertEqual(doc.count, 0, 'Count should be unchanged');
});

test('set updates nested field', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ settings: { theme: 'light', lang: 'en' } });
  db.set(id, 'settings.theme', 'dark');
  const doc = db.get(id);
  assertEqual(doc.settings.theme, 'dark', 'Theme should be dark');
  assertEqual(doc.settings.lang, 'en', 'Lang should be unchanged');
});

test('set updates array element by index', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ messages: [
    { text: 'hello', author: 'alice' },
    { text: 'world', author: 'bob' }
  ]});
  db.set(id, 'messages.1.text', 'earth');
  const doc = db.get(id);
  assertEqual(doc.messages[1].text, 'earth', 'Second message text should be updated');
  assertEqual(doc.messages[1].author, 'bob', 'Author should be unchanged');
  assertEqual(doc.messages[0].text, 'hello', 'First message should be unchanged');
});

test('set creates new field', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ existing: true });
  db.set(id, 'newField', 42);
  const doc = db.get(id);
  assertEqual(doc.existing, true, 'Existing field should be unchanged');
  assertEqual(doc.newField, 42, 'New field should be created');
});

test('set on nonexistent doc throws', async () => {
  const db = Database.openInMemory();
  let threw = false;
  try { db.set('ghost', 'x', 1); } catch (e) { threw = true; }
  assert(threw, 'Should throw for nonexistent doc');
});

test('remove deletes top-level field', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ keep: true, drop: 'me' });
  db.remove(id, 'drop');
  const doc = db.get(id);
  assertEqual(doc.keep, true, 'Keep should remain');
  assert(doc.drop === undefined, 'Drop should be removed');
});

test('remove deletes nested field', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ settings: { theme: 'dark', volume: 80 } });
  db.remove(id, 'settings.volume');
  const doc = db.get(id);
  assertEqual(doc.settings.theme, 'dark', 'Theme should remain');
  assert(doc.settings.volume === undefined, 'Volume should be removed');
});

test('remove shifts array elements', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ items: [10, 20, 30, 40] });
  db.remove(id, 'items.1');
  const doc = db.get(id);
  assertEqual(doc.items, [10, 30, 40], 'Array should shift after remove');
});

test('remove on nonexistent doc throws', async () => {
  const db = Database.openInMemory();
  let threw = false;
  try { db.remove('ghost', 'x'); } catch (e) { threw = true; }
  assert(threw, 'Should throw for nonexistent doc');
});

test('arrayPush appends to array field', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ tags: ['a'] });
  db.arrayPush(id, 'tags', 'b');
  db.arrayPush(id, 'tags', 'c');
  const doc = db.get(id);
  assertEqual(doc.tags, ['a', 'b', 'c'], 'Tags should have all elements');
});

test('set/remove/arrayPush return applied flags', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ title: 'old', items: [1], name: 'x' });
  assertEqual(db.set(id, 'title', 'new'), true, 'Resolved set applies');
  assertEqual(db.set(id, 'title', 'new'), true, 'Same-value set is still applied');
  assertEqual(db.set(id, 'no.such.path', 1), false, 'Missing intermediate reports false');
  assertEqual(db.remove(id, 'title'), true, 'Resolved remove applies');
  assertEqual(db.remove(id, 'title'), false, 'Removing a missing field reports false');
  assertEqual(db.arrayPush(id, 'tags', 'a'), true, 'Array creation applies');
  assertEqual(db.arrayPush(id, 'items.9', 2), false, 'Out-of-bounds push reports false');
  assertEqual(db.arrayPush(id, 'name', 2), false, 'Push onto a non-array reports false');
  assertEqual(db.arrayPush(id, 'items.0', 2), false, 'Push onto a scalar element reports false');
});

test('set + remove + arrayPush persist and replay', async () => {
  const dir = createTempDir();
  const path = join(dir, 'atomic_replay.jsonl');

  const id = (() => {
    const db = new Database(path);
    const id = db.insert({ messages: [{ text: 'hi' }], title: 'init', count: 0 });
    db.arrayPush(id, 'messages', { text: 'there' });
    db.set(id, 'title', 'updated');
    db.set(id, 'count', 2);
    db.set(id, 'messages.0.text', 'hello');
    db.remove(id, 'messages.1.text');
    db.flush();
    return id;
  })();

  const db2 = new Database(path);
  const doc = db2.get(id);
  assertEqual(doc.title, 'updated', 'Title should persist');
  assertEqual(doc.count, 2, 'Count should persist');
  assertEqual(doc.messages[0].text, 'hello', 'First message should be edited');
  assertEqual(doc.messages[1].text, undefined, 'Second message text should be removed');

  rmSync(dir, { recursive: true, force: true });
});

test('stress: rapid set same path', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ counter: 0 });
  for (let i = 1; i <= 500; i++) {
    db.set(id, 'counter', i);
  }
  const doc = db.get(id);
  assertEqual(doc.counter, 500, 'Counter should be 500 after 500 sets');
});

test('stress: set array elements then compact', async () => {
  const dir = createTempDir();
  const path = join(dir, 'stress_compact.jsonl');

  const id = await (async () => {
    const db = new Database(path);
    const id = db.insert({ items: [] });
    for (let i = 0; i < 100; i++) {
      db.arrayPush(id, 'items', { v: i });
    }
    for (let i = 0; i < 50; i++) {
      db.remove(id, 'items.0');
    }
    for (let i = 0; i < 50; i++) {
      db.set(id, `items.${i}.v`, i * 100);
    }
    await db.compact();
    return id;
  })();

  const db2 = new Database(path);
  const doc = db2.get(id);
  assertEqual(doc.items.length, 50, 'Should have 50 items after removes');
  assertEqual(doc.items[0].v, 0, 'First item v should be 0');
  assertEqual(doc.items[49].v, 4900, 'Last item v should be 4900');

  rmSync(dir, { recursive: true, force: true });
});

test('full update overwrites prior set patches', async () => {
  const dir = createTempDir();
  const path = join(dir, 'overwrite.jsonl');

  const id = (() => {
    const db = new Database(path);
    const id = db.insert({ x: 1, y: 2 });
    db.set(id, 'x', 99);
    db.update(id, { z: 3 });
    db.set(id, 'z', 42);
    db.flush();
    return id;
  })();

  const db2 = new Database(path);
  const doc = db2.get(id);
  assert(doc.x === undefined, 'x should not exist after full update');
  assert(doc.y === undefined, 'y should not exist after full update');
  assertEqual(doc.z, 42, 'z should be 42');

  rmSync(dir, { recursive: true, force: true });
});

// ─── Phase 10: Lifecycle (close) ────────────────────────────────────

test('close is exposed on the public wrapper', async () => {
  const db = Database.openInMemory();
  assert(typeof db.close === 'function', 'Wrapper should expose close()');
  db.close();
});

test('close is idempotent and later operations report it', async () => {
  const db = Database.openInMemory();
  const id = db.insert({ v: 1 });

  db.close();
  db.close(); // a second call must not throw

  let message = null;
  try {
    db.get(id);
  } catch (e) {
    message = e.message;
  }
  assert(message !== null, 'get after close should throw');
  assert(/closed/i.test(message), `expected a "closed" error, got: ${message}`);

  let insertMessage = null;
  try {
    db.insert({ v: 2 });
  } catch (e) {
    insertMessage = e.message;
  }
  assert(/closed/i.test(insertMessage), 'insert after close should report closed');
});

test('close releases the folder so it can be renamed', async () => {
  // A live file handle keeps the folder locked on Windows, which is the whole
  // point of the operation.
  const dir = createTempDir();
  const db = Database.open(join(dir, 'lifecycle.jsonl'));
  db.insert({ v: 1 });

  db.close();

  const moved = `${dir}-moved`;
  renameSync(dir, moved);
  assert(existsSync(moved), 'folder should be renameable once the db is closed');

  rmSync(moved, { recursive: true, force: true });
});

test('open with options does not leave a second handle behind', async () => {
  // open(path, options) used to construct a throwaway binding first, which only
  // the GC released — so close() left the folder locked anyway.
  const dir = createTempDir();
  const db = Database.open(join(dir, 'options.jsonl'), { persistence: 'immediate' });
  db.insert({ v: 1 });

  db.close();

  const moved = `${dir}-moved`;
  renameSync(dir, moved);
  assert(existsSync(moved), 'folder should be renameable when opened with options');

  rmSync(moved, { recursive: true, force: true });
});

test('pending work should be awaited before close releases the folder', async () => {
  // A running async task holds its own reference to the database, so close()
  // alone does not release the folder while one is in flight. Finishing pending
  // work first is the supported sequence; this pins it.
  const dir = createTempDir();
  const db = Database.open(join(dir, 'drain.jsonl'));
  db.insert({ v: 1 });
  db.insert({ v: 2 });

  const pending = db.query({ v: { $gte: 1 } });
  db.close();

  const rows = await pending;
  assertEqual(rows.length, 2, 'in-flight query should still complete after close');

  const moved = `${dir}-moved`;
  renameSync(dir, moved);
  assert(existsSync(moved), 'folder should be renameable once pending work is drained');

  rmSync(moved, { recursive: true, force: true });
});

// ─── Phase 11: Item Buckets (kind: "items" in meta.json) ────────────

section('Phase 11: Item Buckets');

const { writeFileSync, mkdirSync: mkDir } = require('fs');

const HELLO_SHA256 = 'b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9';

function itemsDb(extraPolicy) {
  const dir = createTempDir();
  writeFileSync(join(dir, 'meta.json'), JSON.stringify({
    buckets: { media: { kind: 'items', ...extraPolicy } }
  }));
  return { db: new Database(join(dir, 'data.jsonl')), dir };
}

test('createItem requires the kind declaration', async () => {
  const dir = createTempDir();
  const db = new Database(join(dir, 'data.jsonl'));
  let threw = false;
  try { db.createItem('media'); } catch (e) { threw = true; }
  assert(threw, 'createItem without declaration should throw');
  rmSync(dir, { recursive: true, force: true });
});

test('reserve → stream → commit → read → list → delete → restore', async () => {
  const { db, dir } = itemsDb();

  // Reserve: engine hands out the folder path.
  const { itemId, path } = db.createItem('media');
  assert(itemId.startsWith('itm_'), 'item id should be itm_-prefixed');
  assert(existsSync(path), 'item folder should exist');
  assertEqual(db.readItem('media', itemId).state, 'reserved', 'fresh item is reserved');

  // The application streams bytes itself; variants + description are
  // opaque payload.
  writeFileSync(join(path, 'photo.png'), 'hello world');
  writeFileSync(join(path, 'photo_720.webp'), 'variant');
  writeFileSync(join(path, 'asset.json'), '{"title":"x"}');

  // Commit: one write; custom facts pass through.
  db.commitItem('media', itemId, {
    name: 'photo.png', size: 11, mime: 'image/png', sha256: HELLO_SHA256,
    custom: { width: 100 }
  });
  const rec = db.readItem('media', itemId);
  assertEqual(rec.state, 'live', 'committed item is live');
  assertEqual(rec.facts.custom.width, 100, 'caller facts preserved');
  assertEqual(db.verifyItemBuckets(), [], 'verify should be clean');

  // List + state filter.
  assertEqual(db.listItems('media').length, 1, 'one item listed');
  assertEqual(db.listItems('media', 'reserved').length, 0, 'none reserved');
  assertEqual(db.listItems('media', 'live').length, 1, 'one live');

  // Delete: folder moves to trash whole.
  db.deleteItem('media', itemId);
  let threw = false;
  try { db.readItem('media', itemId); } catch (e) { threw = true; }
  assert(threw, 'deleted item should not read');
  assert(!existsSync(path), 'folder should leave the active bucket');
  assert(
    existsSync(join(dir, '_trash', 'files', 'media', itemId, 'photo_720.webp')),
    'trash should hold the whole folder, variants included'
  );

  // Restore: tombstone lifted, folder back whole.
  db.restoreItem('media', itemId);
  assertEqual(db.readItem('media', itemId).state, 'live', 'restored item is live');
  assert(existsSync(join(path, 'asset.json')), 'description file restored too');
  assertEqual(db.verifyItemBuckets(), [], 'verify clean after restore');

  rmSync(dir, { recursive: true, force: true });
});

test('commitItem validates facts', async () => {
  const { db, dir } = itemsDb();
  const { itemId } = db.createItem('media');

  for (const bad of [
    { size: 11, sha256: HELLO_SHA256 },                    // no name
    { name: 'x', sha256: HELLO_SHA256 },                   // no size
    { name: 'x', size: 11 },                               // no sha256
    { name: 'x', size: 11, sha256: 'nothex' },             // bad sha256
  ]) {
    let threw = false;
    try { db.commitItem('media', itemId, bad); } catch (e) { threw = true; }
    assert(threw, `facts ${JSON.stringify(bad)} should throw`);
  }

  rmSync(dir, { recursive: true, force: true });
});

test('sweepReservedItems sweeps stale reservations only', async () => {
  const { db, dir } = itemsDb({ reserved_ttl_seconds: 0 });

  // A live item and an abandoned reservation.
  const { itemId: live, path: livePath } = db.createItem('media');
  writeFileSync(join(livePath, 'photo.png'), 'hello world');
  db.commitItem('media', live, { name: 'photo.png', size: 11, sha256: HELLO_SHA256 });
  const { itemId: stale, path: stalePath } = db.createItem('media');

  assertEqual(db.sweepReservedItems('media'), 1, 'one stale reservation swept');
  assert(!existsSync(stalePath), 'swept folder trashed');
  assertEqual(db.readItem('media', live).state, 'live', 'live item untouched');
  assertEqual(db.sweepReservedItems('media'), 0, 'nothing left to sweep');

  rmSync(dir, { recursive: true, force: true });
});

test('item records replay across reopen', async () => {
  const { db, dir } = itemsDb();
  const { itemId, path } = db.createItem('media');
  writeFileSync(join(path, 'photo.png'), 'hello world');
  db.commitItem('media', itemId, { name: 'photo.png', size: 11, sha256: HELLO_SHA256 });
  const { itemId: reserved } = db.createItem('media');
  db.close();

  const db2 = new Database(join(dir, 'data.jsonl'));
  assertEqual(db2.readItem('media', itemId).state, 'live', 'committed item replayed');
  assertEqual(db2.readItem('media', reserved).state, 'reserved', 'reserved item replayed');
  assertEqual(db2.verifyItemBuckets(), [], 'verify clean after reopen');

  rmSync(dir, { recursive: true, force: true });
});

// ─── Results ─────────────────────────────────────────────────────────

await run();

console.log(`\n${'='.repeat(70)}`);
console.log(`Results: ${passed} passed, ${failed} failed, ${passed + failed} total`);

if (failed > 0) {
  console.log('\nFailed tests:');
  errors.forEach(({ name, error }) => {
    console.log(`  ✗ ${name}: ${error}`);
  });
  process.exit(1);
} else {
  console.log('\nAll tests passed! ✓');
  process.exit(0);
}
})();
