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
const { existsSync, mkdirSync, rmSync } = require('fs');
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

function test(name, fn) {
  try {
    fn();
    passed++;
    console.log(`  ✓ ${name}`);
  } catch (e) {
    failed++;
    errors.push({ name, error: e.message });
    console.log(`  ✗ ${name}: ${e.message}`);
  }
}

function section(title) {
  console.log(`\n── ${title} ${'─'.repeat(Math.max(0, 60 - title.length))}`);
}

function createTempDir() {
  const dir = join(os.tmpdir(), `ndb-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
  mkdirSync(dir, { recursive: true });
  return dir;
}

// ─── Tests ───────────────────────────────────────────────────────────

console.log('nDB N-API Integration Tests');
console.log('='.repeat(70));

// ─── Phase 1: Core CRUD ─────────────────────────────────────────────

section('Phase 1: Core CRUD');

test('open creates database file', () => {
  const dir = createTempDir();
  const path = join(dir, 'test.jsonl');
  const db = new Database(path);
  assert(existsSync(path), 'File should exist');
  assert(db.isEmpty(), 'New database should be empty');
  rmSync(dir, { recursive: true, force: true });
});

test('openInMemory creates in-memory database', () => {
  const db = Database.openInMemory();
  assert(db.isEmpty(), 'In-memory database should be empty');
  assertEqual(db.len(), 0, 'Length should be 0');
});

test('insert returns NanoID', () => {
  const db = Database.openInMemory();
  const id = db.insert({ title: 'Hello' });
  assertEqual(id.length, 16, 'ID should be 16 chars');
  assert(/^[a-zA-Z0-9]+$/.test(id), 'ID should be base62');
});

test('insert with prefix returns prefixed NanoID', () => {
  const db = Database.openInMemory();
  const id = db.insertWithPrefix('conv', { msg: 'hi' });
  assert(id.startsWith('conv_'), 'ID should start with prefix');
  assertEqual(id.length, 21, 'Prefixed ID should be 21 chars (prefix_ + 16)');
});

test('get by ID returns document', () => {
  const db = Database.openInMemory();
  const id = db.insert({ title: 'Test', value: 42 });
  const doc = db.get(id);
  assertEqual(doc.title, 'Test', 'Title should match');
  assertEqual(doc.value, 42, 'Value should match');
  assertEqual(doc._id, id, '_id should match');
});

test('get throws for nonexistent ID', () => {
  const db = Database.openInMemory();
  let threw = false;
  try {
    db.get('nonexistent');
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw for nonexistent ID');
});

test('update replaces document', () => {
  const db = Database.openInMemory();
  const id = db.insert({ v: 1 });
  db.update(id, { v: 2 });
  const doc = db.get(id);
  assertEqual(doc.v, 2, 'Value should be updated');
  assertEqual(doc._id, id, '_id should be preserved');
});

test('update throws for nonexistent ID', () => {
  const db = Database.openInMemory();
  let threw = false;
  try {
    db.update('nonexistent', { v: 1 });
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw for nonexistent ID');
});

test('delete soft-deletes document', () => {
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

test('delete throws for nonexistent ID', () => {
  const db = Database.openInMemory();
  let threw = false;
  try {
    db.delete('nonexistent');
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw for nonexistent ID');
});

test('iter returns all documents', () => {
  const db = Database.openInMemory();
  db.insert({ a: 1 });
  db.insert({ b: 2 });
  db.insert({ c: 3 });
  const docs = db.iter();
  assertEqual(docs.length, 3, 'Should return 3 docs');
});

test('contains checks existence', () => {
  const db = Database.openInMemory();
  const id = db.insert({ x: 1 });
  assert(db.contains(id), 'Should contain inserted ID');
  assert(!db.contains('nonexistent'), 'Should not contain random ID');
});

test('len returns correct count', () => {
  const db = Database.openInMemory();
  assertEqual(db.len(), 0, 'Empty db');
  db.insert({ a: 1 });
  assertEqual(db.len(), 1, '1 doc');
  db.insert({ b: 2 });
  assertEqual(db.len(), 2, '2 docs');
});

test('isEmpty works correctly', () => {
  const db = Database.openInMemory();
  assert(db.isEmpty(), 'Should be empty');
  db.insert({ a: 1 });
  assert(!db.isEmpty(), 'Should not be empty');
});

// ─── Phase 2: Persistence & Reload ──────────────────────────────────

section('Phase 2: Persistence & Reload');

test('data persists across database reopen', () => {
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

test('update persists across reopen', () => {
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

test('delete persists across reopen', () => {
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

test('deletedIds returns soft-deleted IDs', () => {
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

test('find by field equality', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice', age: 30 });
  db.insert({ name: 'Bob', age: 25 });
  db.insert({ name: 'Alice', age: 35 });

  const results = db.find('name', 'Alice');
  assertEqual(results.length, 2, 'Should find 2 Alices');
});

test('find by numeric value', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice', age: 30 });
  db.insert({ name: 'Bob', age: 25 });

  const results = db.find('age', 25);
  assertEqual(results.length, 1, 'Should find 1 doc with age 25');
  assertEqual(results[0].name, 'Bob', 'Should be Bob');
});

test('find returns empty for no match', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice' });
  const results = db.find('name', 'Charlie');
  assertEqual(results.length, 0, 'Should find 0 docs');
});

test('findRange returns documents in range', () => {
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

test('query with $eq', () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active', name: 'A' });
  db.insert({ status: 'deleted', name: 'B' });
  db.insert({ status: 'active', name: 'C' });

  const results = db.query({ status: { $eq: 'active' } });
  assertEqual(results.length, 2, 'Should find 2 active');
});

test('query with $gt', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 50 });
  db.insert({ name: 'C', score: 90 });

  const results = db.query({ score: { $gt: 40 } });
  assertEqual(results.length, 2, 'Should find 2 with score > 40');
});

test('query with $and', () => {
  const db = Database.openInMemory();
  db.insert({ user: 'alice', status: 'active', score: 100 });
  db.insert({ user: 'bob', status: 'active', score: 50 });
  db.insert({ user: 'alice', status: 'deleted', score: 200 });

  const results = db.query({
    $and: [
      { user: { $eq: 'alice' } },
      { status: { $eq: 'active' } }
    ]
  });
  assertEqual(results.length, 1, 'Should find 1 matching $and');
  assertEqual(results[0].score, 100, 'Score should be 100');
});

test('query with $or', () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active' });
  db.insert({ status: 'pending' });
  db.insert({ status: 'deleted' });

  const results = db.query({
    $or: [
      { status: { $eq: 'active' } },
      { status: { $eq: 'pending' } }
    ]
  });
  assertEqual(results.length, 2, 'Should find 2 matching $or');
});

test('query with $not', () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active' });
  db.insert({ status: 'deleted' });

  const results = db.query({
    $not: { status: { $eq: 'deleted' } }
  });
  assertEqual(results.length, 1, 'Should find 1 not deleted');
});

test('query with $in', () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active' });
  db.insert({ status: 'pending' });
  db.insert({ status: 'deleted' });

  const results = db.query({ status: { $in: ['active', 'pending'] } });
  assertEqual(results.length, 2, 'Should find 2 in array');
});

test('query with $exists', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', avatar: 'yes' });
  db.insert({ name: 'B' });

  const results = db.query({ avatar: { $exists: true } });
  assertEqual(results.length, 1, 'Should find 1 with avatar');
});

test('queryWith with limit and sort', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'C', score: 30 });
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 20 });

  const results = db.queryWith(
    {},
    { limit: 2, sortBy: 'score', sortDir: 'asc' }
  );
  assertEqual(results.length, 2, 'Should limit to 2');
  assertEqual(results[0].name, 'A', 'First should be A (lowest score)');
  assertEqual(results[1].name, 'B', 'Second should be B');
});

test('queryWith with offset', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 20 });
  db.insert({ name: 'C', score: 30 });

  const results = db.queryWith(
    {},
    { sortBy: 'score', sortDir: 'asc', offset: 1 }
  );
  assertEqual(results.length, 2, 'Should skip 1');
  assertEqual(results[0].name, 'B', 'First should be B (offset 1)');
});

test('queryWith with desc sort', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 20 });
  db.insert({ name: 'C', score: 30 });

  const results = db.queryWith(
    {},
    { sortBy: 'score', sortDir: 'desc' }
  );
  assertEqual(results[0].name, 'C', 'First should be C (highest)');
  assertEqual(results[2].name, 'A', 'Last should be A (lowest)');
});

// ─── Phase 5: Index Management ──────────────────────────────────────

section('Phase 5: Index Management');

test('createIndex and hasIndex', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice', age: 30 });
  db.createIndex('name');
  assert(db.hasIndex('name'), 'Should have name index');
  assert(!db.hasIndex('age'), 'Should not have age index');
});

test('find uses index', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'Alice', age: 30 });
  db.insert({ name: 'Bob', age: 25 });
  db.createIndex('name');

  const results = db.find('name', 'Alice');
  assertEqual(results.length, 1, 'Should find via index');
  assertEqual(results[0].name, 'Alice', 'Name should match');
});

test('dropIndex removes index', () => {
  const db = Database.openInMemory();
  db.createIndex('name');
  assert(db.hasIndex('name'), 'Should exist');
  db.dropIndex('name');
  assert(!db.hasIndex('name'), 'Should be gone');
});

test('createBTreeIndex works', () => {
  const db = Database.openInMemory();
  db.insert({ name: 'A', score: 10 });
  db.insert({ name: 'B', score: 50 });
  db.createBTreeIndex('score');
  assert(db.hasIndex('score'), 'Should have score BTree index');
});

// ─── Phase 6: Compaction & Trash ────────────────────────────────────

section('Phase 6: Compaction & Trash');

test('compact removes deleted docs from file', () => {
  const dir = createTempDir();
  const path = join(dir, 'compact.jsonl');

  const id = (() => {
    const db = new Database(path);
    const id = db.insert({ keep: true });
    const delId = db.insert({ delete: true });
    db.delete(delId);
    db.flush();
    db.compact();
    return id;
  })();

  const db2 = new Database(path);
  assertEqual(db2.len(), 1, 'Should have 1 doc after compact');
  const doc = db2.get(id);
  assertEqual(doc.keep, true, 'Kept doc should survive compact');

  rmSync(dir, { recursive: true, force: true });
});

test('restore recovers deleted document', () => {
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

test('storeFile and getFile round-trip', () => {
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

test('listFiles returns stored files', () => {
  const dir = createTempDir();
  const path = join(dir, 'list.jsonl');
  const db = new Database(path);

  db.storeFile('docs', 'a.txt', Buffer.from('aaa'), 'text/plain');
  db.storeFile('docs', 'b.txt', Buffer.from('bbb'), 'text/plain');

  const files = db.listFiles('docs');
  assertEqual(files.length, 2, 'Should list 2 files');

  rmSync(dir, { recursive: true, force: true });
});

test('deleteFile removes file', () => {
  const dir = createTempDir();
  const path = join(dir, 'del.jsonl');
  const db = new Database(path);

  const meta = db.storeFile('temp', 'del.txt', Buffer.from('delete me'), 'text/plain');
  db.deleteFile('temp', meta.id, meta.ext);

  let threw = false;
  try {
    db.getFile('temp', meta.id, meta.ext);
  } catch (e) {
    threw = true;
  }
  assert(threw, 'Should throw after file deleted');

  rmSync(dir, { recursive: true, force: true });
});

test('file deduplication by content hash', () => {
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

test('full lifecycle: insert, query, update, delete, compact', () => {
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
  const aliceDocs = db.query({ user: { $eq: 'alice' } });
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
  db.compact();

  // Reopen and verify
  db.flush();
  const db2 = new Database(path);
  assertEqual(db2.len(), 8, 'Should have 8 after compact + reload');

  rmSync(dir, { recursive: true, force: true });
});

test('nested document fields with dot notation in queries', () => {
  const db = Database.openInMemory();
  db.insert({ user: { name: 'Alice', address: { city: 'Berlin' } } });
  db.insert({ user: { name: 'Bob', address: { city: 'Tokyo' } } });

  const results = db.query({ 'user.name': { $eq: 'Alice' } });
  assertEqual(results.length, 1, 'Should find 1 with dot notation');
  assertEqual(results[0].user.address.city, 'Berlin', 'Nested field should work');
});

test('query with $ne, $gte, $lte, $nin', () => {
  const db = Database.openInMemory();
  db.insert({ status: 'active', score: 10 });
  db.insert({ status: 'pending', score: 50 });
  db.insert({ status: 'deleted', score: 90 });

  // $ne
  const notDeleted = db.query({ status: { $ne: 'deleted' } });
  assertEqual(notDeleted.length, 2, '$ne should find 2');

  // $gte
  const gte = db.query({ score: { $gte: 50 } });
  assertEqual(gte.length, 2, '$gte should find 2');

  // $lte
  const lte = db.query({ score: { $lte: 50 } });
  assertEqual(lte.length, 2, '$lte should find 2');

  // $nin
  const nin = db.query({ status: { $nin: ['deleted', 'pending'] } });
  assertEqual(nin.length, 1, '$nin should find 1');
});

test('Database.open with persistence option', () => {
  const dir = createTempDir();
  const path = join(dir, 'opts.jsonl');
  const db = Database.open(path, { persistence: 'lazy' });
  db.insert({ x: 1 });
  db.flush();
  assertEqual(db.len(), 1, 'Should work with lazy persistence');
  rmSync(dir, { recursive: true, force: true });
});

test('concurrent operations sequence', () => {
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

// ─── Results ─────────────────────────────────────────────────────────

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
