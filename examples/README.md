# nDB Examples

This directory contains example applications demonstrating various nDB integration patterns.

## Available Examples

### `basic_usage.rs`

A simple command-line example showing:
- Creating a database and collection
- Inserting documents
- Searching with and without filters
- Deleting and compacting

**Run:**
```bash
cargo run --example basic_usage
```

### `web_service.rs`

Demonstrates building a web service with Axum (requires additional dependencies).

**Features shown:**
- HTTP API for search and insert
- Shared state management
- Health checks

**Run:**
```bash
# Uncomment the code in the file and add dependencies to Cargo.toml:
# axum = "0.7"
# tokio = { version = "1", features = ["full"] }
# serde = { version = "1.0", features = ["derive"] }

cargo run --example web_service
```

**Test:**
```bash
# Health check
curl http://localhost:3000/health

# Search
curl -X POST http://localhost:3000/search \
  -H "Content-Type: application/json" \
  -d '{"collection":"products","vector":[0.1,0.2,0.3,0.4],"top_k":5}'

# Insert
curl -X POST http://localhost:3000/insert \
  -H "Content-Type: application/json" \
  -d '{"collection":"products","id":"item1","vector":[0.1,0.2,0.3,0.4]}'
```

### `rag_system.rs`

A Retrieval-Augmented Generation (RAG) system example showing:
- Document chunk storage
- Context retrieval for LLM prompts
- HNSW index management

**Run:**
```bash
cargo run --example rag_system
```

## Creating Your Own Example

To create a new example:

1. Create a file `examples/my_example.rs`
2. Add required dev-dependencies to `Cargo.toml` if needed
3. Run with `cargo run --example my_example`

## Common Patterns

### Error Handling

All examples use `Result` with the `?` operator:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::open("./data")?;
    // ...
    Ok(())
}
```

### Resource Cleanup

nDB uses RAII - resources are cleaned up automatically when values go out of scope:

```rust
{
    let collection = db.get_collection("temp")?;
    // Collection is available here
} // Collection reference dropped here
```

### Temporary Data

Examples use `./example_data` directories. These are ignored by git and can be safely deleted:

```bash
rm -rf ./example_data
```

## Next Steps

After running these examples, see:
- [User Guide](../docs/user-guide.md) - Complete API reference
- [Integration Guide](../docs/integration-guide.md) - Production integration patterns
- [Architecture Decision Records](../docs/adr/) - Design rationale
