/**
 * nDB Node.js Native Bindings - Git Submodule Version
 * 
 * This module loads the native nDB bindings directly.
 * Build the native module first with: cargo build --release -p ndb-node
 * 
 * For git submodule workflow:
 * 1. Add nDB as submodule: git submodule add https://github.com/ndb/ndb.git ndb
 * 2. Build: cd ndb && cargo build --release -p ndb-node
 * 3. The loader below will find the .node file (or .dll/.so/.dylib)
 */

const { existsSync } = require('fs');
const { join, dirname } = require('path');

// Determine the correct native binary name based on platform
function getNativeBinaryName() {
  const platform = process.platform;
  const arch = process.arch;
  
  // Map platform/arch to binary name
  const names = {
    'win32': {
      'x64': 'ndb-node.win32-x64-msvc.node',
      'arm64': 'ndb-node.win32-arm64-msvc.node'
    },
    'darwin': {
      'x64': 'ndb-node.darwin-x64.node',
      'arm64': 'ndb-node.darwin-arm64.node'
    },
    'linux': {
      'x64': 'ndb-node.linux-x64-gnu.node',
      'arm64': 'ndb-node.linux-arm64-gnu.node'
    }
  };
  
  const platformNames = names[platform];
  if (!platformNames) {
    throw new Error(`Unsupported platform: ${platform}`);
  }
  
  const binaryName = platformNames[arch];
  if (!binaryName) {
    throw new Error(`Unsupported architecture ${arch} on ${platform}`);
  }
  
  return binaryName;
}

// Find the native binary
function findNativeBinary() {
  const binaryName = getNativeBinaryName();
  const moduleDir = __dirname;
  
  // Search paths in order of preference
  const searchPaths = [
    // 1. Same directory as this file (if copied/renamed)
    join(moduleDir, binaryName),
    // 2. Raw DLL name (Windows dev builds)
    join(moduleDir, 'ndb_node.dll'),
    // 3. Parent directory (target/release relative to napi folder)
    join(moduleDir, '..', 'target', 'release', 'ndb_node.dll'),
    join(moduleDir, '..', 'target', 'release', 'libndb_node.so'),
    join(moduleDir, '..', 'target', 'release', 'libndb_node.dylib'),
    // 4. Direct build output (various platforms)
    join(moduleDir, 'ndb_node.node'),
    join(moduleDir, 'ndb_node.dll'),
    join(moduleDir, 'libndb_node.so'),
    join(moduleDir, 'libndb_node.dylib'),
  ];
  
  for (const path of searchPaths) {
    if (existsSync(path)) {
      return path;
    }
  }
  
  throw new Error(
    `Native binary not found. Searched:\n` +
    searchPaths.map(p => `  - ${p}`).join('\n') +
    `\n\nBuild instructions:\n` +
    `  cargo build --release -p ndb-node\n` +
    `\nThen either:\n` +
    `  - Copy target/release/ndb_node.dll to ${binaryName}\n` +
    `  - Or create a symlink\n` +
    `  - Or set NODE_NDB_NATIVE_PATH environment variable`
  );
}

// Allow override via environment variable
const nativePath = process.env.NODE_NDB_NATIVE_PATH || findNativeBinary();

// Load the native module
let nativeBinding;
try {
  nativeBinding = require(nativePath);
} catch (e) {
  throw new Error(`Failed to load native module from ${nativePath}: ${e.message}`);
}

// Export the classes
module.exports.Database = nativeBinding.Database;
module.exports.Collection = nativeBinding.Collection;
module.exports.FilterBuilder = nativeBinding.FilterBuilder;

// Also export the native path for debugging
module.exports.NATIVE_PATH = nativePath;
