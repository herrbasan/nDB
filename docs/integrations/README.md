# nDB Integrations

This directory contains integration guides for using nDB in different environments and languages.

## Integration Options

All three integration approaches are **officially supported and maintained**. Choose based on your deployment constraints, not hierarchy.

| Integration | Best For | Performance | File System |
|-------------|----------|-------------|-------------|
| **[N-API](./napi.md)** | Node.js servers, maximum performance | Native speed | Full (mmap) |
| **[WebAssembly](./wasm.md)** | Browser/Edge, no native dependencies | ~80-90% native | In-memory only |
| **[gRPC](./grpc.md)** | Multi-language, distributed systems | Network overhead | Full (service) |

## Quick Selection Guide

```
┌─────────────────────────────────────────────────────────────┐
│  What is your deployment environment?                       │
└─────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
   Node.js server       Browser/Edge           Multi-language
   (Express, etc.)      (Vercel, CF)          or microservices
        │                     │                     │
        ▼                     ▼                     ▼
   ┌─────────┐          ┌─────────┐           ┌─────────┐
   │  N-API  │          │   WASM  │           │  gRPC   │
   │  napi   │          │   wasm  │           │  grpc   │
   └─────────┘          └─────────┘           └─────────┘
```

## Installation

### N-API (Node.js)

```bash
npm install ndb-node
```

### WebAssembly

```bash
npm install ndb-wasm
```

### gRPC Client (Node.js)

```bash
npm install @grpc/grpc-js @grpc/proto-loader ndb-grpc-client
```

## Feature Comparison

| Feature | N-API | WASM | gRPC |
|---------|-------|------|------|
| **Performance** | Native | ~80-90% | Network overhead |
| **Persistence** | Memory-mapped files | In-memory / manual | Memory-mapped files |
| **SIMD** | AVX2/AVX-512 | SIMD128 | AVX2/AVX-512 (server) |
| **Threading** | Multi-thread | Single-thread | Multi-thread |
| **Bundle Size** | ~5MB (platform-specific) | ~2MB (universal) | ~500KB (client) |
| **TypeScript** | Optional definitions | Optional definitions | Generated from proto |
| **Browser** | ❌ | ✅ | ✅ (via HTTP/2) |
| **Edge Functions** | ❌ | ✅ | ✅ |
| **Multi-language** | ❌ (Node.js only) | ✅ (any WASM runtime) | ✅ (any gRPC client) |

## Code Examples

### N-API

```javascript
const { Database } = require('ndb-node');

const db = new Database('./data');
const coll = db.createCollection('docs', 768);

coll.insert('id', vector, JSON.stringify(payload));
const results = coll.search({ vector: query, topK: 10 });
```

### WebAssembly

```javascript
import init, { WasmDB } from 'ndb-wasm';

await init();
const db = new WasmDB();
const coll = db.createCollection('docs', 768);

coll.insert('id', new Float32Array(vector), JSON.stringify(payload));
const results = coll.search(new Float32Array(query), 10);
```

### gRPC

```javascript
const { VectorServiceClient } = require('ndb-grpc-client');

const client = new VectorServiceClient('localhost:50051');

await client.insert({ collection: 'docs', document: { id, vector, payload } });
const { results } = await client.search({ collection: 'docs', vector: query, topK: 10 });
```

## TypeScript Support

All three integrations provide TypeScript definitions:

- **N-API**: JSDoc annotations + optional `@types/ndb-node`
- **WASM**: JSDoc annotations + optional type definitions
- **gRPC**: Auto-generated from `.proto` files

All examples use **plain JavaScript** - TypeScript is completely optional.

## Getting Help

- **N-API issues**: See [N-API troubleshooting](./napi.md#troubleshooting)
- **WASM issues**: See [WASM troubleshooting](./wasm.md#troubleshooting)
- **gRPC issues**: See [gRPC troubleshooting](./grpc.md#troubleshooting)
- **General issues**: https://github.com/ndb/ndb/issues
