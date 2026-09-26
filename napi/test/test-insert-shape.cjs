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

const NON_OBJECTS = [
  'this is not a document',
  JSON.stringify(null),
  JSON.stringify(7),
  JSON.stringify([1, 2, 3]),
  JSON.stringify(true),
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
    `non-object rejected: ${bad}`,
    message === null ? 'no error thrown' : message
  );
}

// Surviving the loop above is the whole point: the database is still usable.
check(typeof db.insert({ after: true }) === 'string', 'database still usable afterwards');
check(db.len() === 2, 'only the two valid documents were stored', `len=${db.len()}`);

// update() and insertWithPrefix() take a serialized document too.
let updateRejected = false;
try {
  db.update(db.iter()[0]._id, JSON.stringify(5));
} catch (e) {
  updateRejected = /JSON object/.test(e.message);
}
check(updateRejected, 'update() rejects a non-object document');

let prefixRejected = false;
try {
  db.insertWithPrefix('conv', JSON.stringify('nope'));
} catch (e) {
  prefixRejected = /JSON object/.test(e.message);
}
check(prefixRejected, 'insertWithPrefix() rejects a non-object document');

// The guard is about documents only: set/arrayPush take arbitrary JSON.
const id = db.insert({ v: 1 });
for (const [label, value] of [
  ['scalar', '3.5'],
  ['null', 'null'],
  ['boolean', 'false'],
  ['array', '[1,2]'],
]) {
  let ok = true;
  try {
    db.set(id, `f_${label}`, value);
  } catch (e) {
    ok = false;
  }
  check(ok, `set() still accepts a ${label} value`);
}

if (typeof db.close === 'function') db.close();
fs.rmSync(dir, { recursive: true, force: true });

console.log('='.repeat(60));
if (failed === 0) {
  console.log('All shape-guard tests passed.');
  process.exit(0);
}
console.log(`${failed} shape-guard test(s) failed.`);
process.exit(1);
