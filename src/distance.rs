//! SIMD-accelerated distance functions for vector similarity search.
//!
//! This module provides distance metrics optimized using the `wide` crate
//! for portable SIMD (AVX2, AVX-512, NEON).
//!
//! # Distance Metrics
//!
//! - **DotProduct**: Higher scores indicate greater similarity. Range: unbounded.
//! - **Cosine**: Cosine similarity (normalized dot product). Range: [-1, 1].
//! - **Euclidean**: Euclidean distance (L2). Lower scores indicate greater similarity.
//!
//! # Alignment
//!
//! Vectors in segments are 64-byte aligned. The SIMD implementations use
//! aligned loads when possible for optimal performance.

use crate::error::{Error, Result};

/// Distance metric for similarity search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Distance {
    /// Dot product: sum(a[i] * b[i]). Higher is more similar.
    DotProduct,
    /// Cosine similarity: dot(a, b) / (||a|| * ||b||). Range [-1, 1].
    Cosine,
    /// Euclidean distance: sqrt(sum((a[i] - b[i])^2)). Lower is more similar.
    Euclidean,
}

impl Distance {
    /// Compute the distance between two vectors.
    ///
    /// # Arguments
    ///
    /// * `a` - First vector
    /// * `b` - Second vector (must have same length as `a`)
    ///
    /// # Returns
    ///
    /// The distance score. For DotProduct and Cosine, higher is better.
    /// For Euclidean, lower is better.
    ///
    /// # Errors
    ///
    /// Returns `Error::WrongDimension` if vectors have different lengths.
    pub fn compute(&self, a: &[f32], b: &[f32]) -> Result<f32> {
        if a.len() != b.len() {
            return Err(Error::WrongDimension {
                expected: a.len(),
                got: b.len(),
            });
        }

        let score = match self {
            Distance::DotProduct => dot_product_simd(a, b),
            Distance::Cosine => cosine_similarity_simd(a, b),
            Distance::Euclidean => euclidean_distance_simd(a, b),
        };

        Ok(score)
    }

    /// Returns true if higher scores indicate greater similarity.
    pub fn higher_is_better(&self) -> bool {
        matches!(self, Distance::DotProduct | Distance::Cosine)
    }
}

/// Compute dot product using SIMD.
///
/// Uses `f32x8` from the `wide` crate for 8-wide SIMD operations.
/// Falls back to scalar for remaining elements.
pub fn dot_product_simd(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());

    let dim = a.len();
    let mut sum = wide::f32x8::ZERO;

    // Process 8 elements at a time
    let chunks = dim / 8;
    for i in 0..chunks {
        let offset = i * 8;
        let va = wide::f32x8::from(&a[offset..offset + 8]);
        let vb = wide::f32x8::from(&b[offset..offset + 8]);
        sum = sum + (va * vb);
    }

    // Horizontal sum of SIMD vector
    let mut result: f32 = sum.to_array().iter().sum();

    // Handle remaining elements
    let remainder = dim % 8;
    if remainder > 0 {
        let start = chunks * 8;
        for i in 0..remainder {
            result += a[start + i] * b[start + i];
        }
    }

    result
}

/// Compute cosine similarity using SIMD.
///
/// Cosine similarity = dot(a, b) / (||a|| * ||b||)
///
/// Returns 0.0 if either vector has zero magnitude.
pub fn cosine_similarity_simd(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());

    let dot = dot_product_simd(a, b);
    let norm_a = l2_norm_simd(a);
    let norm_b = l2_norm_simd(b);

    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Compute Euclidean distance using SIMD.
///
/// L2 distance = sqrt(sum((a[i] - b[i])^2))
///
/// Returns the actual distance (not squared).
pub fn euclidean_distance_simd(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());

    let dim = a.len();
    let mut sum = wide::f32x8::ZERO;

    // Process 8 elements at a time
    let chunks = dim / 8;
    for i in 0..chunks {
        let offset = i * 8;
        let va = wide::f32x8::from(&a[offset..offset + 8]);
        let vb = wide::f32x8::from(&b[offset..offset + 8]);
        let diff = va - vb;
        sum = sum + (diff * diff);
    }

    // Horizontal sum of SIMD vector
    let mut result: f32 = sum.to_array().iter().sum();

    // Handle remaining elements
    let remainder = dim % 8;
    if remainder > 0 {
        let start = chunks * 8;
        for i in 0..remainder {
            let diff = a[start + i] - b[start + i];
            result += diff * diff;
        }
    }

    result.sqrt()
}

/// Compute L2 norm (magnitude) of a vector using SIMD.
fn l2_norm_simd(v: &[f32]) -> f32 {
    dot_product_simd(v, v).sqrt()
}

/// Scalar fallback implementations for comparison and testing.
pub mod scalar {
    /// Scalar dot product.
    pub fn dot_product(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    /// Scalar cosine similarity.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot = dot_product(a, b);
        let norm_a = l2_norm(a);
        let norm_b = l2_norm(b);

        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a * norm_b)
        }
    }

    /// Scalar Euclidean distance.
    pub fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
        let sum: f32 = a.iter().zip(b.iter()).map(|(x, y)| (x - y).powi(2)).sum();
        sum.sqrt()
    }

    /// Scalar L2 norm.
    fn l2_norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(a: f32, b: f32, epsilon: f32) {
        assert!(
            (a - b).abs() < epsilon,
            "Expected {} to be close to {} (epsilon={})",
            a,
            b,
            epsilon
        );
    }

    #[test]
    fn test_dot_product_basic() {
        let a = vec![1.0, 2.0, 3.0, 4.0];
        let b = vec![5.0, 6.0, 7.0, 8.0];

        let simd = dot_product_simd(&a, &b);
        let scalar = scalar::dot_product(&a, &b);

        // Expected: 1*5 + 2*6 + 3*7 + 4*8 = 5 + 12 + 21 + 32 = 70
        assert_eq!(scalar, 70.0);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_dot_product_aligned() {
        // 8 elements - exactly one SIMD chunk
        let a = vec![1.0; 8];
        let b = vec![2.0; 8];

        let simd = dot_product_simd(&a, &b);
        let scalar = scalar::dot_product(&a, &b);

        // Expected: 8 * (1 * 2) = 16
        assert_eq!(scalar, 16.0);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_dot_product_with_remainder() {
        // 10 elements - one SIMD chunk + 2 remainder
        let a = vec![1.0; 10];
        let b = vec![2.0; 10];

        let simd = dot_product_simd(&a, &b);
        let scalar = scalar::dot_product(&a, &b);

        // Expected: 10 * (1 * 2) = 20
        assert_eq!(scalar, 20.0);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_cosine_same_vector() {
        let a = vec![1.0, 2.0, 3.0, 4.0];

        let simd = cosine_similarity_simd(&a, &a);
        let scalar = scalar::cosine_similarity(&a, &a);

        // Cosine of a vector with itself is 1.0
        assert_close(scalar, 1.0, 1e-6);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_cosine_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];

        let simd = cosine_similarity_simd(&a, &b);
        let scalar = scalar::cosine_similarity(&a, &b);

        // Orthogonal vectors have 0 cosine similarity
        assert_close(scalar, 0.0, 1e-6);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_cosine_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];

        let simd = cosine_similarity_simd(&a, &b);
        let scalar = scalar::cosine_similarity(&a, &b);

        // Opposite vectors have -1 cosine similarity
        assert_close(scalar, -1.0, 1e-6);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_cosine_zero_vector() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![0.0, 0.0, 0.0];

        let simd = cosine_similarity_simd(&a, &b);
        let scalar = scalar::cosine_similarity(&a, &b);

        // Zero vector should return 0.0
        assert_eq!(scalar, 0.0);
        assert_eq!(simd, 0.0);
    }

    #[test]
    fn test_euclidean_basic() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 6.0, 8.0];

        let simd = euclidean_distance_simd(&a, &b);
        let scalar = scalar::euclidean_distance(&a, &b);

        // Expected: sqrt((4-1)^2 + (6-2)^2 + (8-3)^2) = sqrt(9 + 16 + 25) = sqrt(50) ≈ 7.071
        let expected = (9.0f32 + 16.0 + 25.0).sqrt();
        assert_close(scalar, expected, 1e-6);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_euclidean_same_vector() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];

        let simd = euclidean_distance_simd(&a, &a);
        let scalar = scalar::euclidean_distance(&a, &a);

        // Distance from a vector to itself is 0
        assert_close(scalar, 0.0, 1e-6);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_euclidean_aligned() {
        // 8 elements - exactly one SIMD chunk
        let a = vec![0.0; 8];
        let b = vec![1.0; 8];

        let simd = euclidean_distance_simd(&a, &b);
        let scalar = scalar::euclidean_distance(&a, &b);

        // Expected: sqrt(8 * 1^2) = sqrt(8) ≈ 2.828
        let expected = (8.0f32).sqrt();
        assert_close(scalar, expected, 1e-6);
        assert_close(simd, scalar, 1e-6);
    }

    #[test]
    fn test_distance_enum() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];

        let dot = Distance::DotProduct.compute(&a, &b).unwrap();
        assert_close(dot, 0.0, 1e-6);

        let cos = Distance::Cosine.compute(&a, &b).unwrap();
        assert_close(cos, 0.0, 1e-6);

        let euclid = Distance::Euclidean.compute(&a, &b).unwrap();
        assert_close(euclid, 2.0f32.sqrt(), 1e-6);
    }

    #[test]
    fn test_distance_dimension_mismatch() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];

        let result = Distance::DotProduct.compute(&a, &b);
        assert!(matches!(
            result,
            Err(Error::WrongDimension { expected: 3, got: 2 })
        ));
    }

    #[test]
    fn test_higher_is_better() {
        assert!(Distance::DotProduct.higher_is_better());
        assert!(Distance::Cosine.higher_is_better());
        assert!(!Distance::Euclidean.higher_is_better());
    }

    #[test]
    fn test_large_dimension() {
        // Test with larger dimensions commonly used in embeddings
        let dims = [384, 768, 1536];

        for dim in dims {
            let a: Vec<f32> = (0..dim).map(|i| i as f32 / dim as f32).collect();
            let b: Vec<f32> = (0..dim).map(|i| (dim - i) as f32 / dim as f32).collect();

            let dot_simd = dot_product_simd(&a, &b);
            let dot_scalar = scalar::dot_product(&a, &b);
            assert_close(dot_simd, dot_scalar, 1e-2);

            let cos_simd = cosine_similarity_simd(&a, &b);
            let cos_scalar = scalar::cosine_similarity(&a, &b);
            assert_close(cos_simd, cos_scalar, 1e-2);

            let euclid_simd = euclidean_distance_simd(&a, &b);
            let euclid_scalar = scalar::euclidean_distance(&a, &b);
            assert_close(euclid_simd, euclid_scalar, 1e-2);
        }
    }

    // Property-based tests for distance computation
    use proptest::prelude::*;
    use super::scalar;

    proptest! {

        // Property: SIMD and scalar implementations produce identical results
        #[test]
        fn prop_dot_product_simd_scalar_match(
            a in prop::collection::vec(-10.0f32..10.0, 1..100),
            b in prop::collection::vec(-10.0f32..10.0, 1..100)
        ) {
            let max_len = a.len().max(b.len());
            let mut a = a;
            let mut b = b;
            a.resize(max_len, 0.0);
            b.resize(max_len, 0.0);

            let simd_result = dot_product_simd(&a, &b);
            let scalar_result = scalar::dot_product(&a, &b);

            prop_assert!(
                (simd_result - scalar_result).abs() < 1e-3,
                "SIMD: {}, Scalar: {}", simd_result, scalar_result
            );
        }

        #[test]
        fn prop_cosine_similarity_simd_scalar_match(
            a in prop::collection::vec(-10.0f32..10.0, 1..100),
            b in prop::collection::vec(-10.0f32..10.0, 1..100)
        ) {
            let max_len = a.len().max(b.len());
            let mut a = a;
            let mut b = b;
            a.resize(max_len, 0.0);
            b.resize(max_len, 0.0);

            let simd_result = cosine_similarity_simd(&a, &b);
            let scalar_result = scalar::cosine_similarity(&a, &b);

            prop_assert!(
                (simd_result - scalar_result).abs() < 1e-3,
                "SIMD: {}, Scalar: {}", simd_result, scalar_result
            );
        }

        #[test]
        fn prop_euclidean_distance_simd_scalar_match(
            a in prop::collection::vec(-10.0f32..10.0, 1..100),
            b in prop::collection::vec(-10.0f32..10.0, 1..100)
        ) {
            let max_len = a.len().max(b.len());
            let mut a = a;
            let mut b = b;
            a.resize(max_len, 0.0);
            b.resize(max_len, 0.0);

            let simd_result = euclidean_distance_simd(&a, &b);
            let scalar_result = scalar::euclidean_distance(&a, &b);

            prop_assert!(
                (simd_result - scalar_result).abs() < 1e-3,
                "SIMD: {}, Scalar: {}", simd_result, scalar_result
            );
        }

        // Property: Cosine similarity is bounded [-1, 1]
        #[test]
        fn prop_cosine_bounded(
            a in prop::collection::vec(-10.0f32..10.0, 1..100),
            b in prop::collection::vec(-10.0f32..10.0, 1..100)
        ) {
            let a_norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let b_norm = b.iter().map(|x| x * x).sum::<f32>().sqrt();

            prop_assume!(a_norm > 1e-10);
            prop_assume!(b_norm > 1e-10);

            let max_len = a.len().max(b.len());
            let mut a = a;
            let mut b = b;
            a.resize(max_len, 0.0);
            b.resize(max_len, 0.0);

            let cosine = cosine_similarity_simd(&a, &b);

            prop_assert!(cosine >= -1.0 - 1e-6, "Cosine {} < -1", cosine);
            prop_assert!(cosine <= 1.0 + 1e-6, "Cosine {} > 1", cosine);
        }

        // Property: Cosine similarity is symmetric: cos(a,b) == cos(b,a)
        #[test]
        fn prop_cosine_symmetric(
            a in prop::collection::vec(-10.0f32..10.0, 1..100),
            b in prop::collection::vec(-10.0f32..10.0, 1..100)
        ) {
            let max_len = a.len().max(b.len());
            let mut a = a;
            let mut b = b;
            a.resize(max_len, 0.0);
            b.resize(max_len, 0.0);

            let cos_ab = cosine_similarity_simd(&a, &b);
            let cos_ba = cosine_similarity_simd(&b, &a);

            prop_assert!(
                (cos_ab - cos_ba).abs() < 1e-5,
                "cos(a,b) = {}, cos(b,a) = {}", cos_ab, cos_ba
            );
        }

        // Property: Euclidean distance is symmetric
        #[test]
        fn prop_euclidean_symmetric(
            a in prop::collection::vec(-10.0f32..10.0, 1..100),
            b in prop::collection::vec(-10.0f32..10.0, 1..100)
        ) {
            let max_len = a.len().max(b.len());
            let mut a = a;
            let mut b = b;
            a.resize(max_len, 0.0);
            b.resize(max_len, 0.0);

            let dist_ab = euclidean_distance_simd(&a, &b);
            let dist_ba = euclidean_distance_simd(&b, &a);

            prop_assert!(
                (dist_ab - dist_ba).abs() < 1e-5,
                "dist(a,b) = {}, dist(b,a) = {}", dist_ab, dist_ba
            );
        }

        // Property: Euclidean distance is non-negative
        #[test]
        fn prop_euclidean_non_negative(
            a in prop::collection::vec(-10.0f32..10.0, 1..100),
            b in prop::collection::vec(-10.0f32..10.0, 1..100)
        ) {
            let max_len = a.len().max(b.len());
            let mut a = a;
            let mut b = b;
            a.resize(max_len, 0.0);
            b.resize(max_len, 0.0);

            let dist = euclidean_distance_simd(&a, &b);

            prop_assert!(dist >= 0.0, "Euclidean distance is negative: {}", dist);
        }

        // Property: Distance::compute produces consistent results with standalone functions
        #[test]
        fn prop_distance_enum_consistent(
            a in prop::collection::vec(-10.0f32..10.0, 1..100),
            b in prop::collection::vec(-10.0f32..10.0, 1..100)
        ) {
            let max_len = a.len().max(b.len());
            let mut a = a;
            let mut b = b;
            a.resize(max_len, 0.0);
            b.resize(max_len, 0.0);

            let dot_enum = Distance::DotProduct.compute(&a, &b).unwrap();
            let dot_func = dot_product_simd(&a, &b);
            prop_assert!((dot_enum - dot_func).abs() < 1e-5);

            let cos_enum = Distance::Cosine.compute(&a, &b).unwrap();
            let cos_func = cosine_similarity_simd(&a, &b);
            prop_assert!((cos_enum - cos_func).abs() < 1e-5);

            let euclid_enum = Distance::Euclidean.compute(&a, &b).unwrap();
            let euclid_func = euclidean_distance_simd(&a, &b);
            prop_assert!((euclid_enum - euclid_func).abs() < 1e-5);
        }
    }
}
