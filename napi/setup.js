#!/usr/bin/env node
/**
 * Setup script for nDB Node.js bindings (git submodule workflow)
 * 
 * Usage: node setup.js
 * 
 * This script:
 * 1. Builds the native Rust module
 * 2. Creates the appropriate symlink/copy for your platform
 */

const { execSync } = require('child_process');
const { existsSync, copyFileSync, symlinkSync, constants } = require('fs');
const { join } = require('path');

const platform = process.platform;
const arch = process.arch;

// Platform-specific configurations
const configs = {
  'win32': {
    'x64': { source: '..\\target\\release\\ndb_node.dll', target: 'ndb-node.win32-x64-msvc.node' },
    'arm64': { source: '..\\target\\release\\ndb_node.dll', target: 'ndb-node.win32-arm64-msvc.node' }
  },
  'darwin': {
    'x64': { source: '../target/release/libndb_node.dylib', target: 'ndb-node.darwin-x64.node' },
    'arm64': { source: '../target/release/libndb_node.dylib', target: 'ndb-node.darwin-arm64.node' }
  },
  'linux': {
    'x64': { source: '../target/release/libndb_node.so', target: 'ndb-node.linux-x64-gnu.node' },
    'arm64': { source: '../target/release/libndb_node.so', target: 'ndb-node.linux-arm64-gnu.node' }
  }
};

function main() {
  console.log(`Setting up nDB Node.js bindings for ${platform}-${arch}...\n`);
  
  const platformConfig = configs[platform];
  if (!platformConfig) {
    console.error(`Error: Unsupported platform: ${platform}`);
    process.exit(1);
  }
  
  const config = platformConfig[arch];
  if (!config) {
    console.error(`Error: Unsupported architecture ${arch} on ${platform}`);
    process.exit(1);
  }
  
  const sourcePath = join(__dirname, config.source);
  const targetPath = join(__dirname, config.target);
  
  // Step 1: Build the native module
  console.log('Step 1: Building native module...');
  try {
    execSync('cargo build --release -p ndb-node', {
      cwd: join(__dirname, '..'),
      stdio: 'inherit'
    });
    console.log('Build successful!\n');
  } catch (e) {
    console.error('Build failed:', e.message);
    process.exit(1);
  }
  
  // Step 2: Check if source exists
  if (!existsSync(sourcePath)) {
    console.error(`Error: Built binary not found at ${sourcePath}`);
    console.error('The build may have failed or produced a differently named file.');
    process.exit(1);
  }
  
  // Step 3: Create link/copy
  console.log('Step 2: Creating platform-specific binary...');
  console.log(`  Source: ${sourcePath}`);
  console.log(`  Target: ${targetPath}`);
  
  try {
    // Remove existing file if present
    if (existsSync(targetPath)) {
      require('fs').unlinkSync(targetPath);
    }
    
    if (platform === 'win32') {
      // Windows: Copy file (symlinks require special permissions)
      copyFileSync(sourcePath, targetPath);
      console.log('  Copied (Windows)\n');
    } else {
      // Unix: Try symlink first, fall back to copy
      try {
        symlinkSync(sourcePath, targetPath);
        console.log('  Symlinked (Unix)\n');
      } catch (e) {
        copyFileSync(sourcePath, targetPath);
        console.log('  Copied (symlink failed)\n');
      }
    }
  } catch (e) {
    console.error('Error:', e.message);
    process.exit(1);
  }
  
  console.log('Setup complete!');
  console.log('\nYou can now use nDB in your Node.js project:');
  console.log(`  const { Database } = require('./ndb/napi');`);
  console.log('\nOr run the example:');
  console.log(`  node examples/basic.js`);
}

main();
