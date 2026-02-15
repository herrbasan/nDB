# nDB Scope and Boundaries

> **What nDB is, what it isn't, and why**

---

## Core Scope

nDB does **one thing well**: **high-performance vector storage and similarity search**.

```
┌─────────────────────────────────────────────────────────────┐
│                      YOUR APPLICATION                       │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────────┐ │
│  │   Text/     │───▶│  Embedding  │───▶│  nDB: Vector    │ │
│  │   Data      │    │  Service    │    │  Storage/Search │ │
│  │  (Your job) │    │  (Your job) │    │   (Our job)     │ │
│  └─────────────┘    └─────────────┘    └─────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

### nDB Handles:
- ✅ Vector storage (memory-mapped, zero-copy)
- ✅ Similarity search (exact & HNSW approximate)
- ✅ Multi-collection management
- ✅ Persistence and crash recovery
- ✅ Multi-process safety (single writer, multiple readers)

### nDB Does NOT Handle:
- ❌ Text embedding generation
- ❌ Model management
- ❌ Tokenization
- ❌ External API calls (OpenAI, etc.)
- ❌ Image/audio processing

---

## Why This Boundary?

### 1. Separation of Concerns

Embedding is a **different problem** with different constraints:

| Aspect | Embedding | Vector Search |
|--------|-----------|---------------|
| **Dependencies** | Heavy (models, APIs) | None (self-contained) |
| **Latency** | 100ms-10s (API call) | <1ms (local) |
| **Failure modes** | Network, rate limits, costs | Local disk, memory |
| **Updates** | Model versions, API changes | Stable file format |

### 2. Existing Solutions Are Excellent

Don't reinvent what already works:

| Use Case | Recommended Solution |
|----------|---------------------|
| Cloud API embeddings | OpenAI, Cohere, Voyage AI |
| Local embeddings | Ollama, sentence-transformers |
| Self-hosted models | HuggingFace, text-embeddings-inference |
| Multi-modal | CLIP (OpenAI), ImageBind (Meta) |

### 3. Zero-Cost Abstractions

Per nDB philosophy: **"Pay only for what you use"**.

Embedding integration would add:
- 100MB-2GB of model files (optional but confusing)
- Network dependency for cloud APIs
- Configuration complexity (API keys, rate limits)
- Version management (model updates)

**Current nDB binary: ~5MB**  
**With embedding models: 500MB-2GB**

---

## Integration Patterns

### Pattern 1: Cloud API (OpenAI, etc.)

```javascript
const { Database } = require('ndb-node');
const OpenAI = require('openai');

const openai = new OpenAI({ apiKey: process.env.OPENAI_API_KEY });
const db = new Database('./data');
const coll = db.createCollection('docs', 1536);

async function ingestDocument(id, text, metadata) {
  // 1. Generate embedding (your responsibility)
  const response = await openai.embeddings.create({
    model: 'text-embedding-3-small',
    input: text,
  });
  const embedding = response.data[0].embedding;
  
  // 2. Store in nDB (nDB's responsibility)
  coll.insert(id, embedding, JSON.stringify({ ...metadata, text }));
}
```

### Pattern 2: Local Embeddings (Ollama)

```javascript
const ollama = require('ollama');

async function ingestLocal(id, text) {
  // Local embedding - no network, no API keys
  const response = await ollama.embeddings({
    model: 'nomic-embed-text',
    prompt: text,
  });
  
  coll.insert(id, response.embedding, JSON.stringify({ text }));
}
```

### Pattern 3: Batch Processing

```javascript
async function ingestBatch(documents) {
  // Batch embedding for efficiency
  const texts = documents.map(d => d.text);
  const embeddings = await embedBatch(texts, 100); // Batch size 100
  
  // Prepare documents
  const docs = documents.map((doc, i) => ({
    id: doc.id,
    vector: embeddings[i],
    payload: JSON.stringify(doc.metadata)
  }));
  
  // Bulk insert
  coll.insertBatch(docs);
  coll.flush();
}
```

---

## Optional Helpers (Future)

We may provide **separate, optional** helper packages for common integration patterns:

```bash
# Optional convenience packages (NOT part of core)
npm install ndb-openai-helper    # OpenAI integration patterns
npm install ndb-ollama-helper    # Ollama integration patterns
```

These would be:
- **Separate repositories** from core nDB
- **Community-maintained** or examples only
- **Not required** for nDB functionality

---

## FAQ

### "But Pinecone/Weaviate/Chroma handle embedding..."

Those are **managed services** with different constraints:
- They control the infrastructure
- They charge per embedding + storage + search
- They can update models centrally

nDB is **embedded software** you run yourself:
- You control the infrastructure
- You choose your embedding provider
- You pay only for what you use (no nDB fees)

### "I want a one-line ingest experience"

Use a managed service if that's your priority. nDB trades convenience for:
- **Performance** (native speed, no network hops)
- **Control** (choose any embedding model)
- **Cost** (no per-request fees)
- **Privacy** (data stays on your machine)

### "What about local models?"

If you need offline embeddings, run Ollama or similar alongside nDB:

```yaml
# docker-compose.yml
services:
  ollama:
    image: ollama/ollama
    volumes:
      - ollama:/root/.ollama
    
  ndb-grpc:
    image: ndb/ndb-grpc
    volumes:
      - ndb-data:/data
```

Your application connects to both.

---

## Summary

| Question | Answer |
|----------|--------|
| Does nDB generate embeddings? | **No** |
| Will it ever? | **Not in core** (optional helpers maybe) |
| Why? | Separation of concerns, existing solutions are excellent |
| What should I use? | OpenAI, Ollama, HuggingFace, or any embedding service |
| How do I integrate? | Generate embeddings → Pass vectors to nDB |

**nDB is a vector database, not an embedding service.**

Use the best tool for each job.
