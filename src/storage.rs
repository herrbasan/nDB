//! JSON Lines storage I/O.
//!
//! Append-only file format. Each line is one complete JSON object.
//! First line is the `_meta` header: format version plus the journal lineage
//! (`source_epoch_uuid`, `last_seq`, `min_available_seq`). Subsequent lines are
//! documents, patches or tombstones, each carrying its commit `_seq`.
//! Base-state lines written by a compaction carry no `_seq`.

use crate::error::{Error, Result};
use crate::report;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

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
///
/// One `write_all` for payload + terminator: two calls per line doubled the
/// syscall count on the hot path and let a crash split the payload from its
/// newline (P6).
pub fn append_line(file: &mut File, path: &Path, line: &str) -> Result<()> {
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    file.write_all(&buf)
        .map_err(Error::io_err(path, "append line"))?;
    Ok(())
}

/// Append and fsync (for Immediate persistence mode).
pub fn append_line_sync(file: &mut File, path: &Path, line: &str) -> Result<()> {
    let mut buf = Vec::with_capacity(line.len() + 1);
    buf.extend_from_slice(line.as_bytes());
    buf.push(b'\n');
    file.write_all(&buf)
        .map_err(Error::io_err(path, "append line"))?;
    file.flush().map_err(Error::io_err(path, "flush"))?;
    file.sync_all()
        .map_err(Error::io_err(path, "fsync after append"))?;
    Ok(())
}

/// Fsync the directory that holds `path`, so a completed rename is durable.
///
/// A rename is only a directory-entry mutation: without flushing the parent,
/// a power loss can leave the old entry or no entry at all even though the
/// file's own bytes were fsynced (H1).
pub fn sync_parent_dir(path: &Path) -> Result<()> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    sync_dir(dir).map_err(Error::io_err(dir, "fsync parent directory"))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot `sync_all` a directory without a handle opened for write and
/// with `FILE_FLAG_BACKUP_SEMANTICS`; a read-only handle makes
/// `FlushFileBuffers` fail with `ERROR_ACCESS_DENIED`. Opened this way it
/// succeeds on NTFS for a normal user — proven by
/// `windows_directory_fsync_is_honest` in this module's tests.
#[cfg(windows)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?;
    file.sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// One journal line that failed to parse.
#[derive(Debug, Clone)]
struct BadLine {
    /// 1-based physical line number.
    line: usize,
    /// Byte offset of the line's first byte.
    start: u64,
    /// Why the line was rejected.
    message: String,
}

/// What one streaming pass over a journal found.
#[derive(Debug, Default)]
struct JournalScan {
    meta: Option<MetaInner>,
    docs: Vec<Value>,
    /// Byte offset just past the last complete, valid line.
    last_good_offset: u64,
    /// File size when the pass started.
    total_bytes: u64,
    /// Physical lines read.
    total_lines: usize,
    /// Lines that were kept (including blank separators and the `_meta` line).
    kept_lines: usize,
    /// The final line was an incomplete write (no terminator, or invalid JSON
    /// with nothing after it).
    torn_tail: Option<BadLine>,
    /// The first corrupt line that is followed by more content (repair mode).
    mid_journal: Option<BadLine>,
}

fn corruption_at(path: &Path, bad: &BadLine) -> Error {
    Error::corruption(
        path,
        format!(
            "line {} is corrupt and is followed by more journal lines ({}); \
             refusing to silently skip mid-journal corruption. Run `ndb repair {}` to \
             quarantine the file and truncate at the last good line.",
            bad.line,
            bad.message,
            path.display()
        ),
    )
}

/// Stream a JSONL journal, tracking the exact byte offset of the last good
/// line so a torn tail can be truncated and a mid-journal hole can be refused.
///
/// In `strict` mode mid-journal corruption (and a corrupt `_meta` header) is an
/// `Error::Corruption`; a torn *final* line is always recovered from by
/// truncating to `last_good_offset`. In repair mode the first mid-journal
/// corrupt line stops the scan instead of failing, so `ndb repair` can report
/// and truncate.
fn scan_journal(path: &Path, strict: bool) -> Result<JournalScan> {
    let file = File::open(path).map_err(Error::io_err(path, "open JSONL for read"))?;
    let total_bytes = file
        .metadata()
        .map_err(Error::io_err(path, "stat JSONL file"))?
        .len();
    let mut reader = BufReader::new(file);

    let mut scan = JournalScan {
        total_bytes,
        ..JournalScan::default()
    };
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut offset: u64 = 0;
    let mut first_content_seen = false;
    // A corrupt terminated line whose successor has not been read yet: if the
    // file ends here it is a torn tail, otherwise it is mid-journal corruption.
    let mut pending: Option<BadLine> = None;

    loop {
        buf.clear();
        let read = reader
            .read_until(b'\n', &mut buf)
            .map_err(Error::io_err(path, "read JSONL line"))?;
        if read == 0 {
            break;
        }
        scan.total_lines += 1;
        let line_no = scan.total_lines;
        let start = offset;
        offset += read as u64;
        let terminated = buf.last() == Some(&b'\n');
        let mut body = &buf[..if terminated { read - 1 } else { read }];
        if body.last() == Some(&b'\r') {
            body = &body[..body.len() - 1];
        }

        // Every line is written as payload + '\n' in one call, so a final line
        // without a terminator never completed its write.
        if !terminated {
            scan.torn_tail = Some(BadLine {
                line: line_no,
                start,
                message: "final line has no terminator (interrupted write)".to_string(),
            });
            break;
        }

        let line = match std::str::from_utf8(body) {
            Ok(line) => line,
            Err(e) => {
                let bad = BadLine {
                    line: line_no,
                    start,
                    message: format!("not valid UTF-8: {e}"),
                };
                if let Some(previous) = pending.replace(bad) {
                    return finish_mid_journal(path, strict, scan, previous);
                }
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            scan.kept_lines += 1;
            scan.last_good_offset = offset;
            continue;
        }

        if !first_content_seen {
            first_content_seen = true;
            if trimmed.contains("\"_meta\"") {
                match serde_json::from_str::<MetaHeader>(trimmed) {
                    Ok(header) => {
                        scan.meta = Some(header._meta);
                        scan.kept_lines += 1;
                        scan.last_good_offset = offset;
                        continue;
                    }
                    Err(e) => {
                        // A header that is complete JSON but not a valid
                        // header is corruption, never a legacy fallback (M2).
                        if serde_json::from_str::<Value>(trimmed).is_ok() {
                            let bad = BadLine {
                                line: line_no,
                                start,
                                message: format!("corrupt _meta header: {e}"),
                            };
                            return finish_mid_journal(path, strict, scan, bad);
                        }
                        let bad = BadLine {
                            line: line_no,
                            start,
                            message: format!("unparseable _meta header: {e}"),
                        };
                        if let Some(previous) = pending.replace(bad) {
                            return finish_mid_journal(path, strict, scan, previous);
                        }
                        continue;
                    }
                }
            }
        }

        match serde_json::from_str::<Value>(trimmed) {
            Ok(value) => {
                if let Some(previous) = pending.take() {
                    return finish_mid_journal(path, strict, scan, previous);
                }
                scan.docs.push(value);
                scan.kept_lines += 1;
                scan.last_good_offset = offset;
            }
            Err(e) => {
                let bad = BadLine {
                    line: line_no,
                    start,
                    message: format!("invalid JSON: {e}"),
                };
                if let Some(previous) = pending.replace(bad) {
                    return finish_mid_journal(path, strict, scan, previous);
                }
            }
        }
    }

    // Nothing followed the bad line: it is the torn tail of an interrupted
    // write, not a hole in the middle of the journal.
    if let Some(bad) = pending {
        scan.torn_tail = Some(bad);
    }
    Ok(scan)
}

fn finish_mid_journal(
    path: &Path,
    strict: bool,
    mut scan: JournalScan,
    bad: BadLine,
) -> Result<JournalScan> {
    if strict {
        return Err(corruption_at(path, &bad));
    }
    scan.mid_journal = Some(bad);
    Ok(scan)
}

fn truncate_to(path: &Path, offset: u64) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(Error::io_err(path, "open JSONL for truncation"))?;
    file.set_len(offset)
        .map_err(Error::io_err(path, "truncate JSONL tail"))?;
    file.sync_all()
        .map_err(Error::io_err(path, "fsync truncated JSONL"))?;
    sync_parent_dir(path)
}

/// Read all documents from a JSONL file.
/// Returns a vector of parsed JSON values (skips _meta header line).
/// Last write wins: later entries for the same _id overwrite earlier ones.
///
/// **Crash recovery:** a torn *final* line (an interrupted write) is truncated
/// away, with a warning, before the caller can append — otherwise the next
/// append would concatenate onto the corrupt bytes and be silently lost.
/// Mid-journal corruption is never skipped: it fails loudly with
/// `Error::Corruption` naming the line and `ndb repair`.
pub fn read_all(path: &Path) -> Result<Vec<Value>> {
    Ok(read_all_with_meta(path)?.1)
}

/// Read the `_meta` header and all journal lines.
///
/// The header is `None` when the file has none (empty file, or a legacy journal
/// whose first line is a document) — the caller then treats the journal as
/// pre-lineage. Same crash-recovery contract as `read_all`.
pub fn read_all_with_meta(path: &Path) -> Result<(Option<MetaInner>, Vec<Value>)> {
    let scan = scan_journal(path, true)?;

    if let Some(torn) = &scan.torn_tail {
        let lost = scan.total_bytes.saturating_sub(scan.last_good_offset);
        report::warning(
            "torn journal tail",
            format!(
                "{}: line {} (byte {}) is an incomplete write ({} byte(s) after the last good line); \
                 truncating to {} byte(s) so the next append cannot concatenate onto corrupt data",
                path.display(),
                torn.line,
                torn.start,
                lost,
                scan.last_good_offset
            ),
        );
        truncate_to(path, scan.last_good_offset)?;
    }

    Ok((scan.meta, scan.docs))
}

/// What `repair_journal` did, in the operator's terms.
#[derive(Debug, Clone)]
pub struct RepairReport {
    /// Complete records preserved (documents, excluding the `_meta` line).
    pub kept_records: usize,
    /// Byte length of the preserved prefix.
    pub kept_bytes: u64,
    /// File length before repair.
    pub total_bytes: u64,
    /// Bytes discarded.
    pub removed_bytes: u64,
    /// Lines discarded (the corrupt line plus everything after it).
    pub removed_lines: usize,
    /// 1-based number of the first discarded line, when anything was removed.
    pub removed_from_line: Option<usize>,
    /// Copy of the pre-repair file, when anything was removed.
    pub backup: Option<PathBuf>,
}

/// Quarantine a corrupt journal and truncate it at the last good line.
///
/// Copies the original to `<file>.corrupt.<timestamp>.bak` first, so nothing
/// is destroyed: the bytes that are being cut are exactly the bytes the
/// operator may still want to salvage by hand.
pub fn repair_journal(path: &Path) -> Result<RepairReport> {
    let scan = scan_journal(path, false)?;
    let first_bad = scan
        .mid_journal
        .clone()
        .or_else(|| scan.torn_tail.clone());
    let removed_bytes = scan.total_bytes.saturating_sub(scan.last_good_offset);
    let kept_lines = scan.kept_lines;

    let mut report = RepairReport {
        kept_records: scan.docs.len(),
        kept_bytes: scan.last_good_offset,
        total_bytes: scan.total_bytes,
        removed_bytes,
        removed_lines: scan.total_lines.saturating_sub(kept_lines),
        removed_from_line: first_bad.as_ref().map(|bad| bad.line),
        backup: None,
    };

    if first_bad.is_none() || removed_bytes == 0 {
        return Ok(report);
    }

    let backup = corrupt_backup_path(path);
    fs::copy(path, &backup).map_err(Error::io_err(path, "copy journal to backup"))?;
    {
        let file = OpenOptions::new()
            .write(true)
            .open(&backup)
            .map_err(Error::io_err(&backup, "open journal backup"))?;
        file.sync_all()
            .map_err(Error::io_err(&backup, "fsync journal backup"))?;
    }
    truncate_to(path, scan.last_good_offset)?;
    report.backup = Some(backup);
    Ok(report)
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let name = format!(
        "{}.corrupt.{}.{}.bak",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "data.jsonl".to_string()),
        stamp.as_secs(),
        stamp.subsec_millis()
    );
    path.with_file_name(name)
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
    sync_parent_dir(path)?;

    Ok(())
}

/// Append a single document to the persistent trash file.
/// Initializes the file with a meta header if it doesn't exist.
///
/// Fsynced before returning: the document's restorability depends on this
/// record, and `delete()` commits the tombstone after calling this — a power
/// loss between the two would otherwise leave a tombstone with no restorable
/// copy (C5).
pub fn append_doc_trash(trash_path: &Path, doc: &Value) -> Result<()> {
    if !trash_path.exists() {
        init_file(trash_path, &SourceLineage::fresh())?;
    }
    let mut file = open_for_append(trash_path)?;
    let line = serde_json::to_string(doc)?;
    append_line_sync(&mut file, trash_path, &line)?;
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
    fn read_all_truncates_a_torn_tail_and_keeps_earlier_records() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("truncated.jsonl");
        init_file(&path, &SourceLineage::fresh()).unwrap();

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", r#"{"_id":"ok","v":1}"#).unwrap();
        // Torn final write: JSON without its terminator.
        write!(file, "{}", r#"{"_id":"broken","v":2"#).unwrap();
        drop(file);

        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["_id"], "ok");

        // The corrupt bytes are gone, so the next append cannot be glued onto
        // them and lost on the following reopen.
        let mut file = open_for_append(&path).unwrap();
        append_line(&mut file, &path, r#"{"_id":"next","v":3}"#).unwrap();
        drop(file);
        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[1]["_id"], "next");
    }

    #[test]
    fn read_all_refuses_mid_journal_corruption() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hole.jsonl");
        init_file(&path, &SourceLineage::fresh()).unwrap();

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", r#"{"_id":"ok","v":1}"#).unwrap();
        writeln!(file, "{}", r#"{"_id":"broken","v":2"#).unwrap();
        writeln!(file, "{}", r#"{"_id":"also_ok","v":3}"#).unwrap();
        drop(file);

        let err = read_all(&path).unwrap_err();
        match err {
            Error::Corruption { message, .. } => {
                assert!(message.contains("line 3"), "must name the line: {message}");
                assert!(message.contains("ndb repair"), "must name the fix: {message}");
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
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
    fn read_all_refuses_a_completely_corrupted_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("garbage.jsonl");
        // No meta header, and the first line is invalid while more lines
        // follow: this is a corrupt journal, not a legacy one (M2).
        fs::write(&path, "not json at all\nalso not json\n").unwrap();

        assert!(matches!(read_all(&path), Err(Error::Corruption { .. })));
    }

    #[test]
    fn read_all_refuses_a_corrupt_meta_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("badmeta.jsonl");
        // Complete JSON, but not a header — silently degrading to legacy mode
        // would re-derive the journal epoch from nothing (M2).
        fs::write(&path, "{\"_meta\":5}\n{\"_id\":\"a\"}\n").unwrap();

        match read_all_with_meta(&path) {
            Err(Error::Corruption { message, .. }) => {
                assert!(message.contains("_meta"), "{message}");
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
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
        drop(file);

        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["_id"], "good");

        // The torn tail was removed from the file itself.
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("partial"), "torn bytes must be truncated");
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn repair_quarantines_a_corrupt_journal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("repair.jsonl");
        init_file(&path, &SourceLineage::fresh()).unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", r#"{"_id":"a","v":1}"#).unwrap();
        writeln!(file, "{}", r#"{"_id":"b","v":2}"#).unwrap();
        writeln!(file, "{}", r#"{"_id":"c","v":3"#).unwrap();
        writeln!(file, "{}", r#"{"_id":"d","v":4}"#).unwrap();
        drop(file);

        let report = repair_journal(&path).unwrap();
        assert_eq!(report.kept_records, 2);
        assert_eq!(report.removed_from_line, Some(4));
        assert!(report.removed_bytes > 0);
        let backup = report.backup.expect("repair keeps the original bytes");
        assert!(backup.exists());
        assert_eq!(
            fs::read_to_string(&backup).unwrap().lines().count(),
            5,
            "backup is byte-for-byte the pre-repair file"
        );

        // The repaired journal is openable and the next append survives.
        let docs = read_all(&path).unwrap();
        assert_eq!(docs.len(), 2);
        let mut file = open_for_append(&path).unwrap();
        append_line(&mut file, &path, r#"{"_id":"e","v":5}"#).unwrap();
        drop(file);
        assert_eq!(read_all(&path).unwrap().len(), 3);
    }

    #[test]
    fn repair_reports_nothing_to_do_on_a_clean_journal() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("clean.jsonl");
        init_file(&path, &SourceLineage::fresh()).unwrap();
        let mut file = open_for_append(&path).unwrap();
        append_line(&mut file, &path, r#"{"_id":"a"}"#).unwrap();
        drop(file);

        let report = repair_journal(&path).unwrap();
        assert_eq!(report.kept_records, 1);
        assert_eq!(report.removed_bytes, 0);
        assert!(report.backup.is_none());
    }

    /// Empirical check of the Windows rule `sync_dir` documents: a directory
    /// handle opened for write with `FILE_FLAG_BACKUP_SEMANTICS` can be
    /// flushed by a non-elevated process.
    #[cfg(windows)]
    #[test]
    fn windows_directory_fsync_is_honest() {
        let dir = TempDir::new().unwrap();
        sync_dir(dir.path()).expect("NTFS directory flush must succeed with write access");
    }

    #[test]
    fn sync_parent_dir_works_for_a_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("parent.jsonl");
        init_file(&path, &SourceLineage::fresh()).unwrap();
        sync_parent_dir(&path).unwrap();
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
