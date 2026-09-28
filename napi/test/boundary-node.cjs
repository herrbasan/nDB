// Boundary-tax probe, Node side: opens the SAME folder the Rust twin
// (benches/boundary_rust.rs) built and runs the SAME loops through the
// napi binding — including the wrapper's JSON.parse, because that is
// what real Node consumers pay per call.
// Run manually AFTER the Rust probe, from napi/:
//   node test/boundary-node.cjs

const { Database } = require('../index.js');
const os = require('os');
const path = require('path');
const { performance } = require('perf_hooks');

const dataPath = path.join(os.tmpdir(), 'ndb_boundary', 'data.jsonl');

async function main() {
  const db = new Database(dataPath);
  console.log(`opened ${dataPath} (${db.len()} docs)`);

  // ── 1-hit query (boundary overhead floor) ────────────────────────
  let t = performance.now();
  let n = 200;
  for (let i = 0; i < n; i++) {
    const r = await db.query({ i: { $eq: 4999 } });
    if (r.length !== 1) throw new Error(`expected 1 hit, got ${r.length}`);
  }
  console.log(`query_1hit        ${((
    (performance.now() - t) * 1000) / n).toFixed(1).padStart(10)} µs/op`);

  // ── 500-hit query (~300 KB of results per op) ────────────────────
  t = performance.now();
  n = 50;
  for (let i = 0; i < n; i++) {
    const r = await db.query({ status: { $eq: 'active' } });
    if (r.length !== 500) throw new Error(`expected 500 hits, got ${r.length}`);
  }
  console.log(`query_500hits     ${((
    (performance.now() - t) * 1000) / n).toFixed(1).padStart(10)} µs/op`);

  // ── same 500 hits, PROJECTED to 3 small fields (queryPage) ────────
  t = performance.now();
  n = 50;
  for (let i = 0; i < n; i++) {
    const page = await db.queryPage(
      { status: { $eq: 'active' } },
      { sortBy: 'i', sortDir: 'asc', limit: 500 },
      ['i', 'status', 'title']
    );
    if (page.total !== 500 || page.results.length !== 500) {
      throw new Error(`expected 500/500, got ${page.total}/${page.results.length}`);
    }
  }
  console.log(`queryPage_500hits ${((
    (performance.now() - t) * 1000) / n).toFixed(1).padStart(10)} µs/op`);

  // ── get one doc (~600 B per op) ──────────────────────────────────
  const probe = await db.query({ i: { $eq: 0 } });
  const firstActive = probe[0]._id;
  t = performance.now();
  n = 2000;
  for (let i = 0; i < n; i++) {
    const doc = db.get(firstActive);
    if (typeof doc.body !== 'string') throw new Error('bad doc');
  }
  console.log(`get_1doc          ${((
    (performance.now() - t) * 1000) / n).toFixed(1).padStart(10)} µs/op`);

  // ── iter: whole DB (~3 MB per op) ────────────────────────────────
  t = performance.now();
  n = 5;
  for (let i = 0; i < n; i++) {
    const all = db.iter();
    if (all.length !== 5000) throw new Error(`expected 5000, got ${all.length}`);
  }
  console.log(`iter_5000docs     ${((
    (performance.now() - t) * 1000) / n).toFixed(1).padStart(10)} µs/op`);

  db.close();
}

main().catch((e) => { console.error(e); process.exit(1); });
