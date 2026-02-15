# nDB gRPC Integration

> **Standalone vector database service**  
> **Version:** 0.1.0  
> **Last Updated:** 2026-02-15

---

## Overview

nDB gRPC provides a **standalone executable** that runs nDB as a network service. Any language with gRPC support can connect to it—Python, Go, Node.js, Java, etc.

### When to Use This

| Scenario | Solution |
|----------|----------|
| **Python app needs nDB** | Run `ndb-grpc.exe`, connect via gRPC client |
| **Multi-language team** | One binary, many clients |
| **Separate database server** | Run on dedicated machine/container |
| **Microservices** | Vector service independent of app logic |

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                Machine / Container                          │
│  ┌─────────────────────────────────────────────────────┐   │
│  │  ndb-grpc (standalone executable)                   │   │
│  │  ┌─────────────┐      ┌─────────────────────────┐   │   │
│  │  │ gRPC Server │──────│ nDB Engine              │   │   │
│  │  │  :50051     │      │ • Collections           │   │   │
│  │  └─────────────┘      │ • HNSW index            │   │   │
│  │                       │ • Memory-mapped files   │   │   │
│  │                       └─────────────────────────┘   │   │
│  └─────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┼───────────────┐
              ▼               ▼               ▼
        ┌─────────┐    ┌─────────┐    ┌─────────┐
        │  Node   │    │ Python  │    │   Go    │
        │  Client │    │  Client │    │  Client │
        └─────────┘    └─────────┘    └─────────┘
```

---

## Quick Start

### 1. Download the Binary

```bash
# Linux x64
curl -LO https://github.com/ndb/ndb/releases/latest/download/ndb-grpc-linux-x64.tar.gz
tar xzf ndb-grpc-linux-x64.tar.gz

# Windows x64 (PowerShell)
Invoke-WebRequest https://github.com/ndb/ndb/releases/latest/download/ndb-grpc-windows-x64.zip -OutFile ndb-grpc.zip
Expand-Archive ndb-grpc.zip -DestinationPath C:\ndb

# macOS (Apple Silicon)
curl -LO https://github.com/ndb/ndb/releases/latest/download/ndb-grpc-macos-arm64.tar.gz
tar xzf ndb-grpc-macos-arm64.tar.gz
```

### 2. Run the Server

```bash
# Start with defaults (port 50051, data in ./data)
./ndb-grpc

# Or specify options
./ndb-grpc --data-dir /var/lib/ndb --port 50051
```

### 3. Connect from Your Language

```javascript
// Node.js example
const client = new VectorService('localhost:50051', grpc.credentials.createInsecure());
const results = await client.search({ vector: query, topK: 10 });
```

---

## Installation

### Option 1: Pre-built Binary (Recommended)

Download from GitHub releases:

| Platform | Download |
|----------|----------|
| Linux x64 | `ndb-grpc-linux-x64.tar.gz` |
| Linux ARM64 | `ndb-grpc-linux-arm64.tar.gz` |
| Windows x64 | `ndb-grpc-windows-x64.zip` |
| macOS x64 | `ndb-grpc-macos-x64.tar.gz` |
| macOS ARM64 | `ndb-grpc-macos-arm64.tar.gz` |

```bash
# Example: Linux install
wget https://github.com/ndb/ndb/releases/latest/download/ndb-grpc-linux-x64.tar.gz
tar xzf ndb-grpc-linux-x64.tar.gz
sudo mv ndb-grpc /usr/local/bin/
sudo chmod +x /usr/local/bin/ndb-grpc
```

### Option 2: Cargo Install

```bash
cargo install ndb-grpc
```

### Option 3: Docker

```bash
docker run -p 50051:50051 -v $(pwd)/data:/data ndb/ndb-grpc:latest
```

---

## CLI Reference

### Basic Usage

```bash
ndb-grpc [OPTIONS]
```

### Flags

| Flag | Environment Variable | Default | Description |
|------|---------------------|---------|-------------|
| `--data-dir` | `NDB_DATA_DIR` | `./data` | Database storage directory |
| `--port` | `NDB_PORT` | `50051` | gRPC server port |
| `--host` | `NDB_HOST` | `0.0.0.0` | Bind address (`127.0.0.1` for local only) |
| `--tls-cert` | `NDB_TLS_CERT` | - | TLS certificate file |
| `--tls-key` | `NDB_TLS_KEY` | - | TLS private key file |
| `--read-only` | `NDB_READ_ONLY` | `false` | Run in read-only mode |
| `--max-msg-size` | `NDB_MAX_MSG_SIZE` | `64MB` | Max gRPC message size |

### Examples

```bash
# Basic - local development
ndb-grpc

# Production - specific directories
ndb-grpc --data-dir /var/lib/ndb --port 50051

# Local only (no external access)
ndb-grpc --host 127.0.0.1

# With TLS
ndb-grpc --tls-cert server.crt --tls-key server.key

# Read replica (read-only access)
ndb-grpc --data-dir /shared/ndb --read-only
```

---

## Running as a Service

### Windows Service

```powershell
# Download and extract to C:\ndb
# Create data directory
New-Item -ItemType Directory -Force -Path C:\ndb\data

# Install using nssm (https://nssm.cc/)
nssm install nDB-GRPC C:\ndb\ndb-grpc.exe
nssm set nDB-GRPC AppDirectory C:\ndb
nssm set nDB-GRPC AppParameters --data-dir C:\ndb\data --host 127.0.0.1
nssm start nDB-GRPC

# Check status
nssm status nDB-GRPC
```

### Linux systemd

```ini
# /etc/systemd/system/ndb-grpc.service
[Unit]
Description=nDB gRPC Vector Database
After=network.target

[Service]
Type=simple
User=ndb
Group=ndb
WorkingDirectory=/var/lib/ndb
ExecStart=/usr/local/bin/ndb-grpc --data-dir /var/lib/ndb --port 50051
Restart=always
RestartSec=5
Environment="RUST_LOG=info"

[Install]
WantedBy=multi-user.target
```

```bash
# Setup
sudo useradd -r -s /bin/false ndb
sudo mkdir -p /var/lib/ndb
sudo chown ndb:ndb /var/lib/ndb
sudo cp ndb-grpc /usr/local/bin/
sudo cp ndb-grpc.service /etc/systemd/system/

# Start
sudo systemctl daemon-reload
sudo systemctl enable ndb-grpc
sudo systemctl start ndb-grpc
sudo systemctl status ndb-grpc

# Logs
sudo journalctl -u ndb-grpc -f
```

### macOS LaunchAgent

```xml
<!-- ~/Library/LaunchAgents/com.ndb.grpc.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.ndb.grpc</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/ndb-grpc</string>
        <string>--data-dir</string>
        <string>/Users/yourname/ndb-data</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
```

```bash
launchctl load ~/Library/LaunchAgents/com.ndb.grpc.plist
launchctl start com.ndb.grpc
```

---

## Client Usage

### Protocol Buffer Definition

Save as `ndb.proto`:

```protobuf
syntax = "proto3";
package ndb;

service VectorService {
  rpc CreateCollection(CreateCollectionRequest) returns (Collection);
  rpc GetCollection(GetCollectionRequest) returns (Collection);
  rpc ListCollections(Empty) returns (CollectionList);
  
  rpc Insert(InsertRequest) returns (Document);
  rpc Get(GetRequest) returns (Document);
  rpc Delete(DeleteRequest) returns (DeleteResponse);
  
  rpc Search(SearchRequest) returns (SearchResponse);
  
  rpc Flush(CollectionRequest) returns (Empty);
  rpc Compact(CollectionRequest) returns (CompactionResult);
  rpc BuildIndex(BuildIndexRequest) returns (Empty);
}

message Empty {}

message Collection {
  string name = 1;
  uint32 dimension = 2;
  string durability = 3;
}

message CreateCollectionRequest {
  string name = 1;
  uint32 dimension = 2;
  string durability = 3;  // "buffered" or "sync"
}

message GetCollectionRequest {
  string name = 1;
}

message CollectionList {
  repeated Collection collections = 1;
}

message Document {
  string id = 1;
  repeated float vector = 2;
  string payload = 3;  // JSON string
}

message InsertRequest {
  string collection = 1;
  Document document = 2;
}

message GetRequest {
  string collection = 1;
  string id = 2;
}

message DeleteRequest {
  string collection = 1;
  string id = 2;
}

message DeleteResponse {
  bool deleted = 1;
}

message SearchRequest {
  string collection = 1;
  repeated float vector = 2;
  uint32 top_k = 3;
  string distance = 4;      // "cosine", "dot", "euclidean"
  bool approximate = 5;     // Use HNSW index
  uint32 ef = 6;            // HNSW quality parameter
  string filter = 7;        // JSON filter expression
}

message Match {
  string id = 1;
  float score = 2;
  string payload = 3;
}

message SearchResponse {
  repeated Match results = 1;
  uint32 total_candidates = 2;
}

message CollectionRequest {
  string name = 1;
}

message CompactionResult {
  uint32 docs_before = 1;
  uint32 docs_after = 2;
}

message BuildIndexRequest {
  string collection = 1;
}
```

### Node.js Client

```bash
npm install @grpc/grpc-js @grpc/proto-loader
```

```javascript
const grpc = require('@grpc/grpc-js');
const protoLoader = require('@grpc/proto-loader');
const path = require('path');

// Load proto
const PROTO_PATH = path.join(__dirname, 'ndb.proto');
const packageDef = protoLoader.loadSync(PROTO_PATH);
const ndbProto = grpc.loadPackageDefinition(packageDef).ndb;

// Create client
const client = new ndbProto.VectorService(
  'localhost:50051',
  grpc.credentials.createInsecure()
);

// Promise wrapper
const call = (method) => (req) => new Promise((resolve, reject) => {
  client[method](req, (err, res) => err ? reject(err) : resolve(res));
});

// API
const api = {
  createCollection: call('createCollection'),
  insert: call('insert'),
  search: call('search'),
};

// Usage
async function main() {
  // Create collection
  await api.createCollection({
    name: 'documents',
    dimension: 768,
    durability: 'sync'
  });

  // Insert
  await api.insert({
    collection: 'documents',
    document: {
      id: 'doc1',
      vector: new Array(768).fill(0.1),
      payload: JSON.stringify({ title: 'Hello' })
    }
  });

  // Search
  const { results } = await api.search({
    collection: 'documents',
    vector: new Array(768).fill(0.15),
    topK: 10,
    approximate: true
  });

  console.log(results);
}

main().catch(console.error);
```

### Python Client

```bash
pip install grpcio grpcio-tools
```

```python
import grpc
import ndb_pb2
import ndb_pb2_grpc
import json

# Generate code from proto
# python -m grpc_tools.protoc -I. --python_out=. --grpc_python_out=. ndb.proto

# Connect
channel = grpc.insecure_channel('localhost:50051')
stub = ndb_pb2_grpc.VectorServiceStub(channel)

# Create collection
stub.CreateCollection(ndb_pb2.CreateCollectionRequest(
    name='documents',
    dimension=768,
    durability='sync'
))

# Insert
stub.Insert(ndb_pb2.InsertRequest(
    collection='documents',
    document=ndb_pb2.Document(
        id='doc1',
        vector=[0.1] * 768,
        payload=json.dumps({'title': 'Hello'})
    )
))

# Search
response = stub.Search(ndb_pb2.SearchRequest(
    collection='documents',
    vector=[0.15] * 768,
    top_k=10,
    approximate=True
))

for match in response.results:
    print(f"{match.id}: {match.score}")
```

### Go Client

```bash
go get google.golang.org/grpc
```

```go
package main

import (
    "context"
    "fmt"
    "log"
    
    "google.golang.org/grpc"
    pb "your-module/ndb"
)

func main() {
    conn, err := grpc.Dial("localhost:50051", grpc.WithInsecure())
    if err != nil {
        log.Fatal(err)
    }
    defer conn.Close()
    
    client := pb.NewVectorServiceClient(conn)
    ctx := context.Background()
    
    // Create collection
    client.CreateCollection(ctx, &pb.CreateCollectionRequest{
        Name:       "documents",
        Dimension:  768,
        Durability: "sync",
    })
    
    // Search
    res, err := client.Search(ctx, &pb.SearchRequest{
        Collection:  "documents",
        Vector:      make([]float32, 768),
        TopK:        10,
        Approximate: true,
    })
    if err != nil {
        log.Fatal(err)
    }
    
    for _, m := range res.Results {
        fmt.Printf("%s: %f\n", m.Id, m.Score)
    }
}
```

---

## Docker Deployment

### Quick Run

```bash
docker run -d \
  --name ndb-grpc \
  -p 50051:50051 \
  -v $(pwd)/data:/data \
  -e NDB_DATA_DIR=/data \
  ndb/ndb-grpc:latest
```

### Docker Compose

```yaml
# docker-compose.yml
version: '3.8'

services:
  ndb:
    image: ndb/ndb-grpc:latest
    ports:
      - "50051:50051"
    volumes:
      - ndb-data:/data
    environment:
      - NDB_DATA_DIR=/data
      - NDB_HOST=0.0.0.0
      - RUST_LOG=info
    restart: unless-stopped

volumes:
  ndb-data:
```

```bash
docker-compose up -d
docker-compose logs -f
```

### Kubernetes

```yaml
# k8s-deployment.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ndb-grpc
spec:
  replicas: 1
  selector:
    matchLabels:
      app: ndb-grpc
  template:
    metadata:
      labels:
        app: ndb-grpc
    spec:
      containers:
      - name: ndb
        image: ndb/ndb-grpc:latest
        ports:
        - containerPort: 50051
        volumeMounts:
        - name: data
          mountPath: /data
        env:
        - name: NDB_DATA_DIR
          value: /data
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: ndb-data
---
apiVersion: v1
kind: Service
metadata:
  name: ndb-grpc
spec:
  selector:
    app: ndb-grpc
  ports:
  - port: 50051
    targetPort: 50051
```

---

## Read Replica Setup

For scaling reads, run multiple instances with `--read-only`:

```
┌──────────────────────────────────────────┐
│           Writer (Primary)               │
│     ndb-grpc --data-dir /shared          │
└──────────────────────────────────────────┘
                   │
         ┌─────────┴─────────┐
         │    Shared Volume  │
         └─────────┬─────────┘
                   │
    ┌──────────────┼──────────────┐
    ▼              ▼              ▼
┌────────┐    ┌────────┐    ┌────────┐
│ Reader │    │ Reader │    │ Reader │
│  :50051│    │  :50052│    │  :50053│
│ --read │    │ --read │    │ --read │
│ -only  │    │ -only  │    │ -only  │
└────────┘    └────────┘    └────────┘
```

```bash
# Primary (writes)
ndb-grpc --data-dir /shared/ndb --port 50051

# Readers (read-only)
ndb-grpc --data-dir /shared/ndb --port 50052 --read-only
ndb-grpc --data-dir /shared/ndb --port 50053 --read-only
```

**Note:** Only one writer allowed. Writers use file locking to prevent corruption.

---

## TLS / Security

### Generate Self-Signed Certificates

```bash
# Generate CA
openssl req -new -x509 -days 365 -keyout ca.key -out ca.crt

# Generate server cert
openssl req -new -keyout server.key -out server.csr
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -out server.crt -days 365
```

### Server with TLS

```bash
ndb-grpc --tls-cert server.crt --tls-key server.key
```

### Client with TLS

```javascript
const fs = require('fs');
const credentials = grpc.credentials.createSsl(
  fs.readFileSync('ca.crt')
);
const client = new ndbProto.VectorService('server:50051', credentials);
```

---

## Troubleshooting

### Connection Refused

```bash
# Check server is running
grpcurl -plaintext localhost:50051 list

# Check firewall
nc -zv localhost 50051

# Check logs
journalctl -u ndb-grpc -f
```

### Permission Denied

```bash
# Fix data directory permissions
sudo chown -R $(whoami):$(whoami) /var/lib/ndb
```

### High Memory Usage

```bash
# Check collection stats
grpcurl -plaintext localhost:50051 ndb.VectorService/GetStats

# Compact to reclaim space
grpcurl -plaintext localhost:50051 ndb.VectorService/Compact
```

### Collection Locked

```bash
# Another instance has the database open
# Check for running processes
lsof /var/lib/ndb/*/LOCK
```

---

## Building from Source

If you need to compile the binary yourself:

```bash
# Clone
git clone https://github.com/ndb/ndb
cd ndb/ndb-grpc

# Build release binary
cargo build --release

# Output: target/release/ndb-grpc
```

### Cross-Compile

```bash
# Windows from Linux
cargo build --release --target x86_64-pc-windows-gnu

# ARM64
cargo build --release --target aarch64-unknown-linux-gnu
```

---

## License

MIT OR Apache-2.0 (same as nDB)
