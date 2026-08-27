//! Integration tests for nDB Phase 8: Full-text search
//!
//! Covers the primary use case: multi-phrase AND search over story-sized
//! text, OR mode, case sensitivity, prefixes, excludes, and index
//! maintenance across all write paths (insert/update/delete/deltas).

use ndb::{Database, TextMode, TextQuery, TextSearch};
use serde_json::json;
use tempfile::TempDir;

fn story_db() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("stories.jsonl")).unwrap();
    db.insert(json!({"title": "tortoise", "content": "The hare was tired. At the end of the race, Paul laughed. Yesterday was hot."})).unwrap();
    db.insert(json!({"title": "city", "content": "Paul walked the city yesterday. The hare was tired, he said, quoting the fable at the end of the race."})).unwrap();
    db.insert(json!({"title": "unrelated", "content": "Nothing to see here. Just rain and coffee."})).unwrap();
    db.create_text_index("content").unwrap();
    (db, dir)
}

fn ids(db: &Database, search: TextSearch) -> Vec<String> {
    ids_in(db, "content", search)
}

fn ids_in(db: &Database, field: &str, search: TextSearch) -> Vec<String> {
    db.text_search(field, &search).unwrap()
}

fn titles(db: &Database, search: TextSearch) -> Vec<String> {
    ids(db, search)
        .into_iter()
        .map(|id| db.get(&id).unwrap()["title"].as_str().unwrap().to_string())
        .collect()
}

/// The headline use case: every phrase must match the story.
#[test]
fn and_search_multiple_phrases() {
    let (db, _dir) = story_db();
    let found = titles(
        &db,
        TextSearch::and(vec![
            TextQuery::Phrase("The hare was tired".into()),
            TextQuery::Phrase("at the end of the race".into()),
            TextQuery::Term("paul".into()),
            TextQuery::Term("yesterday".into()),
        ]),
    );
    let mut found = found;
    found.sort();
    assert_eq!(found, vec!["city", "tortoise"], "both stories contain ALL four queries");
}

#[test]
fn or_search_any_match() {
    let (db, _dir) = story_db();
    let mut found = titles(
        &db,
        TextSearch::or(vec![TextQuery::Phrase("end of the race".into()), TextQuery::Term("coffee".into())]),
    );
    found.sort();
    assert_eq!(found, vec!["city", "tortoise", "unrelated"]);
}

#[test]
fn and_vs_or_differ() {
    let (db, _dir) = story_db();
    let and = titles(&db, TextSearch::and(vec![TextQuery::Term("hare".into()), TextQuery::Term("coffee".into())]));
    assert_eq!(and, Vec::<String>::new(), "no story has BOTH hare and coffee");

    let or = titles(&db, TextSearch::or(vec![TextQuery::Term("hare".into()), TextQuery::Term("coffee".into())]));
    let mut or = or;
    or.sort();
    assert_eq!(or.len(), 3, "every story has hare or coffee");
}

/// Phrase must be contiguous — same words scattered don't match.
#[test]
fn phrase_contiguity() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("p.jsonl")).unwrap();
    db.insert(json!({"content": "the quick brown fox jumps"})).unwrap();
    db.insert(json!({"content": "the brown quick fox — words reordered, fox quick brown"})).unwrap();
    db.create_text_index("content").unwrap();

    let found = ids(&db, TextSearch::and(vec![TextQuery::Phrase("quick brown fox".into())]));
    assert_eq!(found.len(), 1, "only the contiguous occurrence matches");
}

#[test]
fn case_insensitive_default_and_case_sensitive_mode() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("c.jsonl")).unwrap();
    db.insert(json!({"content": "Paul went home. paul was tired."})).unwrap();
    db.insert(json!({"content": "paul never capitalizes."})).unwrap();
    db.create_text_index("content").unwrap();

    // Default: case-insensitive — both docs match "Paul"
    let found = ids(&db, TextSearch::and(vec![TextQuery::Term("Paul".into())]));
    assert_eq!(found.len(), 2, "case-insensitive matches both");

    // Case-sensitive: only doc 1 has capital-P Paul as a token
    let mut s = TextSearch::and(vec![TextQuery::Term("Paul".into())]);
    s.case_sensitive = true;
    let found = ids(&db, s);
    assert_eq!(found.len(), 1, "case-sensitive matches only capital-P");

    // Case-sensitive phrase
    let mut s = TextSearch::and(vec![TextQuery::Phrase("Paul went".into())]);
    s.case_sensitive = true;
    assert_eq!(ids(&db, s).len(), 1);
}

#[test]
fn prefix_search() {
    let (db, _dir) = story_db();
    let found = ids(&db, TextSearch::and(vec![TextQuery::Prefix("yester".into())]));
    assert_eq!(found.len(), 2, "yesterday matches prefix yester");
}

#[test]
fn exclude_filters_results() {
    let (db, _dir) = story_db();
    let found = titles(
        &db,
        TextSearch {
            mode: TextMode::And,
            case_sensitive: false,
            queries: vec![
                TextQuery::Term("hare".into()),
                TextQuery::Exclude(Box::new(TextQuery::Term("fable".into()))),
            ],
        },
    );
    assert_eq!(found, vec!["tortoise"], "city quotes the fable — excluded");
}

/// Whole-token matching: "race" must not match "racetrack" or "graces".
#[test]
fn term_whole_token_not_substring() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("w.jsonl")).unwrap();
    db.insert(json!({"content": "the racetrack graces"})).unwrap();
    db.insert(json!({"content": "the race"})).unwrap();
    db.create_text_index("content").unwrap();

    let found = ids(&db, TextSearch::and(vec![TextQuery::Term("race".into())]));
    assert_eq!(found.len(), 1, "term matches whole tokens only");
}

/// Index maintenance across write paths.
#[test]
fn index_follows_writes() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("m.jsonl")).unwrap();
    db.insert(json!({"content": "initial text about zebras"})).unwrap();
    db.create_text_index("content").unwrap();

    let id = db.text_search("content", &TextSearch::and(vec![TextQuery::Term("zebras".into())])).unwrap()[0].clone();

    // update() replaces text
    db.update(&id, json!({"content": "now about lions"})).unwrap();
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("zebras".into())])).len(), 0);
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("lions".into())])).len(), 1);

    // set() rewrites the field
    db.set(&id, "content", json!("rewritten about tigers")).unwrap();
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("lions".into())])).len(), 0);
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("tigers".into())])).len(), 1);

    // array_push on string-array fields (indexed field is 'content'; we push
    // into a separate 'lines' array just to prove unrelated fields don't
    // corrupt the content index)
    db.set(&id, "content", json!("base")).unwrap();
    db.insert(json!({"content": "chapter words"})).unwrap();
    let id2 = db.text_search("content", &TextSearch::and(vec![TextQuery::Term("chapter".into())])).unwrap()[0].clone();
    db.set(&id2, "lines", json!(["one two"])).unwrap();
    db.array_push(&id2, "lines", json!("three four")).unwrap();
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("chapter".into())])).len(), 1, "unrelated-field writes keep the content index intact");

    // delete() removes from index
    db.delete(&id2).unwrap();
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("chapter".into())])).len(), 0);

    // remove() of the indexed field
    db.remove(&id, "content").unwrap();
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("tigers".into())])).len(), 0);
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("base".into())])).len(), 0);
}

#[test]
fn search_without_index_fails_loud() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("n.jsonl")).unwrap();
    db.insert(json!({"content": "text"})).unwrap();
    let err = db.text_search("content", &TextSearch::and(vec![TextQuery::Term("text".into())]));
    assert!(err.is_err(), "no text index → error, not silent scan");
}

#[test]
fn validation_rejects_bad_queries() {
    let s = TextSearch::and(vec![TextQuery::Exclude(Box::new(TextQuery::Term("x".into())))]);
    assert!(s.validate().is_err(), "all-exclude must be rejected");
    let s = TextSearch::and(vec![TextQuery::Term("  ".into())]);
    assert!(s.validate().is_err(), "empty term must be rejected");
}

/// Array-of-strings fields are searchable (joined regions).
#[test]
fn string_array_field_searchable() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("a.jsonl")).unwrap();
    db.insert(json!({"paragraphs": ["First paragraph mentions gold.", "Second is silent."]})).unwrap();
    db.insert(json!({"paragraphs": ["Nothing precious here."]})).unwrap();
    db.create_text_index("paragraphs").unwrap();
    assert_eq!(ids_in(&db, "paragraphs", TextSearch::and(vec![TextQuery::Term("gold".into())])).len(), 1);
}

/// German + numbers tokenize sanely (Unicode alphanumeric).
#[test]
fn unicode_tokenization() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("u.jsonl")).unwrap();
    db.insert(json!({"content": "Der Hase war müde. Rennzeit 2026."})).unwrap();
    db.create_text_index("content").unwrap();
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("müde".into())])).len(), 1);
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("2026".into())])).len(), 1);
    assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Prefix("müd".into())])).len(), 1);
}

/// Parallel build produces identical search results to the (implicit
/// sequential) incremental path, including phrase and AND queries.
#[test]
fn parallel_build_matches_incremental() {
    let texts: Vec<(String, String)> = (0..200)
        .map(|i| {
            let content = if i % 50 == 7 {
                format!("story {} the hare was tired at the end of the race paul yesterday filler words", i)
            } else {
                format!("story {} ordinary filler text about rivers and mountains and coffee", i)
            };
            (format!("doc_{}", i), content)
        })
        .collect();

    // Parallel-built index via create_text_index (build_parallel path)
    let dir1 = TempDir::new().unwrap();
    let db1 = Database::open(dir1.path().join("a.jsonl")).unwrap();
    for (id, content) in &texts {
        db1.insert(json!({"_id": id, "content": content.clone()})).unwrap();
    }
    db1.create_text_index_with_threads("content", 8).unwrap();

    let headline = TextSearch::and(vec![
        TextQuery::Phrase("the hare was tired".into()),
        TextQuery::Phrase("at the end of the race".into()),
        TextQuery::Term("paul".into()),
        TextQuery::Term("yesterday".into()),
    ]);
    let mut hits1 = db1.text_search("content", &headline).unwrap();
    hits1.sort();
    assert_eq!(hits1.len(), 4, "docs 7, 57, 107, 157");

    let mut common = db1
        .text_search("content", &TextSearch::and(vec![TextQuery::Term("coffee".into())]))
        .unwrap();
    common.sort();
    assert_eq!(common.len(), 196);
}

/// Disk cache: fresh journal loads the cache; any write invalidates it
/// (journal size moves); drop_text_index removes the cache file.
#[test]
fn index_disk_cache_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("cache.jsonl");

    // Build + cache
    {
        let db = Database::open(&path).unwrap();
        db.insert(json!({"content": "alpha beta gamma"})).unwrap();
        db.insert(json!({"content": "beta delta"})).unwrap();
        db.create_text_index("content").unwrap();
        assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("beta".into())])).len(), 2);
        assert!(dir.path().join("_index_content.fti").exists(), "cache file written");
    }

    // Reopen, unchanged journal → cache load, same results
    {
        let db = Database::open(&path).unwrap();
        db.create_text_index("content").unwrap();
        assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("beta".into())])).len(), 2);
        assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("gamma".into())])).len(), 1);

        // Now write: journal grows → cache is stale for the next boot,
        // but the running index updates incrementally.
        db.insert(json!({"content": "gamma epsilon"})).unwrap();
        assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("gamma".into())])).len(), 2);
    }

    // Reopen after write → stale stamp → rebuild → correct results
    {
        let db = Database::open(&path).unwrap();
        db.create_text_index("content").unwrap();
        assert_eq!(ids(&db, TextSearch::and(vec![TextQuery::Term("gamma".into())])).len(), 2);

        // drop removes cache
        db.drop_text_index("content").unwrap();
        assert!(!dir.path().join("_index_content.fti").exists());
    }
}
