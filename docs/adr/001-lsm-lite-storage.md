# ADR 001: LSM-Lite Storage Architecture

**Date:** 2026-02-14  
**Status:** Accepted  
**Deciders:** nDB Contributors

## Context

nDB needed a storage architecture that balances:
- Write throughput for batch embedding ingestion
- Instant recovery regardless of dataset size
- Durability guarantees for production use
- Simplicity for embedded deployment

## Decision

We adopted an **LSM-Lite** architecture:

```
Memtable (RAM) → WAL (disk) → Flush → Segments (mmap)
```

### Components

| Component | Purpose | Persistence |
|-----------|---------|-------------|
| Memtable | Recent writes, fast lookup | WAL-backed |
| WAL | Durability log | Append-only file |
| Segments | Immutable historical data | Memory-mapped |
| Manifest | Atomic state transitions | Atomic rename |

## Consequences

### Positive

- **Instant recovery**: Mmap segments require no parsing
- **Fast writes**: Append-only WAL and memtable
- **Crash-safe**: WAL replay on startup
- **Simple compaction**: Merge segments, rebuild index

### Negative

- **Read amplification**: Search across memtable + all segments
- **Space overhead**: Deleted docs persist until compaction
- **Write amplification**: Compaction rewrites data

## Alternatives Considered

### B-Tree
- Rejected: Write amplification, complex crash recovery

### Pure In-Memory
- Rejected: No durability, long startup times

### Full LSM (RocksDB-style)
- Rejected: Too complex for embedded use case

## References

- LSM Tree paper: O'Neil et al., "The Log-Structured Merge-Tree"
- nDB storage docs: `src/wal.rs`, `src/segment.rs`
