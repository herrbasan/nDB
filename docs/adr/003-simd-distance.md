# ADR 003: SIMD Distance Computation

**Date:** 2026-02-14  
**Status:** Accepted  
**Deciders:** nDB Contributors

## Context

Distance computation dominates search time. For 768-dimensional vectors:
- Scalar: ~1500 FMA operations per comparison
- Need hardware acceleration for sub-millisecond search

## Decision

We use **SIMD via the `wide` crate** for portable acceleration.

### Implementation

```rust
// f32x8 for 8-lane SIMD (AVX2/AVX-512/NEON)
pub fn dot_product_simd(a: &[f32], b: &[f32]) -> f32 {
    let chunks = a.chunks_exact(8);
    let remainder = chunks.remainder();
    
    let mut sum = f32x8::ZERO;
    for (a_chunk, b_chunk) in chunks.zip(b.chunks_exact(8)) {
        let a_vec = f32x8::from_slice(a_chunk);
        let b_vec = f32x8::from_slice(b_chunk);
        sum += a_vec * b_vec;
    }
    
    sum.sum() + scalar_remainder(remainder, &b[...])
}
```

### Supported Operations

- Dot product: `f32x8` multiply-add
- Cosine: Normalize + dot product
- Euclidean: Difference squared + sum

## Consequences

### Positive

- **4-8x speedup** vs scalar on AVX2
- **Portable**: Same code for x86_64, ARM
- **Stable Rust**: No intrinsics or unsafe required

### Negative

- **8-element granularity**: Remainder handled separately
- **Warmup required**: First call initializes dispatch

## Verification

Property-based tests verify SIMD vs scalar parity:

```rust
proptest! {
    #[test]
    fn prop_dot_product_simd_scalar_match(
        a in vec(-10.0f32..10.0, 1..100),
        b in vec(-10.0f32..10.0, 1..100)
    ) {
        prop_assert!(
            (dot_product_simd(&a, &b) - dot_product_scalar(&a, &b)).abs() < 1e-3
        );
    }
}
```

## Alternatives Considered

### std::arch intrinsics
- Rejected: Requires unsafe, platform-specific

### Auto-vectorization
- Rejected: Unreliable across Rust versions

### BLAS/LAPACK
- Rejected: External dependency, overkill for dot products

## References

- `wide` crate: https://docs.rs/wide
- Distance tests: `src/distance.rs`
