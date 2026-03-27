//! JSON Lines storage I/O.
//!
//! Append-only file format. Each line is one complete JSON object.
//! First line is `_meta` header with version info.
//! Subsequent lines are documents or tombstones.

use crate::error::{Error, Result};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Current storage format version.
const STORAGE_VERSION: u64 = 1;

/// Meta header written as first line of every JSONL file.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct MetaHeader {
    _meta: MetaInner,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
struct MetaInner {
    version: u64,
    created: String,
}

/// Create the `_meta` header line.
fn meta_line() -> String {
    let header = MetaHeader {
        _meta: MetaInner {
            version: STORAGE_VERSION,
            created: chrono_free_timestamp(),
        },
    };
    serde_json::to_string(&header).unwrap()
}

/// Simple timestamp without chrono dependency.
fn chrono_free_timestamp() -> String {
    // Use std::time as UNIX timestamp — good enough for metadata
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Format as ISO-8601 date (UTC approximation)
    format!("{}", secs)
}

/// Initialize a new JSONL file with _meta header.
/// Creates parent directories if needed.
pub fn init_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(Error::io_err(parent, "create directories"))?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(Error::io_err(path, "create JSONL file"))?;
    writeln!(file, "{}", meta_line()).map_err(Error::io_err(path, "write meta header"))?;
    file.flush().map_err(Error::io_err(path, "flush meta header"))?;
    Ok(())
}

/// Open an existing JSONL file for appending.
pub fn open_for_append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(Error::io_err(path, "open JSONL for append"))
}

/// Append a document line to the file.
pub fn append_line(file: &mut File, path: &Path, line: &str) -> Result<()> {
    writeln!(file, "{}", line).map_err(Error::io_err(path, "append line"))?;
    Ok(())
}

/// Append and fsync (for Immediate persistence mode).
pub fn append_line_sync(file: &mut File, path: &Path, line: &str) -> Result<()> {
    writeln!(file, "{}", line).map_err(Error::io_err(path, "append line"))?;
    file.flush().map_err(Error::io_err(path, "flush"))?;
    file.sync_all()
        .map_err(Error::io_err(path, "fsync after append"))?;
    Ok(())
}

/// Read all documents from a JSONL file.
/// Returns a vector of parsed JSON values (skips _meta header line).
/// Last write wins: later entries for the same _id overwrite earlier ones.
///
/// **Crash recovery:** Malformed or truncated lines (e.g. from power loss
/// during write) are skipped with a warning. Only complete, valid JSON
/// lines are loaded. This trades a potentially lost last write for
/// deterministic startup behavior.
pub fn read_all(path: &Path) -> Result<Vec<Value>> {
    let file = File::open(path).map_err(Error::io_err(path, "open JSONL for read"))?;
    let reader = BufReader::new(file);
    let mut docs = Vec::new();
    let mut corrupted_lines = 0usize;

    for (line_num, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                // I/O error reading a line — likely truncated at EOF
                eprintln!(
                    "ndb: skipping unreadable line {} in {}: {}",
                    line_num,
                    path.display(),
                    e
                );
                corrupted_lines += 1;
                continue;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip _meta header
        if trimmed.contains("\"_meta\"") && line_num == 0 {
            // Validate meta header is parseable
            if serde_json::from_str::<Value>(trimmed).is_err() {
                eprintln!(
                    "ndb: corrupted meta header in {}, attempting recovery",
                    path.display()
                );
                corrupted_lines += 1;
            }
            continue;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(val) => docs.push(val),
            Err(e) => {
                // Malformed JSON — likely a truncated write from crash
                eprintln!(
                    "ndb: skipping corrupted line {} in {}: {}",
                    line_num + 1,
                    path.display(),
                    e
                );
                corrupted_lines += 1;
            }
        }
    }

    if corrupted_lines > 0 {
        eprintln!(
            "ndb: recovered {} corrupted line(s) in {} ({} valid docs loaded)",
            corrupted_lines,
            path.display(),
            docs.len()
        );
    }

    Ok(docs)
}

/// Rewrite a JSONL file with only the given documents.
/// Writes to a temp file first, then atomic rename.
pub fn rewrite_atomic(path: &Path, docs: &[&Value]) -> Result<()> {
    let tmp_path = path.with_extension("jsonl.tmp");

    {
        let mut tmp_file = File::create(&tmp_path)
            .map_err(Error::io_err(&tmp_path, "create temp file for compaction"))?;
        // Write meta header
        writeln!(tmp_file, "{}", meta_line())
            .map_err(Error::io_err(&tmp_path, "write meta header"))?;
        // Write all active docs
        for doc in docs {
            let line = serde_json::to_string(doc)?;
            writeln!(tmp_file, "{}", line)
                .map_err(Error::io_err(&tmp_path, "write doc during compaction"))?;
        }
        tmp_file
            .flush()
            .map_err(Error::io_err(&tmp_path, "flush temp file"))?;
        tmp_file
            .sync_all()
            .map_err(Error::io_err(&tmp_path, "fsync temp file"))?;
    }

    // Atomic rename
    fs::rename(&tmp_path, path).map_err(Error::io_err(path, "atomic rename after compaction"))?;

    Ok(())
}

/// Append documents to a trash file (dated archive).
pub fn append_trash(trash_dir: &Path, collection_name: &str, docs: &[&Value]) -> Result<()> {
    fs::create_dir_all(trash_dir).map_err(Error::io_err(trash_dir, "create trash directory"))?;

    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let trash_file = trash_dir.join(format!("{}_{}.jsonl", collection_name, secs));

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&trash_file)
        .map_err(Error::io_err(&trash_file, "create trash file"))?;

    for doc in docs {
        let line = serde_json::to_string(doc)?;
        writeln!(file, "{}", line).map_err(Error::io_err(&trash_file, "write trash doc"))?;
    }

    file.flush()
        .map_err(Error::io_err(&trash_file, "flush trash file"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_creates_file_with_meta() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        init_file(&path).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"_meta\""));
        assert!(lines[0].contains("\"version\""));
    }

    #[test]
    fn init_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/deep/test.jsonl");
        init_file(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn init_fails_if_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        init_file(&path).unwrap();
        assert!(init_file(&path).is_err());
    }

    #[test]
    fn append_and_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        init_file(&path).unwrap();

        let mut file = open_for_append(&path).unwrap();
        let doc = serde_json::json!({"_id": "abc123", "name": "test"});
        append_line(&mut file, &path, &serde_json::to_string(&doc).unwrap()).unwrap();

        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["name"], "test");
    }

    #[test]
    fn read_skips_meta_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        init_file(&path).unwrap();

        let mut file = open_for_append(&path).unwrap();
        let doc = serde_json::json!({"_id": "x"});
        append_line(&mut file, &path, &serde_json::to_string(&doc).unwrap()).unwrap();

        let docs = read_all(&path).unwrap();
        // Should only have the doc, not the meta header
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["_id"], "x");
    }

    #[test]
    fn rewrite_atomic_replaces_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        init_file(&path).unwrap();

        let mut file = open_for_append(&path).unwrap();
        let doc1 = serde_json::json!({"_id": "a", "v": 1});
        let doc2 = serde_json::json!({"_id": "b", "v": 2});
        let doc3 = serde_json::json!({"_id": "a", "_deleted": 999});
        append_line(&mut file, &path, &serde_json::to_string(&doc1).unwrap()).unwrap();
        append_line(&mut file, &path, &serde_json::to_string(&doc2).unwrap()).unwrap();
        append_line(&mut file, &path, &serde_json::to_string(&doc3).unwrap()).unwrap();

        // Rewrite with only doc2 (doc1 was deleted)
        let active = vec![&doc2];
        rewrite_atomic(&path, &active).unwrap();

        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["_id"], "b");
    }

    // ─── Phase 6: Corruption Recovery Tests ──────────────────────────

    #[test]
    fn read_all_skips_truncated_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("truncated.jsonl");
        init_file(&path).unwrap();

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        // Valid doc
        writeln!(file, "{}", r#"{"_id":"ok","v":1}"#).unwrap();
        // Truncated JSON (simulates power loss during write)
        writeln!(file, "{}", r#"{"_id":"broken","v":2"#).unwrap();
        // Another valid doc
        writeln!(file, "{}", r#"{"_id":"also_ok","v":3}"#).unwrap();

        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["_id"], "ok");
        assert_eq!(docs[1]["_id"], "also_ok");
    }

    #[test]
    fn read_all_handles_empty_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("gaps.jsonl");
        init_file(&path).unwrap();

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file).unwrap();
        writeln!(file, "{}", r#"{"_id":"a"}"#).unwrap();
        writeln!(file, "   ").unwrap();
        writeln!(file, "{}", r#"{"_id":"b"}"#).unwrap();

        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 2);
    }

    #[test]
    fn read_all_handles_completely_corrupted_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("garbage.jsonl");
        // Write garbage (no meta header)
        fs::write(&path, "not json at all\nalso not json\n").unwrap();

        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 0);
    }

    #[test]
    fn read_all_handles_partial_last_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("partial.jsonl");
        init_file(&path).unwrap();

        // Write valid content then append partial line (no trailing newline)
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", r#"{"_id":"good","v":1}"#).unwrap();
        // Simulate truncated write: partial JSON without newline
        write!(file, "{}", r#"{"_id":"partial","v":2"#).unwrap();

        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["_id"], "good");
    }
}
