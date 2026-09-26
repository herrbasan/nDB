// Regression test: a non-object document must be rejected as an error, never
// abort the host process.
//
// Why this exists as a separate process-level test: the failure mode is not a
// thrown error but a panicked Rust thread, which kills Node with
// 0xC0000409 (-1073740791). A regressed run therefore cannot report a failed
// assertion — it dies. That is the signal: a non-zero exit here means the
// process-abort bug came back.
//
//   node test/test-insert-shape.cjs

const path = require('path');
const fs = require('fs');
const { Database } = require(path.join(__dirname, '..', 'index.js'));

// Real JSON values, not pre-stringified ones: the wrapper calls
// JSON.stringify(doc) itself, so passing `JSON.stringify(null)` would test the
// string "null" and never the null branch.
const NON_OBJECTS = [
  'this is not a document',
  null,
  7,
  [1, 2, 3],
  true,
];

let failed = 0;

function check(ok, label, detail) {
  if (ok) {
    console.log(`  \u2713 ${label}`);
  } else {
    console.log(`  \u2717 ${label}${detail ? ` — ${detail}` : ''}`);
    failed++;
  }
}

const dir = fs.mkdtempSync(path.join(require('os').tmpdir(), 'ndb-insert-shape-'));
const db = Database.open(path.join(dir, 'data.jsonl'), { persistence: 'immediate' });

console.log('nDB insert() shape guard');
console.log('='.repeat(60));

check(typeof db.insert({ ok: true }) === 'string', 'object document accepted');

for (const bad of NON_OBJECTS) {
  let message = null;
  try {
    db.insert(bad);
  } catch (e) {
    message = e.message;
  }
  check(
    message !== null && /JSON object/.test(message),
    `non-object rejected: ${JSON.stringify(bad)}`,
    message === null ? 'no error thrown' : message
  );
}

// Surviving the loop above is the whole point: the database is still usable.
check(typeof db.insert({ after: true }) === 'string', 'database still usable afterwards');
check(db.len() === 2, 'only the two valid documents were stored', `len=${db.len()}`);

// update() and insertWithPrefix() take a serialized document too.
let updateRejected = false;
try {
  db.update(db.iter()[0]._id, 5);
} catch (e) {
  updateRejected = /JSON object/.test(e.message);
}
check(updateRejected, 'update() rejects a non-object document');

let prefixRejected = false;
try {
  db.insertWithPrefix('conv', 'nope');
} catch (e) {
  prefixRejected = /JSON object/.test(e.message);
}
check(prefixRejected, 'insertWithPrefix() rejects a non-object document');

// The guard is about documents only: set/arrayPush take arbitrary JSON, and the
// values here must be real types for that to mean anything.
const id = db.insert({ v: 1 });
for (const [label, value] of [
  ['scalar', 3.5],
  ['null', null],
  ['boolean', false],
  ['array', [1, 2]],
  ['object', { k: 'v' }],
]) {
  let ok = true;
  try {
    db.set(id, `f_${label}`, value);
  } catch (e) {
    ok = false;
  }
  check(ok, `set() still accepts a ${label} value`);
}

// And the stored values are what was sent, not stringified copies.
const stored = db.get(id);
check(stored.f_scalar === 3.5, 'scalar stored as a number', typeof stored.f_scalar);
check(stored.f_null === null, 'null stored as null', String(stored.f_null));
check(stored.f_boolean === false, 'boolean stored as a boolean', typeof stored.f_boolean);
check(Array.isArray(stored.f_array), 'array stored as an array', typeof stored.f_array);

if (typeof db.close === 'function') db.close();
fs.rmSync(dir, { recursive: true, force: true });

console.log('='.repeat(60));
if (failed === 0) {
  console.log('All shape-guard tests passed.');
  process.exit(0);
}
console.log(`${failed} shape-guard test(s) failed.`);
process.exit(1);
