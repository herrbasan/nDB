// Guards the test harness itself (issue #8).
//
// test-napi.js used to count a test as passed the moment it was *called*, so an
// async test that later rejected was reported as green and the run exited 0.
// This builds a copy of that suite with one test that rejects after an await and
// asserts the run reports a failure. If the harness ever stops awaiting again,
// this fails loudly instead of quietly inflating the pass count.
//
//   node test/test-harness.cjs

const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const src = path.join(__dirname, 'test-napi.js');
const probe = path.join(__dirname, '.harness-probe.cjs');

const marker = '// ─── Results ─────────────────────────────────────────────────────────';
const source = fs.readFileSync(src, 'utf8');
if (!source.includes(marker)) {
  throw new Error(`results marker not found in ${src} — update test-harness.cjs`);
}

const injected = [
  "test('PROBE: async rejection must be reported', async () => {",
  '  await new Promise((resolve) => setTimeout(resolve, 20));',
  "  throw new Error('probe: this rejection must be counted as a failure');",
  '});',
  '',
  marker,
].join('\n');

fs.writeFileSync(probe, source.replace(marker, injected));

// test-napi.js resolves its binding through ../index.js, so it must stay in this
// directory while it runs.
let output = '';
let code = 0;
try {
  output = execFileSync(process.execPath, [probe], { encoding: 'utf8' });
} catch (e) {
  output = `${e.stdout || ''}${e.stderr || ''}`;
  code = e.status;
}

fs.rmSync(probe, { force: true });

const reported = /Results: (\d+) passed, (\d+) failed/.exec(output);
const sawProbe = /PROBE: async rejection must be reported/.test(output);
const honest = code === 1 && reported !== null && reported[2] === '1' && sawProbe;

console.log('nDB test-harness self-check');
console.log('='.repeat(60));
console.log(`  exit code    : ${code} (expected 1)`);
console.log(`  reported     : ${reported ? reported[0] : '(not found)'}`);
console.log(`  probe listed : ${sawProbe ? 'yes' : 'no'}`);

if (!honest) {
  console.log('\n✗ HARNESS BROKEN: an async rejection did not fail the run.');
  process.exit(1);
}
console.log('\n✓ Harness reports async rejections as failures.');
process.exit(0);
