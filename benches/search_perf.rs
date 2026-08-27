// Quick perf probe: build a text index over a synthetic story corpus and
// run the headline AND-phrase query. Not a unit test — run manually:
//   cargo run --release --example search_perf
// Numbers printed to stdout.

use ndb::{Database, TextQuery, TextSearch};
use std::time::Instant;

fn main() {
    let db = Database::open_in_memory().unwrap();

    // ~1000 stories × ~50KB = ~50MB corpus
    let story_words: Vec<&str> = "the hare was tired at end of race paul yesterday forest \
        river mountain castle dragon knight sword shield whisper shadow echo light dark storm \
        thunder rain coffee bread journey traveler village market song dance laughter silence \
        morning evening winter summer autumn spring stone path bridge garden window door key"
        .split_whitespace()
        .collect();

    println!("inserting stories...");
    let t0 = Instant::now();
    let mut total_chars = 0usize;
    for i in 0..1000 {
        let mut text = String::with_capacity(60_000);
        // deterministic pseudo-random content, seeded per story
        let mut seed = (i as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        loop {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let idx = ((seed >> 33) as usize) % story_words.len();
            text.push_str(story_words[idx]);
            text.push(' ');
            if text.len() >= 55_000 {
                break;
            }
        }
        // needle phrases in specific stories
        if i == 137 || i == 642 {
            text.push_str(" the hare was tired at the end of the race and paul remembered yesterday ");
        }
        if i % 100 == 0 {
            text.push_str(" paul went home yesterday "); // partial matches for OR
        }
        total_chars += text.len();
        db.insert(serde_json::json!({ "story": i, "content": text })).unwrap();
    }
    println!(
        "inserted {} stories, {:.1} MB text in {:?}",
        1000,
        total_chars as f64 / 1e6,
        t0.elapsed()
    );

    println!("building text index...");
    let t1 = Instant::now();
    db.create_text_index("content").unwrap();
    println!("index built in {:?}", t1.elapsed());

    // Headline query: 2 phrases + 2 terms, AND
    let search = TextSearch::and(vec![
        TextQuery::Phrase("the hare was tired".into()),
        TextQuery::Phrase("at the end of the race".into()),
        TextQuery::Term("paul".into()),
        TextQuery::Term("yesterday".into()),
    ]);
    let t2 = Instant::now();
    let hits = db.text_search("content", &search).unwrap();
    let q1 = t2.elapsed();
    assert_eq!(hits.len(), 2, "expected the two needle stories");
    println!("AND phrase query: {} hits in {:?}", hits.len(), q1);

    // Single common term
    let t3 = Instant::now();
    let hits = db
        .text_search("content", &TextSearch::and(vec![TextQuery::Term("forest".into())]))
        .unwrap();
    println!("single common term: {} hits in {:?}", hits.len(), t3.elapsed());

    // OR query
    let t4 = Instant::now();
    let hits = db
        .text_search(
            "content",
            &TextSearch::or(vec![
                TextQuery::Phrase("end of the race".into()),
                TextQuery::Term("coffee".into()),
            ]),
        )
        .unwrap();
    println!("OR query: {} hits in {:?}", hits.len(), t4.elapsed());

    // Write-while-indexed cost (array_push-like set on content)
    let id = db
        .text_search("content", &TextSearch::and(vec![TextQuery::Term("forest".into())]))
        .unwrap()[0]
        .clone();
    let t5 = Instant::now();
    db.set(&id, "content", serde_json::json!("rewritten small text")).unwrap();
    println!("set() with reindex: {:?}", t5.elapsed());
}
