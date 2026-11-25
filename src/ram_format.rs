//! nDB RAM-first storage artefacts (format v3).
//!
//! Three files, one lineage, every byte CRC-32 protected:
//!
//! - `manifest.json` — the generation/lineage authority of a v3 database.
//! - `base.gen_<N>.ndb` — an immutable, *streamed* snapshot of the published
//!   document set at `base_seq`. Records are written straight out of
//!   [`DocVal::write_json`]; nothing re-parses JSON to produce the file.
//! - `wal.gen_<N>.ndb` — an append-only log of **operation-level** mutations
//!   grouped into batches that end in a `Commit` frame. The `Commit` is the
//!   durability boundary and the only thing recovery trusts.
//!
//! Two rules shape the module:
//!
//! 1. **Every disk mutation goes through [`Io`].** Writes, syncs, renames,
//!    removals, truncations and parent-directory fsyncs are all trait calls, so
//!    `FaultIo` can fail the exact step a crash would interrupt. Opening a
//!    handle for reading or appending is not a mutation and uses `std::fs`.
//! 2. **A torn tail is not corruption.** Bytes after the last intact `Commit`
//!    are an interrupted append: [`scan_ram_wal`] reports the offset and
//!    nothing else; [`truncate_ram_wal_tail`] clears it before any new append.
//!    Anything inconsistent *at or before* a `Commit` is
//!    [`Error::Corruption`], naming file, offset and sequence — never repaired
//!    implicitly, never silently dropped.

use crate::crc32::{crc32, Crc32Hasher};
use crate::docval::{DocRecord, DocVal};
use crate::error::{Error, Result};
use crate::io::Io;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// On-disk format version of every v3 artefact.
pub const RAM_STORAGE_VERSION: u32 = 3;

/// Name of the generation manifest inside a v3 database folder.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Base-file magic (`NDBBASE3`).
pub const BASE_MAGIC: [u8; 8] = *b"NDBBASE3";
/// Base-file footer magic (`NDBEND03`).
pub const BASE_END_MAGIC: [u8; 8] = *b"NDBEND03";
/// WAL-file magic (`NDBWAL03`).
pub const WAL_MAGIC: [u8; 8] = *b"NDBWAL03";

/// Header fields covered by the base header CRC (magic, version, base_seq, doc_count).
const BASE_HEADER_FIELDS_LEN: usize = 8 + 4 + 8 + 8;
/// Full base header: fields plus the CRC.
pub const BASE_HEADER_LEN: usize = BASE_HEADER_FIELDS_LEN + 4;
/// Base footer: magic, doc_count, whole-file CRC.
pub const BASE_FOOTER_LEN: usize = 8 + 8 + 4;
/// Per-document record prefix: id_len, doc_seq, json_len.
const BASE_RECORD_PREFIX_LEN: usize = 4 + 8 + 4;

/// WAL header fields covered by the header CRC (magic, version, wal_gen).
const WAL_HEADER_FIELDS_LEN: usize = 8 + 4 + 8;
/// Full WAL header: fields plus the CRC.
pub const WAL_HEADER_LEN: usize = WAL_HEADER_FIELDS_LEN + 4;

/// Tag of the `Commit` frame. Op frames use tags 1..=7.
pub const COMMIT_TAG: u8 = 8;
/// Frame body prefix: tag + seq.
const FRAME_BODY_FIXED: usize = 1 + 8;
/// Commit payload: start_seq, end_seq, frame_count, batch_crc.
const COMMIT_PAYLOAD_LEN: usize = 8 + 8 + 4 + 4;
/// Byte prefix of frames that a valid writer never emits a body shorter than.
const MIN_FRAME_BODY: usize = FRAME_BODY_FIXED;
/// Hard ceiling on a frame body, so a corrupt length can never request a
/// multi-gigabyte allocation. 4 MB chat documents fit with six orders of
/// magnitude to spare.
pub const MAX_FRAME_BODY: u32 = 512 * 1024 * 1024;

/// File name of a base generation.
pub fn base_file_name(generation: u64) -> String {
    format!("base.gen_{generation}.ndb")
}

/// File name of a WAL generation.
pub fn wal_file_name(generation: u64) -> String {
    format!("wal.gen_{generation}.ndb")
}

/// Full path of a base generation inside `dir`.
pub fn base_file_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(base_file_name(generation))
}

/// Full path of a WAL generation inside `dir`.
pub fn wal_file_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(wal_file_name(generation))
}

/// Full path of `manifest.json` inside `dir`.
pub fn ram_manifest_path(dir: &Path) -> PathBuf {
    dir.join(MANIFEST_FILE)
}

/// Full path of `manifest.json` inside `dir` (unprefixed alias).
pub fn manifest_path(dir: &Path) -> PathBuf {
    ram_manifest_path(dir)
}

/// One persisted secondary index declaration.
///
/// The JSON shape is fixed by `manifest.json`: `{"field": ..., "kind": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexSpec {
    /// Document field the index is built on.
    pub field: String,
    /// Index structure.
    pub kind: IndexKind,
}

/// The two index structures nDB persists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKind {
    /// O(1) equality lookups.
    #[serde(rename = "hash")]
    Hash,
    /// Ordered range scans.
    #[serde(rename = "btree")]
    BTree,
}

impl IndexKind {
    /// Binary tag used in the persisted index section.
    pub fn tag(self) -> u8 {
        match self {
            IndexKind::Hash => 1,
            IndexKind::BTree => 2,
        }
    }

    /// Parse the binary tag; `None` for an unknown value.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(IndexKind::Hash),
            2 => Some(IndexKind::BTree),
            _ => None,
        }
    }
}

// ─── Manifest ───────────────────────────────────────────────────────────────

/// The generation and lineage authority of a v3 database folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RamManifest {
    /// Must be [`RAM_STORAGE_VERSION`].
    pub version: u32,
    /// Canonical UUIDv4 of the journal's history.
    pub source_epoch_uuid: String,
    /// Active base generation (`base.gen_<N>.ndb`).
    pub base_gen: u64,
    /// Active WAL generation (`wal.gen_<N>.ndb`).
    pub wal_gen: u64,
    /// WAL generation a compaction has begun to write into; absent = none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_wal_gen: Option<u64>,
    /// Highest sequence committed anywhere in the lineage.
    pub last_seq: u64,
    /// Oldest sequence still replayable through the change feed.
    pub min_available_seq: u64,
    /// Persisted secondary index declarations.
    pub indexes: Vec<IndexSpec>,
}

/// Read and strictly validate `manifest.json` from `dir`.
///
/// Fails loud on a missing file, malformed JSON, an unknown field, any version
/// other than [`RAM_STORAGE_VERSION`], an empty epoch, a duplicate index,
/// `min_available_seq` beyond `last_seq + 1`, or `pending_wal_gen == wal_gen`
/// (a compaction never targets the generation already in service).
pub fn read_ram_manifest(dir: &Path) -> Result<RamManifest> {
    let path = ram_manifest_path(dir);
    let bytes = std::fs::read(&path).map_err(Error::io_err(&path, "read ram manifest"))?;
    let manifest: RamManifest = serde_json::from_slice(&bytes)
        .map_err(|e| Error::corruption(&path, format!("manifest is not valid v3 JSON: {e}")))?;
    validate_manifest(&manifest, &path)?;
    Ok(manifest)
}

/// Write `manifest.json` atomically: temp file, `sync_all`, rename, parent fsync.
///
/// A crash inside the sequence leaves either the old or the new manifest on
/// disk — never a mixture.
pub fn write_ram_manifest_atomic(dir: &Path, manifest: &RamManifest, io: &dyn Io) -> Result<()> {
    let path = ram_manifest_path(dir);
    validate_manifest(manifest, &path)?;
    let mut json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| Error::Serialization(format!("serialize ram manifest: {e}")))?;
    json.push(b'\n');
    write_file_atomic(&path, &json, io, "ram manifest")
}

fn validate_manifest(manifest: &RamManifest, path: &Path) -> Result<()> {
    if manifest.version != RAM_STORAGE_VERSION {
        return Err(Error::corruption(
            path,
            format!(
                "manifest version is {}, expected {RAM_STORAGE_VERSION}",
                manifest.version
            ),
        ));
    }
    if manifest.source_epoch_uuid.is_empty() {
        return Err(Error::corruption(
            path,
            "manifest source_epoch_uuid is empty",
        ));
    }
    if let Some(pending) = manifest.pending_wal_gen {
        if pending == manifest.wal_gen {
            return Err(Error::corruption(
                path,
                format!(
                    "manifest pending_wal_gen {pending} equals the active wal_gen; a compaction never targets the live generation"
                ),
            ));
        }
    }
    if manifest.min_available_seq > manifest.last_seq + 1 {
        return Err(Error::corruption(
            path,
            format!(
                "manifest min_available_seq {} is beyond last_seq {} + 1: history would have a hole",
                manifest.min_available_seq, manifest.last_seq
            ),
        ));
    }
    let mut fields: BTreeSet<&str> = BTreeSet::new();
    for spec in &manifest.indexes {
        if spec.field.is_empty() {
            return Err(Error::corruption(
                path,
                "manifest declares an index with an empty field name",
            ));
        }
        if !fields.insert(spec.field.as_str()) {
            return Err(Error::corruption(
                path,
                format!("manifest declares index {:?} twice", spec.field),
            ));
        }
    }
    Ok(())
}

// ─── Base file ──────────────────────────────────────────────────────────────

/// Result of [`read_ram_base_file`].
#[derive(Debug)]
pub struct RamBaseLoad {
    /// Highest sequence folded into this base.
    pub base_seq: u64,
    /// Documents in the file, validated against the header and footer.
    pub doc_count: u64,
    /// Published records, in the file's (id-sorted) order.
    pub docs: Vec<Arc<DocRecord>>,
    /// Bytes read from disk.
    pub bytes_read: u64,
}

/// Stream a base generation from `docs`, atomically publishing it at `path`.
///
/// The file is written to a temporary sibling, fsynced, renamed and the parent
/// directory fsynced, so a crash leaves either the previous file or the
/// complete new one — never a partially-written base under the final name.
/// Returns the document count written.
///
/// Records are appended in id order (deterministic bytes for the same set) and
/// the JSON comes directly from [`DocVal::write_json`] — no re-parse.
pub fn write_ram_base_file(
    path: &Path,
    base_seq: u64,
    docs: &[Arc<DocRecord>],
    io: &dyn Io,
) -> Result<u64> {
    let mut ordered: Vec<&Arc<DocRecord>> = docs.iter().collect();
    ordered.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen: HashSet<&str> = HashSet::with_capacity(ordered.len());
    for doc in &ordered {
        if doc.id.is_empty() {
            return Err(Error::invalid_arg(
                "base file documents must have a non-empty id",
            ));
        }
        if doc.doc_seq > base_seq {
            return Err(Error::invalid_arg(format!(
                "document {:?} is at seq {} but the base sequence is {base_seq}: a base cannot hold a newer document",
                doc.id, doc.doc_seq
            )));
        }
        if !seen.insert(doc.id.as_ref()) {
            return Err(Error::invalid_arg(format!(
                "base file would contain document {:?} twice",
                doc.id
            )));
        }
    }
    let doc_count = u64::try_from(ordered.len())
        .map_err(|_| Error::invalid_arg("base file document count exceeds u64"))?;

    let tmp = temp_sibling(path, "tmp")?;
    let mut file = io
        .create_new(&tmp)
        .map_err(Error::io_err(&tmp, "create temporary ram base file"))?;

    let mut file_crc = Crc32Hasher::new();
    let header = encode_base_header(base_seq, doc_count);
    io.write_all(&mut file, &header)
        .map_err(Error::io_err(&tmp, "write ram base header"))?;
    file_crc.update(&header);

    let mut record = Vec::new();
    for doc in ordered {
        let id_len = u32::try_from(doc.id.len())
            .map_err(|_| Error::invalid_arg("document id exceeds u32 bytes"))?;
        record.clear();
        record.reserve(BASE_RECORD_PREFIX_LEN + doc.id.len() + 64);
        record.extend_from_slice(&[0u8; BASE_RECORD_PREFIX_LEN]);
        record.extend_from_slice(doc.id.as_bytes());
        let json_start = record.len();
        doc.doc.write_json(&mut record);
        let json_len = u32::try_from(record.len() - json_start)
            .map_err(|_| Error::invalid_arg("document JSON exceeds u32 bytes"))?;
        record[0..4].copy_from_slice(&id_len.to_le_bytes());
        record[4..12].copy_from_slice(&doc.doc_seq.to_le_bytes());
        record[12..16].copy_from_slice(&json_len.to_le_bytes());
        let record_crc = crc32(&record);
        file_crc.update(&record);
        record.extend_from_slice(&record_crc.to_le_bytes());
        io.write_all(&mut file, &record)
            .map_err(Error::io_err(&tmp, "write ram base record"))?;
    }

    let mut footer = Vec::with_capacity(BASE_FOOTER_LEN);
    footer.extend_from_slice(&BASE_END_MAGIC);
    footer.extend_from_slice(&doc_count.to_le_bytes());
    footer.extend_from_slice(&file_crc.finalize().to_le_bytes());
    io.write_all(&mut file, &footer)
        .map_err(Error::io_err(&tmp, "write ram base footer"))?;
    io.sync_all(&file)
        .map_err(Error::io_err(&tmp, "sync ram base file"))?;
    drop(file);
    io.rename(&tmp, path)
        .map_err(Error::io_err(path, "publish ram base file"))?;
    io.sync_parent_dir(path)
        .map_err(Error::io_err(path, "sync directory after ram base publish"))?;
    Ok(doc_count)
}

/// Read, fully verify and materialise a base generation.
///
/// Every CRC (header, per record, whole file), both magics, the version, the
/// doc count, each record's ordering and the absence of trailing bytes are
/// checked before the file is accepted. A truncated, reordered or bit-flipped
/// base is [`Error::Corruption`] naming file, offset and record index.
pub fn read_ram_base_file(path: &Path) -> Result<RamBaseLoad> {
    let file = File::open(path).map_err(Error::io_err(path, "open ram base file"))?;
    let total_bytes = file
        .metadata()
        .map_err(Error::io_err(path, "stat ram base file"))?
        .len();
    let minimum = (BASE_HEADER_LEN + BASE_FOOTER_LEN) as u64;
    if total_bytes < minimum {
        return Err(Error::corruption(
            path,
            format!("base file is {total_bytes} bytes, shorter than the {minimum}-byte header plus footer"),
        ));
    }
    let mut reader = BufReader::new(file);

    let mut header = [0u8; BASE_HEADER_LEN];
    read_exact(&mut reader, &mut header, path, 0, "base header")?;
    if header[0..8] != BASE_MAGIC {
        return Err(Error::corruption(path, "base header magic is not NDBBASE3"));
    }
    let version = u32::from_le_bytes(header[8..12].try_into().expect("4-byte window"));
    if version != RAM_STORAGE_VERSION {
        return Err(Error::corruption(
            path,
            format!("base header version is {version}, expected {RAM_STORAGE_VERSION}"),
        ));
    }
    let stored_header_crc = u32::from_le_bytes(header[28..32].try_into().expect("4-byte window"));
    let actual_header_crc = crc32(&header[..BASE_HEADER_FIELDS_LEN]);
    if stored_header_crc != actual_header_crc {
        return Err(Error::corruption(
            path,
            format!(
                "base header CRC mismatch at byte 0: stored {stored_header_crc:#010x}, computed {actual_header_crc:#010x}"
            ),
        ));
    }
    let base_seq = u64::from_le_bytes(header[12..20].try_into().expect("8-byte window"));
    let doc_count = u64::from_le_bytes(header[20..28].try_into().expect("8-byte window"));

    let mut file_crc = Crc32Hasher::new();
    file_crc.update(&header);
    let mut offset = BASE_HEADER_LEN as u64;
    let mut docs: Vec<Arc<DocRecord>> = Vec::new();
    let mut ids: HashSet<Arc<str>> = HashSet::new();

    for index in 0..doc_count {
        let mut prefix = [0u8; BASE_RECORD_PREFIX_LEN];
        read_exact(&mut reader, &mut prefix, path, offset, "base record prefix")?;
        offset += BASE_RECORD_PREFIX_LEN as u64;
        let id_len = u32::from_le_bytes(prefix[0..4].try_into().expect("4-byte window")) as usize;
        let doc_seq = u64::from_le_bytes(prefix[4..12].try_into().expect("8-byte window"));
        let json_len =
            u32::from_le_bytes(prefix[12..16].try_into().expect("4-byte window")) as usize;
        if id_len == 0 || json_len == 0 {
            return Err(Error::corruption(
                path,
                format!(
                    "base record {index} at byte {offset} declares id_len {id_len} / json_len {json_len}"
                ),
            ));
        }
        // A corrupt length must never become an allocation: both fields must
        // fit in the bytes the file still has (plus its CRC and the footer).
        let still_needed = id_len as u64 + json_len as u64 + 4 + BASE_FOOTER_LEN as u64;
        if still_needed > total_bytes - offset {
            return Err(Error::corruption(
                path,
                format!(
                    "base record {index} at byte {offset} claims {id_len}+{json_len} payload bytes, so it is truncated: only {} remain",
                    total_bytes - offset
                ),
            ));
        }
        let mut id_buf = vec![0u8; id_len];
        read_exact(&mut reader, &mut id_buf, path, offset, "base record id")?;
        offset += id_len as u64;
        let mut json_buf = vec![0u8; json_len];
        read_exact(&mut reader, &mut json_buf, path, offset, "base record json")?;
        offset += json_len as u64;

        let mut record_crc = Crc32Hasher::new();
        record_crc.update(&prefix);
        record_crc.update(&id_buf);
        record_crc.update(&json_buf);
        let mut crc_buf = [0u8; 4];
        read_exact(&mut reader, &mut crc_buf, path, offset, "base record CRC")?;
        let stored_record_crc = u32::from_le_bytes(crc_buf);
        let actual_record_crc = record_crc.finalize();
        if stored_record_crc != actual_record_crc {
            return Err(Error::corruption(
                path,
                format!(
                    "base record {index} (doc_seq {doc_seq}) CRC mismatch at byte {offset}: stored {stored_record_crc:#010x}, computed {actual_record_crc:#010x}"
                ),
            ));
        }
        offset += 4;

        let id = std::str::from_utf8(&id_buf).map_err(|e| {
            Error::corruption(
                path,
                format!("base record {index} at byte {offset} has a non-UTF-8 id: {e}"),
            )
        })?;
        if doc_seq > base_seq {
            return Err(Error::corruption(
                path,
                format!(
                    "base record {index} ({id:?}) is at seq {doc_seq}, above the base sequence {base_seq}"
                ),
            ));
        }
        let value: Value = serde_json::from_slice(&json_buf).map_err(|e| {
            Error::corruption(
                path,
                format!("base record {index} ({id:?}) at byte {offset} is not valid JSON: {e}"),
            )
        })?;
        let id: Arc<str> = Arc::from(id);
        if !ids.insert(Arc::clone(&id)) {
            return Err(Error::corruption(
                path,
                format!("base record {index} repeats document id {id:?}"),
            ));
        }
        file_crc.update(&prefix);
        file_crc.update(&id_buf);
        file_crc.update(&json_buf);
        docs.push(DocRecord::new(id, doc_seq, DocVal::from_value(&value)));
    }

    let mut footer = [0u8; BASE_FOOTER_LEN];
    read_exact(&mut reader, &mut footer, path, offset, "base footer")?;
    if footer[0..8] != BASE_END_MAGIC {
        return Err(Error::corruption(
            path,
            format!("base footer magic is not NDBEND03 at byte {offset}"),
        ));
    }
    let footer_count = u64::from_le_bytes(footer[8..16].try_into().expect("8-byte window"));
    if footer_count != doc_count {
        return Err(Error::corruption(
            path,
            format!("base footer declares {footer_count} documents, the header {doc_count}"),
        ));
    }
    let stored_file_crc = u32::from_le_bytes(footer[16..20].try_into().expect("4-byte window"));
    let actual_file_crc = file_crc.finalize();
    if stored_file_crc != actual_file_crc {
        return Err(Error::corruption(
            path,
            format!(
                "base file CRC mismatch at byte {}: stored {stored_file_crc:#010x}, computed {actual_file_crc:#010x}",
                offset + 16
            ),
        ));
    }
    offset += BASE_FOOTER_LEN as u64;
    if offset != total_bytes {
        return Err(Error::corruption(
            path,
            format!(
                "base file has {} trailing bytes after the footer at byte {offset}",
                total_bytes - offset
            ),
        ));
    }

    Ok(RamBaseLoad {
        base_seq,
        doc_count,
        docs,
        bytes_read: total_bytes,
    })
}

fn encode_base_header(base_seq: u64, doc_count: u64) -> Vec<u8> {
    let mut header = Vec::with_capacity(BASE_HEADER_LEN);
    header.extend_from_slice(&BASE_MAGIC);
    header.extend_from_slice(&RAM_STORAGE_VERSION.to_le_bytes());
    header.extend_from_slice(&base_seq.to_le_bytes());
    header.extend_from_slice(&doc_count.to_le_bytes());
    let crc = crc32(&header);
    header.extend_from_slice(&crc.to_le_bytes());
    header
}

// ─── WAL operations ─────────────────────────────────────────────────────────

/// One field mutation inside a [`RamWalOp::Patch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RamWalPatchOp {
    /// Set `path` to a JSON value.
    Set {
        /// Dotted field path.
        path: String,
        /// Serialized JSON value.
        value_json: Vec<u8>,
    },
    /// Remove `path`.
    Remove {
        /// Dotted field path.
        path: String,
    },
    /// Append a JSON value to the array at `path`.
    ArrayPush {
        /// Dotted field path.
        path: String,
        /// Serialized JSON value.
        value_json: Vec<u8>,
    },
}

impl RamWalPatchOp {
    /// Construct a `Set`.
    pub fn set(path: impl Into<String>, value_json: impl Into<Vec<u8>>) -> Self {
        RamWalPatchOp::Set {
            path: path.into(),
            value_json: value_json.into(),
        }
    }

    /// Construct a `Remove`.
    pub fn remove(path: impl Into<String>) -> Self {
        RamWalPatchOp::Remove { path: path.into() }
    }

    /// Construct an `ArrayPush`.
    pub fn array_push(path: impl Into<String>, value_json: impl Into<Vec<u8>>) -> Self {
        RamWalPatchOp::ArrayPush {
            path: path.into(),
            value_json: value_json.into(),
        }
    }

    fn tag(&self) -> u8 {
        match self {
            RamWalPatchOp::Set { .. } => 1,
            RamWalPatchOp::Remove { .. } => 2,
            RamWalPatchOp::ArrayPush { .. } => 3,
        }
    }

    /// Encoded size in bytes of this op inside a patch payload.
    fn payload_bytes(&self) -> usize {
        match self {
            RamWalPatchOp::Remove { path } => 1 + 4 + path.len(),
            RamWalPatchOp::Set { path, value_json }
            | RamWalPatchOp::ArrayPush { path, value_json } => {
                1 + 4 + path.len() + 4 + value_json.len()
            }
        }
    }
}

/// One durable mutation in a v3 WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RamWalOp {
    /// Insert a document.
    Insert {
        /// Document id.
        id: String,
        /// Serialized JSON document.
        doc_json: Vec<u8>,
    },
    /// Replace a document's body.
    Replace {
        /// Document id.
        id: String,
        /// Serialized JSON document.
        doc_json: Vec<u8>,
    },
    /// Apply field mutations to a document.
    Patch {
        /// Document id.
        id: String,
        /// Ordered field operations.
        ops: Vec<RamWalPatchOp>,
    },
    /// Delete a document.
    Delete {
        /// Document id.
        id: String,
    },
    /// Restore a deleted document.
    Restore {
        /// Document id.
        id: String,
        /// Serialized JSON document.
        doc_json: Vec<u8>,
    },
    /// Create a secondary index. Consumes **no** document sequence.
    IndexAdd {
        /// Indexed field.
        field: String,
        /// Index structure.
        kind: IndexKind,
    },
    /// Drop a secondary index. Consumes **no** document sequence.
    IndexDrop {
        /// Indexed field.
        field: String,
    },
}

impl RamWalOp {
    fn tag(&self) -> u8 {
        match self {
            RamWalOp::Insert { .. } => 1,
            RamWalOp::Replace { .. } => 2,
            RamWalOp::Patch { .. } => 3,
            RamWalOp::Delete { .. } => 4,
            RamWalOp::Restore { .. } => 5,
            RamWalOp::IndexAdd { .. } => 6,
            RamWalOp::IndexDrop { .. } => 7,
        }
    }

    /// Whether this operation consumes a document sequence number.
    pub fn consumes_seq(&self) -> bool {
        !matches!(self, RamWalOp::IndexAdd { .. } | RamWalOp::IndexDrop { .. })
    }
}

/// One framed record: a mutation plus the sequence it was committed at.
///
/// Schema operations ([`RamWalOp::IndexAdd`] / [`RamWalOp::IndexDrop`]) carry
/// `seq == 0` — they are deliberately outside the document sequence so a feed
/// consumer's watermark never parks in front of an event that does not exist
/// (herrbasan/nDB#17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamWalRecord {
    /// Sequence of the batch slot this record occupies; `0` for schema ops.
    pub seq: u64,
    /// The mutation.
    pub op: RamWalOp,
}

impl RamWalRecord {
    /// An `Insert` at `seq`.
    pub fn insert(seq: u64, id: impl Into<String>, doc_json: impl Into<Vec<u8>>) -> Self {
        RamWalRecord {
            seq,
            op: RamWalOp::Insert {
                id: id.into(),
                doc_json: doc_json.into(),
            },
        }
    }

    /// A `Replace` at `seq`.
    pub fn replace(seq: u64, id: impl Into<String>, doc_json: impl Into<Vec<u8>>) -> Self {
        RamWalRecord {
            seq,
            op: RamWalOp::Replace {
                id: id.into(),
                doc_json: doc_json.into(),
            },
        }
    }

    /// A `Patch` at `seq`.
    pub fn patch(seq: u64, id: impl Into<String>, ops: Vec<RamWalPatchOp>) -> Self {
        RamWalRecord {
            seq,
            op: RamWalOp::Patch { id: id.into(), ops },
        }
    }

    /// A `Delete` at `seq`.
    pub fn delete(seq: u64, id: impl Into<String>) -> Self {
        RamWalRecord {
            seq,
            op: RamWalOp::Delete { id: id.into() },
        }
    }

    /// A `Restore` at `seq`.
    pub fn restore(seq: u64, id: impl Into<String>, doc_json: impl Into<Vec<u8>>) -> Self {
        RamWalRecord {
            seq,
            op: RamWalOp::Restore {
                id: id.into(),
                doc_json: doc_json.into(),
            },
        }
    }

    /// An `IndexAdd` (no document sequence).
    pub fn index_add(field: impl Into<String>, kind: IndexKind) -> Self {
        RamWalRecord {
            seq: 0,
            op: RamWalOp::IndexAdd {
                field: field.into(),
                kind,
            },
        }
    }

    /// An `IndexDrop` (no document sequence).
    pub fn index_drop(field: impl Into<String>) -> Self {
        RamWalRecord {
            seq: 0,
            op: RamWalOp::IndexDrop {
                field: field.into(),
            },
        }
    }

    /// Encoded size of this record's op payload in bytes — the number of bytes
    /// [`encode_op_payload`] writes for it.
    ///
    /// RAM accounting charges a patched change-ring entry for the bytes it can
    /// additionally retain, so it needs this size without encoding the payload.
    pub fn payload_bytes(&self) -> usize {
        match &self.op {
            RamWalOp::Insert { id, doc_json }
            | RamWalOp::Replace { id, doc_json }
            | RamWalOp::Restore { id, doc_json } => 4 + id.len() + doc_json.len(),
            RamWalOp::Delete { id } => 4 + id.len(),
            RamWalOp::Patch { id, ops } => {
                4 + id.len() + 4 + ops.iter().map(|op| op.payload_bytes()).sum::<usize>()
            }
            RamWalOp::IndexAdd { field, .. } => 4 + field.len() + 1,
            RamWalOp::IndexDrop { field } => 4 + field.len(),
        }
    }
}

/// One committed batch: every op in it became durable at the same fsync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamWalBatch {
    /// First document sequence in the batch (`0` for a schema-only batch).
    pub start_seq: u64,
    /// Last document sequence in the batch (`0` for a schema-only batch).
    pub end_seq: u64,
    /// Op frames in source order.
    pub records: Vec<RamWalRecord>,
    /// Byte offset just past this batch's `Commit` frame.
    pub end_offset: u64,
}

/// Read-only result of scanning a WAL generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamWalScan {
    /// Generation number from the header.
    pub wal_gen: u64,
    /// Bytes in the file.
    pub total_bytes: u64,
    /// Header length (the first append begins here).
    pub header_len: u64,
    /// Committed batches in order.
    pub batches: Vec<RamWalBatch>,
    /// Highest committed document sequence (`0` when no batch consumed one).
    pub last_seq: u64,
    /// Offset just past the last intact `Commit` frame.
    pub last_commit_end: u64,
    /// When set, everything from this offset on is an interrupted append.
    pub torn_tail_offset: Option<u64>,
}

// ─── WAL frames ─────────────────────────────────────────────────────────────

fn encode_frame(tag: u8, seq: u64, payload: &[u8]) -> Result<Vec<u8>> {
    let body_len = FRAME_BODY_FIXED
        .checked_add(payload.len())
        .ok_or_else(|| Error::invalid_arg("WAL frame body length overflows usize"))?;
    let body_len = u32::try_from(body_len)
        .map_err(|_| Error::invalid_arg("WAL frame body exceeds u32 bytes"))?;
    let mut frame = Vec::with_capacity(4 + body_len as usize + 4);
    frame.extend_from_slice(&body_len.to_le_bytes());
    frame.push(tag);
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.extend_from_slice(payload);
    let crc = crc32(&frame[4..]);
    frame.extend_from_slice(&crc.to_le_bytes());
    Ok(frame)
}

fn encode_op_frame(record: &RamWalRecord) -> Result<Vec<u8>> {
    let payload = encode_op_payload(&record.op)?;
    encode_frame(record.op.tag(), record.seq, &payload)
}

fn encode_op_payload(op: &RamWalOp) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match op {
        RamWalOp::Insert { id, doc_json }
        | RamWalOp::Replace { id, doc_json }
        | RamWalOp::Restore { id, doc_json } => {
            put_id(&mut out, id)?;
            if doc_json.is_empty() {
                return Err(Error::invalid_arg(format!(
                    "{:?} for document {id:?} carries an empty JSON body",
                    op.tag()
                )));
            }
            out.extend_from_slice(doc_json);
        }
        RamWalOp::Delete { id } => put_id(&mut out, id)?,
        RamWalOp::Patch { id, ops } => {
            put_id(&mut out, id)?;
            if ops.is_empty() {
                return Err(Error::invalid_arg(format!(
                    "patch for document {id:?} has no operations; a no-op is not a commit"
                )));
            }
            let count = u32::try_from(ops.len())
                .map_err(|_| Error::invalid_arg("patch op count exceeds u32"))?;
            out.extend_from_slice(&count.to_le_bytes());
            for patch in ops {
                out.push(patch.tag());
                match patch {
                    RamWalPatchOp::Remove { path } => put_str(&mut out, path, "patch path")?,
                    RamWalPatchOp::Set { path, value_json }
                    | RamWalPatchOp::ArrayPush { path, value_json } => {
                        put_str(&mut out, path, "patch path")?;
                        if value_json.is_empty() {
                            return Err(Error::invalid_arg(format!(
                                "patch for document {id:?} sets {path:?} to an empty JSON body"
                            )));
                        }
                        let len = u32::try_from(value_json.len()).map_err(|_| {
                            Error::invalid_arg("patch value JSON exceeds u32 bytes")
                        })?;
                        out.extend_from_slice(&len.to_le_bytes());
                        out.extend_from_slice(value_json);
                    }
                }
            }
        }
        RamWalOp::IndexAdd { field, kind } => {
            put_str(&mut out, field, "index field")?;
            out.push(kind.tag());
        }
        RamWalOp::IndexDrop { field } => put_str(&mut out, field, "index field")?,
    }
    Ok(out)
}

fn encode_commit_frame(start_seq: u64, end_seq: u64, frame_count: u32, batch_crc: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(COMMIT_PAYLOAD_LEN);
    payload.extend_from_slice(&start_seq.to_le_bytes());
    payload.extend_from_slice(&end_seq.to_le_bytes());
    payload.extend_from_slice(&frame_count.to_le_bytes());
    payload.extend_from_slice(&batch_crc.to_le_bytes());
    encode_frame(COMMIT_TAG, end_seq, &payload).expect("commit payload fits u32")
}

fn put_id(out: &mut Vec<u8>, id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(Error::invalid_arg("WAL record has an empty document id"));
    }
    let len =
        u32::try_from(id.len()).map_err(|_| Error::invalid_arg("document id exceeds u32 bytes"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(id.as_bytes());
    Ok(())
}

fn put_str(out: &mut Vec<u8>, text: &str, what: &str) -> Result<()> {
    if text.is_empty() {
        return Err(Error::invalid_arg(format!(
            "WAL record has an empty {what}"
        )));
    }
    let len = u32::try_from(text.len())
        .map_err(|_| Error::invalid_arg(format!("{what} exceeds u32 bytes")))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

fn decode_wal_header(header: &[u8; WAL_HEADER_LEN], path: &Path) -> Result<u64> {
    if header[0..8] != WAL_MAGIC {
        return Err(Error::corruption(path, "WAL header magic is not NDBWAL03"));
    }
    let version = u32::from_le_bytes(header[8..12].try_into().expect("4-byte window"));
    if version != RAM_STORAGE_VERSION {
        return Err(Error::corruption(
            path,
            format!("WAL header version is {version}, expected {RAM_STORAGE_VERSION}"),
        ));
    }
    let stored_crc = u32::from_le_bytes(header[20..24].try_into().expect("4-byte window"));
    let actual_crc = crc32(&header[..WAL_HEADER_FIELDS_LEN]);
    if stored_crc != actual_crc {
        return Err(Error::corruption(
            path,
            format!(
                "WAL header CRC mismatch at byte 0: stored {stored_crc:#010x}, computed {actual_crc:#010x}"
            ),
        ));
    }
    Ok(u64::from_le_bytes(
        header[12..20].try_into().expect("8-byte window"),
    ))
}

fn encode_wal_header(wal_gen: u64) -> Vec<u8> {
    let mut header = Vec::with_capacity(WAL_HEADER_LEN);
    header.extend_from_slice(&WAL_MAGIC);
    header.extend_from_slice(&RAM_STORAGE_VERSION.to_le_bytes());
    header.extend_from_slice(&wal_gen.to_le_bytes());
    let crc = crc32(&header);
    header.extend_from_slice(&crc.to_le_bytes());
    header
}

/// Bounds-checked reader over one frame payload.
struct PayloadReader<'a> {
    buf: &'a [u8],
    at: usize,
    path: &'a Path,
    frame_offset: u64,
}

impl<'a> PayloadReader<'a> {
    fn new(buf: &'a [u8], path: &'a Path, frame_offset: u64) -> Self {
        PayloadReader {
            buf,
            at: 0,
            path,
            frame_offset,
        }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.at
    }

    fn fail(&self, what: &str) -> Error {
        Error::corruption(
            self.path,
            format!(
                "WAL frame at byte {}: {what} (payload has {} bytes left at offset {})",
                self.frame_offset,
                self.remaining(),
                self.at
            ),
        )
    }

    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8]> {
        if self.remaining() < len {
            return Err(self.fail(&format!("truncated {what}")));
        }
        let slice = &self.buf[self.at..self.at + len];
        self.at += len;
        Ok(slice)
    }

    fn u8(&mut self, what: &str) -> Result<u8> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4, what)?.try_into().expect("4-byte window"),
        ))
    }

    fn u64(&mut self, what: &str) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8, what)?.try_into().expect("8-byte window"),
        ))
    }

    fn string(&mut self, what: &str) -> Result<String> {
        let len = self.u32(&format!("{what} length"))? as usize;
        if len == 0 {
            return Err(self.fail(&format!("empty {what}")));
        }
        let bytes = self.take(len, what)?;
        std::str::from_utf8(bytes)
            .map(|text| text.to_string())
            .map_err(|e| self.fail(&format!("{what} is not UTF-8: {e}")))
    }

    fn finish(&self, what: &str) -> Result<()> {
        if self.remaining() != 0 {
            return Err(self.fail(&format!("{what} has trailing bytes")));
        }
        Ok(())
    }
}

fn decode_op(tag: u8, payload: &[u8], path: &Path, frame_offset: u64) -> Result<RamWalOp> {
    let mut reader = PayloadReader::new(payload, path, frame_offset);
    let op = match tag {
        1 | 2 | 5 => {
            let id = reader.string("document id")?;
            let doc_json = reader.take(reader.remaining(), "document JSON")?.to_vec();
            if doc_json.is_empty() {
                return Err(reader.fail("empty document JSON"));
            }
            match tag {
                1 => RamWalOp::Insert { id, doc_json },
                2 => RamWalOp::Replace { id, doc_json },
                _ => RamWalOp::Restore { id, doc_json },
            }
        }
        3 => {
            let id = reader.string("document id")?;
            let count = reader.u32("patch op count")? as usize;
            if count == 0 {
                return Err(reader.fail("patch with no operations"));
            }
            let mut ops = Vec::new();
            for _ in 0..count {
                let op_tag = reader.u8("patch op tag")?;
                let path_field = reader.string("patch path")?;
                let op = match op_tag {
                    1 | 3 => {
                        let len = reader.u32("patch value length")? as usize;
                        if len == 0 {
                            return Err(reader.fail("patch value is empty"));
                        }
                        let value_json = reader.take(len, "patch value JSON")?.to_vec();
                        if op_tag == 1 {
                            RamWalPatchOp::Set {
                                path: path_field,
                                value_json,
                            }
                        } else {
                            RamWalPatchOp::ArrayPush {
                                path: path_field,
                                value_json,
                            }
                        }
                    }
                    2 => RamWalPatchOp::Remove { path: path_field },
                    other => {
                        return Err(reader.fail(&format!("unknown patch op tag {other}")));
                    }
                };
                ops.push(op);
            }
            RamWalOp::Patch { id, ops }
        }
        4 => RamWalOp::Delete {
            id: reader.string("document id")?,
        },
        6 => {
            let field = reader.string("index field")?;
            let kind_tag = reader.u8("index kind")?;
            let kind = IndexKind::from_tag(kind_tag)
                .ok_or_else(|| reader.fail(&format!("unknown index kind tag {kind_tag}")))?;
            RamWalOp::IndexAdd { field, kind }
        }
        7 => RamWalOp::IndexDrop {
            field: reader.string("index field")?,
        },
        other => return Err(reader.fail(&format!("unknown WAL op tag {other}"))),
    };
    reader.finish("WAL frame payload")?;
    Ok(op)
}

fn decode_commit(payload: &[u8], path: &Path, frame_offset: u64) -> Result<(u64, u64, u32, u32)> {
    if payload.len() != COMMIT_PAYLOAD_LEN {
        return Err(Error::corruption(
            path,
            format!(
                "Commit frame at byte {frame_offset} carries {} payload bytes, expected {COMMIT_PAYLOAD_LEN}",
                payload.len()
            ),
        ));
    }
    let mut reader = PayloadReader::new(payload, path, frame_offset);
    let start_seq = reader.u64("commit start_seq")?;
    let end_seq = reader.u64("commit end_seq")?;
    let frame_count = reader.u32("commit frame_count")?;
    let batch_crc = reader.u32("commit batch_crc")?;
    Ok((start_seq, end_seq, frame_count, batch_crc))
}

// ─── WAL scan (read-only) ───────────────────────────────────────────────────

struct RamWalScanCore {
    scan: RamWalScan,
    corruption: Option<Error>,
}

/// Scan a WAL generation, strictly read-only.
///
/// Committed batches land in [`RamWalScan::batches`]. A trailing incomplete
/// batch — op frames with no `Commit`, a frame cut short at EOF, or a final
/// frame whose CRC does not match — sets
/// [`RamWalScan::torn_tail_offset`] and is **never** treated as data.
///
/// Corruption is anything that contradicts an already-present `Commit` or the
/// header: a bad CRC before the end of the file, a `Commit` whose `frame_count`
/// or `batch_crc` disagrees with the frames before it, a malformed payload, a
/// non-contiguous sequence, or a truncated header. Those return
/// [`Error::Corruption`] naming the file, byte offset and sequence.
pub fn scan_ram_wal(path: &Path) -> Result<RamWalScan> {
    let core = scan_ram_wal_core(path)?;
    match core.corruption {
        Some(error) => Err(error),
        None => Ok(core.scan),
    }
}

fn scan_ram_wal_core(path: &Path) -> Result<RamWalScanCore> {
    let file = File::open(path).map_err(Error::io_err(path, "open ram WAL"))?;
    let total_bytes = file
        .metadata()
        .map_err(Error::io_err(path, "stat ram WAL"))?
        .len();
    if total_bytes < WAL_HEADER_LEN as u64 {
        return Err(Error::corruption(
            path,
            format!("WAL header is truncated: {total_bytes} bytes, need {WAL_HEADER_LEN}"),
        ));
    }
    let mut reader = BufReader::new(file);
    let mut header = [0u8; WAL_HEADER_LEN];
    read_exact(&mut reader, &mut header, path, 0, "WAL header")?;
    let wal_gen = decode_wal_header(&header, path)?;

    let mut offset = WAL_HEADER_LEN as u64;
    let mut batches: Vec<RamWalBatch> = Vec::new();
    let mut pending: Vec<RamWalRecord> = Vec::new();
    let mut pending_bytes: Vec<u8> = Vec::new();
    let mut last_commit_end = offset;
    let mut last_seq = 0u64;
    let mut torn_tail_offset: Option<u64> = None;
    let mut corruption: Option<Error> = None;

    while offset < total_bytes {
        let remaining = total_bytes - offset;
        if remaining < 4 {
            torn_tail_offset = Some(last_commit_end);
            break;
        }
        let mut length_buf = [0u8; 4];
        read_exact(
            &mut reader,
            &mut length_buf,
            path,
            offset,
            "WAL frame length",
        )?;
        let body_len = u32::from_le_bytes(length_buf) as u64;
        if body_len < MIN_FRAME_BODY as u64 {
            corruption = Some(Error::corruption(
                path,
                format!(
                    "WAL frame at byte {offset} declares a {body_len}-byte body, below the {MIN_FRAME_BODY}-byte minimum"
                ),
            ));
            break;
        }
        if body_len > u64::from(MAX_FRAME_BODY) {
            corruption = Some(Error::corruption(
                path,
                format!(
                    "WAL frame at byte {offset} declares a {body_len}-byte body, above the {MAX_FRAME_BODY}-byte limit"
                ),
            ));
            break;
        }
        let frame_total = 4 + body_len + 4;
        if remaining < frame_total {
            torn_tail_offset = Some(last_commit_end);
            break;
        }
        offset += 4;
        let mut body = vec![0u8; body_len as usize];
        read_exact(&mut reader, &mut body, path, offset, "WAL frame body")?;
        let body_offset = offset;
        offset += body_len;
        let mut crc_buf = [0u8; 4];
        read_exact(&mut reader, &mut crc_buf, path, offset, "WAL frame CRC")?;
        offset += 4;
        let stored_crc = u32::from_le_bytes(crc_buf);
        let actual_crc = crc32(&body);
        let seq = if body.len() >= FRAME_BODY_FIXED {
            u64::from_le_bytes(body[1..9].try_into().expect("8-byte window"))
        } else {
            0
        };
        if stored_crc != actual_crc {
            if offset == total_bytes {
                // The final frame never finished writing: an interrupted
                // append, not damage to committed data.
                torn_tail_offset = Some(last_commit_end);
                break;
            }
            corruption = Some(Error::corruption(
                path,
                format!(
                    "WAL frame at byte {body_offset} (seq {seq}) fails CRC: stored {stored_crc:#010x}, computed {actual_crc:#010x}, and {} bytes follow it",
                    total_bytes - offset
                ),
            ));
            break;
        }

        let tag = body[0];
        if tag == COMMIT_TAG {
            let (start_seq, end_seq, frame_count, batch_crc) =
                match decode_commit(&body[FRAME_BODY_FIXED..], path, body_offset) {
                    Ok(commit) => commit,
                    Err(error) => {
                        corruption = Some(error);
                        break;
                    }
                };
            if frame_count as usize != pending.len() {
                corruption = Some(Error::corruption(
                    path,
                    format!(
                        "Commit at byte {body_offset} claims {frame_count} frames, {} op frames precede it",
                        pending.len()
                    ),
                ));
                break;
            }
            let actual_batch_crc = crc32(&pending_bytes);
            if batch_crc != actual_batch_crc {
                corruption = Some(Error::corruption(
                    path,
                    format!(
                        "Commit at byte {body_offset} (seq {end_seq}) batch_crc mismatch: stored {batch_crc:#010x}, computed {actual_batch_crc:#010x}"
                    ),
                ));
                break;
            }
            let mut doc_seqs = pending
                .iter()
                .filter(|r| r.op.consumes_seq())
                .map(|r| r.seq);
            let first = doc_seqs.next();
            match first {
                None => {
                    if start_seq != 0 || end_seq != 0 {
                        corruption = Some(Error::corruption(
                            path,
                            format!(
                                "Commit at byte {body_offset} declares seq {start_seq}..={end_seq} but the batch holds no document operations"
                            ),
                        ));
                        break;
                    }
                }
                Some(first_seq) => {
                    if start_seq != first_seq || end_seq != seq {
                        corruption = Some(Error::corruption(
                            path,
                            format!(
                                "Commit at byte {body_offset} declares seq {start_seq}..={end_seq}, the frames span {first_seq}..={seq}"
                            ),
                        ));
                        break;
                    }
                    if end_seq - start_seq + 1
                        != pending.iter().filter(|r| r.op.consumes_seq()).count() as u64
                    {
                        corruption = Some(Error::corruption(
                            path,
                            format!(
                                "Commit at byte {body_offset} declares a {}-sequence span, the batch holds {} document operations",
                                end_seq - start_seq + 1,
                                pending.iter().filter(|r| r.op.consumes_seq()).count()
                            ),
                        ));
                        break;
                    }
                }
            }
            if end_seq != 0 {
                if last_seq != 0 && start_seq != last_seq + 1 {
                    corruption = Some(Error::corruption(
                        path,
                        format!(
                            "batch at byte {body_offset} starts at seq {start_seq}, the previous batch ended at {last_seq}"
                        ),
                    ));
                    break;
                }
                last_seq = end_seq;
            }
            batches.push(RamWalBatch {
                start_seq,
                end_seq,
                records: std::mem::take(&mut pending),
                end_offset: offset,
            });
            pending_bytes.clear();
            last_commit_end = offset;
        } else {
            let op = match decode_op(tag, &body[FRAME_BODY_FIXED..], path, body_offset) {
                Ok(op) => op,
                Err(error) => {
                    corruption = Some(error);
                    break;
                }
            };
            if op.consumes_seq() {
                if seq == 0 {
                    corruption = Some(Error::corruption(
                        path,
                        format!(
                            "WAL frame at byte {body_offset} is a document operation with seq 0"
                        ),
                    ));
                    break;
                }
                if let Some(previous) = pending.iter().rev().find(|r| r.op.consumes_seq()) {
                    if seq != previous.seq + 1 {
                        corruption = Some(Error::corruption(
                            path,
                            format!(
                                "WAL frame at byte {body_offset} jumps to seq {seq} after {}",
                                previous.seq
                            ),
                        ));
                        break;
                    }
                }
            } else if seq != 0 {
                corruption = Some(Error::corruption(
                    path,
                    format!(
                        "WAL schema operation at byte {body_offset} carries document seq {seq}; schema changes consume none"
                    ),
                ));
                break;
            }
            pending_bytes.extend_from_slice(&body);
            pending.push(RamWalRecord { seq, op });
        }
    }

    if corruption.is_none() && !pending.is_empty() {
        // Op frames without their `Commit`: the append was interrupted at the
        // batch boundary. Nothing of it is durable.
        torn_tail_offset = Some(last_commit_end);
    }

    Ok(RamWalScanCore {
        scan: RamWalScan {
            wal_gen,
            total_bytes,
            header_len: WAL_HEADER_LEN as u64,
            batches,
            last_seq,
            last_commit_end,
            torn_tail_offset,
        },
        corruption,
    })
}

// ─── WAL writer ─────────────────────────────────────────────────────────────

/// Appends committed batches to one v3 WAL generation.
///
/// `append_batch_and_sync` encodes every op frame plus the trailing `Commit`
/// into one buffer and issues **one** `write_all` and **one** `sync_all`: that
/// fsync is the batch's durability point.
pub struct RamWalWriter {
    file: File,
    path: PathBuf,
    wal_gen: u64,
    last_seq: u64,
    io: Arc<dyn Io>,
}

impl std::fmt::Debug for RamWalWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RamWalWriter")
            .field("path", &self.path)
            .field("wal_gen", &self.wal_gen)
            .field("last_seq", &self.last_seq)
            .finish_non_exhaustive()
    }
}

impl RamWalWriter {
    /// Create the generation (header + fsync + parent fsync) or open an
    /// existing one after verifying its header and full scan.
    ///
    /// An existing WAL with a torn tail fails here: recovery must call
    /// [`truncate_ram_wal_tail`] before any append, so a stale tail can never
    /// be silently extended.
    pub fn create_or_open(path: &Path, wal_gen: u64, io: Arc<dyn Io>) -> Result<Self> {
        if !path.exists() {
            let mut file = io
                .create_new(path)
                .map_err(Error::io_err(path, "create ram WAL generation"))?;
            io.write_all(&mut file, &encode_wal_header(wal_gen))
                .map_err(Error::io_err(path, "write ram WAL header"))?;
            io.sync_all(&file)
                .map_err(Error::io_err(path, "sync ram WAL header"))?;
            io.sync_parent_dir(path)
                .map_err(Error::io_err(path, "sync directory after ram WAL create"))?;
            return Ok(RamWalWriter {
                file,
                path: path.to_path_buf(),
                wal_gen,
                last_seq: 0,
                io,
            });
        }

        let mut file = File::open(path).map_err(Error::io_err(path, "open ram WAL header"))?;
        let mut header = [0u8; WAL_HEADER_LEN];
        let mut read = 0usize;
        while read < WAL_HEADER_LEN {
            match file.read(&mut header[read..]) {
                Ok(0) => break,
                Ok(count) => read += count,
                Err(error) => return Err(Error::io_err(path, "read ram WAL header")(error)),
            }
        }
        if read < WAL_HEADER_LEN {
            return Err(Error::corruption(
                path,
                format!("WAL header is truncated: {read} bytes, need {WAL_HEADER_LEN}"),
            ));
        }
        drop(file);
        let found_gen = decode_wal_header(&header, path)?;
        if found_gen != wal_gen {
            return Err(Error::corruption(
                path,
                format!("WAL header names generation {found_gen}, the manifest expects {wal_gen}"),
            ));
        }
        let scan = scan_ram_wal(path)?;
        if let Some(torn) = scan.torn_tail_offset {
            return Err(Error::corruption(
                path,
                format!(
                    "WAL has a torn tail at byte {torn} ({} bytes); truncate_ram_wal_tail must clear it before appending",
                    scan.total_bytes - torn
                ),
            ));
        }
        let file = open_append(path)?;
        Ok(RamWalWriter {
            file,
            path: path.to_path_buf(),
            wal_gen,
            last_seq: scan.last_seq,
            io,
        })
    }

    /// Encode, append and fsync one batch. Returns the batch's `end_seq`
    /// (`0` for a schema-only batch).
    ///
    /// The trailing `Commit` carries `batch_crc` over the concatenated frame
    /// bodies (`tag + seq + payload`) it names. The trailing CRC is excluded on
    /// purpose: a checksum over bytes that already contain their own checksum
    /// is invariant under a re-sealed bit flip, and would never catch one.
    ///
    /// Every invariant that makes the batch replayable — non-empty, contiguous
    /// document sequences, schema ops without a sequence, ids and payloads
    /// non-empty — is checked *before* a byte is written.
    pub fn append_batch_and_sync(&mut self, records: &[RamWalRecord]) -> Result<u64> {
        let (buffer, end_seq) = self.prepare_batch(records)?;
        self.append_prepared_and_sync(&buffer, end_seq)?;
        Ok(end_seq)
    }

    /// Validate `records`, reject a sequence that has already been committed,
    /// and encode the whole batch (op frames plus its `Commit`) into one
    /// buffer. Returns `(buffer, end_seq)`.
    ///
    /// Splitting preparation from the append lets a caller retry the durable
    /// step without re-encoding, and lets it know the batch's byte length
    /// before anything reaches the file.
    pub fn prepare_batch(&mut self, records: &[RamWalRecord]) -> Result<(Vec<u8>, u64)> {
        let (start_seq, end_seq) = validate_batch(records)?;
        if start_seq != 0 && start_seq <= self.last_seq {
            return Err(Error::invalid_arg(format!(
                "batch starts at seq {start_seq}, but this WAL already committed through seq {}",
                self.last_seq
            )));
        }
        let mut buffer = Vec::new();
        let mut batch_hasher = Crc32Hasher::new();
        for record in records {
            let frame = encode_op_frame(record)?;
            // Only the body: a CRC that covers a frame *including* that frame's
            // own CRC is invariant under a re-sealed bit flip (the remainder
            // cancels), so the batch CRC would never notice tampering.
            batch_hasher.update(&frame[4..frame.len() - 4]);
            buffer.extend_from_slice(&frame);
        }
        let frame_count = u32::try_from(records.len())
            .map_err(|_| Error::invalid_arg("batch frame count exceeds u32"))?;
        let commit = encode_commit_frame(start_seq, end_seq, frame_count, batch_hasher.finalize());
        buffer.extend_from_slice(&commit);
        Ok((buffer, end_seq))
    }

    /// Write a buffer produced by [`RamWalWriter::prepare_batch`] and fsync it.
    ///
    /// This is the durability point: on `Ok` the batch is stable on disk. A
    /// caller that sees an error must treat the tail as uncertain — the writer
    /// marks nothing and advances nothing.
    pub fn append_prepared_and_sync(&mut self, buffer: &[u8], end_seq: u64) -> Result<()> {
        self.io
            .write_all(&mut self.file, buffer)
            .map_err(Error::io_err(&self.path, "append ram WAL batch"))?;
        self.io
            .sync_all(&self.file)
            .map_err(Error::io_err(&self.path, "sync ram WAL batch"))?;
        if end_seq != 0 {
            self.last_seq = end_seq;
        }
        Ok(())
    }

    /// Current file length in bytes.
    ///
    /// Recovery and the retry path use this as the rollback point: a batch
    /// that failed before its `Commit` reached disk is truncated back here
    /// before a retry, so a partial write can never be extended twice.
    pub fn file_len(&self) -> Result<u64> {
        self.file
            .metadata()
            .map(|meta| meta.len())
            .map_err(Error::io_err(&self.path, "stat ram WAL"))
    }

    /// Move the append position back to `len`.
    ///
    /// Paired with an `Io::truncate_and_sync` to the same length: the handle
    /// must write at `len`, not at the offset a failed `write_all` reached,
    /// or the retry would leave a sparse hole.
    pub fn rewind_to(&mut self, len: u64) -> Result<()> {
        use std::io::Seek;
        self.file
            .seek(std::io::SeekFrom::Start(len))
            .map(|_| ())
            .map_err(Error::io_err(&self.path, "seek ram WAL"))
    }

    /// Flush the generation's data and metadata to stable storage.
    pub fn sync(&self) -> Result<()> {
        self.io
            .sync_all(&self.file)
            .map_err(Error::io_err(&self.path, "sync ram WAL generation"))
    }

    /// Generation number of this WAL file.
    pub fn wal_gen(&self) -> u64 {
        self.wal_gen
    }

    /// Highest document sequence committed through this writer.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Path of this WAL file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn validate_batch(records: &[RamWalRecord]) -> Result<(u64, u64)> {
    if records.is_empty() {
        return Err(Error::invalid_arg(
            "an empty batch is not a commit; stage nothing instead",
        ));
    }
    let mut start_seq = 0u64;
    let mut end_seq = 0u64;
    let mut previous: Option<u64> = None;
    for record in records {
        if record.op.consumes_seq() {
            if record.seq == 0 {
                return Err(Error::invalid_arg(
                    "document operations must carry a non-zero sequence",
                ));
            }
            if let Some(prev) = previous {
                if record.seq != prev + 1 {
                    return Err(Error::invalid_arg(format!(
                        "batch sequences must be contiguous: {} follows {prev}",
                        record.seq
                    )));
                }
            } else {
                start_seq = record.seq;
            }
            previous = Some(record.seq);
            end_seq = record.seq;
        } else if record.seq != 0 {
            return Err(Error::invalid_arg(format!(
                "schema operations consume no document sequence; found seq {}",
                record.seq
            )));
        }
    }
    Ok((start_seq, end_seq))
}

// ─── Torn-tail truncation & repair ──────────────────────────────────────────

/// Truncate a WAL to `good_offset` and fsync it.
///
/// Recovery calls this after [`scan_ram_wal`] reports a torn tail, before any
/// new append. Idempotent: truncating to an offset the file already has is a
/// no-op fsync, which is exactly what a crash-during-truncation rerun needs.
pub fn truncate_ram_wal_tail(path: &Path, good_offset: u64, io: &dyn Io) -> Result<()> {
    let len = std::fs::metadata(path)
        .map_err(Error::io_err(path, "stat ram WAL before truncate"))?
        .len();
    if good_offset < WAL_HEADER_LEN as u64 || good_offset > len {
        return Err(Error::invalid_arg(format!(
            "refusing to truncate {} to {good_offset}: the file is {len} bytes and the header is {WAL_HEADER_LEN}",
            path.display()
        )));
    }
    io.truncate_and_sync(path, good_offset)
        .map_err(Error::io_err(path, "truncate ram WAL tail"))
}

/// What [`repair_ram_wal`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamWalRepairReport {
    /// Bytes in the file before repair.
    pub scanned_bytes: u64,
    /// Offset recovery keeps (just past the last intact `Commit`).
    pub good_offset: u64,
    /// Bytes removed from the live file (still present in the quarantine copy).
    pub truncated_bytes: u64,
    /// Highest sequence that survives.
    pub last_valid_seq: u64,
    /// Committed batches kept.
    pub batches_kept: usize,
    /// Quarantine copy of the pre-repair bytes, when anything was removed.
    pub quarantine_path: Option<PathBuf>,
    /// Whether the removed bytes were mid-log corruption (not just a torn tail).
    pub was_corrupt: bool,
    /// Human-readable description of the failure that triggered the repair.
    pub issue: Option<String>,
}

/// Quarantine whatever follows the last intact `Commit` and truncate the WAL.
///
/// The full pre-repair file is copied to `<name>.corrupt.<unix-nanos>.bak`
/// through [`Io`] **before** the live file is truncated, so no byte is
/// destroyed. A corrupt *header* cannot be repaired (there is no good prefix)
/// and is returned as [`Error::Corruption`].
pub fn repair_ram_wal(path: &Path, io: &dyn Io) -> Result<RamWalRepairReport> {
    let core = scan_ram_wal_core(path)?;
    let scan = core.scan;
    let truncated_bytes = scan.total_bytes - scan.last_commit_end;
    let mut quarantine_path = None;
    if truncated_bytes > 0 {
        let backup = backup_path(path)?;
        let bytes = std::fs::read(path).map_err(Error::io_err(path, "read WAL for quarantine"))?;
        let mut file = io
            .create_new(&backup)
            .map_err(Error::io_err(&backup, "create WAL quarantine copy"))?;
        io.write_all(&mut file, &bytes)
            .map_err(Error::io_err(&backup, "write WAL quarantine copy"))?;
        io.sync_all(&file)
            .map_err(Error::io_err(&backup, "sync WAL quarantine copy"))?;
        drop(file);
        io.sync_parent_dir(&backup).map_err(Error::io_err(
            &backup,
            "sync directory after WAL quarantine",
        ))?;
        quarantine_path = Some(backup);
        io.truncate_and_sync(path, scan.last_commit_end)
            .map_err(Error::io_err(path, "truncate ram WAL tail"))?;
    }
    Ok(RamWalRepairReport {
        scanned_bytes: scan.total_bytes,
        good_offset: scan.last_commit_end,
        truncated_bytes,
        last_valid_seq: scan.last_seq,
        batches_kept: scan.batches.len(),
        quarantine_path,
        was_corrupt: core.corruption.is_some(),
        issue: core.corruption.map(|error| error.to_string()),
    })
}

// ─── Filesystem check ───────────────────────────────────────────────────────

/// Result of [`fsck_ram_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RamFsckReport {
    /// Directory audited.
    pub dir: PathBuf,
    /// Active base generation.
    pub base_gen: u64,
    /// Active WAL generation.
    pub wal_gen: u64,
    /// Highest sequence folded into the base.
    pub base_seq: u64,
    /// Documents in the base.
    pub doc_count: u64,
    /// Committed batches in the primary WAL.
    pub wal_batches: usize,
    /// Highest sequence anywhere in the lineage.
    pub last_seq: u64,
    /// WAL generation a stalled compaction was writing into.
    pub pending_wal_gen: Option<u64>,
    /// Batches in the pending WAL, when one is declared.
    pub pending_batches: Option<usize>,
    /// Declared secondary indexes.
    pub indexes: Vec<IndexSpec>,
    /// Non-fatal findings (torn tails that recovery will truncate).
    pub warnings: Vec<String>,
}

/// Validate a v3 database directory end to end.
///
/// Checks the manifest, fully verifies the base file, scans the primary WAL and
/// the optional `pending_wal_gen`, and proves sequence continuity from
/// `base_seq` through the last batch of each log up to `manifest.last_seq`.
/// Corruption is returned, never repaired; a torn tail is reported as a warning
/// because recovery truncates it.
pub fn fsck_ram_dir(dir: &Path) -> Result<RamFsckReport> {
    let manifest = read_ram_manifest(dir)?;
    let base_path = base_file_path(dir, manifest.base_gen);
    let base = read_ram_base_file(&base_path)?;
    if base.base_seq > manifest.last_seq {
        return Err(Error::corruption(
            &base_path,
            format!(
                "base sequence {} is above the manifest last_seq {}",
                base.base_seq, manifest.last_seq
            ),
        ));
    }

    let mut warnings = Vec::new();
    let wal_path = wal_file_path(dir, manifest.wal_gen);
    let wal = scan_ram_wal(&wal_path)?;
    if let Some(torn) = wal.torn_tail_offset {
        warnings.push(format!(
            "{}: torn tail at byte {torn}; recovery truncates {} bytes",
            wal_path.display(),
            wal.total_bytes - torn
        ));
    }
    let last_seq = check_continuity(&wal, base.base_seq, &wal_path)?;
    if last_seq < manifest.last_seq {
        return Err(Error::corruption(
            dir,
            format!(
                "lineage reaches seq {last_seq}, the manifest declares last_seq {}",
                manifest.last_seq
            ),
        ));
    }
    if last_seq > manifest.last_seq {
        // The manifest's `last_seq` is advanced by compaction, so a WAL that
        // has committed past it is the normal state between compactions — and
        // recovery accepts it. Requiring equality would make fsck reject every
        // live-written database.
        warnings.push(format!(
            "manifest last_seq {} lags the lineage at {last_seq}; the next compaction publishes it",
            manifest.last_seq
        ));
    }

    // A pending generation holds writes that landed after the freeze, so it
    // continues from `last_seq` and may legitimately run past it (crash C2/C4).
    let mut pending_batches = None;
    let mut lineage_seq = last_seq;
    if let Some(pending_gen) = manifest.pending_wal_gen {
        let pending_path = wal_file_path(dir, pending_gen);
        let pending = scan_ram_wal(&pending_path)?;
        if let Some(torn) = pending.torn_tail_offset {
            warnings.push(format!(
                "{}: torn tail at byte {torn}; recovery truncates {} bytes",
                pending_path.display(),
                pending.total_bytes - torn
            ));
        }
        lineage_seq = check_continuity(&pending, last_seq, &pending_path)?;
        pending_batches = Some(pending.batches.len());
    }

    Ok(RamFsckReport {
        dir: dir.to_path_buf(),
        base_gen: manifest.base_gen,
        wal_gen: manifest.wal_gen,
        base_seq: base.base_seq,
        doc_count: base.doc_count,
        wal_batches: wal.batches.len(),
        last_seq: lineage_seq,
        pending_wal_gen: manifest.pending_wal_gen,
        pending_batches,
        indexes: manifest.indexes,
        warnings,
    })
}

fn check_continuity(scan: &RamWalScan, from_seq: u64, path: &Path) -> Result<u64> {
    let mut last_seq = from_seq;
    for batch in &scan.batches {
        if batch.end_seq == 0 {
            continue;
        }
        if batch.start_seq != last_seq + 1 {
            return Err(Error::corruption(
                path,
                format!(
                    "batch at byte {} starts at seq {}, the lineage is at {last_seq}",
                    batch.end_offset, batch.start_seq
                ),
            ));
        }
        last_seq = batch.end_seq;
    }
    Ok(last_seq)
}

// ─── Shared helpers ─────────────────────────────────────────────────────────

fn read_exact<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    path: &Path,
    offset: u64,
    what: &str,
) -> Result<()> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Err(Error::corruption(
            path,
            format!("{what} is truncated at byte {offset}"),
        )),
        Err(error) => Err(Error::io_err(path, what)(error)),
    }
}

fn open_append(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(crate::io::FILE_SHARE_ALL);
    }
    options
        .open(path)
        .map_err(Error::io_err(path, "open ram WAL for append"))
}

fn write_file_atomic(path: &Path, bytes: &[u8], io: &dyn Io, what: &str) -> Result<()> {
    let tmp = temp_sibling(path, "tmp")?;
    let mut file = io
        .create_new(&tmp)
        .map_err(Error::io_err(&tmp, format!("create temporary {what}")))?;
    io.write_all(&mut file, bytes)
        .map_err(Error::io_err(&tmp, format!("write temporary {what}")))?;
    io.sync_all(&file)
        .map_err(Error::io_err(&tmp, format!("sync temporary {what}")))?;
    drop(file);
    io.rename(&tmp, path)
        .map_err(Error::io_err(path, format!("publish {what}")))?;
    io.sync_parent_dir(path).map_err(Error::io_err(
        path,
        format!("sync directory after {what} publish"),
    ))?;
    Ok(())
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique sibling so two in-flight writers can never share a temp file.
fn temp_sibling(path: &Path, tag: &str) -> Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        Error::invalid_arg(format!(
            "{} has no file name to derive a temp from",
            path.display()
        ))
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::invalid_arg(format!("system clock is before 1970: {e}")))?
        .as_nanos();
    let ordinal = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut sibling = name.to_os_string();
    sibling.push(format!(".{tag}.{}.{nanos}.{ordinal}", std::process::id()));
    Ok(path.with_file_name(sibling))
}

fn backup_path(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        Error::invalid_arg(format!("{} has no file name to quarantine", path.display()))
    })?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::invalid_arg(format!("system clock is before 1970: {e}")))?
        .as_nanos();
    let ordinal = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut backup = name.to_os_string();
    backup.push(format!(".corrupt.{nanos}.{ordinal}.bak"));
    Ok(path.with_file_name(backup))
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{IoOp, RealIo};
    use serde_json::json;
    use std::io::Write as _;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    fn record(id: &str, seq: u64, json_text: &str) -> Arc<DocRecord> {
        let value: Value = serde_json::from_str(json_text).expect("test JSON");
        DocRecord::new(Arc::from(id), seq, DocVal::from_value(&value))
    }

    fn sample_docs() -> Vec<Arc<DocRecord>> {
        vec![
            record("a", 1, r#"{"n":1,"tag":"a"}"#),
            record("b", 2, r#"{"n":2,"tag":"b","nested":{"deep":[1,2,3]}}"#),
            record("c", 3, r#"{"n":3,"tag":"c"}"#),
        ]
    }

    fn manifest() -> RamManifest {
        RamManifest {
            version: RAM_STORAGE_VERSION,
            source_epoch_uuid: "6f1c5c1e-0000-4000-8000-000000000001".to_string(),
            base_gen: 4,
            wal_gen: 4,
            pending_wal_gen: None,
            last_seq: 7,
            min_available_seq: 1,
            indexes: vec![IndexSpec {
                field: "tag".to_string(),
                kind: IndexKind::Hash,
            }],
        }
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        let mut file = File::create(path).expect("create file");
        file.write_all(bytes).expect("write bytes");
        file.sync_all().expect("sync file");
    }

    fn read_bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("read file")
    }

    fn corruption_message<T: std::fmt::Debug>(result: Result<T>) -> String {
        match result {
            Ok(value) => panic!("expected corruption, got {value:?}"),
            Err(Error::Corruption { message, .. }) => message,
            Err(other) => panic!("expected Error::Corruption, got {other:?}"),
        }
    }

    fn base_path(dir: &Path) -> PathBuf {
        base_file_path(dir, 0)
    }

    fn write_base(dir: &Path, docs: &[Arc<DocRecord>], base_seq: u64) -> PathBuf {
        let path = base_path(dir);
        write_ram_base_file(&path, base_seq, docs, &RealIo).expect("write base");
        path
    }

    // ── Base file ─────────────────────────────────────────────────────────

    #[test]
    fn base_round_trips_documents_directory_and_sequence() {
        let dir = temp_dir();
        let docs = sample_docs();
        let path = write_base(dir.path(), &docs, 3);
        let loaded = read_ram_base_file(&path).expect("read base");
        assert_eq!(loaded.base_seq, 3);
        assert_eq!(loaded.doc_count, 3);
        assert_eq!(loaded.docs.len(), 3);
        for (expected, actual) in docs.iter().zip(loaded.docs.iter()) {
            assert_eq!(expected.id, actual.id);
            assert_eq!(expected.doc_seq, actual.doc_seq);
            assert_eq!(expected.doc.to_value(), actual.doc.to_value());
        }
    }

    #[test]
    fn base_round_trips_a_four_megabyte_conversation() {
        let dir = temp_dir();
        let big = "x".repeat(4 * 1024 * 1024);
        let value = json!({ "messages": [ { "role": "user", "text": big } ] });
        let docs = vec![DocRecord::new(
            Arc::from("chat"),
            9,
            DocVal::from_value(&value),
        )];
        let path = write_base(dir.path(), &docs, 9);
        let loaded = read_ram_base_file(&path).expect("read base");
        assert_eq!(loaded.doc_count, 1);
        assert_eq!(loaded.docs[0].doc.to_value(), value);
    }

    #[test]
    fn base_truncation_fails_loud() {
        let dir = temp_dir();
        let docs = sample_docs();
        let path = write_base(dir.path(), &docs, 3);
        let bytes = read_bytes(&path);
        for cut in [
            0usize,
            8,
            BASE_HEADER_LEN,
            bytes.len() - 1,
            bytes.len() - 10,
        ] {
            let truncated = dir.path().join(format!("cut.{cut}.ndb"));
            write_bytes(&truncated, &bytes[..cut]);
            let message = corruption_message(read_ram_base_file(&truncated));
            assert!(
                message.contains("truncated") || message.contains("shorter than"),
                "cut {cut}: {message}"
            );
        }
    }

    #[test]
    fn base_record_bit_flip_fails_loud() {
        let dir = temp_dir();
        let docs = sample_docs();
        let path = write_base(dir.path(), &docs, 3);
        let mut bytes = read_bytes(&path);
        bytes[BASE_HEADER_LEN + 20] ^= 0x40;
        write_bytes(&path, &bytes);
        let message = corruption_message(read_ram_base_file(&path));
        assert!(message.contains("CRC mismatch"), "{message}");
    }

    #[test]
    fn base_footer_bit_flip_fails_loud() {
        let dir = temp_dir();
        let docs = sample_docs();
        let path = write_base(dir.path(), &docs, 3);
        let mut bytes = read_bytes(&path);
        let footer_crc = bytes.len() - 2;
        bytes[footer_crc] ^= 0x01;
        write_bytes(&path, &bytes);
        let message = corruption_message(read_ram_base_file(&path));
        assert!(message.contains("file CRC mismatch"), "{message}");
    }

    #[test]
    fn base_header_bit_flip_fails_loud() {
        let dir = temp_dir();
        let path = write_base(dir.path(), &sample_docs(), 3);
        let mut bytes = read_bytes(&path);
        bytes[14] ^= 0x01;
        write_bytes(&path, &bytes);
        let message = corruption_message(read_ram_base_file(&path));
        assert!(message.contains("header CRC mismatch"), "{message}");
    }

    #[test]
    fn base_duplicate_id_is_refused_before_writing() {
        let dir = temp_dir();
        let docs = vec![record("a", 1, r#"{"n":1}"#), record("a", 2, r#"{"n":2}"#)];
        let error = write_ram_base_file(&base_path(dir.path()), 2, &docs, &RealIo)
            .expect_err("duplicate id");
        assert!(error.to_string().contains("twice"), "{error}");
        assert!(!base_path(dir.path()).exists());
    }

    #[test]
    fn base_document_newer_than_the_base_is_refused() {
        let dir = temp_dir();
        let docs = vec![record("a", 5, r#"{"n":1}"#)];
        let error = write_ram_base_file(&base_path(dir.path()), 4, &docs, &RealIo)
            .expect_err("doc seq above base");
        assert!(
            error
                .to_string()
                .contains("a base cannot hold a newer document"),
            "{error}"
        );
    }

    #[test]
    fn base_rejects_trailing_bytes() {
        let dir = temp_dir();
        let path = write_base(dir.path(), &sample_docs(), 3);
        let mut bytes = read_bytes(&path);
        bytes.extend_from_slice(b"junk");
        write_bytes(&path, &bytes);
        let message = corruption_message(read_ram_base_file(&path));
        assert!(message.contains("trailing bytes"), "{message}");
    }

    // ── WAL round-trip ────────────────────────────────────────────────────

    fn all_ops_batch(start: u64) -> Vec<RamWalRecord> {
        vec![
            RamWalRecord::insert(start, "a", br#"{"n":1}"#.to_vec()),
            RamWalRecord::replace(start + 1, "a", br#"{"n":2}"#.to_vec()),
            RamWalRecord::patch(
                start + 2,
                "a",
                vec![
                    RamWalPatchOp::set("nested.value", br#"42"#.to_vec()),
                    RamWalPatchOp::array_push("tags", br#""new""#.to_vec()),
                    RamWalPatchOp::remove("obsolete"),
                ],
            ),
            RamWalRecord::delete(start + 3, "b"),
            RamWalRecord::restore(start + 4, "b", br#"{"n":2}"#.to_vec()),
            RamWalRecord::index_add("tag", IndexKind::BTree),
            RamWalRecord::index_drop("legacy"),
        ]
    }

    fn open_writer(dir: &Path, wal_gen: u64) -> RamWalWriter {
        let path = wal_file_path(dir, wal_gen);
        RamWalWriter::create_or_open(&path, wal_gen, Arc::new(RealIo)).expect("open writer")
    }

    #[test]
    fn wal_batch_round_trips_every_operation() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        let batch = all_ops_batch(1);
        let end = writer.append_batch_and_sync(&batch).expect("append batch");
        assert_eq!(end, 5);

        let scan = scan_ram_wal(&path).expect("scan");
        assert_eq!(scan.wal_gen, 0);
        assert_eq!(scan.torn_tail_offset, None);
        assert_eq!(scan.last_seq, 5);
        assert_eq!(scan.batches.len(), 1);
        assert_eq!(scan.batches[0].start_seq, 1);
        assert_eq!(scan.batches[0].end_seq, 5);
        assert_eq!(scan.batches[0].records, batch);
        assert_eq!(scan.last_commit_end, scan.total_bytes);
    }

    #[test]
    fn payload_bytes_matches_the_encoded_payload() {
        for record in all_ops_batch(1) {
            let encoded = encode_op_payload(&record.op).expect("encode payload");
            assert_eq!(
                record.payload_bytes(),
                encoded.len(),
                "accounted size drifted from the frame encoding for {:?}",
                record.op
            );
        }
    }

    #[test]
    fn wal_schema_only_batch_has_zero_sequence_bounds() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        let batch = vec![
            RamWalRecord::index_add("tag", IndexKind::Hash),
            RamWalRecord::index_drop("legacy"),
        ];
        assert_eq!(writer.append_batch_and_sync(&batch).expect("append"), 0);
        let scan = scan_ram_wal(&path).expect("scan");
        assert_eq!(scan.batches.len(), 1);
        assert_eq!(scan.batches[0].start_seq, 0);
        assert_eq!(scan.batches[0].end_seq, 0);
        assert_eq!(scan.last_seq, 0);
        assert_eq!(writer.last_seq(), 0);
    }

    #[test]
    fn wal_writer_rejects_non_contiguous_and_sequence_consuming_violations() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        let gap = vec![
            RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec()),
            RamWalRecord::insert(3, "b", br#"{"n":2}"#.to_vec()),
        ];
        assert!(writer
            .append_batch_and_sync(&gap)
            .expect_err("sequence gap")
            .to_string()
            .contains("contiguous"));
        let schema_seq = vec![RamWalRecord {
            seq: 4,
            op: RamWalOp::IndexDrop {
                field: "x".to_string(),
            },
        }];
        assert!(writer
            .append_batch_and_sync(&schema_seq)
            .expect_err("schema seq")
            .to_string()
            .contains("consume no document sequence"));
        assert_eq!(
            writer
                .append_batch_and_sync(&[])
                .expect_err("empty")
                .to_string(),
            "invalid argument: an empty batch is not a commit; stage nothing instead"
        );
        assert_eq!(scan_ram_wal(&path).expect("scan").batches.len(), 0);
    }

    #[test]
    fn wal_writer_refuses_to_reuse_committed_sequences() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{}"#.to_vec())])
            .expect("first batch");
        drop(writer);
        let mut reopened =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("reopen");
        assert_eq!(reopened.last_seq(), 1);
        let error = reopened
            .append_batch_and_sync(&[RamWalRecord::insert(1, "b", br#"{}"#.to_vec())])
            .expect_err("reused seq");
        assert!(
            error
                .to_string()
                .contains("already committed through seq 1"),
            "{error}"
        );
    }

    // ── Torn tail (R0) ────────────────────────────────────────────────────

    fn append_raw(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new().append(true).open(path).expect("append");
        file.write_all(bytes).expect("append bytes");
        file.sync_all().expect("sync");
    }

    #[test]
    fn uncommitted_op_frames_are_a_torn_tail() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec())])
            .expect("batch");
        let last_commit_end = scan_ram_wal(&path).expect("scan").last_commit_end;
        append_raw(
            &path,
            &encode_op_frame(&RamWalRecord::insert(2, "b", br#"{"n":2}"#.to_vec())).expect("frame"),
        );
        let scan = scan_ram_wal(&path).expect("scan");
        assert_eq!(scan.batches.len(), 1);
        assert_eq!(scan.last_seq, 1);
        assert_eq!(scan.torn_tail_offset, Some(last_commit_end));
    }

    #[test]
    fn truncated_commit_frame_is_a_torn_tail() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec())])
            .expect("batch");
        let last_commit_end = scan_ram_wal(&path).expect("scan").last_commit_end;
        let op =
            encode_op_frame(&RamWalRecord::insert(2, "b", br#"{"n":2}"#.to_vec())).expect("frame");
        let commit = encode_commit_frame(2, 2, 1, crc32(&op));
        append_raw(&path, &op);
        append_raw(&path, &commit[..commit.len() - 5]);
        let scan = scan_ram_wal(&path).expect("scan");
        assert_eq!(scan.batches.len(), 1);
        assert_eq!(scan.torn_tail_offset, Some(last_commit_end));
    }

    #[test]
    fn truncated_final_frame_crc_is_a_torn_tail() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec())])
            .expect("batch");
        let last_commit_end = scan_ram_wal(&path).expect("scan").last_commit_end;
        let mut frame =
            encode_op_frame(&RamWalRecord::insert(2, "b", br#"{"n":2}"#.to_vec())).expect("frame");
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        append_raw(&path, &frame);
        let scan = scan_ram_wal(&path).expect("scan");
        assert_eq!(scan.torn_tail_offset, Some(last_commit_end));
    }

    #[test]
    fn truncate_tail_clears_the_suffix_and_allows_a_clean_append() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec())])
            .expect("batch");
        let last_commit_end = scan_ram_wal(&path).expect("scan").last_commit_end;
        append_raw(
            &path,
            &encode_op_frame(&RamWalRecord::insert(2, "b", br#"{"n":2}"#.to_vec())).expect("frame"),
        );
        truncate_ram_wal_tail(&path, last_commit_end, &RealIo).expect("truncate");
        let scan = scan_ram_wal(&path).expect("scan after truncate");
        assert_eq!(scan.torn_tail_offset, None);
        assert_eq!(scan.total_bytes, last_commit_end);

        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("reopen writer");
        assert_eq!(writer.last_seq(), 1);
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(2, "b", br#"{"n":2}"#.to_vec())])
            .expect("append after truncate");
        let scan = scan_ram_wal(&path).expect("scan after append");
        assert_eq!(scan.batches.len(), 2);
        assert_eq!(scan.last_seq, 2);
        assert_eq!(scan.torn_tail_offset, None);
    }

    #[test]
    fn truncate_tail_refuses_to_extend_the_file_or_cut_the_header() {
        let dir = temp_dir();
        let (path, first_end) = batch_one_and_two(dir.path());
        let len = read_bytes(&path).len() as u64;
        assert!(truncate_ram_wal_tail(&path, len + 1, &RealIo).is_err());
        assert!(truncate_ram_wal_tail(&path, 3, &RealIo).is_err());
        assert_eq!(read_bytes(&path).len() as u64, len);
        truncate_ram_wal_tail(&path, first_end, &RealIo).expect("a legal offset");
        assert_eq!(read_bytes(&path).len() as u64, first_end);
    }

    #[test]
    fn absurd_frame_length_is_corruption_not_a_torn_tail() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        append_raw(&path, &u32::MAX.to_le_bytes());
        append_raw(&path, &[0u8; 16]);
        let message = corruption_message(scan_ram_wal(&path));
        assert!(message.contains("above the"), "{message}");
    }

    #[test]
    fn writer_refuses_to_open_a_torn_tail() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{}"#.to_vec())])
            .expect("batch");
        drop(writer);
        append_raw(
            &path,
            &encode_op_frame(&RamWalRecord::insert(2, "b", br#"{}"#.to_vec())).expect("frame"),
        );
        let error = RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo))
            .expect_err("torn tail must block appends");
        assert!(error.to_string().contains("torn tail"), "{error}");
    }

    // ── Mid-log corruption ────────────────────────────────────────────────

    fn batch_one_and_two(dir: &Path) -> (PathBuf, u64) {
        let path = wal_file_path(dir, 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec())])
            .expect("batch 1");
        let first_end = scan_ram_wal(&path).expect("scan").last_commit_end;
        writer
            .append_batch_and_sync(&[
                RamWalRecord::insert(2, "b", br#"{"n":2}"#.to_vec()),
                RamWalRecord::insert(3, "c", br#"{"n":3}"#.to_vec()),
            ])
            .expect("batch 2");
        (path, first_end)
    }

    #[test]
    fn bit_flip_inside_a_committed_batch_is_corruption() {
        let dir = temp_dir();
        let (path, _) = batch_one_and_two(dir.path());
        let mut bytes = read_bytes(&path);
        let target = WAL_HEADER_LEN + 5;
        bytes[target] ^= 0x10;
        write_bytes(&path, &bytes);
        let message = corruption_message(scan_ram_wal(&path));
        assert!(message.contains("fails CRC"), "{message}");
        assert!(message.contains("bytes follow it"), "{message}");
    }

    #[test]
    fn tampered_batch_crc_is_corruption() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[
                RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec()),
                RamWalRecord::insert(2, "b", br#"{"n":2}"#.to_vec()),
            ])
            .expect("batch");

        // Flip a byte in an op frame payload, then repair that frame's own CRC
        // so only the Commit's batch_crc can notice.
        let mut bytes = read_bytes(&path);
        // Frame layout: [len][tag][seq:8][id_len:4][id:1][json...][crc].
        let target = WAL_HEADER_LEN + 4 + 9 + 4 + 1 + 2;
        bytes[target] ^= 0x01;
        let body_len = u32::from_le_bytes(
            bytes[WAL_HEADER_LEN..WAL_HEADER_LEN + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let body_start = WAL_HEADER_LEN + 4;
        let frame_crc_at = body_start + body_len;
        let new_crc = crc32(&bytes[body_start..frame_crc_at]);
        bytes[frame_crc_at..frame_crc_at + 4].copy_from_slice(&new_crc.to_le_bytes());
        write_bytes(&path, &bytes);

        let message = corruption_message(scan_ram_wal(&path));
        assert!(message.contains("batch_crc mismatch"), "{message}");
    }

    #[test]
    fn tampered_commit_batch_crc_field_is_corruption() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec())])
            .expect("batch");
        let mut bytes = read_bytes(&path);
        // Commit frame is last: [len][tag][seq][payload(24)][crc]; batch_crc is
        // the last 4 payload bytes, i.e. 8 bytes before EOF.
        let batch_crc_at = bytes.len() - 8;
        bytes[batch_crc_at] ^= 0xFF;
        // Re-seal the Commit frame so the failure is provably the batch CRC,
        // not the frame CRC.
        let frame_start = bytes.len() - 4 - 9 - COMMIT_PAYLOAD_LEN - 4;
        let body_start = frame_start + 4;
        let body_end = bytes.len() - 4;
        let new_crc = crc32(&bytes[body_start..body_end]);
        bytes[body_end..body_end + 4].copy_from_slice(&new_crc.to_le_bytes());
        write_bytes(&path, &bytes);
        let message = corruption_message(scan_ram_wal(&path));
        assert!(message.contains("batch_crc mismatch"), "{message}");
    }

    #[test]
    fn commit_frame_count_mismatch_is_corruption() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{"n":1}"#.to_vec())])
            .expect("batch");
        let mut bytes = read_bytes(&path);
        let frame_start = bytes.len() - 4 - 9 - COMMIT_PAYLOAD_LEN - 4;
        let count_at = frame_start + 4 + 9 + 16;
        bytes[count_at..count_at + 4].copy_from_slice(&7u32.to_le_bytes());
        let body_start = frame_start + 4;
        let body_end = bytes.len() - 4;
        let new_crc = crc32(&bytes[body_start..body_end]);
        bytes[body_end..body_end + 4].copy_from_slice(&new_crc.to_le_bytes());
        write_bytes(&path, &bytes);
        let message = corruption_message(scan_ram_wal(&path));
        assert!(message.contains("claims 7 frames"), "{message}");
    }

    #[test]
    fn corrupt_wal_header_is_corruption() {
        let dir = temp_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut writer =
            RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("create writer");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{}"#.to_vec())])
            .expect("batch");
        drop(writer);
        let mut bytes = read_bytes(&path);
        bytes[12] ^= 0x01;
        write_bytes(&path, &bytes);
        let message = corruption_message(scan_ram_wal(&path));
        assert!(message.contains("header CRC mismatch"), "{message}");
    }

    #[test]
    fn repair_quarantines_a_mid_log_corruption_and_recovers_the_prefix() {
        let dir = temp_dir();
        let (path, first_end) = batch_one_and_two(dir.path());
        let original = read_bytes(&path);
        let mut damaged = original.clone();
        damaged[first_end as usize + 6] ^= 0x20;
        write_bytes(&path, &damaged);

        assert!(
            scan_ram_wal(&path).is_err(),
            "damage must be loud before repair"
        );
        let report = repair_ram_wal(&path, &RealIo).expect("repair");
        assert!(report.was_corrupt);
        assert!(report.issue.is_some());
        assert_eq!(report.good_offset, first_end);
        assert_eq!(report.truncated_bytes, original.len() as u64 - first_end);
        assert_eq!(report.last_valid_seq, 1);
        assert_eq!(report.batches_kept, 1);
        let quarantine = report.quarantine_path.expect("quarantine copy");
        assert_eq!(read_bytes(&quarantine), damaged);
        assert_eq!(read_bytes(&path).len() as u64, first_end);

        let scan = scan_ram_wal(&path).expect("scan after repair");
        assert_eq!(scan.batches.len(), 1);
        assert_eq!(scan.torn_tail_offset, None);
        let mut writer = RamWalWriter::create_or_open(&path, 0, Arc::new(RealIo)).expect("reopen");
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(2, "d", br#"{"n":4}"#.to_vec())])
            .expect("append after repair");
    }

    #[test]
    fn repair_on_a_clean_wal_is_a_no_op() {
        let dir = temp_dir();
        let (path, _) = batch_one_and_two(dir.path());
        let before = read_bytes(&path);
        let report = repair_ram_wal(&path, &RealIo).expect("repair");
        assert!(!report.was_corrupt);
        assert_eq!(report.truncated_bytes, 0);
        assert_eq!(report.quarantine_path, None);
        assert_eq!(report.batches_kept, 2);
        assert_eq!(report.last_valid_seq, 3);
        assert_eq!(read_bytes(&path), before);
    }

    #[test]
    fn repair_clears_a_torn_tail_with_quarantine() {
        let dir = temp_dir();
        let (path, first_end) = batch_one_and_two(dir.path());
        // Cut the second batch mid-frame, keeping the first intact.
        let bytes = read_bytes(&path);
        write_bytes(&path, &bytes[..first_end as usize + 12]);
        let report = repair_ram_wal(&path, &RealIo).expect("repair");
        assert!(!report.was_corrupt, "a torn tail is not corruption");
        assert_eq!(report.good_offset, first_end);
        assert!(report.truncated_bytes > 0);
        assert!(report.quarantine_path.is_some());
        assert_eq!(scan_ram_wal(&path).expect("scan").torn_tail_offset, None);
    }

    // ── Manifest ──────────────────────────────────────────────────────────

    #[test]
    fn manifest_round_trips_including_pending_wal_gen() {
        let dir = temp_dir();
        let mut value = manifest();
        write_ram_manifest_atomic(dir.path(), &value, &RealIo).expect("write");
        assert_eq!(read_ram_manifest(dir.path()).expect("read"), value);

        value.pending_wal_gen = Some(5);
        value.wal_gen = 4;
        write_ram_manifest_atomic(dir.path(), &value, &RealIo).expect("write pending");
        let loaded = read_ram_manifest(dir.path()).expect("read pending");
        assert_eq!(loaded.pending_wal_gen, Some(5));

        let text = String::from_utf8(read_bytes(&ram_manifest_path(dir.path()))).expect("utf8");
        assert!(
            !text.contains("null"),
            "pending None is omitted, not null: {text}"
        );
    }

    #[test]
    fn manifest_omits_an_absent_pending_generation() {
        let dir = temp_dir();
        write_ram_manifest_atomic(dir.path(), &manifest(), &RealIo).expect("write");
        let text = String::from_utf8(read_bytes(&ram_manifest_path(dir.path()))).expect("utf8");
        assert!(!text.contains("pending_wal_gen"), "{text}");
    }

    #[test]
    fn manifest_rejects_a_foreign_version_and_unknown_fields() {
        let dir = temp_dir();
        let mut value = manifest();
        value.version = 2;
        let error = write_ram_manifest_atomic(dir.path(), &value, &RealIo).expect_err("version");
        assert!(error.to_string().contains("expected 3"), "{error}");

        write_ram_manifest_atomic(dir.path(), &manifest(), &RealIo).expect("write");
        let path = ram_manifest_path(dir.path());
        let mut text = String::from_utf8(read_bytes(&path)).expect("utf8");
        text = text.replace("\"base_gen\"", "\"unexpected_field\": 1,\n  \"base_gen\"");
        write_bytes(&path, text.as_bytes());
        let message = corruption_message(read_ram_manifest(dir.path()));
        assert!(message.contains("unknown field"), "{message}");
    }

    #[test]
    fn manifest_rejects_pending_equal_to_the_live_wal() {
        let dir = temp_dir();
        let mut value = manifest();
        value.pending_wal_gen = Some(value.wal_gen);
        let error = write_ram_manifest_atomic(dir.path(), &value, &RealIo).expect_err("pending");
        assert!(
            error
                .to_string()
                .contains("never targets the live generation"),
            "{error}"
        );
    }

    // ── fsck ──────────────────────────────────────────────────────────────

    fn clean_dir() -> (tempfile::TempDir, RamManifest) {
        let dir = temp_dir();
        let base_seq = 3;
        let docs = vec![record("a", 1, r#"{"n":1}"#), record("b", 3, r#"{"n":2}"#)];
        write_ram_base_file(&base_path(dir.path()), base_seq, &docs, &RealIo).expect("base");
        let mut writer = open_writer(dir.path(), 0);
        writer
            .append_batch_and_sync(&[
                RamWalRecord::insert(4, "c", br#"{"n":3}"#.to_vec()),
                RamWalRecord::insert(5, "d", br#"{"n":4}"#.to_vec()),
            ])
            .expect("wal");
        let manifest = RamManifest {
            version: RAM_STORAGE_VERSION,
            source_epoch_uuid: "6f1c5c1e-0000-4000-8000-000000000002".to_string(),
            base_gen: 0,
            wal_gen: 0,
            pending_wal_gen: None,
            last_seq: 5,
            min_available_seq: 1,
            indexes: vec![IndexSpec {
                field: "tag".to_string(),
                kind: IndexKind::Hash,
            }],
        };
        write_ram_manifest_atomic(dir.path(), &manifest, &RealIo).expect("manifest");
        (dir, manifest)
    }

    #[test]
    fn fsck_accepts_a_clean_directory() {
        let (dir, manifest) = clean_dir();
        let report = fsck_ram_dir(dir.path()).expect("fsck");
        assert_eq!(report.base_gen, manifest.base_gen);
        assert_eq!(report.base_seq, 3);
        assert_eq!(report.doc_count, 2);
        assert_eq!(report.wal_batches, 1);
        assert_eq!(report.last_seq, 5);
        assert_eq!(report.pending_wal_gen, None);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn fsck_detects_a_sequence_hole_in_the_wal() {
        let (dir, mut manifest) = clean_dir();
        let path = wal_file_path(dir.path(), 0);
        std::fs::remove_file(&path).expect("remove wal");
        let mut writer = open_writer(dir.path(), 0);
        writer
            .append_batch_and_sync(&[RamWalRecord::insert(6, "c", br#"{"n":3}"#.to_vec())])
            .expect("wal");
        manifest.last_seq = 6;
        write_ram_manifest_atomic(dir.path(), &manifest, &RealIo).expect("manifest");
        let error = fsck_ram_dir(dir.path()).expect_err("hole");
        assert!(error.to_string().contains("starts at seq 6"), "{error}");
    }

    #[test]
    fn fsck_detects_a_last_seq_mismatch() {
        let (dir, mut manifest) = clean_dir();
        manifest.last_seq = 9;
        write_ram_manifest_atomic(dir.path(), &manifest, &RealIo).expect("manifest");
        let error = fsck_ram_dir(dir.path()).expect_err("mismatch");
        assert!(
            error.to_string().contains("lineage reaches seq 5"),
            "{error}"
        );
    }

    #[test]
    fn fsck_tolerates_a_manifest_that_lags_the_wal() {
        // `last_seq` is published by compaction only, so a WAL that has
        // committed past it is the normal state between compactions.
        let (dir, mut manifest) = clean_dir();
        manifest.last_seq = 4;
        write_ram_manifest_atomic(dir.path(), &manifest, &RealIo).expect("manifest");
        let report = fsck_ram_dir(dir.path()).expect("a lagging manifest is not corruption");
        assert_eq!(report.last_seq, 5);
        assert!(
            report.warnings.iter().any(|w| w.contains("lags the lineage")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn fsck_detects_a_corrupt_wal() {
        let (dir, _) = clean_dir();
        let path = wal_file_path(dir.path(), 0);
        let mut bytes = read_bytes(&path);
        bytes[WAL_HEADER_LEN + 6] ^= 0x08;
        write_bytes(&path, &bytes);
        assert!(fsck_ram_dir(dir.path()).is_err());
    }

    #[test]
    fn fsck_detects_a_truncated_base() {
        let (dir, _) = clean_dir();
        let path = base_path(dir.path());
        let bytes = read_bytes(&path);
        write_bytes(&path, &bytes[..bytes.len() - 3]);
        assert!(fsck_ram_dir(dir.path()).is_err());
    }

    #[test]
    fn fsck_walks_a_pending_generation() {
        let (dir, mut manifest) = clean_dir();
        let mut pending =
            RamWalWriter::create_or_open(&wal_file_path(dir.path(), 1), 1, Arc::new(RealIo))
                .expect("pending writer");
        pending
            .append_batch_and_sync(&[RamWalRecord::insert(6, "e", br#"{"n":5}"#.to_vec())])
            .expect("pending batch");
        manifest.pending_wal_gen = Some(1);
        manifest.last_seq = 5;
        write_ram_manifest_atomic(dir.path(), &manifest, &RealIo).expect("manifest");
        let report = fsck_ram_dir(dir.path()).expect("fsck");
        assert_eq!(report.pending_wal_gen, Some(1));
        assert_eq!(report.pending_batches, Some(1));
        assert_eq!(report.last_seq, 6);
    }

    // ── FaultIo: manifest publication ─────────────────────────────────────

    #[cfg(any(test, feature = "fault-io"))]
    mod fault_io {
        use super::*;
        use crate::io::{FaultAction, FaultIo, FaultPlan};

        fn armed(op: IoOp) -> FaultIo {
            let io = FaultIo::new();
            io.arm(FaultPlan {
                op,
                trigger_at: 1,
                action: FaultAction::Error(std::io::ErrorKind::Other),
            });
            io
        }

        #[test]
        fn manifest_write_failures_leave_the_old_manifest_intact() {
            for op in [IoOp::WriteAll, IoOp::SyncAll, IoOp::Rename] {
                let dir = temp_dir();
                let old = manifest();
                write_ram_manifest_atomic(dir.path(), &old, &RealIo).expect("seed manifest");

                let mut next = old.clone();
                next.last_seq = 99;
                let io = armed(op);
                let error = write_ram_manifest_atomic(dir.path(), &next, &io)
                    .expect_err("the armed fault must fail the write");
                assert!(error.to_string().contains("manifest"), "{op:?}: {error}");
                assert_eq!(io.calls(op), 1, "{op:?} was not the failing step");
                assert_eq!(
                    read_ram_manifest(dir.path()).expect("old manifest survives"),
                    old,
                    "{op:?} must leave the previous manifest in place"
                );
            }
        }

        #[test]
        fn manifest_dir_sync_failure_fails_loud_after_an_atomic_rename() {
            let dir = temp_dir();
            let old = manifest();
            write_ram_manifest_atomic(dir.path(), &old, &RealIo).expect("seed manifest");
            let mut next = old.clone();
            next.last_seq = 42;
            let io = armed(IoOp::SyncParentDir);
            let error = write_ram_manifest_atomic(dir.path(), &next, &io)
                .expect_err("directory fsync failure must surface");
            assert!(error.to_string().contains("sync directory"), "{error}");
            // The rename has already happened; the guarantee is atomicity —
            // a complete, valid manifest either way, never a mixture.
            let loaded = read_ram_manifest(dir.path()).expect("manifest is complete");
            assert!(loaded == old || loaded == next);
        }

        #[test]
        fn base_write_failures_never_publish_a_partial_file() {
            for op in [IoOp::WriteAll, IoOp::SyncAll, IoOp::Rename] {
                let dir = temp_dir();
                let path = base_path(dir.path());
                let docs = sample_docs();
                let io = FaultIo::new();
                io.arm(FaultPlan {
                    op,
                    trigger_at: 1,
                    action: FaultAction::Error(std::io::ErrorKind::Other),
                });
                assert!(
                    write_ram_base_file(&path, 3, &docs, &io).is_err(),
                    "{op:?} must fail the base write"
                );
                assert_eq!(io.calls(op), 1);
                assert!(!path.exists(), "{op:?} must not publish a partial base");
            }
        }

        #[test]
        fn wal_header_failures_are_loud_and_create_no_batch() {
            let dir = temp_dir();
            let path = wal_file_path(dir.path(), 0);
            let io = FaultIo::new();
            io.arm(FaultPlan {
                op: IoOp::WriteAll,
                trigger_at: 1,
                action: FaultAction::Error(std::io::ErrorKind::Other),
            });
            let error = RamWalWriter::create_or_open(&path, 0, Arc::new(io))
                .expect_err("header write must fail");
            assert!(
                error.to_string().contains("write ram WAL header"),
                "{error}"
            );
        }

        #[test]
        fn wal_append_failure_surfaces_and_stays_on_disk_untouched() {
            let dir = temp_dir();
            let path = wal_file_path(dir.path(), 0);
            let io = Arc::new(FaultIo::new());
            let mut writer =
                RamWalWriter::create_or_open(&path, 0, io.clone()).expect("create writer");
            let before = read_bytes(&path);
            io.arm(FaultPlan {
                op: IoOp::SyncAll,
                trigger_at: io.calls(IoOp::SyncAll) + 1,
                action: FaultAction::Error(std::io::ErrorKind::Other),
            });
            let error = writer
                .append_batch_and_sync(&[RamWalRecord::insert(1, "a", br#"{}"#.to_vec())])
                .expect_err("sync failure must surface");
            assert!(error.to_string().contains("sync ram WAL batch"), "{error}");
            // The bytes reached the file, but no Commit may be trusted without
            // the fsync: the caller has to poison, not retry blindly.
            assert_eq!(writer.last_seq(), 0);
            assert!(read_bytes(&path).len() > before.len());
        }
    }
}
