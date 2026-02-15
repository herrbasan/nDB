use std::collections::HashMap;

/// Bidirectional mapping between string IDs and dense internal u32 IDs.
/// 
/// This enables efficient HNSW graph storage (u32 IDs) while maintaining
/// a user-friendly string API.
#[derive(Debug, Clone, Default)]
pub struct IdMapping {
    /// Maps external string ID to internal u32.
    str_to_int: HashMap<String, u32>,
    /// Maps internal u32 back to string ID.
    int_to_str: HashMap<u32, String>,
    /// Next internal ID to assign.
    next_id: u32,
}

impl IdMapping {
    /// Create a new empty ID mapping.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            str_to_int: HashMap::with_capacity(capacity),
            int_to_str: HashMap::with_capacity(capacity),
            next_id: 0,
        }
    }

    /// Insert a string ID, returning its internal u32.
    /// If the ID already exists, returns the existing internal ID.
    pub fn insert(&mut self, id: String) -> u32 {
        if let Some(&internal) = self.str_to_int.get(&id) {
            return internal;
        }

        let internal = self.next_id;
        self.str_to_int.insert(id.clone(), internal);
        self.int_to_str.insert(internal, id);
        self.next_id = self.next_id.wrapping_add(1);
        internal
    }

    /// Get internal ID for a string ID, if it exists.
    pub fn get_internal(&self, id: &str) -> Option<u32> {
        self.str_to_int.get(id).copied()
    }

    /// Get string ID for an internal ID, if it exists.
    pub fn get_external(&self, internal: u32) -> Option<&str> {
        self.int_to_str.get(&internal).map(|s| s.as_str())
    }

    /// Check if a string ID exists.
    pub fn contains_external(&self, id: &str) -> bool {
        self.str_to_int.contains_key(id)
    }

    /// Check if an internal ID exists.
    pub fn contains_internal(&self, internal: u32) -> bool {
        self.int_to_str.contains_key(&internal)
    }

    /// Remove a mapping by string ID.
    /// Returns the internal ID if it existed.
    pub fn remove(&mut self, id: &str) -> Option<u32> {
        let internal = self.str_to_int.remove(id)?;
        self.int_to_str.remove(&internal);
        Some(internal)
    }

    /// Number of mappings.
    pub fn len(&self) -> usize {
        self.str_to_int.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.str_to_int.is_empty()
    }

    /// Iterate over external -> internal mappings.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &u32)> {
        self.str_to_int.iter()
    }

    /// Get the next ID that would be assigned.
    pub fn next_id(&self) -> u32 {
        self.next_id
    }

    /// Convert to a vector of (internal_id, external_id) pairs.
    /// Suitable for serialization to segment file.
    pub fn to_vec(&self) -> Vec<(u32, String)> {
        self.int_to_str
            .iter()
            .map(|(&internal, external)| (internal, external.clone()))
            .collect()
    }

    /// Build from a vector of (internal_id, external_id) pairs.
    /// Returns error if the data is inconsistent (duplicate IDs).
    pub fn from_vec(pairs: Vec<(u32, String)>) -> crate::Result<Self> {
        let mut mapping = Self::with_capacity(pairs.len());
        
        for (internal, external) in pairs {
            if mapping.int_to_str.contains_key(&internal) {
                return Err(crate::Error::invalid_arg(
                    "id_mapping",
                    format!("duplicate internal ID: {}", internal),
                ));
            }
            if mapping.str_to_int.contains_key(&external) {
                return Err(crate::Error::invalid_arg(
                    "id_mapping",
                    format!("duplicate external ID: {}", external),
                ));
            }
            
            mapping.int_to_str.insert(internal, external.clone());
            mapping.str_to_int.insert(external, internal);
            mapping.next_id = mapping.next_id.max(internal.wrapping_add(1));
        }
        
        Ok(mapping)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_mapping() {
        let mut mapping = IdMapping::new();
        
        let id1 = mapping.insert("doc1".to_string());
        let id2 = mapping.insert("doc2".to_string());
        
        assert_ne!(id1, id2);
        assert_eq!(mapping.get_external(id1), Some("doc1"));
        assert_eq!(mapping.get_external(id2), Some("doc2"));
        assert_eq!(mapping.get_internal("doc1"), Some(id1));
        assert_eq!(mapping.get_internal("doc2"), Some(id2));
    }

    #[test]
    fn test_duplicate_insert() {
        let mut mapping = IdMapping::new();
        
        let id1 = mapping.insert("doc1".to_string());
        let id1_again = mapping.insert("doc1".to_string());
        
        assert_eq!(id1, id1_again);
        assert_eq!(mapping.len(), 1);
    }

    #[test]
    fn test_remove() {
        let mut mapping = IdMapping::new();
        
        let id = mapping.insert("doc1".to_string());
        assert_eq!(mapping.remove("doc1"), Some(id));
        assert_eq!(mapping.get_internal("doc1"), None);
        assert_eq!(mapping.get_external(id), None);
    }

    #[test]
    fn test_roundtrip_vec() {
        let mut mapping = IdMapping::new();
        mapping.insert("doc1".to_string());
        mapping.insert("doc2".to_string());
        mapping.insert("doc3".to_string());
        
        let vec = mapping.to_vec();
        let restored = IdMapping::from_vec(vec).unwrap();
        
        assert_eq!(mapping.len(), restored.len());
        assert_eq!(mapping.get_internal("doc1"), restored.get_internal("doc1"));
        assert_eq!(mapping.get_internal("doc2"), restored.get_internal("doc2"));
        assert_eq!(mapping.get_internal("doc3"), restored.get_internal("doc3"));
    }
}
