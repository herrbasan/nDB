//! Mongo-like Filter DSL for metadata filtering.
//!
//! This module provides a type-safe filter DSL for querying documents based on
//! their metadata payload. Filters support equality, comparison, logical operators,
//! and nested field access via dot notation.
//!
//! # Example
//!
//! ```ignore
//! use ndb::{Filter, Search};
//!
//! // Simple equality filter
//! let filter = Filter::eq("category", "books");
//!
//! // Comparison filter
//! let filter = Filter::gt("year", 2020);
//!
//! // Combined filters
//! let filter = Filter::and([
//!     Filter::eq("status", "active"),
//!     Filter::gt("score", 4.5),
//! ]);
//!
//! // Nested field access
//! let filter = Filter::eq("user.name", "alice");
//!
//! // Use in search
//! let results = collection.search(
//!     Search::new(&query)
//!         .top_k(10)
//!         .filter(filter)
//! )?;
//! ```

use serde_json::Value;

/// A filter predicate for matching documents by their metadata payload.
///
/// Filters can be combined using logical operators (`And`, `Or`) and support
/// nested field access via dot notation (e.g., `"user.name"`).
///
/// # Type Coercion
///
/// Numeric comparisons automatically coerce between integers and floats.
/// For example, `Filter::gt("count", 5)` will match documents with
/// `{ "count": 5.5 }`.
///
/// # Missing Fields
///
/// If a filter references a field that doesn't exist in the document's
/// payload, the filter evaluates to `false` and the document is excluded.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Equality: field == value
    Eq { field: String, value: Value },

    /// Greater than: field > value
    Gt { field: String, value: Value },

    /// Greater than or equal: field >= value
    Gte { field: String, value: Value },

    /// Less than: field < value
    Lt { field: String, value: Value },

    /// Less than or equal: field <= value
    Lte { field: String, value: Value },

    /// In array: field IN values
    In { field: String, values: Vec<Value> },

    /// Logical AND: all filters must match
    And(Vec<Filter>),

    /// Logical OR: any filter must match
    Or(Vec<Filter>),
}

impl Filter {
    /// Create an equality filter.
    ///
    /// # Example
    /// ```
    /// use ndb::Filter;
    /// use serde_json::json;
    ///
    /// let filter = Filter::eq("category", "books");
    /// ```
    pub fn eq(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Self::Eq {
            field: field.into(),
            value: value.into(),
        }
    }

    /// Create a greater-than filter.
    ///
    /// # Example
    /// ```
    /// use ndb::Filter;
    ///
    /// let filter = Filter::gt("year", 2020);
    /// ```
    pub fn gt(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Self::Gt {
            field: field.into(),
            value: value.into(),
        }
    }

    /// Create a greater-than-or-equal filter.
    pub fn gte(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Self::Gte {
            field: field.into(),
            value: value.into(),
        }
    }

    /// Create a less-than filter.
    pub fn lt(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Self::Lt {
            field: field.into(),
            value: value.into(),
        }
    }

    /// Create a less-than-or-equal filter.
    pub fn lte(field: impl Into<String>, value: impl Into<Value>) -> Self {
        Self::Lte {
            field: field.into(),
            value: value.into(),
        }
    }

    /// Create an "in" filter.
    ///
    /// Matches if the field value is present in the provided array.
    ///
    /// # Example
    /// ```
    /// use ndb::Filter;
    /// use serde_json::json;
    ///
    /// let filter = Filter::in_("status", ["active", "pending"]);
    /// ```
    pub fn in_(field: impl Into<String>, values: impl IntoIterator<Item = impl Into<Value>>) -> Self {
        Self::In {
            field: field.into(),
            values: values.into_iter().map(Into::into).collect(),
        }
    }

    /// Create a logical AND filter.
    ///
    /// All provided filters must match for the document to be included.
    ///
    /// # Example
    /// ```
    /// use ndb::Filter;
    ///
    /// let filter = Filter::and([
    ///     Filter::eq("category", "books"),
    ///     Filter::gt("year", 2020),
    /// ]);
    /// ```
    pub fn and(filters: impl IntoIterator<Item = Filter>) -> Self {
        Self::And(filters.into_iter().collect())
    }

    /// Create a logical OR filter.
    ///
    /// At least one provided filter must match for the document to be included.
    ///
    /// # Example
    /// ```
    /// use ndb::Filter;
    ///
    /// let filter = Filter::or([
    ///     Filter::eq("category", "books"),
    ///     Filter::eq("category", "articles"),
    /// ]);
    /// ```
    pub fn or(filters: impl IntoIterator<Item = Filter>) -> Self {
        Self::Or(filters.into_iter().collect())
    }

    /// Evaluate this filter against a JSON payload.
    ///
    /// Returns `true` if the payload matches the filter criteria.
    ///
    /// # Arguments
    ///
    /// * `payload` - The document's metadata payload as a JSON value
    ///
    /// # Returns
    ///
    /// `true` if the filter matches, `false` otherwise.
    pub fn evaluate(&self, payload: &Value) -> bool {
        match self {
            Self::Eq { field, value } => {
                match get_field(payload, field) {
                    Some(field_value) => values_equal(field_value, value),
                    None => false, // Missing field = no match
                }
            }
            Self::Gt { field, value } => {
                match get_field(payload, field) {
                    Some(field_value) => compare_values(field_value, value) == Some(Ordering::Greater),
                    None => false,
                }
            }
            Self::Gte { field, value } => {
                match get_field(payload, field) {
                    Some(field_value) => {
                        let cmp = compare_values(field_value, value);
                        cmp == Some(Ordering::Greater) || cmp == Some(Ordering::Equal)
                    }
                    None => false,
                }
            }
            Self::Lt { field, value } => {
                match get_field(payload, field) {
                    Some(field_value) => compare_values(field_value, value) == Some(Ordering::Less),
                    None => false,
                }
            }
            Self::Lte { field, value } => {
                match get_field(payload, field) {
                    Some(field_value) => {
                        let cmp = compare_values(field_value, value);
                        cmp == Some(Ordering::Less) || cmp == Some(Ordering::Equal)
                    }
                    None => false,
                }
            }
            Self::In { field, values } => {
                match get_field(payload, field) {
                    Some(field_value) => values.iter().any(|v| values_equal(field_value, v)),
                    None => false,
                }
            }
            Self::And(filters) => filters.iter().all(|f| f.evaluate(payload)),
            Self::Or(filters) => filters.iter().any(|f| f.evaluate(payload)),
        }
    }
}

use std::cmp::Ordering;

/// Get a field from a JSON value using dot notation.
///
/// # Examples
///
/// ```
/// use serde_json::json;
///
/// let payload = json!({"user": {"name": "alice", "age": 30}});
///
/// // Direct field access
/// assert_eq!(
///     ndb::filter::get_field(&payload, "user"),
///     Some(&json!({"name": "alice", "age": 30}))
/// );
///
/// // Nested field access
/// assert_eq!(
///     ndb::filter::get_field(&payload, "user.name"),
///     Some(&json!("alice"))
/// );
/// ```
pub fn get_field<'a>(payload: &'a Value, field: &str) -> Option<&'a Value> {
    let mut current = payload;
    
    for part in field.split('.') {
        match current {
            Value::Object(map) => {
                current = map.get(part)?;
            }
            _ => return None,
        }
    }
    
    Some(current)
}

/// Compare two JSON values, with numeric coercion.
///
/// Returns `Some(Ordering)` if the values are comparable, `None` otherwise.
/// Numeric types are coerced: integers can be compared with floats.
fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        // Both numbers - coerce to f64 for comparison
        (Value::Number(a_num), Value::Number(b_num)) => {
            let a_f64 = a_num.as_f64()?;
            let b_f64 = b_num.as_f64()?;
            a_f64.partial_cmp(&b_f64)
        }
        // Strings
        (Value::String(a_str), Value::String(b_str)) => Some(a_str.cmp(b_str)),
        // Booleans
        (Value::Bool(a_bool), Value::Bool(b_bool)) => Some(a_bool.cmp(b_bool)),
        // Mixed types - not comparable
        _ => None,
    }
}

/// Check if two JSON values are equal, with numeric coercion.
///
/// Numeric values are compared by their numeric value, so `5` equals `5.0`.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        // Both numbers - coerce to f64
        (Value::Number(a_num), Value::Number(b_num)) => {
            match (a_num.as_f64(), b_num.as_f64()) {
                (Some(a_f64), Some(b_f64)) => (a_f64 - b_f64).abs() < f64::EPSILON,
                _ => false,
            }
        }
        // Same type - use standard equality
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Equality tests
    #[test]
    fn test_filter_eq_string() {
        let filter = Filter::eq("name", "alice");
        let payload = json!({"name": "alice"});
        assert!(filter.evaluate(&payload));

        let payload2 = json!({"name": "bob"});
        assert!(!filter.evaluate(&payload2));
    }

    #[test]
    fn test_filter_eq_number() {
        let filter = Filter::eq("count", 5);
        let payload = json!({"count": 5});
        assert!(filter.evaluate(&payload));

        // Integer vs float coercion
        let payload2 = json!({"count": 5.0});
        assert!(filter.evaluate(&payload2));

        let payload3 = json!({"count": 10});
        assert!(!filter.evaluate(&payload3));
    }

    #[test]
    fn test_filter_eq_bool() {
        let filter = Filter::eq("active", true);
        let payload = json!({"active": true});
        assert!(filter.evaluate(&payload));

        let payload2 = json!({"active": false});
        assert!(!filter.evaluate(&payload2));
    }

    #[test]
    fn test_filter_eq_missing_field() {
        let filter = Filter::eq("missing", "value");
        let payload = json!({"other": "field"});
        assert!(!filter.evaluate(&payload));
    }

    // Comparison tests
    #[test]
    fn test_filter_gt() {
        let filter = Filter::gt("score", 4.5);
        let payload = json!({"score": 5.0});
        assert!(filter.evaluate(&payload));

        let payload2 = json!({"score": 4.5});
        assert!(!filter.evaluate(&payload2));

        let payload3 = json!({"score": 4.0});
        assert!(!filter.evaluate(&payload3));
    }

    #[test]
    fn test_filter_gte() {
        let filter = Filter::gte("score", 4.5);
        assert!(filter.evaluate(&json!({"score": 5.0})));
        assert!(filter.evaluate(&json!({"score": 4.5})));
        assert!(!filter.evaluate(&json!({"score": 4.0})));
    }

    #[test]
    fn test_filter_lt() {
        let filter = Filter::lt("score", 4.5);
        assert!(!filter.evaluate(&json!({"score": 5.0})));
        assert!(!filter.evaluate(&json!({"score": 4.5})));
        assert!(filter.evaluate(&json!({"score": 4.0})));
    }

    #[test]
    fn test_filter_lte() {
        let filter = Filter::lte("score", 4.5);
        assert!(!filter.evaluate(&json!({"score": 5.0})));
        assert!(filter.evaluate(&json!({"score": 4.5})));
        assert!(filter.evaluate(&json!({"score": 4.0})));
    }

    #[test]
    fn test_filter_numeric_coercion() {
        // Integer filter vs float value
        let filter = Filter::gt("count", 5);
        assert!(filter.evaluate(&json!({"count": 6})));
        assert!(filter.evaluate(&json!({"count": 5.5}))); // Coercion
        assert!(!filter.evaluate(&json!({"count": 5})));
        assert!(!filter.evaluate(&json!({"count": 4.9})));
    }

    // In operator tests
    #[test]
    fn test_filter_in() {
        let filter = Filter::in_("status", ["active", "pending"]);
        assert!(filter.evaluate(&json!({"status": "active"})));
        assert!(filter.evaluate(&json!({"status": "pending"})));
        assert!(!filter.evaluate(&json!({"status": "inactive"})));
    }

    #[test]
    fn test_filter_in_numeric() {
        let filter = Filter::in_("id", [1, 2, 3]);
        assert!(filter.evaluate(&json!({"id": 2})));
        assert!(!filter.evaluate(&json!({"id": 4})));
        
        // Type coercion: 2 == 2.0
        assert!(filter.evaluate(&json!({"id": 2.0})));
    }

    // Logical operator tests
    #[test]
    fn test_filter_and() {
        let filter = Filter::and([
            Filter::eq("category", "books"),
            Filter::gt("year", 2020),
        ]);

        assert!(filter.evaluate(&json!({"category": "books", "year": 2021})));
        assert!(!filter.evaluate(&json!({"category": "books", "year": 2019}))); // year fails
        assert!(!filter.evaluate(&json!({"category": "movies", "year": 2021}))); // category fails
    }

    #[test]
    fn test_filter_or() {
        let filter = Filter::or([
            Filter::eq("category", "books"),
            Filter::eq("category", "articles"),
        ]);

        assert!(filter.evaluate(&json!({"category": "books"})));
        assert!(filter.evaluate(&json!({"category": "articles"})));
        assert!(!filter.evaluate(&json!({"category": "movies"})));
    }

    #[test]
    fn test_filter_nested_and_or() {
        let filter = Filter::and([
            Filter::or([
                Filter::eq("category", "books"),
                Filter::eq("category", "articles"),
            ]),
            Filter::gt("year", 2020),
        ]);

        assert!(filter.evaluate(&json!({"category": "books", "year": 2021})));
        assert!(filter.evaluate(&json!({"category": "articles", "year": 2022})));
        assert!(!filter.evaluate(&json!({"category": "movies", "year": 2021}))); // category fails
        assert!(!filter.evaluate(&json!({"category": "books", "year": 2019}))); // year fails
    }

    // Nested field tests
    #[test]
    fn test_filter_nested_field() {
        let filter = Filter::eq("user.name", "alice");
        let payload = json!({"user": {"name": "alice", "age": 30}});
        assert!(filter.evaluate(&payload));

        let payload2 = json!({"user": {"name": "bob", "age": 25}});
        assert!(!filter.evaluate(&payload2));
    }

    #[test]
    fn test_filter_deeply_nested() {
        let filter = Filter::eq("a.b.c", "value");
        let payload = json!({"a": {"b": {"c": "value"}}});
        assert!(filter.evaluate(&payload));

        let payload2 = json!({"a": {"b": {"c": "other"}}});
        assert!(!filter.evaluate(&payload2));
    }

    #[test]
    fn test_filter_nested_missing() {
        let filter = Filter::eq("user.name", "alice");
        // Missing "user" field entirely
        assert!(!filter.evaluate(&json!({"other": "value"})));
        // "user" exists but is not an object
        assert!(!filter.evaluate(&json!({"user": "not an object"})));
        // "user" exists but "name" is missing
        assert!(!filter.evaluate(&json!({"user": {"age": 30}})));
    }

    #[test]
    fn test_filter_nested_comparison() {
        let filter = Filter::gt("user.age", 25);
        let payload = json!({"user": {"name": "alice", "age": 30}});
        assert!(filter.evaluate(&payload));

        let payload2 = json!({"user": {"name": "bob", "age": 20}});
        assert!(!filter.evaluate(&payload2));
    }

    // Edge cases
    #[test]
    fn test_filter_empty_and() {
        // Empty AND should return true (vacuous truth)
        let filter = Filter::And(vec![]);
        assert!(filter.evaluate(&json!({})));
    }

    #[test]
    fn test_filter_empty_or() {
        // Empty OR should return false
        let filter = Filter::Or(vec![]);
        assert!(!filter.evaluate(&json!({})));
    }

    #[test]
    fn test_filter_mixed_types_not_comparable() {
        // String vs number - not comparable
        let filter = Filter::gt("field", 5);
        let payload = json!({"field": "10"});
        assert!(!filter.evaluate(&payload));
    }

    #[test]
    fn test_filter_null_values() {
        let filter = Filter::eq("field", serde_json::Value::Null);
        assert!(filter.evaluate(&json!({"field": null})));
        
        // Null is not equal to missing
        assert!(!filter.evaluate(&json!({"other": "value"})));
    }

    // get_field tests
    #[test]
    fn test_get_field_simple() {
        let payload = json!({"name": "alice"});
        assert_eq!(get_field(&payload, "name"), Some(&json!("alice")));
        assert_eq!(get_field(&payload, "missing"), None);
    }

    #[test]
    fn test_get_field_nested() {
        let payload = json!({
            "user": {
                "profile": {
                    "name": "alice"
                }
            }
        });
        assert_eq!(get_field(&payload, "user.profile.name"), Some(&json!("alice")));
        assert_eq!(get_field(&payload, "user.profile"), Some(&json!({"name": "alice"})));
        assert_eq!(get_field(&payload, "user"), Some(&json!({"profile": {"name": "alice"}})));
    }

    #[test]
    fn test_get_field_array_index() {
        // Arrays are not supported in field paths currently
        let payload = json!({"items": ["a", "b", "c"]});
        assert_eq!(get_field(&payload, "items.0"), None); // Not supported
    }

    // Property-based tests for Filter evaluation
    use proptest::prelude::*;

    proptest! {

        // Property: Eq filter is reflexive - a value equals itself
        #[test]
        fn prop_eq_reflexive(value in 0i64..10000) {
            let filter = Filter::eq("field", value);
            let payload = json!({"field": value});
            prop_assert!(filter.evaluate(&payload));
        }

        // Property: Eq filter is deterministic - same input always produces same output
        #[test]
        fn prop_eq_deterministic(value in 0i64..10000, other_value in 0i64..10000) {
            let filter = Filter::eq("field", value);
            let payload = json!({"field": other_value});
            let first_result = filter.evaluate(&payload);
            let second_result = filter.evaluate(&payload);
            prop_assert_eq!(first_result, second_result);
        }

        // Property: Gt and Lt are mutually exclusive for the same value
        #[test]
        fn prop_gt_lt_exclusive(threshold in 0.0f64..100.0, value in 0.0f64..100.0) {
            let gt_filter = Filter::gt("field", threshold);
            let lt_filter = Filter::lt("field", threshold);
            let payload = json!({"field": value});

            let is_gt = gt_filter.evaluate(&payload);
            let is_lt = lt_filter.evaluate(&payload);

            // A value cannot be both greater than and less than the same threshold
            prop_assert!(!(is_gt && is_lt));

            // If value != threshold, exactly one should be true (or both false if equal)
            if (value - threshold).abs() > f64::EPSILON {
                // For any value not equal to threshold, exactly one of these is true:
                // - value > threshold (gt is true)
                // - value < threshold (lt is true)
                prop_assert!(is_gt || is_lt || value == threshold);
            }
        }

        // Property: Gte is equivalent to (Gt OR Eq)
        #[test]
        fn prop_gte_equivalent(value in 0.0f64..100.0, threshold in 0.0f64..100.0) {
            let gte_filter = Filter::gte("field", threshold);
            let gt_filter = Filter::gt("field", threshold);
            let eq_filter = Filter::eq("field", threshold);

            let payload = json!({"field": value});

            let gte_result = gte_filter.evaluate(&payload);
            let gt_or_eq_result = gt_filter.evaluate(&payload) || eq_filter.evaluate(&payload);

            prop_assert_eq!(gte_result, gt_or_eq_result);
        }

        // Property: Lte is equivalent to (Lt OR Eq)
        #[test]
        fn prop_lte_equivalent(value in 0.0f64..100.0, threshold in 0.0f64..100.0) {
            let lte_filter = Filter::lte("field", threshold);
            let lt_filter = Filter::lt("field", threshold);
            let eq_filter = Filter::eq("field", threshold);

            let payload = json!({"field": value});

            let lte_result = lte_filter.evaluate(&payload);
            let lt_or_eq_result = lt_filter.evaluate(&payload) || eq_filter.evaluate(&payload);

            prop_assert_eq!(lte_result, lt_or_eq_result);
        }

        // Property: And with empty vector returns true (vacuous truth)
        #[test]
        fn prop_and_empty_always_true(seed in 0u64..1000) {
            let filter = Filter::And(vec![]);
            // Generate a random payload
            let payload = json!({
                "field1": seed,
                "field2": format!("value_{}", seed),
                "field3": seed % 2 == 0
            });
            prop_assert!(filter.evaluate(&payload));
        }

        // Property: Or with empty vector returns false
        #[test]
        fn prop_or_empty_always_false(seed in 0u64..1000) {
            let filter = Filter::Or(vec![]);
            let payload = json!({
                "field1": seed,
                "field2": format!("value_{}", seed),
                "field3": seed % 2 == 0
            });
            prop_assert!(!filter.evaluate(&payload));
        }

        // Property: Numeric coercion - integers equal their float equivalents
        #[test]
        fn prop_numeric_coercion_eq(int_val in 0i64..10000) {
            let float_val = int_val as f64;

            let int_filter = Filter::eq("field", int_val);
            let float_filter = Filter::eq("field", float_val);

            // A payload with integer value should match float filter
            let int_payload = json!({"field": int_val});
            let float_payload = json!({"field": float_val});

            prop_assert!(int_filter.evaluate(&int_payload));
            prop_assert!(int_filter.evaluate(&float_payload)); // int filter matches float payload
            prop_assert!(float_filter.evaluate(&int_payload)); // float filter matches int payload
            prop_assert!(float_filter.evaluate(&float_payload));
        }
    }
}
