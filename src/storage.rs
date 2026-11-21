//! JSON Lines storage I/O.
//!
//! Append-only file format. Each line is one complete JSON object.
//! First line is the `_meta` header: format version plus the journal lineage
//! (`source_epoch_uuid`, `last_seq`, `min_available_seq`). Subsequent lines are
//! documents, patches or tombstones, each carrying its commit `_seq`.
//! Base-state lines written by a compaction carry no `_seq`.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Current storage format version.
const STORAGE_VERSION: u64 = 1;

/// Journal field carrying a mutation's commit sequence. Engine-owned: stripped
/// before a document ever reaches `Database::docs`.
pub const SEQ_FIELD: &str = "_seq";

/// Identity and replay window of one document journal.
///
/// `source_epoch_uuid` survives compaction and export, so a consumer can tell
/// "same history" from "new history". `min_available_seq` is the oldest seq a
/// cursor may resume from; a lower cursor is `Error::CursorExpired`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLineage {
    /// Canonical RFC 4122 UUIDv4 identifying the journal's history.
    pub source_epoch_uuid: String,
    /// Highest committed sequence number. `0` means nothing committed yet.
    pub last_seq: u64,
    /// Oldest seq still replayable through `changes_since`.
    pub min_available_seq: u64,
}

impl SourceLineage {
    /// A brand-new lineage: fresh epoch, nothing committed, full history.
    pub fn fresh() -> Self {
        SourceLineage {
            source_epoch_uuid: generate_epoch_uuid(),
            last_seq: 0,
            min_available_seq: 1,
        }
    }
}

/// What a committed mutation did to one document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// The document's full post-mutation state.
    Upsert(Value),
    /// The document was deleted.
    Delete,
}

/// One committed document mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeEvent {
    /// Commit sequence; strictly ascending across the journal.
    pub seq: u64,
    /// Document id.
    pub id: String,
    /// Post-mutation state or deletion.
    pub kind: ChangeKind,
}

/// A batch of changes plus the lineage they belong to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeBatch {
    /// Lineage after the last event in `events`.
    pub lineage: SourceLineage,
    /// Events with `seq > since_seq`, ascending.
    pub events: Vec<ChangeEvent>,
}

/// A consistent document set and the seq watermark it was taken at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Lineage as of the snapshot.
    pub lineage: SourceLineage,
    /// Documents, sorted by id.
    pub docs: Vec<(String, Value)>,
}

/// Generate a canonical RFC 4122 UUIDv4 string.
pub fn generate_epoch_uuid() -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&fastrand::u64(..).to_le_bytes());
    bytes[8..].copy_from_slice(&fastrand::u64(..).to_le_bytes());
    format_uuid(bytes)
}

/// A deterministic UUID for a journal written before lineage tracking: derived
/// from `_meta.created`, so reopening the same uncompacted file reports the
/// same epoch instead of a fresh one every boot.
pub fn legacy_epoch_uuid(created: &str) -> String {
    fn fnv1a(seed: u64, bytes: &[u8]) -> u64 {
        let mut h = seed;
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }
    let first = fnv1a(0xcbf2_9ce4_8422_2325, created.as_bytes());
    let second = fnv1a(first ^ 0x9e37_79b9_7f4a_7c15, created.as_bytes());
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&first.to_be_bytes());
    bytes[8..].copy_from_slice(&second.to_be_bytes());
    format_uuid(bytes)
}

/// Format 16 bytes as a canonical UUID string with v4 version/variant bits.
fn format_uuid(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Meta header written as first line of every JSONL file.
#[derive(Serialize, Deserialize, Debug)]
struct MetaHeader {
    _meta: MetaInner,
}

/// The `_meta` header. The lineage fields are absent from files written before
/// lineage tracking (v1 legacy journals); their defaults keep those files
/// parseable — such a file is replayed as "every base line was a commit".
#[derive(Serialize, Deserialize, Debug)]
pub struct MetaInner {
    /// Storage format version.
    pub version: u64,
    /// Creation stamp (UNIX seconds). Also seeds the legacy epoch UUID.
    #[serde(default)]
    pub created: String,
    /// Journal identity; `None` means a pre-lineage file.
    #[serde(default)]
    pub source_epoch_uuid: Option<String>,
    /// Last sequence number persisted by a compaction/export.
    #[serde(default)]
    pub last_seq: u64,
    /// Oldest replayable seq, persisted by a compaction/export.
    #[serde(default = "default_min_available_seq")]
    pub min_available_seq: u64,
}

fn default_min_available_seq() -> u64 {
    1
}

/// Create the `_meta` header line.
fn meta_line(lineage: &SourceLineage) -> String {
    let header = MetaHeader {
        _meta: MetaInner {
            version: STORAGE_VERSION,
            created: chrono_free_timestamp(),
            source_epoch_uuid: Some(lineage.source_epoch_uuid.clone()),
            last_seq: lineage.last_seq,
            min_available_seq: lineage.min_available_seq,
        },
    };
    serde_json::to_string(&header).expect("meta header is serializable")
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

/// Initialize a new JSONL file with a `_meta` header carrying `lineage`.
/// Creates parent directories if needed.
pub fn init_file(path: &Path, lineage: &SourceLineage) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(Error::io_err(parent, "create directories"))?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(Error::io_err(path, "create JSONL file"))?;
    writeln!(file, "{}", meta_line(lineage)).map_err(Error::io_err(path, "write meta header"))?;
    file.flush()
        .map_err(Error::io_err(path, "flush meta header"))?;
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
    Ok(read_all_with_meta(path)?.1)
}

/// Read the `_meta` header and all journal lines.
///
/// The header is `None` when the file has none (empty file, or a file written
/// by something other than ndb) — the caller then treats the journal as
/// pre-lineage. Same crash-recovery contract as `read_all`.
pub fn read_all_with_meta(path: &Path) -> Result<(Option<MetaInner>, Vec<Value>)> {
    let file = File::open(path).map_err(Error::io_err(path, "open JSONL for read"))?;
    let reader = BufReader::new(file);
    let mut meta = None;
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
            match serde_json::from_str::<MetaHeader>(trimmed) {
                Ok(header) => meta = Some(header._meta),
                Err(e) => {
                    eprintln!(
                        "ndb: corrupted meta header in {}, attempting recovery: {}",
                        path.display(),
                        e
                    );
                    corrupted_lines += 1;
                }
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

    Ok((meta, docs))
}

/// Rewrite a JSONL file with only the given documents.
/// Writes to a temp file first, then atomic rename.
///
/// A fresh lineage: this form is for files that are not a live document
/// journal (the trash file, a brand-new merge target). Rewriting a live
/// journal must use `rewrite_atomic_with_meta` so `seq` is never reset.
pub fn rewrite_atomic(path: &Path, docs: &[&Value]) -> Result<()> {
    rewrite_atomic_with_meta(path, docs, &SourceLineage::fresh())
}

/// Rewrite a JSONL file with only base-state documents and an explicit
/// lineage. The written docs carry no `_seq` — they are the base state a
/// compaction produces, not replayable events.
pub fn rewrite_atomic_with_meta(
    path: &Path,
    docs: &[&Value],
    lineage: &SourceLineage,
) -> Result<()> {
    let tmp_path = path.with_extension("jsonl.tmp");

    {
        let mut tmp_file = File::create(&tmp_path)
            .map_err(Error::io_err(&tmp_path, "create temp file for compaction"))?;
        // Write meta header
        writeln!(tmp_file, "{}", meta_line(lineage))
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

/// Append a single document to the persistent trash file.
/// Initializes the file with a meta header if it doesn't exist.
pub fn append_doc_trash(trash_path: &Path, doc: &Value) -> Result<()> {
    if !trash_path.exists() {
        init_file(trash_path, &SourceLineage::fresh())?;
    }
    let mut file = open_for_append(trash_path)?;
    let line = serde_json::to_string(doc)?;
    writeln!(file, "{}", line).map_err(Error::io_err(trash_path, "write trash doc"))?;
    file.flush()
        .map_err(Error::io_err(trash_path, "flush trash file"))?;
    Ok(())
}

/// Read all documents from a trash file.
/// Reuses the robust read_all logic to skip meta headers and corrupted lines.
pub fn read_trash(trash_path: &Path) -> Result<Vec<Value>> {
    if !trash_path.exists() {
        return Ok(Vec::new());
    }
    read_all(trash_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_creates_file_with_meta() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        init_file(&path, &SourceLineage::fresh()).unwrap();

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
        init_file(&path, &SourceLineage::fresh()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn init_fails_if_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        init_file(&path, &SourceLineage::fresh()).unwrap();
        assert!(init_file(&path, &SourceLineage::fresh()).is_err());
    }

    #[test]
    fn append_and_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        init_file(&path, &SourceLineage::fresh()).unwrap();

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
        init_file(&path, &SourceLineage::fresh()).unwrap();

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
        init_file(&path, &SourceLineage::fresh()).unwrap();

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
        init_file(&path, &SourceLineage::fresh()).unwrap();

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
        init_file(&path, &SourceLineage::fresh()).unwrap();

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
        init_file(&path, &SourceLineage::fresh()).unwrap();

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

    // ─── Lineage ─────────────────────────────────────────────────────

    fn is_uuid_v4(s: &str) -> bool {
        let b = s.as_bytes();
        b.len() == 36
            && b.iter().enumerate().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => *c == b'-',
                _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
            })
            && b[14] == b'4'
            && matches!(b[19], b'8' | b'9' | b'a' | b'b')
    }

    #[test]
    fn epoch_uuid_is_canonical_v4() {
        let a = generate_epoch_uuid();
        let b = generate_epoch_uuid();
        assert!(is_uuid_v4(&a), "{a} is not a canonical v4 UUID");
        assert!(is_uuid_v4(&b), "{b} is not a canonical v4 UUID");
        assert_ne!(a, b);
    }

    #[test]
    fn legacy_epoch_uuid_is_deterministic() {
        assert_eq!(
            legacy_epoch_uuid("1700000000"),
            legacy_epoch_uuid("1700000000")
        );
        assert_ne!(
            legacy_epoch_uuid("1700000000"),
            legacy_epoch_uuid("1700000001")
        );
        assert!(is_uuid_v4(&legacy_epoch_uuid("1700000000")));
    }

    #[test]
    fn meta_round_trips_lineage() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lineage.jsonl");
        let lineage = SourceLineage {
            source_epoch_uuid: generate_epoch_uuid(),
            last_seq: 7,
            min_available_seq: 5,
        };
        init_file(&path, &lineage).unwrap();

        let (meta, docs) = read_all_with_meta(&path).unwrap();
        let meta = meta.expect("meta header");
        assert!(docs.is_empty());
        assert_eq!(
            meta.source_epoch_uuid.as_deref(),
            Some(lineage.source_epoch_uuid.as_str())
        );
        assert_eq!(meta.last_seq, 7);
        assert_eq!(meta.min_available_seq, 5);
    }

    #[test]
    fn legacy_meta_parses_with_lineage_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.jsonl");
        fs::write(
            &path,
            "{\"_meta\":{\"version\":1,\"created\":\"0\"}}\n{\"_id\":\"a\"}\n",
        )
        .unwrap();

        let (meta, docs) = read_all_with_meta(&path).unwrap();
        let meta = meta.expect("legacy meta header still parses");
        assert_eq!(meta.source_epoch_uuid, None);
        assert_eq!(meta.last_seq, 0);
        assert_eq!(meta.min_available_seq, 1);
        assert_eq!(meta.created, "0");
        assert_eq!(docs.len(), 1);
    }
}
