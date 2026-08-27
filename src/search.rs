//! Full-text search: inverted index over document text fields.
//!
//! Opt-in per field (like hash/btree indexes): `db.create_text_index("content")`.
//!
//! Memory budget: ~3-8% of indexed text size. Postings store, per lowercased
//! token, the sorted set of docs containing it — no positions (a position
//! map would cost ~70% of text size, over budget). AND = list intersection,
//! OR = union, both on sorted doc-id lists. Phrases and case-sensitive terms
//! are verified by scanning the raw text of the candidate set only — after
//! set operations that set is small, so verification is microseconds.

use crate::error::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;

/// Internal doc id used in postings (dense, u32).
type DocId = u32;

/// One searchable condition.
#[derive(Debug, Clone)]
pub enum TextQuery {
    /// Single word, matched as a whole token.
    Term(String),
    /// Contiguous phrase ("The hare was tired" — those words in order).
    Phrase(String),
    /// Token prefix ("yester" matches "yesterday").
    Prefix(String),
    /// Negation: docs NOT matching the inner query.
    Exclude(Box<TextQuery>),
}

/// How multiple queries combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextMode {
    And,
    Or,
}

/// A full-text search request.
#[derive(Debug, Clone)]
pub struct TextSearch {
    pub mode: TextMode,
    pub case_sensitive: bool,
    pub queries: Vec<TextQuery>,
}

impl TextSearch {
    pub fn and(queries: Vec<TextQuery>) -> Self {
        Self { mode: TextMode::And, case_sensitive: false, queries }
    }

    pub fn or(queries: Vec<TextQuery>) -> Self {
        Self { mode: TextMode::Or, case_sensitive: false, queries }
    }

    /// Validate: at least one positive query, non-empty strings, no nested exclude.
    pub fn validate(&self) -> Result<()> {
        let has_positive = self.queries.iter().any(|q| !matches!(q, TextQuery::Exclude(_)));
        if !has_positive {
            return Err(Error::invalid_arg("text search requires at least one non-exclude query"));
        }
        for q in &self.queries {
            match q {
                TextQuery::Term(s) | TextQuery::Phrase(s) | TextQuery::Prefix(s) => {
                    if s.trim().is_empty() {
                        return Err(Error::invalid_arg("empty text query"));
                    }
                }
                TextQuery::Exclude(inner) => match inner.as_ref() {
                    TextQuery::Term(s) | TextQuery::Phrase(s) | TextQuery::Prefix(s) => {
                        if s.trim().is_empty() {
                            return Err(Error::invalid_arg("empty text query"));
                        }
                    }
                    TextQuery::Exclude(_) => {
                        return Err(Error::invalid_arg("nested exclude is not supported"))
                    }
                },
            }
        }
        Ok(())
    }
}

/// Lowercase tokenization. Unicode alphanumeric runs are tokens.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                current.push(lc);
            }
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Extract the searchable text of `field` from a document: string fields
/// directly; arrays of strings joined with a space; other types none.
pub fn extract_text(doc: &Value, field: &str) -> Option<String> {
    match doc.get(field) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::with_capacity(arr.len());
            for v in arr {
                if let Value::String(s) = v {
                    parts.push(s.clone());
                }
            }
            if parts.is_empty() { None } else { Some(parts.join(" ")) }
        }
        _ => None,
    }
}

/// Intersection of two sorted doc-id lists.
fn intersect(a: &[DocId], b: &[DocId]) -> Vec<DocId> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// Union of two sorted doc-id lists.
fn union(a: &[DocId], b: &[DocId]) -> Vec<DocId> {
    let mut out = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Difference a \ b of two sorted doc-id lists.
fn difference(a: &[DocId], b: &[DocId]) -> Vec<DocId> {
    let mut out = Vec::with_capacity(a.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out
}

/// Inverted index for one field: token → sorted doc-id list.
#[derive(Default)]
pub struct TextIndex {
    /// field name this index covers
    pub field: String,
    /// lowercased token → sorted doc ids containing it
    postings: HashMap<String, Vec<DocId>>,
    /// dense internal id → document _id ("" = removed tombstone)
    ids: Vec<String>,
    /// document _id → internal id
    lookup: HashMap<String, DocId>,
}

impl TextIndex {
    pub fn new(field: &str) -> Self {
        Self { field: field.to_string(), ..Default::default() }
    }

    /// Index a document (add or replace). Returns false if the doc has no
    /// text for this field.
    pub fn index_doc(&mut self, id: &str, doc: &Value) -> bool {
        self.remove_doc(id);

        let text = match extract_text(doc, &self.field) {
            Some(t) => t,
            None => return false,
        };

        let doc_id = match self.lookup.get(id) {
            Some(d) => *d,
            None => {
                let d = self.ids.len() as DocId;
                self.ids.push(id.to_string());
                self.lookup.insert(id.to_string(), d);
                d
            }
        };

        // Collect this doc's unique tokens, add the doc to each posting.
        let mut tokens = tokenize(&text);
        tokens.sort_unstable();
        tokens.dedup();
        for token in tokens {
            self.postings.entry(token).or_default().push(doc_id);
        }
        true
    }

    /// Remove a document from the index entirely.
    pub fn remove_doc(&mut self, id: &str) {
        if let Some(doc_id) = self.lookup.remove(id) {
            self.ids[doc_id as usize] = String::new();
            self.postings.retain(|_, list| {
                if let Ok(pos) = list.binary_search(&doc_id) {
                    list.remove(pos);
                }
                !list.is_empty()
            });
        }
    }

    /// Number of indexed documents.
    pub fn doc_count(&self) -> usize {
        self.lookup.len()
    }

    fn docs_for_token(&self, token: &str) -> &[DocId] {
        self.postings.get(token).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Candidate docs for one positive query, index-only (lowercase).
    fn candidates_for(&self, query: &TextQuery) -> Vec<DocId> {
        match query {
            TextQuery::Term(s) => self.docs_for_token(&s.to_lowercase()).to_vec(),
            TextQuery::Prefix(p) => {
                let p = p.to_lowercase();
                let mut docs = Vec::new();
                for (token, list) in &self.postings {
                    if token.starts_with(&p) {
                        docs = if docs.is_empty() {
                            list.clone()
                        } else {
                            union(&docs, list)
                        };
                    }
                }
                docs
            }
            // Phrase candidates: docs containing ALL tokens (contiguity
            // verified later against raw text — the candidate set is small).
            TextQuery::Phrase(s) => {
                let tokens = tokenize(s);
                if tokens.is_empty() {
                    return Vec::new();
                }
                // Start from the rarest token for smallest intermediate sets.
                let mut lists: Vec<&[DocId]> = tokens
                    .iter()
                    .map(|t| self.docs_for_token(t))
                    .collect();
                lists.sort_by_key(|l| l.len());
                let mut result = lists[0].to_vec();
                for l in &lists[1..] {
                    result = intersect(&result, l);
                    if result.is_empty() {
                        break;
                    }
                }
                result
            }
            TextQuery::Exclude(inner) => self.candidates_for(inner),
        }
    }

    /// Resolve _ids matching the full search. `get_text` fetches the raw
    /// field text of a document by _id (phrase / case-sensitive verification
    /// of the shortlisted candidates).
    pub fn search<F>(&self, search: &TextSearch, get_text: F) -> Vec<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut positive: Vec<&TextQuery> = Vec::new();
        let mut excludes: Vec<&TextQuery> = Vec::new();
        for q in &search.queries {
            match q {
                TextQuery::Exclude(inner) => excludes.push(inner),
                _ => positive.push(q),
            }
        }
        if positive.is_empty() {
            return Vec::new();
        }

        // Order positive queries by selectivity for AND (rarest first).
        let mut result = match search.mode {
            TextMode::And => {
                let mut lists: Vec<Vec<DocId>> =
                    positive.iter().map(|q| self.candidates_for(q)).collect();
                lists.sort_by_key(|l| l.len());
                let mut result = lists.remove(0);
                for l in &lists {
                    result = intersect(&result, l);
                    if result.is_empty() {
                        break;
                    }
                }
                result
            }
            TextMode::Or => {
                let mut result = self.candidates_for(positive[0]);
                for q in &positive[1..] {
                    result = union(&result, &self.candidates_for(q));
                }
                result
            }
        };
        for q in &excludes {
            let ex = self.candidates_for(q);
            result = difference(&result, &ex);
        }

        // Verification pass (only shortlisted candidates): phrase contiguity
        // and/or case-sensitive matching against raw text.
        let needs_verify = search.case_sensitive
            || positive.iter().any(|q| matches!(q, TextQuery::Phrase(_)))
            || excludes.iter().any(|q| matches!(q, TextQuery::Phrase(_)));

        let mut out = Vec::with_capacity(result.len());
        for doc_id in result {
            let id = &self.ids[doc_id as usize];
            if id.is_empty() {
                continue;
            }
            if needs_verify {
                let text = match get_text(id) {
                    Some(t) => t,
                    None => continue,
                };
                if !verify(&text, search) {
                    continue;
                }
            }
            out.push(id.clone());
        }
        out
    }
}

/// Verify phrase contiguity and/or case-sensitive matching on raw text.
/// Called only for index-shortlisted candidates.
fn verify(text: &str, search: &TextSearch) -> bool {
    let eval_one = |q: &TextQuery| -> bool {
        let target: &TextQuery = match q {
            TextQuery::Exclude(inner) => inner,
            _ => q,
        };
        let s = match target {
            TextQuery::Term(s) | TextQuery::Phrase(s) | TextQuery::Prefix(s) => s,
            TextQuery::Exclude(_) => unreachable!("nested exclude rejected at validate"),
        };
        match target {
            // Terms were already index-matched case-insensitively; for
            // case-sensitive mode compare against the original text.
            TextQuery::Term(_) => {
                if search.case_sensitive {
                    contains_token(text, s)
                } else {
                    true // index already proved presence
                }
            }
            TextQuery::Phrase(_) => {
                if search.case_sensitive {
                    text.contains(s.as_str())
                } else {
                    text.to_lowercase().contains(&s.to_lowercase())
                }
            }
            TextQuery::Prefix(_) => {
                if search.case_sensitive {
                    contains_token_prefix(text, s)
                } else {
                    true // index already proved presence
                }
            }
            TextQuery::Exclude(_) => false,
        }
    };

    match search.mode {
        TextMode::And => search.queries.iter().all(|q| match q {
            TextQuery::Exclude(inner) => !eval_one(q) && !eval_one(inner),
            _ => eval_one(q),
        }) && {
            // AND semantics: every positive matches, every exclude misses.
            true
        },
        TextMode::Or => {
            let mut any = false;
            for q in &search.queries {
                match q {
                    TextQuery::Exclude(inner) => {
                        if eval_one(inner) {
                            return false;
                        }
                    }
                    _ => {
                        if eval_one(q) {
                            any = true;
                        }
                    }
                }
            }
            any
        }
    }
}

/// Whole-token containment with original casing: needle appears bounded by
/// non-alphanumerics in the given-cased haystack.
fn contains_token(hay: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = hay[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !hay[..abs].chars().next_back().map(|c| c.is_alphanumeric()).unwrap_or(false);
        let end = abs + needle.len();
        let after_ok = end >= hay.len()
            || !hay[end..].chars().next().map(|c| c.is_alphanumeric()).unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len().max(1);
    }
    false
}

/// Any token starting with needle (original casing).
fn contains_token_prefix(hay: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = hay[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !hay[..abs].chars().next_back().map(|c| c.is_alphanumeric()).unwrap_or(false);
        if before_ok {
            return true;
        }
        start = abs + needle.len().max(1);
    }
    false
}
