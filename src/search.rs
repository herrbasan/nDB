//! Vector search implementation with exact and approximate similarity search.
//!
//! This module provides:
//! - `Search` builder for configuring search parameters
//! - `Match` representing a search result
//! - Exact search over memtable and segments using SIMD acceleration
//! - Integration with HNSW for approximate search (sub-linear time)
//! - Metadata filtering support
//!
//! # Example
//!
//! ```ignore
//! // Exact search (default)
//! let results = collection.search(
//!     Search::new(&query_vector)
//!         .top_k(10)
//!         .distance(Distance::Cosine)
//! )?;
//!
//! // Approximate search using HNSW
//! let results = collection.search(
//!     Search::new(&query_vector)
//!         .top_k(10)
//!         .distance(Distance::Cosine)
//!         .approximate(true)
//!         .ef(100)
//! )?;
//!
//! // Search with metadata filter
//! let results = collection.search(
//!     Search::new(&query_vector)
//!         .top_k(10)
//!         .filter(Filter::and([
//!             Filter::eq("category", "books"),
//!             Filter::gt("year", 2020),
//!         ]))
//! )?;
//! ```

use crate::distance::{Distance, dot_product_simd, cosine_similarity_simd, euclidean_distance_simd};
use crate::error::{Error, Result};
use crate::filter::Filter;
use crate::memtable::Memtable;
use crate::segment::Segment;
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::sync::Arc;

/// A search query builder.
///
/// Use `Search::new()` to create a query, then chain methods to configure:
/// - `top_k(n)` - number of results to return (default: 10)
/// - `distance(metric)` - distance metric to use (default: Cosine)
/// - `approximate(bool)` - use HNSW approximate search (default: false)
/// - `ef(n)` - HNSW search quality parameter (default: None = use index default)
/// - `filter(f)` - metadata filter to apply to results (default: None)
///
/// # Example
///
/// ```ignore
/// let results = collection.search(
///     Search::new(&query)
///         .top_k(5)
///         .distance(Distance::DotProduct)
///         .approximate(true)
///         .ef(100)
///         .filter(Filter::eq("status", "active"))
/// )?;
/// ```
#[derive(Debug, Clone)]
pub struct Search<'a> {
    /// Query vector (borrowed)
    vector: &'a [f32],
    /// Number of results to return
    top_k: usize,
    /// Distance metric
    distance: Distance,
    /// Use approximate search (HNSW) instead of exact
    approximate: bool,
    /// HNSW ef parameter (None = use index default)
    ef: Option<usize>,
    /// Optional metadata filter
    filter: Option<Filter>,
}

impl<'a> Search<'a> {
    /// Create a new search query.
    ///
    /// # Arguments
    ///
    /// * `vector` - The query vector to search for
    pub fn new(vector: &'a [f32]) -> Self {
        Self {
            vector,
            top_k: 10,
            distance: Distance::Cosine,
            approximate: false,
            ef: None,
            filter: None,
        }
    }

    /// Set the number of results to return.
    pub fn top_k(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }

    /// Set the distance metric.
    pub fn distance(mut self, distance: Distance) -> Self {
        self.distance = distance;
        self
    }

    /// Set whether to use approximate search (HNSW).
    ///
    /// When `true`, the search will use the HNSW index for approximate
    /// nearest neighbor search. If no index exists, falls back to exact search.
    ///
    /// Default: `false` (exact search)
    pub fn approximate(mut self, approximate: bool) -> Self {
        self.approximate = approximate;
        self
    }

    /// Set the HNSW ef (search scope) parameter.
    ///
    /// Higher values give better recall at the cost of speed.
    /// If not set, uses the index's default ef_search value.
    ///
    /// Typical values: 2*M to 10*M where M is the HNSW M parameter.
    pub fn ef(mut self, ef: usize) -> Self {
        self.ef = Some(ef);
        self
    }

    /// Set a metadata filter for the search.
    ///
    /// Filters are applied after vector search (post-filtering). Documents
    /// that don't match the filter are excluded from results. If fewer than
    /// `top_k` documents match the filter, fewer results are returned.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let results = collection.search(
    ///     Search::new(&query)
    ///         .top_k(10)
    ///         .filter(Filter::and([
    ///             Filter::eq("category", "books"),
    ///             Filter::gt("year", 2020),
    ///         ]))
    /// )?;
    /// ```
    pub fn filter(mut self, filter: Filter) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Get the query vector.
    pub fn vector(&self) -> &[f32] {
        self.vector
    }

    /// Get the top_k value.
    pub fn top_k_value(&self) -> usize {
        self.top_k
    }

    /// Get the distance metric.
    pub fn distance_metric(&self) -> Distance {
        self.distance
    }

    /// Check if approximate search is enabled.
    pub fn is_approximate(&self) -> bool {
        self.approximate
    }

    /// Get the ef parameter (if set).
    pub fn ef_value(&self) -> Option<usize> {
        self.ef
    }

    /// Get the filter (if set).
    pub fn filter_ref(&self) -> Option<&Filter> {
        self.filter.as_ref()
    }
}

/// A search result match.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// Document ID (external)
    pub id: String,
    /// Similarity score (meaning depends on distance metric)
    pub score: f32,
    /// Optional payload
    pub payload: Option<serde_json::Value>,
}

impl Match {
    /// Create a new match.
    pub fn new(id: String, score: f32, payload: Option<serde_json::Value>) -> Self {
        Self { id, score, payload }
    }
}

/// Internal candidate for top-k selection.
///
/// Implements Ord for use in BinaryHeap. The ordering is always "higher is better"
/// regardless of distance metric - we invert Euclidean distance scores.
#[derive(Debug, Clone)]
struct Candidate {
    /// Internal ID for tie-breaking
    internal_id: u32,
    /// External document ID
    external_id: String,
    /// Score (higher is always better internally)
    score: f32,
    /// Optional payload
    payload: Option<serde_json::Value>,
}

impl Candidate {
    fn into_match(self) -> Match {
        Match {
            id: self.external_id,
            score: self.score,
            payload: self.payload,
        }
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        // Equality based on score and internal_id for deterministic tie-breaking
        self.score == other.score && self.internal_id == other.internal_id
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap, so we want larger scores first
        // For tie-breaking: higher score wins, then lower internal_id wins
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.internal_id.cmp(&self.internal_id)) // Reverse for min internal_id
    }
}

/// Compute the score between two vectors using the specified distance metric.
///
/// For Euclidean distance, returns the negative distance so that "higher is better"
/// holds uniformly across all metrics.
fn compute_score(query: &[f32], vector: &[f32], distance: Distance) -> f32 {
    match distance {
        Distance::DotProduct => dot_product_simd(query, vector),
        Distance::Cosine => cosine_similarity_simd(query, vector),
        Distance::Euclidean => {
            // Negate so higher is better (closer to 0 distance = higher score)
            -euclidean_distance_simd(query, vector)
        }
    }
}

/// Perform exact (brute-force) search over memtable and segments.
///
/// Scans all active documents and returns the top-k matches.
/// Uses SIMD-accelerated distance computation.
/// Applies metadata filter if provided (post-filtering).
pub fn exact_search(
    memtable: &Memtable,
    segments: &[Arc<Segment>],
    search: &Search<'_>,
) -> Result<Vec<Match>> {
    let query = search.vector();
    let top_k = search.top_k_value();
    let distance = search.distance_metric();
    let filter = search.filter_ref();

    // Validate query dimension against memtable
    if !memtable.is_empty() && memtable.dimension() != query.len() {
        return Err(Error::WrongDimension {
            expected: memtable.dimension(),
            got: query.len(),
        });
    }

    // Validate against segments if memtable is empty
    if memtable.is_empty() && !segments.is_empty() {
        let first_dim = segments[0].dimension();
        if first_dim != query.len() {
            return Err(Error::WrongDimension {
                expected: first_dim,
                got: query.len(),
            });
        }
    }

    // Use a bounded min-heap to track top-k candidates
    // Reverse<Candidate> inverts the ordering so we can easily pop the worst
    let mut heap: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(top_k + 1);

    // Scan memtable with payloads and filtering
    scan_memtable(memtable, query, distance, filter, &mut heap, top_k);

    // Scan segments (oldest to newest - memtable has newest)
    for segment in segments {
        scan_segment(segment, query, distance, filter, &mut heap, top_k)?;
    }

    // Extract results from heap (currently in arbitrary order)
    // Sort by score descending (best first), then by internal_id for determinism
    let mut candidates: Vec<Candidate> = heap.into_iter().map(|r| r.0).collect();
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.internal_id.cmp(&b.internal_id))
    });
    
    let mut results: Vec<Match> = candidates.into_iter().map(|c| c.into_match()).collect();

    // For Euclidean, we negated the distance - restore it
    if distance == Distance::Euclidean {
        for m in &mut results {
            m.score = -m.score;
        }
    }

    // Keep only top_k
    results.truncate(top_k);

    Ok(results)
}

/// Scan memtable and add candidates to the heap.
fn scan_memtable(
    memtable: &Memtable,
    query: &[f32],
    distance: Distance,
    filter: Option<&Filter>,
    heap: &mut BinaryHeap<Reverse<Candidate>>,
    top_k: usize,
) {
    // Iterate over documents in the memtable
    for (internal_id, doc) in memtable.documents().iter() {
        // Skip deleted documents
        if memtable.is_deleted(*internal_id) {
            continue;
        }

        // Get the vector for this document
        let vector_offset = doc.vector_offset;
        let vector = &memtable.vector_buffer()[vector_offset..vector_offset + memtable.dimension()];

        // Compute score first
        let score = compute_score(query, vector, distance);

        // Apply filter
        if let Some(filter) = filter {
            if let Some(payload) = &doc.payload {
                if !filter.evaluate(payload) {
                    continue; // Filter doesn't match, skip
                }
            } else {
                // No payload but filter exists - document is excluded
                continue;
            }
        }

        add_candidate(
            heap,
            Candidate {
                internal_id: *internal_id,
                external_id: doc.external_id.clone(),
                score,
                payload: doc.payload.clone(),
            },
            top_k,
        );
    }
}

/// Add a candidate to the heap, maintaining top-k.
///
/// Uses a bounded min-heap approach: we keep at most k candidates,
/// and the heap is ordered so the worst candidate is at the top.
fn add_candidate(heap: &mut BinaryHeap<Reverse<Candidate>>, candidate: Candidate, top_k: usize) {
    if heap.len() < top_k {
        heap.push(Reverse(candidate));
    } else if let Some(Reverse(worst)) = heap.peek() {
        // heap.peek() gives us the worst of our current top-k
        // If new candidate is better, replace
        if candidate > *worst {
            heap.pop();
            heap.push(Reverse(candidate));
        }
    }
}

/// Scan a segment and add candidates to the heap.
fn scan_segment(
    segment: &Segment,
    query: &[f32],
    distance: Distance,
    filter: Option<&Filter>,
    heap: &mut BinaryHeap<Reverse<Candidate>>,
    top_k: usize,
) -> Result<()> {
    // Iterate over all documents in the segment
    for internal_id in 0..segment.doc_count() as u32 {
        if let Some(vector) = segment.get_vector(internal_id) {
            if let Some(external_id) = segment.get_external_id(internal_id) {
                let payload = segment.get_payload(internal_id);

                // Apply filter before computing score (optimization)
                if let Some(filter) = filter {
                    if let Some(ref p) = payload {
                        if !filter.evaluate(p) {
                            continue; // Filter doesn't match, skip
                        }
                    } else {
                        // No payload but filter exists - document is excluded
                        continue;
                    }
                }

                let score = compute_score(query, vector, distance);

                add_candidate(
                    heap,
                    Candidate {
                        internal_id,
                        external_id,
                        score,
                        payload,
                    },
                    top_k,
                );
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_builder() {
        let query = vec![1.0, 2.0, 3.0];
        let search = Search::new(&query)
            .top_k(5)
            .distance(Distance::DotProduct);

        assert_eq!(search.vector(), &[1.0, 2.0, 3.0]);
        assert_eq!(search.top_k_value(), 5);
        assert_eq!(search.distance_metric(), Distance::DotProduct);
    }

    #[test]
    fn test_search_default() {
        let query = vec![1.0, 2.0, 3.0];
        let search = Search::new(&query);

        assert_eq!(search.top_k_value(), 10); // default
        assert_eq!(search.distance_metric(), Distance::Cosine); // default
    }

    #[test]
    fn test_candidate_ordering() {
        let c1 = Candidate {
            internal_id: 1,
            external_id: "a".to_string(),
            score: 0.9,
            payload: None,
        };
        let c2 = Candidate {
            internal_id: 2,
            external_id: "b".to_string(),
            score: 0.8,
            payload: None,
        };
        let c3 = Candidate {
            internal_id: 3,
            external_id: "c".to_string(),
            score: 0.9, // same score as c1
            payload: None,
        };

        // Higher score should be greater
        assert!(c1 > c2);

        // Same score: lower internal_id should be greater (for tie-breaking)
        assert!(c1 > c3); // id 1 beats id 3
    }

    #[test]
    fn test_compute_score() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];

        // Dot product: 0
        assert!((compute_score(&a, &b, Distance::DotProduct) - 0.0).abs() < 1e-6);

        // Cosine: 0 (orthogonal)
        assert!((compute_score(&a, &b, Distance::Cosine) - 0.0).abs() < 1e-6);

        // Euclidean: -sqrt(2) (negated)
        let expected = -(2.0f32.sqrt());
        assert!((compute_score(&a, &b, Distance::Euclidean) - expected).abs() < 1e-6);
    }
}
