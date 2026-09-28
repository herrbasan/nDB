#!/usr/bin/env node
/**
 * Vendor script: ensure the native ndb-node binary for this platform exists.
 *
 * - If the matching binary is already present (it is committed in the repo
 *   for win32-x64), do nothing — zero network, zero toolchain.
 * - Otherwise download it from the GitHub release matching this package's
 *   version and verify it against the release's SHA-256 sidecar before it
 *   touches the load path. A binary that fails verification is never kept.
 *
 * Unsupported platform/architecture: fails loudly with the build-from-source
 * path (`node napi/setup.js`, requires the Rust toolchain).
 *
 * Usage: node vendor.js
 */

const { existsSync, readFileSync, renameSync, unlinkSync, writeFileSync } = require('fs');
const { createHash } = require('crypto');
const { get } = require('https');
const { join } = require('path');

const BINARIES = {
  'win32:x64': 'ndb-node.win32-x64-msvc.node',
  // Future matrix entries: 'linux:x64': 'ndb-node.linux-x64-gnu.node', …
};

const REPO = 'herrbasan/nDB';

function fail(msg) {
  console.error(`vendor: ${msg}`);
  process.exit(1);
}

/** GET a URL, following redirects, into a Buffer. */
function fetch(url, redirects = 5) {
  return new Promise((resolve, reject) => {
    get(url, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        if (redirects === 0) return reject(new Error(`too many redirects for ${url}`));
        return resolve(fetch(res.headers.location, redirects - 1));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
      }
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve(Buffer.concat(chunks)));
      res.on('error', reject);
    }).on('error', reject);
  });
}

async function main() {
  const key = `${process.platform}:${process.arch}`;
  const name = BINARIES[key];
  if (!name) {
    fail(`no prebuilt binary for ${key}. Build from source: node napi/setup.js (requires Rust).`);
  }

  const target = join(__dirname, name);
  if (existsSync(target)) {
    console.log(`vendor: ${name} already present — nothing to do.`);
    return;
  }

  const { version } = JSON.parse(readFileSync(join(__dirname, 'package.json'), 'utf8'));
  const base = `https://github.com/${REPO}/releases/download/v${version}`;

  console.log(`vendor: fetching ${name} from release v${version}…`);
  const [binary, sidecar] = await Promise.all([
    fetch(`${base}/${name}`),
    fetch(`${base}/${name}.sha256`),
  ]);

  const expected = sidecar.toString('utf8').trim().split(/\s+/)[0];
  const actual = createHash('sha256').update(binary).digest('hex');
  if (actual !== expected) {
    fail(`SHA-256 mismatch for ${name}: expected ${expected}, got ${actual}. Refusing to vendor an unverified binary.`);
  }

  // Atomic-ish: write aside, then rename into the load path.
  const tmp = `${target}.download`;
  writeFileSync(tmp, binary);
  try { unlinkSync(target); } catch { /* not present */ }
  renameSync(tmp, target);

  console.log(`vendor: ${name} v${version} verified (${actual.slice(0, 12)}…) and written to napi/.`);
}

main().catch((e) => fail(e.message));
