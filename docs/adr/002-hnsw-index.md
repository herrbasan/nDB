# ADR 002: HNSW Approximate Search

**Date:** 2026-02-14  
**Status:** Accepted  
**Deciders:** nDB Contributors

## Context

Exact brute-force search becomes slow with >100K vectors. We needed approximate search for:
- Sub-10ms query latency on 10M+ vectors
- Tunable recall vs latency tradeoff
- Index persistence across restarts

## Decision

We implemented **HNSW (Hierarchical Navigable Small World)** graph index.

### Key Parameters

| Parameter | Default | Range | Effect |
|-----------|---------|-------|--------|
| M | 16 | 2-100 | Connections per layer |
| ef_construct | 32 | 10-500 | Build-time search scope |
| ef_search | 64 | 10-500 | Query-time search scope |

### Implementation

```rust
// CSR (Compressed Sparse Row) layout for cache efficiency
pub struct HnswIndex {
    levels: Vec<Vec<u32>>,      // Level assignments
    neighbors: Vec<Vec<u32>>,   // CSR format
    distances: Vec<f32>,        // Edge distances
}
```

## Consequences

### Positive

- **Fast search**: 5-10ms on 10M vectors
- **Tunable recall**: 90-99% typical with proper parameters
- **Simple persistence**: Serialize CSR to file

### Negative

- **Memory overhead**: ~20% of vector data size
- **Build time**: O(n log n), blocks writes during rebuild
- **No incremental updates**: Full rebuild on compaction

## Alternatives Considered

### IVF (Inverted File Index)
- Rejected: Lower recall at same latency

### Product Quantization
- Rejected: Added complexity, memory savings not critical

### Flat Index (Brute Force)
- Rejected: Too slow for >100K vectors

## Performance

| Dataset | Exact | HNSW (ef=64) | Recall |
|---------|-------|--------------|--------|
| 100K | 10ms | 1ms | 95% |
| 1M | 100ms | 2ms | 94% |
| 10M | 1s | 5ms | 93% |

## References

- HNSW paper: Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs"
