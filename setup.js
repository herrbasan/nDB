#!/usr/bin/env node
/**
 * Setup script for nDB CLI binaries (git submodule workflow)
 *
 * Usage: node setup.js
 *
 * Builds the `ndb` CLI (including `ndb serve`) in release mode and copies
 * it to bin/. Run this on each machine after pulling the submodule.
 *
 * For the Node.js bindings, run: node napi/setup.js
 */

const { execSync } = require('child_process');
const { copyFileSync, mkdirSync, existsSync } = require('fs');
const { join } = require('path');

function main() {
  const root = join(__dirname, '.');
  const binDir = join(root, 'bin');
  const exeName = process.platform === 'win32' ? 'ndb.exe' : 'ndb';

  console.log('Building nDB CLI (release)...');
  execSync('cargo build --release --bin ndb', { cwd: root, stdio: 'inherit' });

  const source = join(root, 'target', 'release', exeName);
  if (!existsSync(source)) {
    console.error(`Error: built binary not found at ${source}`);
    process.exit(1);
  }

  mkdirSync(binDir, { recursive: true });
  const target = join(binDir, exeName);
  copyFileSync(source, target);
  console.log(`\nDone: ${target}`);
  console.log('Try: ' + (process.platform === 'win32' ? 'bin\\ndb.exe --help' : 'bin/ndb --help'));
}

main();
