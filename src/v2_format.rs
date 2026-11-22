//! nDB v2 binary storage building blocks.
//!
//! Three artefacts, one lineage, all CRC-32 protected:
//!
//! - `manifest.json` — the **sole generation and lineage authority**. Nothing
//!   derives a generation number, epoch or sequence from a file name; it is
//!   read from here, or the open fails.
//! - `base.gen_<N>.ndb` — an **immutable** mmap-able snapshot: 64-byte header
//!   field block + CRC, packed UTF-8 JSON document bytes sorted by id, a sorted
//!   id directory and the persisted hash/BTree index sections.
//! - `wal.gen_<N>.ndb` — an append-only CRC32-framed log of post-mutation
//!   documents, tombstones and index changes.
//!
//! Every parser validates its CRC and structural bounds before exposing data,
//! and reports the exact byte offset and frame index when it refuses. There is
//! no "skip the bad record" path: a torn tail is either explicitly truncated
//! (`repair_tail` / [`repair_wal_file`]) or reported as corruption.

use crate::crc32::{crc32, Crc32Hasher};
use crate::error::{Error, Result};
use crate::mmap::MmapFile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// On-disk format version of every v2 artefact.
pub const STORAGE_VERSION: u32 = 2;

#[cfg(not(target_endian = "little"))]
compile_error!(
    "nDB's v2 base format stores little-endian integers and exposes zero-copy `&[u32]` index slot views; a big-endian target is not supported"
);

/// Name of the generation manifest inside a v2 database folder.
pub const MANIFEST_FILE: &str = "manifest.json";
/// Temporary file used for the atomic manifest rename.
pub const MANIFEST_TMP_FILE: &str = "manifest.json.tmp";

/// Base-file magic (`NDB2BASE`).
pub const BASE_MAGIC: [u8; 8] = *b"NDB2BASE";
/// WAL-file magic (`NDB2WAL\0`, 8 bytes on disk).
pub const WAL_MAGIC: [u8; 8] = *b"NDB2WAL\0";

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
pub fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(MANIFEST_FILE)
}

// ─── Manifest ───────────────────────────────────────────────────────

/// One persisted secondary index declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V2IndexSpec {
    /// Document field the index is built on.
    pub field: String,
    /// Index structure.
    pub kind: V2IndexKind,
}

/// The two index structures nDB persists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum V2IndexKind {
    /// O(1) equality lookups.
    #[serde(rename = "hash")]
    Hash,
    /// Ordered range scans.
    #[serde(rename = "btree")]
    BTree,
}

impl V2IndexKind {
    /// Binary tag used in the persisted index section.
    pub fn tag(self) -> u8 {
        match self {
            V2IndexKind::Hash => 1,
            V2IndexKind::BTree => 2,
        }
    }

    /// Parse the binary tag; `None` for an unknown value.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(V2IndexKind::Hash),
            2 => Some(V2IndexKind::BTree),
            _ => None,
        }
    }
}

/// The generation and lineage authority of a v2 database folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct V2Manifest {
    /// Must be [`STORAGE_VERSION`].
    pub format_version: u32,
    /// Active base generation (`base.gen_<N>.ndb`).
    pub base_gen: u64,
    /// Active WAL generation (`wal.gen_<N>.ndb`).
    pub wal_gen: u64,
    /// Canonical RFC 4122 UUIDv4 of the journal's history.
    pub source_epoch_uuid: String,
    /// Highest sequence folded into the base.
    pub base_seq: u64,
    /// Oldest sequence still replayable through the WAL.
    pub min_available_seq: u64,
    /// Documents contained in the base generation.
    pub doc_count: u64,
    /// Persisted secondary indexes.
    pub indexes: Vec<V2IndexSpec>,
}

/// Read and validate `manifest.json` from `dir`.
///
/// Fails loud on a missing file, malformed JSON, an unknown field, a version
/// other than 2, an empty epoch or a duplicate index declaration.
pub fn read_manifest(dir: &Path) -> Result<V2Manifest> {
    let path = manifest_path(dir);
    let bytes = fs::read(&path).map_err(Error::io_err(&path, "read v2 manifest"))?;
    let manifest: V2Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| Error::corruption(&path, format!("manifest is not valid v2 JSON: {e}")))?;
    validate_manifest(&manifest, &path)?;
    Ok(manifest)
}

/// Write `manifest.json` atomically: temp file, `sync_all`, rename, parent fsync.
pub fn write_manifest_atomic(dir: &Path, manifest: &V2Manifest) -> Result<()> {
    let path = manifest_path(dir);
    validate_manifest(manifest, &path)?;
    let mut json = serde_json::to_vec_pretty(manifest)
        .map_err(|e| Error::Serialization(format!("serialize v2 manifest: {e}")))?;
    json.push(b'\n');
    write_atomic(&path, &json, "v2 manifest")
}

fn validate_manifest(manifest: &V2Manifest, path: &Path) -> Result<()> {
    if manifest.format_version != STORAGE_VERSION {
        return Err(Error::corruption(
            path,
            format!(
                "manifest format_version is {}, expected {STORAGE_VERSION}",
                manifest.format_version
            ),
        ));
    }
    if manifest.source_epoch_uuid.is_empty() {
        return Err(Error::corruption(path, "manifest source_epoch_uuid is empty"));
    }
    if manifest.min_available_seq > manifest.base_seq + 1 {
        return Err(Error::corruption(
            path,
            format!(
                "manifest min_available_seq {} is beyond base_seq {} + 1: history would have a hole",
                manifest.min_available_seq, manifest.base_seq
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

// ─── Ordered values ─────────────────────────────────────────────────

/// Total order over JSON values, matching `lib.rs`'s `OrderedValue`.
///
/// `Null < Bool(false) < Bool(true) < Number < String < Composite`, numbers by
/// `f64::total_cmp` (`-0.0 < 0.0`, NaN has a fixed place), strings and
/// canonical composite JSON by byte order.
#[derive(Clone, Debug)]
pub enum OrderedValue {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// JSON number, compared as `f64`.
    Number(f64),
    /// JSON string.
    String(String),
    /// Array or object, compared by its canonical JSON text.
    Composite(String),
}

impl OrderedValue {
    /// Convert a JSON value the same way the in-memory indexes do.
    pub fn from_json(value: &Value) -> Self {
        match value {
            Value::Null => OrderedValue::Null,
            Value::Bool(flag) => OrderedValue::Bool(*flag),
            Value::Number(number) => OrderedValue::Number(
                number
                    .as_f64()
                    .expect("serde_json number is not representable as f64"),
            ),
            Value::String(text) => OrderedValue::String(text.clone()),
            other => OrderedValue::Composite(other.to_string()),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            OrderedValue::Null => 0,
            OrderedValue::Bool(_) => 1,
            OrderedValue::Number(_) => 2,
            OrderedValue::String(_) => 3,
            OrderedValue::Composite(_) => 4,
        }
    }

    /// Deterministic binary encoding: tag byte plus payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Append the deterministic binary encoding to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            OrderedValue::Null => put_u8(out, 0),
            OrderedValue::Bool(flag) => {
                put_u8(out, 1);
                put_u8(out, u8::from(*flag));
            }
            OrderedValue::Number(number) => {
                put_u8(out, 2);
                out.extend_from_slice(&number.to_bits().to_le_bytes());
            }
            OrderedValue::String(text) => {
                put_u8(out, 3);
                put_u32(out, length_u32(text.len(), "string key"));
                out.extend_from_slice(text.as_bytes());
            }
            OrderedValue::Composite(json) => {
                put_u8(out, 4);
                put_u32(out, length_u32(json.len(), "composite key"));
                out.extend_from_slice(json.as_bytes());
            }
        }
    }

    /// Decode exactly one value from `bytes`, which must hold nothing else.
    pub fn decode(bytes: &[u8], path: &Path) -> Result<OrderedValue> {
        let mut reader = Reader::new(bytes, path);
        let value = OrderedValue::decode_from(&mut reader)?;
        if reader.remaining() != 0 {
            return Err(Error::corruption(
                path,
                format!(
                    "ordered-value key has {} trailing bytes",
                    reader.remaining()
                ),
            ));
        }
        Ok(value)
    }

    fn decode_from(reader: &mut Reader<'_>) -> Result<OrderedValue> {
        let tag = reader.u8("ordered-value tag")?;
        match tag {
            0 => Ok(OrderedValue::Null),
            1 => match reader.u8("ordered-value bool")? {
                0 => Ok(OrderedValue::Bool(false)),
                1 => Ok(OrderedValue::Bool(true)),
                other => Err(Error::corruption(
                    reader.file,
                    format!("ordered-value bool tag is {other}, expected 0 or 1"),
                )),
            },
            2 => {
                let bits = reader.u64("ordered-value number bits")?;
                Ok(OrderedValue::Number(f64::from_bits(bits)))
            }
            3 => {
                let len = reader.u32("ordered-value string length")? as usize;
                let raw = reader.take(len, "ordered-value string")?;
                let text = std::str::from_utf8(raw).map_err(|e| {
                    Error::corruption(
                        reader.file,
                        format!("ordered-value string is not UTF-8: {e}"),
                    )
                })?;
                Ok(OrderedValue::String(text.to_string()))
            }
            4 => {
                let len = reader.u32("ordered-value composite length")? as usize;
                let raw = reader.take(len, "ordered-value composite")?;
                let text = std::str::from_utf8(raw).map_err(|e| {
                    Error::corruption(
                        reader.file,
                        format!("ordered-value composite is not UTF-8: {e}"),
                    )
                })?;
                Ok(OrderedValue::Composite(text.to_string()))
            }
            other => Err(Error::corruption(
                reader.file,
                format!("unknown ordered-value tag {other}"),
            )),
        }
    }
}

impl PartialEq for OrderedValue {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for OrderedValue {}

impl PartialOrd for OrderedValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (OrderedValue::Null, OrderedValue::Null) => Ordering::Equal,
            (OrderedValue::Bool(a), OrderedValue::Bool(b)) => a.cmp(b),
            (OrderedValue::Number(a), OrderedValue::Number(b)) => a.total_cmp(b),
            (OrderedValue::String(a), OrderedValue::String(b)) => a.cmp(b),
            (OrderedValue::Composite(a), OrderedValue::Composite(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl std::hash::Hash for OrderedValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.rank().hash(state);
        match self {
            OrderedValue::Null => {}
            OrderedValue::Bool(flag) => flag.hash(state),
            OrderedValue::Number(number) => number.to_bits().hash(state),
            OrderedValue::String(text) | OrderedValue::Composite(text) => text.hash(state),
        }
    }
}

// ─── Little-endian helpers ──────────────────────────────────────────

fn put_u8(buf: &mut Vec<u8>, value: u8) {
    buf.push(value);
}

fn put_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, value: u32) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn length_u32(len: usize, what: &str) -> u32 {
    u32::try_from(len).unwrap_or_else(|_| panic!("{what} is {len} bytes, which exceeds u32"))
}

/// Bounds-checked little-endian reader that names the file and offset in every
/// error.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    file: &'a Path,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8], file: &'a Path) -> Self {
        Reader { buf, pos: 0, file }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn position(&self) -> usize {
        self.pos
    }

    fn take(&mut self, len: usize, what: &str) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(len).ok_or_else(|| {
            Error::corruption(
                self.file,
                format!("{what}: length overflow at byte {}", self.pos),
            )
        })?;
        if end > self.buf.len() {
            return Err(Error::corruption(
                self.file,
                format!(
                    "{what}: need {len} bytes at byte {}, only {} remain",
                    self.pos,
                    self.buf.len() - self.pos
                ),
            ));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self, what: &str) -> Result<u8> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &str) -> Result<u16> {
        let raw = self.take(2, what)?;
        Ok(u16::from_le_bytes([raw[0], raw[1]]))
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        let raw = self.take(4, what)?;
        Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn u64(&mut self, what: &str) -> Result<u64> {
        let raw = self.take(8, what)?;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(raw);
        Ok(u64::from_le_bytes(bytes))
    }
}

// ─── Base generation ────────────────────────────────────────────────

/// Bytes covered by the base header CRC: magic through `idx_len`.
pub const BASE_HEADER_FIELDS_LEN: usize = 64;
/// Whole base header: the 64-byte field block plus its trailing CRC-32.
///
/// The plan's field list is magic(8) + version(4) + flags(4) + base_seq(8) +
/// doc_count(8) + dir_offset(8) + dir_len(8) + idx_offset(8) + idx_len(8) =
/// 64 bytes, plus the 4-byte CRC. "CRC over the first 60 bytes" cannot cover
/// `idx_len`; the CRC covers all 64 field bytes instead.
pub const BASE_HEADER_LEN: usize = BASE_HEADER_FIELDS_LEN + 4;

/// Decoded base-file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseHeader {
    /// Format version (must be 2).
    pub version: u32,
    /// Reserved flags (must be 0; an unknown flag fails the open).
    pub flags: u32,
    /// Highest sequence folded into this base.
    pub base_seq: u64,
    /// Number of documents in the base.
    pub doc_count: u64,
    /// File offset of the directory section.
    pub dir_offset: u64,
    /// Directory section length, including its trailing CRC.
    pub dir_len: u64,
    /// File offset of the persisted index section.
    pub idx_offset: u64,
    /// Index section length, including its trailing CRC.
    pub idx_len: u64,
}

/// One document to fold into a base generation.
#[derive(Debug, Clone, Copy)]
pub struct BaseDocInput<'a> {
    /// Document id (sorted ascending in the file).
    pub id: &'a str,
    /// Sequence of the document's last mutation.
    pub doc_seq: u64,
    /// Full UTF-8 JSON document bytes.
    pub data: &'a [u8],
}

impl<'a> BaseDocInput<'a> {
    /// Construct a base document from its id, last mutation sequence and bytes.
    pub fn new(id: &'a str, doc_seq: u64, data: &'a [u8]) -> Self {
        BaseDocInput { id, doc_seq, data }
    }
}

/// One persisted index entry: a key and the document slots that carry it.
#[derive(Debug, Clone, PartialEq)]
pub struct BaseIndexEntry {
    /// Indexed key.
    pub key: OrderedValue,
    /// Document ids carrying the key. Resolved to slots while writing.
    pub ids: Vec<String>,
}

impl BaseIndexEntry {
    /// Construct an index entry.
    pub fn new(key: OrderedValue, ids: Vec<String>) -> Self {
        BaseIndexEntry { key, ids }
    }
}

/// One directory entry, borrowed from the mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BaseDirEntry<'a> {
    /// Slot number (`0..doc_count`), matching persisted index slots.
    pub slot: u32,
    /// Document id.
    pub id: &'a str,
    /// Sequence of the document's last mutation.
    pub doc_seq: u64,
    /// Absolute file offset of the document bytes.
    pub data_offset: u64,
    /// Document byte length.
    pub data_len: u32,
}

/// Write an immutable base generation atomically.
///
/// Documents may be supplied in any order; the file is always sorted by id.
/// Every document must be valid UTF-8 JSON with a unique non-empty id and a
/// `doc_seq` no higher than `base_seq`. Index entries are resolved from ids to
/// slots, so an index that names an unknown document id fails the write.
pub fn write_base_file(
    path: &Path,
    base_seq: u64,
    docs: &[BaseDocInput<'_>],
    indexes: &[(V2IndexSpec, Vec<BaseIndexEntry>)],
) -> Result<()> {
    if docs.len() > u32::MAX as usize {
        return Err(Error::invalid_arg(
            "base generation holds more documents than the 32-bit slot space",
        ));
    }

    let mut order: Vec<usize> = (0..docs.len()).collect();
    order.sort_by(|&a, &b| docs[a].id.cmp(docs[b].id));

    let mut slots: HashMap<&str, u32> = HashMap::with_capacity(docs.len());
    let mut data_offsets: Vec<u64> = Vec::with_capacity(docs.len());
    let mut offset = BASE_HEADER_LEN as u64;
    for (slot, &index) in order.iter().enumerate() {
        let doc = &docs[index];
        if doc.id.is_empty() {
            return Err(Error::invalid_arg("base document id must not be empty"));
        }
        if doc.id.len() > usize::from(u16::MAX) {
            return Err(Error::invalid_arg(format!(
                "base document id {:?} is {} bytes, the directory stores at most {}",
                doc.id,
                doc.id.len(),
                u16::MAX
            )));
        }
        if slots.insert(doc.id, slot as u32).is_some() {
            return Err(Error::invalid_arg(format!(
                "base generation contains document id {:?} twice",
                doc.id
            )));
        }
        if doc.doc_seq > base_seq {
            return Err(Error::invalid_arg(format!(
                "document {:?} has doc_seq {} above base_seq {base_seq}",
                doc.id, doc.doc_seq
            )));
        }
        if u32::try_from(doc.data.len()).is_err() {
            return Err(Error::invalid_arg(format!(
                "document {:?} is {} bytes, larger than u32",
                doc.id,
                doc.data.len()
            )));
        }
        std::str::from_utf8(doc.data).map_err(|e| {
            Error::invalid_arg(format!("document {:?} is not UTF-8: {e}", doc.id))
        })?;
        serde_json::from_slice::<Value>(doc.data).map_err(|e| {
            Error::invalid_arg(format!("document {:?} is not valid JSON: {e}", doc.id))
        })?;
        data_offsets.push(offset);
        offset += doc.data.len() as u64;
    }
    let dir_offset = offset;

    // Directory layout: a fixed-stride `u64` offset table (one entry per
    // document, relative to the packed-entry region) followed by the packed
    // entries themselves and the section CRC. The table is what makes O(1)
    // slot access and O(log n) id lookup possible without a heap copy.
    let mut entries = Vec::new();
    let mut entry_offsets: Vec<u64> = Vec::with_capacity(order.len());
    for (slot, &index) in order.iter().enumerate() {
        let doc = &docs[index];
        entry_offsets.push(entries.len() as u64);
        put_u16(
            &mut entries,
            u16::try_from(doc.id.len()).expect("id length validated above"),
        );
        entries.extend_from_slice(doc.id.as_bytes());
        put_u64(&mut entries, doc.doc_seq);
        put_u64(&mut entries, data_offsets[slot]);
        put_u32(&mut entries, doc.data.len() as u32);
    }
    let mut dir = Vec::with_capacity(entry_offsets.len() * 8 + entries.len() + 4);
    for entry_offset in &entry_offsets {
        put_u64(&mut dir, *entry_offset);
    }
    dir.extend_from_slice(&entries);
    let dir_crc = crc32(&dir);
    put_u32(&mut dir, dir_crc);
    let dir_len = dir.len() as u64;

    let idx_offset = dir_offset + dir_len;
    let idx = build_index_section(indexes, &slots, idx_offset)?;
    let idx_len = idx.len() as u64;

    let header = BaseHeader {
        version: STORAGE_VERSION,
        flags: 0,
        base_seq,
        doc_count: docs.len() as u64,
        dir_offset,
        dir_len,
        idx_offset,
        idx_len,
    };

    let mut buf = Vec::with_capacity(idx_offset as usize + idx.len());
    buf.extend_from_slice(&encode_base_header(&header));
    for &index in &order {
        buf.extend_from_slice(docs[index].data);
    }
    buf.extend_from_slice(&dir);
    buf.extend_from_slice(&idx);
    debug_assert_eq!(buf.len() as u64, idx_offset + idx_len);

    write_atomic(path, &buf, "base generation")
}

/// Pad `buf` so that its absolute file position `base + buf.len()` is a
/// multiple of `align`.
fn pad_to_alignment(buf: &mut Vec<u8>, base: usize, align: usize) {
    while (base + buf.len()) % align != 0 {
        buf.push(0);
    }
}

fn build_index_section(
    indexes: &[(V2IndexSpec, Vec<BaseIndexEntry>)],
    slots: &HashMap<&str, u32>,
    section_start: u64,
) -> Result<Vec<u8>> {
    let mut specs: Vec<&V2IndexSpec> = indexes.iter().map(|(spec, _)| spec).collect();
    specs.sort_by(|a, b| a.field.cmp(&b.field));
    for pair in specs.windows(2) {
        if pair[0].field == pair[1].field {
            return Err(Error::invalid_arg(format!(
                "base index section declares field {:?} twice",
                pair[0].field
            )));
        }
    }

    let section_base = usize::try_from(section_start)
        .map_err(|_| Error::invalid_arg("index section offset exceeds this platform"))?;
    let mut section = Vec::new();
    put_u32(&mut section, length_u32(indexes.len(), "index count"));
    for spec in specs {
        let entries = indexes
            .iter()
            .find(|(candidate, _)| candidate.field == spec.field)
            .map(|(_, entries)| entries)
            .expect("spec came from the index list");

        let mut encoded: Vec<(OrderedValue, Vec<u32>)> = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.ids.is_empty() {
                return Err(Error::invalid_arg(format!(
                    "index {:?} has an entry with no document ids",
                    spec.field
                )));
            }
            let mut entry_slots = Vec::with_capacity(entry.ids.len());
            for id in &entry.ids {
                let slot = *slots.get(id.as_str()).ok_or_else(|| {
                    Error::invalid_arg(format!(
                        "index {:?} references unknown document id {id:?}",
                        spec.field
                    ))
                })?;
                entry_slots.push(slot);
            }
            entry_slots.sort_unstable();
            entry_slots.dedup();
            encoded.push((entry.key.clone(), entry_slots));
        }
        // Sorted with the same total order the on-disk encoding is compared
        // with, so a zero-copy binary search over the mapping is exact.
        encoded.sort_by(|a, b| a.0.cmp(&b.0));
        for pair in encoded.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(Error::invalid_arg(format!(
                    "index {:?} contains the same key twice",
                    spec.field
                )));
            }
        }

        put_u8(&mut section, spec.kind.tag());
        put_u16(
            &mut section,
            u16::try_from(spec.field.len()).map_err(|_| {
                Error::invalid_arg(format!(
                    "index field {:?} is longer than u16",
                    spec.field
                ))
            })?,
        );
        section.extend_from_slice(spec.field.as_bytes());
        put_u32(
            &mut section,
            length_u32(encoded.len(), "index entry count"),
        );

        // Fixed-stride entry-offset table, 8-byte aligned in the file, so the
        // packed slot arrays that follow stay 4-byte aligned (a `&[u32]` view).
        pad_to_alignment(&mut section, section_base, 8);
        let table_pos = section.len();
        section.resize(table_pos + encoded.len() * 8, 0);
        let entries_pos = section.len();
        let mut entry_offsets: Vec<u64> = Vec::with_capacity(encoded.len());
        for (key, entry_slots) in &encoded {
            entry_offsets.push((section.len() - entries_pos) as u64);
            let key_bytes = key.encode();
            put_u32(
                &mut section,
                length_u32(key_bytes.len(), "index key length"),
            );
            section.extend_from_slice(&key_bytes);
            while (section.len() - entries_pos) % 4 != 0 {
                section.push(0);
            }
            put_u32(
                &mut section,
                length_u32(entry_slots.len(), "index slot count"),
            );
            for slot in entry_slots {
                put_u32(&mut section, *slot);
            }
        }
        for (index, offset) in entry_offsets.iter().enumerate() {
            let at = table_pos + index * 8;
            section[at..at + 8].copy_from_slice(&offset.to_le_bytes());
        }
    }
    let crc = crc32(&section);
    put_u32(&mut section, crc);
    Ok(section)
}

fn encode_base_header(header: &BaseHeader) -> [u8; BASE_HEADER_LEN] {
    let mut buf = Vec::with_capacity(BASE_HEADER_LEN);
    buf.extend_from_slice(&BASE_MAGIC);
    put_u32(&mut buf, header.version);
    put_u32(&mut buf, header.flags);
    put_u64(&mut buf, header.base_seq);
    put_u64(&mut buf, header.doc_count);
    put_u64(&mut buf, header.dir_offset);
    put_u64(&mut buf, header.dir_len);
    put_u64(&mut buf, header.idx_offset);
    put_u64(&mut buf, header.idx_len);
    debug_assert_eq!(buf.len(), BASE_HEADER_FIELDS_LEN);
    let crc = crc32(&buf);
    put_u32(&mut buf, crc);
    let mut out = [0u8; BASE_HEADER_LEN];
    out.copy_from_slice(&buf);
    out
}

fn decode_base_header(bytes: &[u8], path: &Path) -> Result<BaseHeader> {
    if bytes.len() < BASE_HEADER_LEN {
        return Err(Error::corruption(
            path,
            format!(
                "base file is {} bytes, shorter than its {BASE_HEADER_LEN}-byte header",
                bytes.len()
            ),
        ));
    }
    let stored_crc = u32::from_le_bytes(
        bytes[BASE_HEADER_FIELDS_LEN..BASE_HEADER_LEN]
            .try_into()
            .expect("4-byte slice"),
    );
    let computed_crc = crc32(&bytes[..BASE_HEADER_FIELDS_LEN]);
    if stored_crc != computed_crc {
        return Err(Error::corruption(
            path,
            format!(
                "base header CRC mismatch: stored {stored_crc:08x}, computed {computed_crc:08x}"
            ),
        ));
    }

    let mut reader = Reader::new(&bytes[..BASE_HEADER_FIELDS_LEN], path);
    let magic = reader.take(8, "base magic")?;
    if magic != BASE_MAGIC {
        return Err(Error::corruption(
            path,
            format!("bad base magic {magic:?}, expected {BASE_MAGIC:?}"),
        ));
    }
    let version = reader.u32("base format version")?;
    if version != STORAGE_VERSION {
        return Err(Error::corruption(
            path,
            format!("base format version is {version}, expected {STORAGE_VERSION}"),
        ));
    }
    let flags = reader.u32("base flags")?;
    if flags != 0 {
        return Err(Error::corruption(
            path,
            format!("base header has unknown flags 0x{flags:08x}"),
        ));
    }
    Ok(BaseHeader {
        version,
        flags,
        base_seq: reader.u64("base base_seq")?,
        doc_count: reader.u64("base doc_count")?,
        dir_offset: reader.u64("base dir_offset")?,
        dir_len: reader.u64("base dir_len")?,
        idx_offset: reader.u64("base idx_offset")?,
        idx_len: reader.u64("base idx_len")?,
    })
}

/// One persisted index, held as a lightweight header over the mapping.
///
/// Open reads the header and the fixed-stride entry-offset table only: no
/// entry is decoded, so a base with 200 000 index entries costs a few hundred
/// bytes of heap, not 200 000 cloned keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedIndex {
    /// The declared index.
    pub spec: V2IndexSpec,
    entry_count: u32,
    table_offset: usize,
    entries_offset: usize,
    entries_end: usize,
}

impl PersistedIndex {
    /// Number of key entries in this index.
    pub fn entry_count(&self) -> u32 {
        self.entry_count
    }
}

/// One end of an encoded index range (owned, so the caller owns no borrow).
#[derive(Debug, Clone)]
pub enum KeyEnd {
    /// No bound.
    Unbounded,
    /// Inclusive bound.
    Included(OrderedValue),
    /// Exclusive bound.
    Excluded(OrderedValue),
}

/// A validated, memory-mapped base generation.
///
/// Everything below the header is borrowed from the mapping: the id directory
/// lives behind a fixed-stride offset table so `find_slot` is a binary search
/// over string slices, and every persisted index is queried by binary search
/// over its own offset table. Neither path allocates per document or per
/// index entry.
#[derive(Debug)]
pub struct MmapBase {
    mmap: MmapFile,
    header: BaseHeader,
    dir_entries: usize,
    dir_entries_end: usize,
    indexes: Vec<PersistedIndex>,
}

impl MmapBase {
    /// Map and fully validate a base generation.
    ///
    /// Validates magic, version, unknown flags, header CRC, section layout,
    /// directory CRC, index CRC and both sections' structure. Nothing is
    /// exposed before every check has passed.
    pub fn open(path: &Path) -> Result<MmapBase> {
        let mmap = MmapFile::open(path)?;
        let len = mmap.len() as u64;
        let header = decode_base_header(mmap.as_slice(), path)?;

        if header.dir_offset < BASE_HEADER_LEN as u64 {
            return Err(Error::corruption(
                path,
                format!(
                    "directory starts at byte {}, inside the {BASE_HEADER_LEN}-byte header",
                    header.dir_offset
                ),
            ));
        }
        let dir_table_len = 8u64
            .checked_mul(header.doc_count)
            .ok_or_else(|| Error::corruption(path, "directory offset table length overflows u64"))?;
        if header.dir_len < 4 || header.dir_len - 4 < dir_table_len {
            return Err(Error::corruption(
                path,
                format!(
                    "directory section is {} bytes, needs at least {} for {} entries",
                    header.dir_len,
                    dir_table_len + 4,
                    header.doc_count
                ),
            ));
        }
        let dir_end = header.dir_offset.checked_add(header.dir_len).ok_or_else(|| {
            Error::corruption(path, "directory section offset overflows u64")
        })?;
        if dir_end > len {
            return Err(Error::corruption(
                path,
                format!("directory section ends at {dir_end}, past the {len}-byte file"),
            ));
        }
        if header.idx_offset != dir_end {
            return Err(Error::corruption(
                path,
                format!(
                    "index section starts at {} but the directory ends at {dir_end}",
                    header.idx_offset
                ),
            ));
        }
        if header.idx_len < 8 {
            return Err(Error::corruption(
                path,
                format!("index section is {} bytes, needs at least 8", header.idx_len),
            ));
        }
        let idx_end = header.idx_offset.checked_add(header.idx_len).ok_or_else(|| {
            Error::corruption(path, "index section offset overflows u64")
        })?;
        if idx_end != len {
            return Err(Error::corruption(
                path,
                format!(
                    "file is {len} bytes but its sections end at {idx_end}: trailing or missing bytes"
                ),
            ));
        }
        if header.doc_count > u64::from(u32::MAX) {
            return Err(Error::corruption(
                path,
                format!(
                    "base declares {} documents, beyond the 32-bit slot space",
                    header.doc_count
                ),
            ));
        }

        let bytes = mmap.as_slice();
        let dir_payload =
            &bytes[header.dir_offset as usize..(dir_end - 4) as usize];
        let dir_crc = u32::from_le_bytes(
            bytes[(dir_end - 4) as usize..dir_end as usize]
                .try_into()
                .expect("4-byte slice"),
        );
        let computed = crc32(dir_payload);
        if dir_crc != computed {
            return Err(Error::corruption(
                path,
                format!("directory CRC mismatch: stored {dir_crc:08x}, computed {computed:08x}"),
            ));
        }

        let idx_payload =
            &bytes[header.idx_offset as usize..(idx_end - 4) as usize];
        let idx_crc = u32::from_le_bytes(
            bytes[(idx_end - 4) as usize..idx_end as usize]
                .try_into()
                .expect("4-byte slice"),
        );
        let computed = crc32(idx_payload);
        if idx_crc != computed {
            return Err(Error::corruption(
                path,
                format!("index section CRC mismatch: stored {idx_crc:08x}, computed {computed:08x}"),
            ));
        }

        let dir_entries = (header.dir_offset as usize)
            .checked_add((header.doc_count as usize) * 8)
            .ok_or_else(|| Error::corruption(path, "directory offset table overflows usize"))?;
        let dir_entries_end = (dir_end - 4) as usize;
        let indexes = parse_index_section(
            idx_payload,
            header.idx_offset as usize,
            path,
            header.doc_count,
        )?;
        let base = MmapBase {
            mmap,
            header,
            dir_entries,
            dir_entries_end,
            indexes,
        };
        base.validate()?;
        Ok(base)
    }

    /// Validate the directory (offset table plus packed entries) and every
    /// persisted index's structure and key order.
    ///
    /// Validation reads the mapping only; it allocates nothing per document or
    /// per index entry, so it stays O(bytes touched) rather than
    /// O(documents × heap objects).
    fn validate(&self) -> Result<()> {
        let path = self.mmap.path();
        let mut previous: Option<&str> = None;
        let mut pos = 0usize;
        let mut count = 0u32;
        while self.dir_entries + pos < self.dir_entries_end {
            if count >= self.slot_count() {
                return Err(Error::corruption(
                    path,
                    format!(
                        "base header declares {} documents but the directory holds more entries",
                        self.slot_count()
                    ),
                ));
            }
            let declared_offset = self.entry_offset_raw(count)?;
            if declared_offset as usize != pos {
                return Err(Error::corruption(
                    path,
                    format!(
                        "directory offset table disagrees with the packed entries: slot {count} starts at {declared_offset}, expected {pos}"
                    ),
                ));
            }
            let span = self.entry_span(pos)?;
            if self.dir_entries + pos + span > self.dir_entries_end {
                return Err(Error::corruption(
                    path,
                    format!("directory entry {count} overruns the directory section"),
                ));
            }
            let entry = self.entry_at(count)?;
            if let Some(previous) = previous {
                if previous >= entry.id {
                    return Err(Error::corruption(
                        path,
                        format!(
                            "directory ids are not strictly ascending: {previous:?} then {:?} at entry {count}",
                            entry.id
                        ),
                    ));
                }
            }
            previous = Some(entry.id);
            pos += span;
            count += 1;
        }

        if count != self.slot_count() {
            return Err(Error::corruption(
                path,
                format!(
                    "base header declares {} documents but the directory holds {count}",
                    self.slot_count()
                ),
            ));
        }
        if self.dir_entries + pos != self.dir_entries_end {
            return Err(Error::corruption(
                path,
                format!(
                    "directory entries fill {pos} bytes but the section holds {}",
                    self.dir_entries_end - self.dir_entries
                ),
            ));
        }

        for index in &self.indexes {
            self.validate_index(index)?;
        }
        Ok(())
    }

    fn validate_index(&self, index: &PersistedIndex) -> Result<()> {
        let path = self.mmap.path();
        let mut previous: Option<&[u8]> = None;
        let mut expected = 0usize;
        for position in 0..index.entry_count {
            let (key, slots) = self.index_entry(index, position);
            if key.is_empty() {
                return Err(Error::corruption(
                    path,
                    format!(
                        "index {:?} entry {position} has an empty key encoding",
                        index.spec.field
                    ),
                ));
            }
            let offset = self.index_entry_offset(index, position);
            if offset != expected {
                return Err(Error::corruption(
                    path,
                    format!(
                        "index {:?} entry offsets are not contiguous at entry {position}",
                        index.spec.field
                    ),
                ));
            }
            if offset % 4 != 0 {
                return Err(Error::corruption(
                    path,
                    format!(
                        "index {:?} entry {position} is not 4-byte aligned",
                        index.spec.field
                    ),
                ));
            }
            if slots.is_empty() {
                return Err(Error::corruption(
                    path,
                    format!(
                        "index {:?} entry {position} carries no document slots",
                        index.spec.field
                    ),
                ));
            }
            for slot in slots {
                if *slot >= self.slot_count() {
                    return Err(Error::corruption(
                        path,
                        format!(
                            "index {:?} references slot {slot}, but the base holds {} documents",
                            index.spec.field,
                            self.slot_count()
                        ),
                    ));
                }
            }
            if let Some(previous) = previous {
                if cmp_encoded_keys(previous, key) != Ordering::Less {
                    return Err(Error::corruption(
                        path,
                        format!(
                            "index {:?} keys are not strictly ascending at entry {position}",
                            index.spec.field
                        ),
                    ));
                }
            }
            previous = Some(key);
            expected = self.index_entry_end(index, offset);
        }
        if index.entries_offset + expected != index.entries_end {
            return Err(Error::corruption(
                path,
                format!(
                    "index {:?} entries fill {} bytes but its region holds {}",
                    index.spec.field,
                    expected,
                    index.entries_end - index.entries_offset
                ),
            ));
        }
        Ok(())
    }

    /// Decoded header.
    pub fn header(&self) -> BaseHeader {
        self.header
    }

    /// Highest sequence folded into this base.
    pub fn base_seq(&self) -> u64 {
        self.header.base_seq
    }

    /// Documents in this base.
    pub fn doc_count(&self) -> u64 {
        self.header.doc_count
    }

    /// Mapped file length.
    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    /// Whether the mapped file is empty (never true for a valid base).
    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// The whole mapped file.
    pub fn as_bytes(&self) -> &[u8] {
        self.mmap.as_slice()
    }

    /// Number of documents in this base (the directory slot space).
    pub fn slot_count(&self) -> u32 {
        self.header.doc_count as u32
    }

    fn entry_offset_raw(&self, slot: u32) -> Result<u64> {
        let at = self.header.dir_offset as usize + slot as usize * 8;
        let bytes = self.mmap.as_slice();
        let raw = bytes.get(at..at + 8).ok_or_else(|| {
            Error::corruption(
                self.mmap.path(),
                format!("directory offset table is truncated at slot {slot}"),
            )
        })?;
        Ok(u64::from_le_bytes(raw.try_into().expect("8-byte slice")))
    }

    fn u16_at(&self, at: usize, what: &str) -> Result<u16> {
        let bytes = self.mmap.as_slice();
        let raw = bytes.get(at..at + 2).ok_or_else(|| {
            Error::corruption(self.mmap.path(), format!("{what}: truncated at byte {at}"))
        })?;
        Ok(u16::from_le_bytes(raw.try_into().expect("2-byte slice")))
    }

    fn u32_at(&self, at: usize, what: &str) -> Result<u32> {
        let bytes = self.mmap.as_slice();
        let raw = bytes.get(at..at + 4).ok_or_else(|| {
            Error::corruption(self.mmap.path(), format!("{what}: truncated at byte {at}"))
        })?;
        Ok(u32::from_le_bytes(raw.try_into().expect("4-byte slice")))
    }

    fn u64_at(&self, at: usize, what: &str) -> Result<u64> {
        let bytes = self.mmap.as_slice();
        let raw = bytes.get(at..at + 8).ok_or_else(|| {
            Error::corruption(self.mmap.path(), format!("{what}: truncated at byte {at}"))
        })?;
        Ok(u64::from_le_bytes(raw.try_into().expect("8-byte slice")))
    }

    fn entry_at(&self, slot: u32) -> Result<BaseDirEntry<'_>> {
        let path = self.mmap.path();
        if slot >= self.slot_count() {
            return Err(Error::corruption(
                path,
                format!("directory slot {slot} is outside 0..{}", self.slot_count()),
            ));
        }
        let offset = self.entry_offset_raw(slot)? as usize;
        let at = self.dir_entries.checked_add(offset).ok_or_else(|| {
            Error::corruption(path, format!("directory slot {slot} offset overflows"))
        })?;
        let bytes = self.mmap.as_slice();
        let id_len = self.u16_at(at, "directory id length")? as usize;
        let id_raw = bytes.get(at + 2..at + 2 + id_len).ok_or_else(|| {
            Error::corruption(path, format!("directory slot {slot} id is truncated"))
        })?;
        let id = std::str::from_utf8(id_raw).map_err(|e| {
            Error::corruption(path, format!("directory slot {slot} id is not UTF-8: {e}"))
        })?;
        let doc_seq = self.u64_at(at + 2 + id_len, "directory doc_seq")?;
        let data_offset = self.u64_at(at + 2 + id_len + 8, "directory data_offset")?;
        let data_len = self.u32_at(at + 2 + id_len + 16, "directory data_len")?;

        if data_offset < BASE_HEADER_LEN as u64 {
            return Err(Error::corruption(
                path,
                format!("directory slot {slot} (id {id:?}) points into the header at {data_offset}"),
            ));
        }
        let data_end = data_offset.checked_add(u64::from(data_len)).ok_or_else(|| {
            Error::corruption(path, format!("directory slot {slot} (id {id:?}) overflows u64"))
        })?;
        if data_end > self.header.dir_offset {
            return Err(Error::corruption(
                path,
                format!(
                    "directory slot {slot} (id {id:?}) data ends at {data_end}, past the data section end {}",
                    self.header.dir_offset
                ),
            ));
        }
        if doc_seq > self.header.base_seq {
            return Err(Error::corruption(
                path,
                format!(
                    "directory slot {slot} (id {id:?}) has doc_seq {doc_seq} above base_seq {}",
                    self.header.base_seq
                ),
            ));
        }
        Ok(BaseDirEntry {
            slot,
            id,
            doc_seq,
            data_offset,
            data_len,
        })
    }

    /// O(1), zero-allocation directory entry for `slot`.
    ///
    /// Panics on an out-of-range slot: the directory was validated at open, so
    /// such a call is a caller bug, not corrupt input.
    pub fn entry_by_slot(&self, slot: u32) -> BaseDirEntry<'_> {
        self.entry_at(slot)
            .expect("base directory validated by MmapBase::open")
    }

    /// O(1), zero-allocation document bytes for `slot`.
    pub fn doc_bytes_by_slot(&self, slot: u32) -> &[u8] {
        let entry = self.entry_by_slot(slot);
        self.doc_bytes(entry.data_offset, entry.data_len)
    }

    /// O(log n), zero-allocation id lookup over the mapped directory.
    pub fn find_slot(&self, id: &str) -> Option<u32> {
        let mut low = 0u32;
        let mut high = self.slot_count();
        while low < high {
            let mid = low + (high - low) / 2;
            let entry = self.entry_by_slot(mid);
            match entry.id.cmp(id) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// O(1) access to a document's bytes.
    ///
    /// Panics if the range lies outside the data section: the directory was
    /// validated at open, so an out-of-range request is a caller bug, not
    /// corrupt input.
    pub fn doc_bytes(&self, offset: u64, len: u32) -> &[u8] {
        let end = offset + u64::from(len);
        assert!(
            offset >= BASE_HEADER_LEN as u64 && end <= self.header.dir_offset,
            "document range [{offset}, {end}) lies outside the data section [{}, {})",
            BASE_HEADER_LEN,
            self.header.dir_offset
        );
        &self.mmap.as_slice()[offset as usize..end as usize]
    }

    /// Iterate the id directory in slot order (ids strictly ascending).
    pub fn iter(&self) -> BaseIter<'_> {
        BaseIter {
            base: self,
            pos: 0,
            next_slot: 0,
            doc_count: self.slot_count(),
        }
    }

    // ─── Zero-copy persisted indexes ────────────────────────────────

    fn index_entry_offset(&self, index: &PersistedIndex, position: u32) -> usize {
        let at = index.table_offset + position as usize * 8;
        self.u64_at(at, "index entry offset")
            .expect("index offset table validated at open") as usize
    }

    /// Relative end of one packed index entry (offset + its byte span).
    fn index_entry_end(&self, index: &PersistedIndex, offset: usize) -> usize {
        let at = index.entries_offset + offset;
        let key_len = self
            .u32_at(at, "index key length")
            .expect("index entry validated at open") as usize;
        let padded = at + 4 + key_len + (4 - key_len % 4) % 4;
        let slot_count = self
            .u32_at(padded, "index slot count")
            .expect("index entry validated at open") as usize;
        offset + 4 + key_len + (4 - key_len % 4) % 4 + 4 + slot_count * 4
    }

    /// Length in bytes of the packed directory entry at `offset`.
    fn entry_span(&self, offset: usize) -> Result<usize> {
        let id_len = self.u16_at(self.dir_entries + offset, "directory id length")? as usize;
        Ok(2 + id_len + 8 + 8 + 4)
    }

    /// The raw encoded key and its slot array for entry `position`.
    ///
    /// The slot array is a zero-copy `&[u32]` view into the mapping: the
    /// writer aligns every entry to 4 bytes and the mapping itself is
    /// page-aligned, so the view is correctly aligned and little-endian.
    pub fn index_entry(&self, index: &PersistedIndex, position: u32) -> (&[u8], &[u32]) {
        let offset = self.index_entry_offset(index, position);
        let at = index.entries_offset + offset;
        let bytes = self.mmap.as_slice();
        let key_len = self
            .u32_at(at, "index key length")
            .expect("index entry validated at open") as usize;
        let key = &bytes[at + 4..at + 4 + key_len];
        let padded = at + 4 + key_len + (4 - key_len % 4) % 4;
        let slot_count = self
            .u32_at(padded, "index slot count")
            .expect("index entry validated at open") as usize;
        let slots_at = padded + 4;
        // SAFETY: `slots_at` is 4-byte aligned (entries are 4-aligned and the
        // slot array starts after a 4-byte count), the mapping is page-aligned,
        // and `validate_index` proved the whole array lies inside the mapping.
        let slots = unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().add(slots_at) as *const u32, slot_count)
        };
        (key, slots)
    }

    /// Decode one persisted index key into an owned `OrderedValue`.
    pub fn index_key_owned(&self, index: &PersistedIndex, position: u32) -> OrderedValue {
        let (key, _) = self.index_entry(index, position);
        OrderedValue::decode(key, self.mmap.path()).expect("index key validated at open")
    }

    /// Binary search for `key`; `None` when the index carries no entry for it.
    pub fn index_slots_for(&self, index: &PersistedIndex, key: &OrderedValue) -> Option<&[u32]> {
        let needle = key.encode();
        let mut low = 0u32;
        let mut high = index.entry_count;
        while low < high {
            let mid = low + (high - low) / 2;
            let (encoded, slots) = self.index_entry(index, mid);
            match cmp_encoded_keys(encoded, &needle) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Some(slots),
            }
        }
        None
    }

    /// Every slot whose key falls inside the bounds, in key order.
    ///
    /// Runs one binary search for the lower bound and then a linear walk over
    /// the contiguous encoded entries until the upper bound is crossed.
    pub fn index_range_slots(
        &self,
        index: &PersistedIndex,
        lower: KeyEnd,
        upper: KeyEnd,
    ) -> Vec<u32> {
        let lower_bytes = enc_bound(lower);
        let upper_bytes = enc_bound(upper);

        let mut low = 0u32;
        let mut high = index.entry_count;
        while low < high {
            let mid = low + (high - low) / 2;
            let (encoded, _) = self.index_entry(index, mid);
            let below = match &lower_bytes {
                None => false,
                Some((needle, inclusive)) => {
                    let ordering = cmp_encoded_keys(encoded, needle);
                    ordering == Ordering::Less || (!*inclusive && ordering == Ordering::Equal)
                }
            };
            if below {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        let mut out = Vec::new();
        let mut position = low;
        while position < index.entry_count {
            let (encoded, slots) = self.index_entry(index, position);
            if let Some((needle, inclusive)) = &upper_bytes {
                let ordering = cmp_encoded_keys(encoded, needle);
                if ordering == Ordering::Greater || (!*inclusive && ordering == Ordering::Equal) {
                    break;
                }
            }
            out.extend_from_slice(slots);
            position += 1;
        }
        out
    }

    /// The persisted index sections, validated at open.
    pub fn indexes(&self) -> &[PersistedIndex] {
        &self.indexes
    }
}

fn enc_bound(bound: KeyEnd) -> Option<(Vec<u8>, bool)> {
    match bound {
        KeyEnd::Unbounded => None,
        KeyEnd::Included(value) => Some((value.encode(), true)),
        KeyEnd::Excluded(value) => Some((value.encode(), false)),
    }
}

/// Compare two encoded `OrderedValue` keys without decoding them into owned
/// values — the total order of [`OrderedValue::cmp`] applied to the on-disk
/// bytes.
fn cmp_encoded_keys(a: &[u8], b: &[u8]) -> Ordering {
    let (a_tag, a_rest) = a.split_first().expect("validated key encoding");
    let (b_tag, b_rest) = b.split_first().expect("validated key encoding");
    if a_tag != b_tag {
        return a_tag.cmp(b_tag);
    }
    match a_tag {
        0 => Ordering::Equal,
        1 => a_rest[0].cmp(&b_rest[0]),
        2 => {
            let left = f64::from_bits(u64::from_le_bytes(
                a_rest[..8].try_into().expect("8-byte number"),
            ));
            let right = f64::from_bits(u64::from_le_bytes(
                b_rest[..8].try_into().expect("8-byte number"),
            ));
            left.total_cmp(&right)
        }
        _ => {
            let a_len = u32::from_le_bytes(a_rest[..4].try_into().expect("4-byte length")) as usize;
            let b_len = u32::from_le_bytes(b_rest[..4].try_into().expect("4-byte length")) as usize;
            a_rest[4..4 + a_len].cmp(&b_rest[4..4 + b_len])
        }
    }
}

/// Iterator over a base generation's directory.
#[derive(Debug)]
pub struct BaseIter<'a> {
    base: &'a MmapBase,
    pos: usize,
    next_slot: u32,
    doc_count: u32,
}

impl<'a> Iterator for BaseIter<'a> {
    type Item = BaseDirEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_slot >= self.doc_count {
            return None;
        }
        let entry = self
            .base
            .entry_at(self.next_slot)
            .expect("directory validated by MmapBase::open");
        self.pos += 1;
        self.next_slot += 1;
        Some(entry)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.doc_count - self.next_slot) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for BaseIter<'_> {}

fn parse_index_section(
    payload: &[u8],
    section_start: usize,
    path: &Path,
    doc_count: u64,
) -> Result<Vec<PersistedIndex>> {
    let doc_count = u32::try_from(doc_count)
        .expect("doc_count validated against u32 before parsing the index section");
    let mut reader = Reader::new(payload, path);
    let declared = reader.u32("index section count")? as usize;
    let mut indexes = Vec::with_capacity(declared);
    for index_number in 0..declared {
        let tag = reader.u8("index kind")?;
        let kind = V2IndexKind::from_tag(tag).ok_or_else(|| {
            Error::corruption(
                path,
                format!("index {index_number} has unknown kind tag {tag}"),
            )
        })?;
        let field_len = usize::from(reader.u16("index field length")?);
        let raw_field = reader.take(field_len, "index field")?;
        let field = std::str::from_utf8(raw_field).map_err(|e| {
            Error::corruption(
                path,
                format!("index {index_number} field is not UTF-8: {e}"),
            )
        })?;
        let entry_count = reader.u32("index entry count")?;

        let absolute = section_start + reader.position();
        let pad = (8 - absolute % 8) % 8;
        reader.take(pad, "index alignment padding")?;
        let table_offset = section_start + reader.position();
        let table = reader.take(entry_count as usize * 8, "index offset table")?;
        let entries_offset = section_start + reader.position();
        let entries_len = index_entries_len(
            payload,
            entries_offset - section_start,
            table,
            entry_count,
            doc_count,
            path,
        )?;
        reader.take(entries_len, "index entries")?;
        let entries_end = entries_offset + entries_len;

        indexes.push(PersistedIndex {
            spec: V2IndexSpec {
                field: field.to_string(),
                kind,
            },
            entry_count,
            table_offset,
            entries_offset,
            entries_end,
        });
    }
    if reader.remaining() != 0 {
        return Err(Error::corruption(
            path,
            format!("index section has {} trailing bytes", reader.remaining()),
        ));
    }
    Ok(indexes)
}

/// Validate one index's packed-entry region structurally and return its length.
///
/// Reads the offset table and every entry header (no allocation): offsets must
/// ascend from 0, every entry must fit inside the region, every slot must name
/// an existing document, and the last entry must end exactly at the region end.
fn index_entries_len(
    payload: &[u8],
    entries_rel: usize,
    table: &[u8],
    entry_count: u32,
    doc_count: u32,
    path: &Path,
) -> Result<usize> {
    if entry_count == 0 {
        return Ok(0);
    }
    let last_offset = entry_offset(table, entry_count - 1, path)? as usize;
    let total = last_offset
        .checked_add(entry_span(payload, entries_rel + last_offset, doc_count, path)?)
        .ok_or_else(|| Error::corruption(path, "index entry region overflows usize"))?;

    let mut expected = 0usize;
    for position in 0..entry_count {
        let offset = entry_offset(table, position, path)? as usize;
        if offset != expected {
            return Err(Error::corruption(
                path,
                format!(
                    "index entry offsets are not contiguous: entry {position} starts at {offset}, expected {expected}"
                ),
            ));
        }
        let span = entry_span(payload, entries_rel + offset, doc_count, path)?;
        expected = offset + span;
        if expected > total {
            return Err(Error::corruption(
                path,
                format!("index entry {position} overruns its region"),
            ));
        }
    }
    if expected != total {
        return Err(Error::corruption(
            path,
            format!("index entries fill {expected} bytes but the region holds {total}"),
        ));
    }
    Ok(total)
}

fn entry_offset(table: &[u8], position: u32, path: &Path) -> Result<u64> {
    let at = position as usize * 8;
    let raw = table.get(at..at + 8).ok_or_else(|| {
        Error::corruption(path, format!("index offset table is truncated at entry {position}"))
    })?;
    Ok(u64::from_le_bytes(raw.try_into().expect("8-byte slice")))
}

fn read_u32(payload: &[u8], at: usize, path: &Path, what: &str) -> Result<u32> {
    let raw = payload.get(at..at + 4).ok_or_else(|| {
        Error::corruption(path, format!("{what}: truncated at byte {at}"))
    })?;
    Ok(u32::from_le_bytes(raw.try_into().expect("4-byte slice")))
}

/// Byte span of one packed entry starting at `at` inside `payload`.
fn entry_span(payload: &[u8], at: usize, doc_count: u32, path: &Path) -> Result<usize> {
    let key_len = read_u32(payload, at, path, "index key length")? as usize;
    if key_len == 0 {
        return Err(Error::corruption(path, "index entry has an empty key encoding"));
    }
    let padded = at + 4 + key_len + (4 - key_len % 4) % 4;
    if padded > payload.len() {
        return Err(Error::corruption(
            path,
            format!("index key at byte {at} ({key_len} bytes) overruns the section"),
        ));
    }
    let slot_count = read_u32(payload, padded, path, "index slot count")? as usize;
    if slot_count == 0 {
        return Err(Error::corruption(
            path,
            format!("index entry at byte {at} carries no document slots"),
        ));
    }
    let slots_at = padded + 4;
    let end = slots_at
        .checked_add(slot_count * 4)
        .ok_or_else(|| Error::corruption(path, "index slot array overflows usize"))?;
    if end > payload.len() {
        return Err(Error::corruption(
            path,
            format!("index slot array at byte {slots_at} overruns the section"),
        ));
    }
    for position in 0..slot_count {
        let slot = read_u32(payload, slots_at + position * 4, path, "index slot")?;
        if slot >= doc_count {
            return Err(Error::corruption(
                path,
                format!(
                    "index references slot {slot}, but the base holds {doc_count} documents"
                ),
            ));
        }
    }
    Ok(end - at)
}


// ─── Write-ahead log ────────────────────────────────────────────────

/// Bytes covered by the WAL header CRC.
pub const WAL_HEADER_FIELDS_LEN: usize = 28;
/// Whole WAL header: the 28-byte field block plus its trailing CRC-32.
pub const WAL_HEADER_LEN: usize = WAL_HEADER_FIELDS_LEN + 4;

const FRAME_LEN_PREFIX: usize = 4;
const FRAME_BODY_FIXED: usize = 1 + 8 + 2;

/// Smallest legal frame body: op tag, seq and id length with an empty id.
pub const MIN_FRAME_BODY: u32 = FRAME_BODY_FIXED as u32;
/// Largest frame body accepted. A larger length prefix is a torn write, never
/// a document.
pub const MAX_FRAME_BODY: u32 = 512 * 1024 * 1024;

/// What a WAL frame committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalOp {
    /// Full post-mutation document bytes.
    Put,
    /// Tombstone; the payload is empty.
    Delete,
    /// Add the index described by the payload.
    IndexAdd,
    /// Drop the index described by the payload.
    IndexDrop,
}

impl WalOp {
    /// Binary tag stored in the frame.
    pub fn tag(self) -> u8 {
        match self {
            WalOp::Put => 1,
            WalOp::Delete => 2,
            WalOp::IndexAdd => 3,
            WalOp::IndexDrop => 4,
        }
    }

    /// Parse a frame tag; `None` for an unknown value.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(WalOp::Put),
            2 => Some(WalOp::Delete),
            3 => Some(WalOp::IndexAdd),
            4 => Some(WalOp::IndexDrop),
            _ => None,
        }
    }
}

/// One decoded WAL frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalRecord {
    /// Commit sequence of the mutation.
    pub seq: u64,
    /// Mutation kind.
    pub op: WalOp,
    /// Document id (the field name for index changes).
    pub id: String,
    /// Payload: JSON document bytes, empty for delete, `V2IndexSpec` JSON for
    /// index changes.
    pub payload: Vec<u8>,
}

impl WalRecord {
    /// A `Put` frame with full post-mutation document bytes.
    pub fn put(seq: u64, id: impl Into<String>, json: impl Into<Vec<u8>>) -> Self {
        WalRecord {
            seq,
            op: WalOp::Put,
            id: id.into(),
            payload: json.into(),
        }
    }

    /// A `Delete` tombstone.
    pub fn delete(seq: u64, id: impl Into<String>) -> Self {
        WalRecord {
            seq,
            op: WalOp::Delete,
            id: id.into(),
            payload: Vec::new(),
        }
    }

    /// An `IndexAdd` frame for `spec`.
    pub fn index_add(seq: u64, spec: &V2IndexSpec) -> Result<Self> {
        Ok(WalRecord {
            seq,
            op: WalOp::IndexAdd,
            id: spec.field.clone(),
            payload: serde_json::to_vec(spec)
                .map_err(|e| Error::Serialization(format!("serialize index spec: {e}")))?,
        })
    }

    /// An `IndexDrop` frame for `spec`.
    pub fn index_drop(seq: u64, spec: &V2IndexSpec) -> Result<Self> {
        Ok(WalRecord {
            seq,
            op: WalOp::IndexDrop,
            id: spec.field.clone(),
            payload: serde_json::to_vec(spec)
                .map_err(|e| Error::Serialization(format!("serialize index spec: {e}")))?,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.id.is_empty() {
            return Err(Error::invalid_arg(
                "WAL frame id must not be empty (document id or index field)",
            ));
        }
        if self.id.len() > usize::from(u16::MAX) {
            return Err(Error::invalid_arg(format!(
                "WAL frame id is {} bytes, the frame stores at most {}",
                self.id.len(),
                u16::MAX
            )));
        }
        match self.op {
            WalOp::Put => {
                std::str::from_utf8(&self.payload).map_err(|e| {
                    Error::invalid_arg(format!("WAL put payload is not UTF-8: {e}"))
                })?;
                serde_json::from_slice::<Value>(&self.payload).map_err(|e| {
                    Error::invalid_arg(format!("WAL put payload is not valid JSON: {e}"))
                })?;
            }
            WalOp::Delete => {
                if !self.payload.is_empty() {
                    return Err(Error::invalid_arg(
                        "WAL delete frame must not carry a payload",
                    ));
                }
            }
            WalOp::IndexAdd | WalOp::IndexDrop => {
                let spec: V2IndexSpec = serde_json::from_slice(&self.payload).map_err(|e| {
                    Error::invalid_arg(format!("WAL index frame payload is not a V2IndexSpec: {e}"))
                })?;
                if spec.field != self.id {
                    return Err(Error::invalid_arg(format!(
                        "WAL index frame id {:?} does not match its spec field {:?}",
                        self.id, spec.field
                    )));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalHeader {
    wal_gen: u64,
    base_seq: u64,
}

fn encode_wal_header(header: &WalHeader) -> [u8; WAL_HEADER_LEN] {
    let mut buf = Vec::with_capacity(WAL_HEADER_LEN);
    buf.extend_from_slice(&WAL_MAGIC);
    put_u32(&mut buf, STORAGE_VERSION);
    put_u64(&mut buf, header.wal_gen);
    put_u64(&mut buf, header.base_seq);
    debug_assert_eq!(buf.len(), WAL_HEADER_FIELDS_LEN);
    let crc = crc32(&buf);
    put_u32(&mut buf, crc);
    let mut out = [0u8; WAL_HEADER_LEN];
    out.copy_from_slice(&buf);
    out
}

fn decode_wal_header(bytes: &[u8], path: &Path) -> Result<WalHeader> {
    if bytes.len() < WAL_HEADER_LEN {
        return Err(Error::corruption(
            path,
            format!(
                "WAL header is {} bytes, needs {WAL_HEADER_LEN}",
                bytes.len()
            ),
        ));
    }
    let stored_crc = u32::from_le_bytes(
        bytes[WAL_HEADER_FIELDS_LEN..WAL_HEADER_LEN]
            .try_into()
            .expect("4-byte slice"),
    );
    let computed_crc = crc32(&bytes[..WAL_HEADER_FIELDS_LEN]);
    if stored_crc != computed_crc {
        return Err(Error::corruption(
            path,
            format!(
                "WAL header CRC mismatch: stored {stored_crc:08x}, computed {computed_crc:08x}"
            ),
        ));
    }

    let mut reader = Reader::new(&bytes[..WAL_HEADER_FIELDS_LEN], path);
    let magic = reader.take(8, "WAL magic")?;
    if magic != WAL_MAGIC {
        return Err(Error::corruption(
            path,
            format!("bad WAL magic {magic:?}, expected {WAL_MAGIC:?}"),
        ));
    }
    let version = reader.u32("WAL format version")?;
    if version != STORAGE_VERSION {
        return Err(Error::corruption(
            path,
            format!("WAL format version is {version}, expected {STORAGE_VERSION}"),
        ));
    }
    Ok(WalHeader {
        wal_gen: reader.u64("WAL generation")?,
        base_seq: reader.u64("WAL base_seq")?,
    })
}

fn encode_frame(record: &WalRecord) -> Result<Vec<u8>> {
    let id_len = u16::try_from(record.id.len()).map_err(|_| {
        Error::invalid_arg(format!(
            "WAL frame id is {} bytes, the frame stores at most {}",
            record.id.len(),
            u16::MAX
        ))
    })?;
    let body_len = FRAME_BODY_FIXED + record.id.len() + record.payload.len();
    let body_len_u32 = u32::try_from(body_len)
        .map_err(|_| Error::invalid_arg("WAL frame body is larger than u32"))?;
    if body_len_u32 > MAX_FRAME_BODY {
        return Err(Error::invalid_arg(format!(
            "WAL frame body is {body_len_u32} bytes, above the {MAX_FRAME_BODY}-byte limit"
        )));
    }

    let mut frame = Vec::with_capacity(FRAME_LEN_PREFIX + body_len + 4);
    put_u32(&mut frame, body_len_u32);
    put_u8(&mut frame, record.op.tag());
    put_u64(&mut frame, record.seq);
    put_u16(&mut frame, id_len);
    frame.extend_from_slice(record.id.as_bytes());
    frame.extend_from_slice(&record.payload);
    debug_assert_eq!(frame.len(), FRAME_LEN_PREFIX + body_len);
    let crc = crc32(&frame);
    put_u32(&mut frame, crc);
    Ok(frame)
}

fn truncate_file(path: &Path, len: u64) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(Error::io_err(path, "open file for truncation"))?;
    file.set_len(len)
        .map_err(Error::io_err(path, "truncate file"))?;
    file.sync_all()
        .map_err(Error::io_err(path, "sync truncated file"))
}

fn open_shared(path: &Path, options: OpenOptions, context: &str) -> Result<File> {
    #[cfg(windows)]
    let options = {
        use std::os::windows::fs::OpenOptionsExt;
        let mut options = options;
        options.share_mode(crate::mmap::FILE_SHARE_ALL);
        options
    };
    options.open(path).map_err(Error::io_err(path, context))
}

/// Appends CRC32-framed records to one WAL generation.
///
/// Sequence continuity is enforced here: a frame whose `seq` does not follow
/// the last accepted frame is rejected before a single byte is written.
#[derive(Debug)]
pub struct WalWriter {
    file: File,
    path: PathBuf,
    header: WalHeader,
    last_seq: u64,
}

impl WalWriter {
    /// Create a new WAL generation with its header, fsynced and parent-synced.
    ///
    /// Fails if the file already exists: a generation is written once.
    pub fn create(path: &Path, wal_gen: u64, base_seq: u64) -> Result<WalWriter> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(Error::io_err(parent, "create WAL directory"))?;
        }
        let mut options = OpenOptions::new();
        options.write(true).read(true).create_new(true);
        let mut file = open_shared(path, options, "create WAL generation")?;
        let header = WalHeader { wal_gen, base_seq };
        file.write_all(&encode_wal_header(&header))
            .map_err(Error::io_err(path, "write WAL header"))?;
        file.sync_all()
            .map_err(Error::io_err(path, "sync new WAL header"))?;
        crate::storage::sync_parent_dir(path)?;
        Ok(WalWriter {
            file,
            path: path.to_path_buf(),
            header,
            last_seq: base_seq,
        })
    }

    /// Open an existing WAL generation for appending and recover its last seq.
    ///
    /// A torn or corrupt tail fails the open: the caller must run
    /// [`replay_wal`] with `repair_tail` or [`repair_wal_file`] first.
    pub fn open(path: &Path) -> Result<WalWriter> {
        let header = read_wal_header(path)?;
        let scan = scan_wal_inner(path, header.base_seq)?;
        if let Some(issue) = scan.issue {
            return Err(Error::corruption(
                path,
                format!(
                    "WAL frame {} at byte {} (of {}): {}; repair the WAL before appending",
                    issue.frame_index, issue.offset, scan.total_bytes, issue.reason
                ),
            ));
        }
        let mut options = OpenOptions::new();
        options.append(true);
        let file = open_shared(path, options, "open WAL for append")?;
        Ok(WalWriter {
            file,
            path: path.to_path_buf(),
            header,
            last_seq: scan.last_seq,
        })
    }

    /// Append one record and fsync it.
    pub fn append(&mut self, record: &WalRecord) -> Result<()> {
        self.append_batch(std::slice::from_ref(record))
    }

    /// Append a batch of records with **one** `write_all` and **one** `sync_all`.
    ///
    /// This is the foundation of group commit: concurrent writers are coalesced
    /// into a single batch, so one fsync covers all of them.
    pub fn append_batch(&mut self, records: &[WalRecord]) -> Result<()> {
        self.append_batch_and_sync(records)
    }

    /// Append a batch and fsync it — the group-commit write.
    pub fn append_batch_and_sync(&mut self, records: &[WalRecord]) -> Result<()> {
        self.write_frames(records, true)
    }

    /// Append a batch without an fsync (`Lazy`/`Scheduled` persistence).
    pub fn append_batch_no_sync(&mut self, records: &[WalRecord]) -> Result<()> {
        self.write_frames(records, false)
    }

    fn write_frames(&mut self, records: &[WalRecord], sync: bool) -> Result<()> {
        if records.is_empty() {
            return Err(Error::invalid_arg("WAL batch must not be empty"));
        }
        let mut expected = self.last_seq;
        let mut buffer = Vec::new();
        for record in records {
            if record.seq != expected + 1 {
                return Err(Error::invalid_arg(format!(
                    "WAL frame seq {} does not follow {} in {}",
                    record.seq,
                    expected,
                    self.path.display()
                )));
            }
            record.validate()?;
            buffer.extend_from_slice(&encode_frame(record)?);
            expected = record.seq;
        }
        self.file
            .write_all(&buffer)
            .map_err(Error::io_err(&self.path, "append WAL frames"))?;
        if sync {
            self.file
                .sync_all()
                .map_err(Error::io_err(&self.path, "sync WAL frames"))?;
        }
        self.last_seq = expected;
        Ok(())
    }

    /// Explicitly fsync the WAL.
    pub fn sync(&mut self) -> Result<()> {
        self.file
            .sync_all()
            .map_err(Error::io_err(&self.path, "sync WAL"))
    }

    /// Highest sequence accepted by this writer.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Base sequence of the WAL's generation.
    pub fn base_seq(&self) -> u64 {
        self.header.base_seq
    }

    /// Generation number of this WAL.
    pub fn wal_gen(&self) -> u64 {
        self.header.wal_gen
    }

    /// Path of this WAL generation.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn read_wal_header(path: &Path) -> Result<WalHeader> {
    let mut file = File::open(path).map_err(Error::io_err(path, "open WAL to read its header"))?;
    let mut bytes = [0u8; WAL_HEADER_LEN];
    file.read_exact(&mut bytes)
        .map_err(Error::io_err(path, "read WAL header"))?;
    decode_wal_header(&bytes, path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IssueKind {
    /// The failing bytes run to EOF: an interrupted append, truncatable.
    Tail,
    /// Bytes follow the failing frame, or the frame violates sequence order.
    Hard,
}

#[derive(Debug)]
struct ScanIssue {
    kind: IssueKind,
    offset: u64,
    frame_index: u64,
    reason: String,
}

#[derive(Debug)]
struct WalScan {
    wal_gen: u64,
    base_seq: u64,
    total_bytes: u64,
    last_good_offset: u64,
    last_seq: u64,
    records: Vec<WalRecord>,
    issue: Option<ScanIssue>,
}

fn read_exact_or_eof<R: Read>(reader: &mut R, buffer: &mut [u8]) -> std::io::Result<bool> {
    match reader.read_exact(buffer) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

fn scan_wal_inner(path: &Path, expected_base_seq: u64) -> Result<WalScan> {
    let file = File::open(path).map_err(Error::io_err(path, "open WAL for scan"))?;
    let total_bytes = file
        .metadata()
        .map_err(Error::io_err(path, "stat WAL for scan"))?
        .len();
    if total_bytes < WAL_HEADER_LEN as u64 {
        return Err(Error::corruption(
            path,
            format!(
                "WAL is {total_bytes} bytes, shorter than its {WAL_HEADER_LEN}-byte header"
            ),
        ));
    }
    let mut reader = BufReader::new(file);
    let mut header_bytes = [0u8; WAL_HEADER_LEN];
    reader
        .read_exact(&mut header_bytes)
        .map_err(Error::io_err(path, "read WAL header"))?;
    let header = decode_wal_header(&header_bytes, path)?;
    if header.base_seq != expected_base_seq {
        return Err(Error::corruption(
            path,
            format!(
                "WAL base_seq is {}, expected {expected_base_seq}",
                header.base_seq
            ),
        ));
    }

    let mut offset = WAL_HEADER_LEN as u64;
    let mut frame_index = 0u64;
    let mut last_seq = header.base_seq;
    let mut records: Vec<WalRecord> = Vec::new();
    let mut issue: Option<ScanIssue> = None;

    while offset < total_bytes {
        let mut len_bytes = [0u8; FRAME_LEN_PREFIX];
        match read_exact_or_eof(&mut reader, &mut len_bytes) {
            Ok(true) => {}
            Ok(false) => {
                issue = Some(ScanIssue {
                    kind: IssueKind::Tail,
                    offset,
                    frame_index,
                    reason: "frame length prefix is truncated at end of file".to_string(),
                });
                break;
            }
            Err(e) => return Err(Error::io_err(path, "read WAL frame length")(e)),
        }
        let body_len = u32::from_le_bytes(len_bytes);
        if body_len < MIN_FRAME_BODY || body_len > MAX_FRAME_BODY {
            issue = Some(ScanIssue {
                kind: IssueKind::Tail,
                offset,
                frame_index,
                reason: format!("invalid frame body length {body_len}"),
            });
            break;
        }
        let frame_len = FRAME_LEN_PREFIX as u64 + u64::from(body_len) + 4;
        if offset + frame_len > total_bytes {
            issue = Some(ScanIssue {
                kind: IssueKind::Tail,
                offset,
                frame_index,
                reason: format!(
                    "frame needs {frame_len} bytes but only {} remain",
                    total_bytes - offset
                ),
            });
            break;
        }

        let mut rest = vec![0u8; body_len as usize + 4];
        match read_exact_or_eof(&mut reader, &mut rest) {
            Ok(true) => {}
            Ok(false) => {
                issue = Some(ScanIssue {
                    kind: IssueKind::Tail,
                    offset,
                    frame_index,
                    reason: "frame body is truncated at end of file".to_string(),
                });
                break;
            }
            Err(e) => return Err(Error::io_err(path, "read WAL frame body")(e)),
        }

        let stored_crc = u32::from_le_bytes(
            rest[body_len as usize..]
                .try_into()
                .expect("4-byte frame CRC slice"),
        );
        let mut hasher = Crc32Hasher::new();
        hasher.update(&len_bytes);
        hasher.update(&rest[..body_len as usize]);
        let computed_crc = hasher.finalize();
        if stored_crc != computed_crc {
            let at_end = offset + frame_len == total_bytes;
            issue = Some(ScanIssue {
                kind: if at_end { IssueKind::Tail } else { IssueKind::Hard },
                offset,
                frame_index,
                reason: format!(
                    "frame CRC mismatch: stored {stored_crc:08x}, computed {computed_crc:08x}{}",
                    if at_end { " at end of file (torn write)" } else { "" }
                ),
            });
            break;
        }

        let body = &rest[..body_len as usize];
        let Some(op) = WalOp::from_tag(body[0]) else {
            issue = Some(ScanIssue {
                kind: IssueKind::Hard,
                offset,
                frame_index,
                reason: format!("unknown op tag {}", body[0]),
            });
            break;
        };
        let mut seq_bytes = [0u8; 8];
        seq_bytes.copy_from_slice(&body[1..9]);
        let seq = u64::from_le_bytes(seq_bytes);
        let id_len = usize::from(u16::from_le_bytes([body[9], body[10]]));
        if FRAME_BODY_FIXED + id_len > body.len() {
            issue = Some(ScanIssue {
                kind: IssueKind::Hard,
                offset,
                frame_index,
                reason: format!(
                    "id length {id_len} overflows the {}-byte frame body",
                    body.len()
                ),
            });
            break;
        }
        let id = match std::str::from_utf8(&body[FRAME_BODY_FIXED..FRAME_BODY_FIXED + id_len]) {
            Ok(id) => id.to_string(),
            Err(e) => {
                issue = Some(ScanIssue {
                    kind: IssueKind::Hard,
                    offset,
                    frame_index,
                    reason: format!("frame id is not UTF-8: {e}"),
                });
                break;
            }
        };
        let payload = body[FRAME_BODY_FIXED + id_len..].to_vec();

        if seq != last_seq + 1 {
            issue = Some(ScanIssue {
                kind: IssueKind::Hard,
                offset,
                frame_index,
                reason: format!("frame seq {seq} does not follow {last_seq}"),
            });
            break;
        }

        match op {
            WalOp::Delete if !payload.is_empty() => {
                issue = Some(ScanIssue {
                    kind: IssueKind::Hard,
                    offset,
                    frame_index,
                    reason: "delete frame carries a payload".to_string(),
                });
                break;
            }
            WalOp::IndexAdd | WalOp::IndexDrop => {
                let spec: std::result::Result<V2IndexSpec, _> =
                    serde_json::from_slice(&payload);
                match spec {
                    Ok(spec) if spec.field == id => {}
                    Ok(spec) => {
                        issue = Some(ScanIssue {
                            kind: IssueKind::Hard,
                            offset,
                            frame_index,
                            reason: format!(
                                "index frame id {id:?} does not match spec field {:?}",
                                spec.field
                            ),
                        });
                        break;
                    }
                    Err(e) => {
                        issue = Some(ScanIssue {
                            kind: IssueKind::Hard,
                            offset,
                            frame_index,
                            reason: format!("index frame payload is not a V2IndexSpec: {e}"),
                        });
                        break;
                    }
                }
            }
            WalOp::Put => {
                if std::str::from_utf8(&payload).is_err() {
                    issue = Some(ScanIssue {
                        kind: IssueKind::Hard,
                        offset,
                        frame_index,
                        reason: "put frame payload is not UTF-8".to_string(),
                    });
                    break;
                }
            }
            WalOp::Delete => {}
        }

        records.push(WalRecord {
            seq,
            op,
            id,
            payload,
        });
        last_seq = seq;
        offset += frame_len;
        frame_index += 1;
    }

    Ok(WalScan {
        wal_gen: header.wal_gen,
        base_seq: header.base_seq,
        total_bytes,
        last_good_offset: offset,
        last_seq,
        records,
        issue,
    })
}

/// Outcome of replaying a WAL generation.
#[derive(Debug, Clone)]
pub struct WalReplay {
    /// Generation number from the WAL header.
    pub wal_gen: u64,
    /// Base sequence the WAL continues from.
    pub base_seq: u64,
    /// Committed frames in sequence order.
    pub records: Vec<WalRecord>,
    /// Highest committed sequence (`base_seq` when the WAL is empty).
    pub last_seq: u64,
    /// Byte offset just past the last good frame.
    pub good_bytes: u64,
    /// File size before any repair.
    pub total_bytes: u64,
    /// Bytes dropped by a tail repair (0 when the WAL was clean).
    pub truncated_tail_bytes: u64,
}

/// Replay a WAL generation.
///
/// A frame that is corrupt **at EOF** is an interrupted append: with
/// `repair_tail` the file is truncated to the last good frame and fsynced, and
/// the dropped byte count is reported. Corruption with bytes after it, a
/// non-monotonic `seq`, or a first frame that does not start at
/// `base_seq + 1` is `Error::Corruption` with both the byte offset and the
/// frame index, and is never repaired here.
pub fn replay_wal(path: &Path, expected_base_seq: u64, repair_tail: bool) -> Result<WalReplay> {
    let scan = scan_wal_inner(path, expected_base_seq)?;
    let mut truncated_tail_bytes = 0;
    if let Some(issue) = scan.issue {
        let description = format!(
            "frame {} at byte {} (of {}): {}",
            issue.frame_index, issue.offset, scan.total_bytes, issue.reason
        );
        match issue.kind {
            IssueKind::Hard => {
                return Err(Error::corruption(
                    path,
                    format!("corrupt WAL {description}; bytes follow it, so it is not a torn tail"),
                ));
            }
            IssueKind::Tail => {
                if !repair_tail {
                    return Err(Error::corruption(
                        path,
                        format!(
                            "torn WAL tail: {description}; {} bytes would be dropped — replay with repair_tail to truncate",
                            scan.total_bytes - scan.last_good_offset
                        ),
                    ));
                }
                truncate_file(path, scan.last_good_offset)?;
                truncated_tail_bytes = scan.total_bytes - scan.last_good_offset;
            }
        }
    }
    Ok(WalReplay {
        wal_gen: scan.wal_gen,
        base_seq: scan.base_seq,
        records: scan.records,
        last_seq: scan.last_seq,
        good_bytes: scan.last_good_offset,
        total_bytes: scan.total_bytes,
        truncated_tail_bytes,
    })
}

/// Stream the WAL and return only frames committed after `since_seq`.
///
/// Bounded RAM: the caller never materialises the base. A torn trailing frame
/// is an append in flight, so it is excluded rather than treated as
/// corruption; structural corruption (bytes after a bad frame) still fails
/// loud.
pub fn scan_wal_since(
    path: &Path,
    expected_base_seq: u64,
    since_seq: u64,
) -> Result<Vec<WalRecord>> {
    let scan = scan_wal_inner(path, expected_base_seq)?;
    if let Some(issue) = scan.issue {
        if issue.kind == IssueKind::Hard {
            return Err(Error::corruption(
                path,
                format!(
                    "corrupt WAL frame {} at byte {} (of {}): {}",
                    issue.frame_index, issue.offset, scan.total_bytes, issue.reason
                ),
            ));
        }
    }
    Ok(scan
        .records
        .into_iter()
        .filter(|record| record.seq > since_seq)
        .collect())
}

/// What [`repair_wal_file`] did.
#[derive(Debug, Clone)]
pub struct WalRepairStats {
    /// Generation number from the WAL header.
    pub wal_gen: u64,
    /// Base sequence the WAL continues from.
    pub base_seq: u64,
    /// File size before repair.
    pub total_bytes: u64,
    /// Bytes kept (the last good frame boundary).
    pub good_bytes: u64,
    /// Bytes dropped from the live file (still present in `backup`).
    pub truncated_bytes: u64,
    /// Frames that survive.
    pub frames_kept: usize,
    /// Highest surviving sequence.
    pub last_seq: u64,
    /// `None` when the WAL was already clean; otherwise the failing frame.
    pub issue: Option<String>,
    /// Backup of the pre-repair bytes, created only when something was dropped.
    pub backup: Option<PathBuf>,
}

/// Quarantine and truncate a WAL's bad tail.
///
/// The whole pre-repair file is copied to `<name>.corrupt.<unix-ts>.bak`
/// **before** it is truncated to the last good frame boundary, so nothing is
/// destroyed. The stats name the failing frame and every dropped byte. A
/// corrupt WAL *header* cannot be repaired automatically and is an error.
pub fn repair_wal_file(path: &Path, expected_base_seq: u64) -> Result<WalRepairStats> {
    let scan = scan_wal_inner(path, expected_base_seq)?;
    let Some(issue) = scan.issue else {
        return Ok(WalRepairStats {
            wal_gen: scan.wal_gen,
            base_seq: scan.base_seq,
            total_bytes: scan.total_bytes,
            good_bytes: scan.last_good_offset,
            truncated_bytes: 0,
            frames_kept: scan.records.len(),
            last_seq: scan.last_seq,
            issue: None,
            backup: None,
        });
    };

    let backup = backup_path(path)?;
    fs::copy(path, &backup).map_err(Error::io_err(path, "back up corrupt WAL"))?;
    let mut backup_options = OpenOptions::new();
    backup_options.write(true);
    open_shared(&backup, backup_options, "open WAL backup for sync")?
        .sync_all()
        .map_err(Error::io_err(&backup, "sync WAL backup"))?;

    truncate_file(path, scan.last_good_offset)?;
    Ok(WalRepairStats {
        wal_gen: scan.wal_gen,
        base_seq: scan.base_seq,
        total_bytes: scan.total_bytes,
        good_bytes: scan.last_good_offset,
        truncated_bytes: scan.total_bytes - scan.last_good_offset,
        frames_kept: scan.records.len(),
        last_seq: scan.last_seq,
        issue: Some(format!(
            "frame {} at byte {} (of {}): {}",
            issue.frame_index, issue.offset, scan.total_bytes, issue.reason
        )),
        backup: Some(backup),
    })
}

fn backup_path(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        Error::invalid_arg(format!("{} has no file name to back up", path.display()))
    })?;
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::invalid_arg(format!("system clock is before 1970: {e}")))?
        .as_secs();
    let mut backup = name.to_os_string();
    backup.push(format!(".corrupt.{seconds}.bak"));
    Ok(path.with_file_name(backup))
}

// ─── Filesystem check ───────────────────────────────────────────────

/// Result of [`fsck_v2_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsckReport {
    /// Base generation audited.
    pub base_gen: u64,
    /// WAL generation audited.
    pub wal_gen: u64,
    /// Base sequence.
    pub base_seq: u64,
    /// Documents in the base.
    pub doc_count: u64,
    /// Persisted indexes in the base.
    pub index_count: usize,
    /// Base file size.
    pub base_bytes: u64,
    /// Committed WAL frames.
    pub wal_frames: usize,
    /// WAL file size.
    pub wal_bytes: u64,
    /// Highest committed sequence (base + WAL).
    pub last_seq: u64,
    /// Oldest replayable sequence.
    pub min_available_seq: u64,
}

/// Audit a v2 database folder without modifying it.
///
/// Verifies the manifest, the base header/directory/index CRCs, that every
/// document is valid UTF-8 JSON, that the base's declared index set matches the
/// manifest, and that the WAL header and every frame CRC are intact with
/// contiguous sequences from `base_seq + 1`.
pub fn fsck_v2_dir(dir: &Path) -> Result<FsckReport> {
    let manifest = read_manifest(dir)?;

    let base_path = base_file_path(dir, manifest.base_gen);
    let base = MmapBase::open(&base_path)?;
    if base.base_seq() != manifest.base_seq {
        return Err(Error::corruption(
            &base_path,
            format!(
                "base carries base_seq {} but the manifest declares {}",
                base.base_seq(),
                manifest.base_seq
            ),
        ));
    }
    if base.doc_count() != manifest.doc_count {
        return Err(Error::corruption(
            &base_path,
            format!(
                "base carries {} documents but the manifest declares {}",
                base.doc_count(),
                manifest.doc_count
            ),
        ));
    }
    if base.indexes().len() != manifest.indexes.len() {
        return Err(Error::corruption(
            &base_path,
            format!(
                "base carries {} indexes but the manifest declares {}",
                base.indexes().len(),
                manifest.indexes.len()
            ),
        ));
    }
    for declared in &manifest.indexes {
        match base
            .indexes()
            .iter()
            .find(|held| held.spec.field == declared.field)
        {
            Some(held) if held.spec.kind == declared.kind => {}
            Some(held) => {
                return Err(Error::corruption(
                    &base_path,
                    format!(
                        "index {:?} is {:?} in the base but {:?} in the manifest",
                        declared.field, held.spec.kind, declared.kind
                    ),
                ));
            }
            None => {
                return Err(Error::corruption(
                    &base_path,
                    format!(
                        "index {:?} is declared in the manifest but missing from the base",
                        declared.field
                    ),
                ));
            }
        }
    }

    for entry in base.iter() {
        let bytes = base.doc_bytes(entry.data_offset, entry.data_len);
        let text = std::str::from_utf8(bytes).map_err(|e| {
            Error::corruption(
                &base_path,
                format!("document {:?} (slot {}) is not UTF-8: {e}", entry.id, entry.slot),
            )
        })?;
        serde_json::from_str::<Value>(text).map_err(|e| {
            Error::corruption(
                &base_path,
                format!(
                    "document {:?} (slot {}) is not valid JSON: {e}",
                    entry.id, entry.slot
                ),
            )
        })?;
    }

    let wal_path = wal_file_path(dir, manifest.wal_gen);
    if !wal_path.exists() {
        return Err(Error::corruption(
            &wal_path,
            "manifest names a WAL generation that does not exist",
        ));
    }
    let scan = scan_wal_inner(&wal_path, manifest.base_seq)?;
    if scan.wal_gen != manifest.wal_gen {
        return Err(Error::corruption(
            &wal_path,
            format!(
                "WAL header carries generation {} but the manifest names {}",
                scan.wal_gen, manifest.wal_gen
            ),
        ));
    }
    if let Some(issue) = scan.issue {
        return Err(Error::corruption(
            &wal_path,
            format!(
                "WAL frame {} at byte {} (of {}): {}",
                issue.frame_index, issue.offset, scan.total_bytes, issue.reason
            ),
        ));
    }

    Ok(FsckReport {
        base_gen: manifest.base_gen,
        wal_gen: manifest.wal_gen,
        base_seq: manifest.base_seq,
        doc_count: manifest.doc_count,
        index_count: base.indexes().len(),
        base_bytes: base.len() as u64,
        wal_frames: scan.records.len(),
        wal_bytes: scan.total_bytes,
        last_seq: scan.last_seq,
        min_available_seq: manifest.min_available_seq,
    })
}

// ─── Atomic writes ──────────────────────────────────────────────────

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn temp_path(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().ok_or_else(|| {
        Error::invalid_arg(format!("{} has no file name for a temporary sibling", path.display()))
    })?;
    let mut temp = name.to_os_string();
    temp.push(".tmp");
    Ok(path.with_file_name(temp))
}

fn write_atomic(path: &Path, bytes: &[u8], what: &str) -> Result<()> {
    let parent = parent_dir(path);
    fs::create_dir_all(parent).map_err(Error::io_err(parent, format!("create directory for {what}")))?;
    let temp = temp_path(path)?;
    let mut file = File::create(&temp)
        .map_err(Error::io_err(&temp, format!("create temporary {what}")))?;
    file.write_all(bytes)
        .map_err(Error::io_err(&temp, format!("write temporary {what}")))?;
    file.sync_all()
        .map_err(Error::io_err(&temp, format!("sync temporary {what}")))?;
    drop(file);
    fs::rename(&temp, path).map_err(Error::io_err(
        &temp,
        format!("rename temporary {what} onto {}", path.display()),
    ))?;
    crate::storage::sync_parent_dir(path)
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const DOC_A: &[u8] = br#"{"n":1,"tag":"a"}"#;
    const DOC_B: &[u8] = br#"{"n":2,"tag":"b"}"#;
    const DOC_C: &[u8] = br#"{"n":3,"tag":"c"}"#;

    fn spec(field: &str, kind: V2IndexKind) -> V2IndexSpec {
        V2IndexSpec {
            field: field.to_string(),
            kind,
        }
    }

    fn text_key(key: &str, ids: &[&str]) -> BaseIndexEntry {
        BaseIndexEntry::new(
            OrderedValue::String(key.to_string()),
            ids.iter().map(|id| id.to_string()).collect(),
        )
    }

    fn number_key(key: f64, ids: &[&str]) -> BaseIndexEntry {
        BaseIndexEntry::new(
            OrderedValue::Number(key),
            ids.iter().map(|id| id.to_string()).collect(),
        )
    }

    /// Three documents in non-sorted input order, both index kinds, one WAL
    /// with a put and a delete.
    fn clean_v2_dir() -> (tempfile::TempDir, V2Manifest) {
        let dir = tempfile::tempdir().unwrap();
        let docs = [
            BaseDocInput::new("doc-c", 5, DOC_C),
            BaseDocInput::new("doc-a", 2, DOC_A),
            BaseDocInput::new("doc-b", 7, DOC_B),
        ];
        let indexes = vec![
            (
                spec("tag", V2IndexKind::Hash),
                vec![
                    text_key("a", &["doc-a"]),
                    text_key("b", &["doc-b"]),
                    text_key("c", &["doc-c"]),
                ],
            ),
            (
                spec("n", V2IndexKind::BTree),
                vec![
                    number_key(1.0, &["doc-a"]),
                    number_key(2.0, &["doc-b"]),
                    number_key(3.0, &["doc-c"]),
                ],
            ),
        ];
        write_base_file(&base_file_path(dir.path(), 1), 7, &docs, &indexes).unwrap();

        let mut wal = WalWriter::create(&wal_file_path(dir.path(), 1), 1, 7).unwrap();
        wal.append(&WalRecord::put(
            8,
            "doc-d",
            br#"{"n":4,"tag":"d"}"#.to_vec(),
        ))
        .unwrap();
        wal.append(&WalRecord::delete(9, "doc-a")).unwrap();

        let manifest = V2Manifest {
            format_version: STORAGE_VERSION,
            base_gen: 1,
            wal_gen: 1,
            source_epoch_uuid: "7f8e4c1a-0000-4000-8000-000000000001".to_string(),
            base_seq: 7,
            min_available_seq: 1,
            doc_count: 3,
            indexes: vec![
                spec("tag", V2IndexKind::Hash),
                spec("n", V2IndexKind::BTree),
            ],
        };
        write_manifest_atomic(dir.path(), &manifest).unwrap();
        (dir, manifest)
    }

    fn tamper(path: &Path, offset: usize, xor: u8) {
        let mut bytes = fs::read(path).unwrap();
        bytes[offset] ^= xor;
        fs::write(path, &bytes).unwrap();
    }

    fn rewrite_base_header_crc(path: &Path) {
        let mut bytes = fs::read(path).unwrap();
        let crc = crc32(&bytes[..BASE_HEADER_FIELDS_LEN]);
        bytes[BASE_HEADER_FIELDS_LEN..BASE_HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
        fs::write(path, &bytes).unwrap();
    }

    fn corruption_message(error: Error) -> String {
        match error {
            Error::Corruption { message, .. } => message,
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    // ── Manifest ────────────────────────────────────────────────────

    #[test]
    fn manifest_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = V2Manifest {
            format_version: STORAGE_VERSION,
            base_gen: 4,
            wal_gen: 5,
            source_epoch_uuid: "epoch-abc".to_string(),
            base_seq: 100,
            min_available_seq: 101,
            doc_count: 42,
            indexes: vec![spec("tag", V2IndexKind::Hash)],
        };
        write_manifest_atomic(dir.path(), &manifest).unwrap();

        let read = read_manifest(dir.path()).unwrap();
        assert_eq!(read, manifest);
        assert!(!dir.path().join(MANIFEST_TMP_FILE).exists());

        let raw = fs::read(manifest_path(dir.path())).unwrap();
        assert_eq!(raw.last(), Some(&b'\n'));
    }

    #[test]
    fn manifest_overwrite_replaces_the_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = V2Manifest {
            format_version: STORAGE_VERSION,
            base_gen: 1,
            wal_gen: 1,
            source_epoch_uuid: "epoch".to_string(),
            base_seq: 0,
            min_available_seq: 1,
            doc_count: 0,
            indexes: vec![],
        };
        write_manifest_atomic(dir.path(), &manifest).unwrap();
        manifest.base_gen = 2;
        manifest.wal_gen = 2;
        write_manifest_atomic(dir.path(), &manifest).unwrap();
        assert_eq!(read_manifest(dir.path()).unwrap().base_gen, 2);
    }

    #[test]
    fn manifest_rejects_a_foreign_format_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            manifest_path(dir.path()),
            br#"{"format_version":1,"base_gen":1,"wal_gen":1,"source_epoch_uuid":"e","base_seq":0,"min_available_seq":1,"doc_count":0,"indexes":[]}"#,
        )
        .unwrap();
        let message = corruption_message(read_manifest(dir.path()).unwrap_err());
        assert!(message.contains("format_version"), "{message}");
    }

    #[test]
    fn manifest_rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            manifest_path(dir.path()),
            br#"{"format_version":2,"base_gen":1,"wal_gen":1,"source_epoch_uuid":"e","base_seq":0,"min_available_seq":1,"doc_count":0,"indexes":[],"surprise":true}"#,
        )
        .unwrap();
        assert!(read_manifest(dir.path()).is_err());
    }

    #[test]
    fn manifest_rejects_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(manifest_path(dir.path()), b"{not json").unwrap();
        assert!(read_manifest(dir.path()).is_err());
    }

    #[test]
    fn manifest_rejects_duplicate_index_fields() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = V2Manifest {
            format_version: STORAGE_VERSION,
            base_gen: 1,
            wal_gen: 1,
            source_epoch_uuid: "epoch".to_string(),
            base_seq: 0,
            min_available_seq: 1,
            doc_count: 0,
            indexes: vec![
                spec("tag", V2IndexKind::Hash),
                spec("tag", V2IndexKind::BTree),
            ],
        };
        let error = write_manifest_atomic(dir.path(), &manifest).unwrap_err();
        assert!(error.to_string().contains("twice"), "{error}");
    }

    #[test]
    fn manifest_rejects_history_holes() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = V2Manifest {
            format_version: STORAGE_VERSION,
            base_gen: 1,
            wal_gen: 1,
            source_epoch_uuid: "epoch".to_string(),
            base_seq: 5,
            min_available_seq: 9,
            doc_count: 0,
            indexes: vec![],
        };
        let error = write_manifest_atomic(dir.path(), &manifest).unwrap_err();
        assert!(error.to_string().contains("hole"), "{error}");
    }

    #[test]
    fn manifest_missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(read_manifest(dir.path()), Err(Error::Io { .. })));
    }

    // ── Ordered values ──────────────────────────────────────────────

    #[test]
    fn ordered_values_round_trip_through_the_binary_encoding() {
        let values = vec![
            OrderedValue::Null,
            OrderedValue::Bool(false),
            OrderedValue::Bool(true),
            OrderedValue::Number(-0.0),
            OrderedValue::Number(1.5e300),
            OrderedValue::Number(f64::NEG_INFINITY),
            OrderedValue::String("hello".to_string()),
            OrderedValue::String(String::new()),
            OrderedValue::Composite("[1,2]".to_string()),
        ];
        let path = Path::new("keys.bin");
        for value in values {
            let encoded = value.encode();
            let decoded = OrderedValue::decode(&encoded, path).unwrap();
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn ordered_value_encoding_is_deterministic() {
        let value = OrderedValue::Composite("{\"a\":1}".to_string());
        assert_eq!(value.encode(), value.encode());
        assert_eq!(
            OrderedValue::Number(2.0).encode(),
            OrderedValue::Number(2.0).encode()
        );
    }

    #[test]
    fn ordered_value_order_matches_the_in_memory_index() {
        let mut values = vec![
            OrderedValue::Composite("{}".to_string()),
            OrderedValue::String("a".to_string()),
            OrderedValue::Number(10.0),
            OrderedValue::Number(9.0),
            OrderedValue::Number(-0.0),
            OrderedValue::Number(0.0),
            OrderedValue::Bool(true),
            OrderedValue::Bool(false),
            OrderedValue::Null,
        ];
        values.sort();
        let ranks: Vec<u8> = values.iter().map(|value| value.rank()).collect();
        assert_eq!(ranks, vec![0, 1, 1, 2, 2, 2, 2, 3, 4]);
        assert_eq!(values[3], OrderedValue::Number(-0.0));
        assert_eq!(values[4], OrderedValue::Number(0.0));
        assert_eq!(values[5], OrderedValue::Number(9.0));
        assert_eq!(values[6], OrderedValue::Number(10.0));
    }

    #[test]
    fn ordered_value_rejects_trailing_bytes_and_unknown_tags() {
        let path = Path::new("keys.bin");
        let mut bytes = OrderedValue::Null.encode();
        bytes.push(0);
        assert!(OrderedValue::decode(&bytes, path).is_err());

        assert!(OrderedValue::decode(&[99], path).is_err());
        assert!(OrderedValue::decode(&[1, 2], path).is_err());
        assert!(OrderedValue::decode(&[3, 1, 0, 0, 0], path).is_err());
    }

    // ── Base generation ────────────────────────────────────────────

    #[test]
    fn base_round_trips_documents_directory_and_indexes() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        assert!(!path.with_file_name("base.gen_1.ndb.tmp").exists());

        let base = MmapBase::open(&path).unwrap();
        assert_eq!(base.base_seq(), 7);
        assert_eq!(base.doc_count(), 3);
        assert_eq!(base.header().version, STORAGE_VERSION);
        assert_eq!(base.header().flags, 0);
        assert!(base.len() > BASE_HEADER_LEN);
        assert!(!base.is_empty());

        let entries: Vec<BaseDirEntry<'_>> = base.iter().collect();
        assert_eq!(base.iter().len(), 3);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec!["doc-a", "doc-b", "doc-c"]
        );
        assert_eq!(
            entries.iter().map(|entry| entry.slot).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(entries[0].doc_seq, 2);
        assert_eq!(entries[1].doc_seq, 7);
        assert_eq!(entries[2].doc_seq, 5);
        assert_eq!(
            base.doc_bytes(entries[0].data_offset, entries[0].data_len),
            DOC_A
        );
        assert_eq!(
            base.doc_bytes(entries[1].data_offset, entries[1].data_len),
            DOC_B
        );
        assert_eq!(
            base.doc_bytes(entries[2].data_offset, entries[2].data_len),
            DOC_C
        );

        let indexes = base.indexes();
        assert_eq!(indexes.len(), 2);
        assert_eq!(indexes[0].spec.field, "n");
        assert_eq!(indexes[0].spec.kind, V2IndexKind::BTree);
        assert_eq!(indexes[0].entry_count(), 3);
        assert_eq!(base.index_key_owned(&indexes[0], 0), OrderedValue::Number(1.0));
        assert_eq!(base.index_key_owned(&indexes[0], 1), OrderedValue::Number(2.0));
        assert_eq!(base.index_key_owned(&indexes[0], 2), OrderedValue::Number(3.0));
        assert_eq!(base.index_entry(&indexes[0], 0).1, &[0u32]);
        assert_eq!(base.index_entry(&indexes[0], 1).1, &[1u32]);
        assert_eq!(base.index_entry(&indexes[0], 2).1, &[2u32]);
        assert_eq!(indexes[1].spec.field, "tag");
        assert_eq!(indexes[1].spec.kind, V2IndexKind::Hash);
        assert_eq!(indexes[1].entry_count(), 3);
        assert_eq!(
            base.index_key_owned(&indexes[1], 0),
            OrderedValue::String("a".to_string())
        );
        assert_eq!(base.index_entry(&indexes[1], 0).1, &[0u32]);

        // Zero-copy probes: equality by binary search, range by bound walk.
        assert_eq!(
            base.index_slots_for(&indexes[0], &OrderedValue::Number(2.0)),
            Some(&[1u32][..])
        );
        assert_eq!(
            base.index_slots_for(&indexes[0], &OrderedValue::Number(9.0)),
            None
        );
        assert_eq!(
            base.index_range_slots(
                &indexes[0],
                KeyEnd::Included(OrderedValue::Number(2.0)),
                KeyEnd::Unbounded
            ),
            vec![1, 2]
        );
    }

    #[test]
    fn base_index_entries_resolve_ids_to_sorted_slots_and_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let docs = [
            BaseDocInput::new("b", 1, DOC_B),
            BaseDocInput::new("a", 1, DOC_A),
        ];
        let indexes = vec![(
            spec("tag", V2IndexKind::Hash),
            vec![text_key("all", &["b", "a", "a"])],
        )];
        let path = base_file_path(dir.path(), 1);
        write_base_file(&path, 1, &docs, &indexes).unwrap();

        let base = MmapBase::open(&path).unwrap();
        assert_eq!(base.indexes()[0].entry_count(), 1);
        assert_eq!(
            base.index_key_owned(&base.indexes()[0], 0),
            OrderedValue::String("all".to_string())
        );
        assert_eq!(base.index_entry(&base.indexes()[0], 0).1, &[0u32, 1u32]);
    }

    #[test]
    fn base_rejects_a_duplicate_document_id() {
        let dir = tempfile::tempdir().unwrap();
        let docs = [
            BaseDocInput::new("dup", 1, DOC_A),
            BaseDocInput::new("dup", 1, DOC_B),
        ];
        let error = write_base_file(&base_file_path(dir.path(), 1), 1, &docs, &[]).unwrap_err();
        assert!(error.to_string().contains("twice"), "{error}");
    }

    #[test]
    fn base_rejects_invalid_json_and_non_utf8_documents() {
        let dir = tempfile::tempdir().unwrap();
        let bad_json = [BaseDocInput::new("x", 1, b"{not json")];
        let error =
            write_base_file(&base_file_path(dir.path(), 1), 1, &bad_json, &[]).unwrap_err();
        assert!(error.to_string().contains("not valid JSON"), "{error}");

        let bad_utf8 = [BaseDocInput::new("x", 1, &[0xFF, 0xFE])];
        let error =
            write_base_file(&base_file_path(dir.path(), 2), 1, &bad_utf8, &[]).unwrap_err();
        assert!(error.to_string().contains("not UTF-8"), "{error}");
    }

    #[test]
    fn base_rejects_a_document_sequence_above_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let docs = [BaseDocInput::new("x", 9, DOC_A)];
        let error = write_base_file(&base_file_path(dir.path(), 1), 5, &docs, &[]).unwrap_err();
        assert!(error.to_string().contains("above base_seq"), "{error}");
    }

    #[test]
    fn base_rejects_an_index_referencing_an_unknown_document() {
        let dir = tempfile::tempdir().unwrap();
        let docs = [BaseDocInput::new("a", 1, DOC_A)];
        let indexes = vec![(
            spec("tag", V2IndexKind::Hash),
            vec![text_key("a", &["ghost"])],
        )];
        let error =
            write_base_file(&base_file_path(dir.path(), 1), 1, &docs, &indexes).unwrap_err();
        assert!(error.to_string().contains("unknown document id"), "{error}");
    }

    #[test]
    fn base_rejects_a_duplicate_index_field() {
        let dir = tempfile::tempdir().unwrap();
        let docs = [BaseDocInput::new("a", 1, DOC_A)];
        let indexes = vec![
            (spec("tag", V2IndexKind::Hash), vec![text_key("a", &["a"])]),
            (spec("tag", V2IndexKind::BTree), vec![text_key("a", &["a"])]),
        ];
        let error =
            write_base_file(&base_file_path(dir.path(), 1), 1, &docs, &indexes).unwrap_err();
        assert!(error.to_string().contains("twice"), "{error}");
    }

    #[test]
    fn base_detects_header_crc_tampering() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        tamper(&path, 20, 0x01);
        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("header CRC mismatch"), "{message}");
    }

    #[test]
    fn base_detects_magic_tampering() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        tamper(&path, 0, 0x01);
        rewrite_base_header_crc(&path);
        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("bad base magic"), "{message}");
    }

    #[test]
    fn base_detects_a_foreign_format_version() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        let mut bytes = fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&(STORAGE_VERSION + 1).to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        rewrite_base_header_crc(&path);
        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("format version"), "{message}");
    }

    #[test]
    fn base_detects_unknown_flags() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        let mut bytes = fs::read(&path).unwrap();
        bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        rewrite_base_header_crc(&path);
        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("unknown flags"), "{message}");
    }

    #[test]
    fn base_detects_directory_crc_tampering() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        let header = decode_base_header(&fs::read(&path).unwrap(), &path).unwrap();
        tamper(&path, header.dir_offset as usize + 3, 0xFF);
        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("directory CRC mismatch"), "{message}");
    }

    #[test]
    fn base_detects_index_crc_tampering() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        let header = decode_base_header(&fs::read(&path).unwrap(), &path).unwrap();
        tamper(&path, header.idx_offset as usize, 0xFF);
        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("index section CRC mismatch"), "{message}");
    }

    #[test]
    fn base_rejects_trailing_bytes() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(0);
        fs::write(&path, &bytes).unwrap();
        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("sections end at"), "{message}");
    }

    #[test]
    fn base_rejects_an_unsorted_directory() {
        let dir = tempfile::tempdir().unwrap();
        let docs = [
            BaseDocInput::new("a", 1, DOC_A),
            BaseDocInput::new("b", 1, DOC_B),
        ];
        let path = base_file_path(dir.path(), 1);
        write_base_file(&path, 1, &docs, &[]).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        let header = decode_base_header(&bytes, &path).unwrap();
        // Swap the two packed entries while leaving the offset table intact:
        // the table then points each slot at the other document.
        let start = header.dir_offset as usize + 2 * 8;
        let end = start + header.dir_len as usize - 4 - 2 * 8;
        let entry_len = (end - start) / 2;
        let first = bytes[start..start + entry_len].to_vec();
        let second = bytes[start + entry_len..end].to_vec();
        bytes[start..start + entry_len].copy_from_slice(&second);
        bytes[start + entry_len..end].copy_from_slice(&first);
        let crc = crc32(&bytes[header.dir_offset as usize..end]);
        let crc_at = header.dir_offset as usize + header.dir_len as usize - 4;
        bytes[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(
            message.contains("not strictly ascending")
                || message.contains("offset table disagrees"),
            "{message}"
        );
    }

    #[test]
    fn base_detects_a_document_count_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let docs = [BaseDocInput::new("a", 1, DOC_A)];
        let path = base_file_path(dir.path(), 1);
        write_base_file(&path, 1, &docs, &[]).unwrap();

        let mut bytes = fs::read(&path).unwrap();
        bytes[24..32].copy_from_slice(&2u64.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        rewrite_base_header_crc(&path);

        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("directory"), "{message}");
    }

    #[test]
    fn empty_base_round_trips_with_no_documents() {
        let dir = tempfile::tempdir().unwrap();
        let path = base_file_path(dir.path(), 1);
        write_base_file(&path, 0, &[], &[]).unwrap();

        let base = MmapBase::open(&path).unwrap();
        assert_eq!(base.doc_count(), 0);
        assert_eq!(base.base_seq(), 0);
        assert_eq!(base.iter().len(), 0);
        assert!(base.indexes().is_empty());
        assert_eq!(base.len(), BASE_HEADER_LEN + 4 + 8);
    }

    #[test]
    fn base_round_trips_composite_index_keys() {
        let dir = tempfile::tempdir().unwrap();
        let docs = [BaseDocInput::new("a", 1, br#"{"k":[1,2]}"#)];
        let indexes = vec![(
            spec("k", V2IndexKind::BTree),
            vec![BaseIndexEntry::new(
                OrderedValue::Composite("[1,2]".to_string()),
                vec!["a".to_string()],
            )],
        )];
        let path = base_file_path(dir.path(), 1);
        write_base_file(&path, 1, &docs, &indexes).unwrap();

        let base = MmapBase::open(&path).unwrap();
        assert_eq!(
            base.index_key_owned(&base.indexes()[0], 0),
            OrderedValue::Composite("[1,2]".to_string())
        );
        assert_eq!(base.index_entry(&base.indexes()[0], 0).1, &[0u32]);
    }

    #[test]
    fn base_rejects_a_truncated_file() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..BASE_HEADER_LEN + 4]).unwrap();
        let message = corruption_message(MmapBase::open(&path).unwrap_err());
        assert!(message.contains("past the"), "{message}");
    }

    // ── WAL ────────────────────────────────────────────────────────

    #[test]
    fn wal_create_refuses_an_existing_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        WalWriter::create(&path, 1, 0).unwrap();
        assert!(WalWriter::create(&path, 1, 0).is_err());
    }

    #[test]
    fn wal_replays_single_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 3);
        let mut wal = WalWriter::create(&path, 3, 10).unwrap();
        wal.append(&WalRecord::put(11, "a", DOC_A.to_vec())).unwrap();
        wal.append(&WalRecord::delete(12, "b")).unwrap();
        assert_eq!(wal.last_seq(), 12);
        assert_eq!(wal.base_seq(), 10);
        assert_eq!(wal.wal_gen(), 3);
        assert_eq!(wal.path(), path.as_path());

        let replay = replay_wal(&path, 10, false).unwrap();
        assert_eq!(replay.wal_gen, 3);
        assert_eq!(replay.base_seq, 10);
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.last_seq, 12);
        assert_eq!(replay.truncated_tail_bytes, 0);
        assert_eq!(replay.good_bytes, fs::metadata(&path).unwrap().len());
        assert_eq!(replay.total_bytes, replay.good_bytes);
        assert_eq!(replay.records[0], WalRecord::put(11, "a", DOC_A.to_vec()));
        assert_eq!(replay.records[1], WalRecord::delete(12, "b"));
    }

    #[test]
    fn wal_batch_append_writes_every_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut wal = WalWriter::create(&path, 1, 0).unwrap();
        let batch = vec![
            WalRecord::put(1, "a", DOC_A.to_vec()),
            WalRecord::put(2, "b", DOC_B.to_vec()),
            WalRecord::put(3, "c", DOC_C.to_vec()),
        ];
        wal.append_batch(&batch).unwrap();
        assert_eq!(wal.last_seq(), 3);

        let replay = replay_wal(&path, 0, false).unwrap();
        assert_eq!(replay.records, batch);
        assert_eq!(replay.last_seq, 3);
        assert_eq!(replay.good_bytes, fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn wal_index_frames_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut wal = WalWriter::create(&path, 1, 0).unwrap();
        let added = spec("tag", V2IndexKind::Hash);
        let dropped = spec("n", V2IndexKind::BTree);
        wal.append(&WalRecord::index_add(1, &added).unwrap())
            .unwrap();
        wal.append(&WalRecord::index_drop(2, &dropped).unwrap())
            .unwrap();

        let replay = replay_wal(&path, 0, false).unwrap();
        assert_eq!(replay.records[0].op, WalOp::IndexAdd);
        assert_eq!(replay.records[0].id, "tag");
        assert_eq!(
            serde_json::from_slice::<V2IndexSpec>(&replay.records[0].payload).unwrap(),
            added
        );
        assert_eq!(replay.records[1].op, WalOp::IndexDrop);
        assert_eq!(replay.records[1].id, "n");
        assert_eq!(
            serde_json::from_slice::<V2IndexSpec>(&replay.records[1].payload).unwrap(),
            dropped
        );
    }

    #[test]
    fn wal_writer_enforces_sequence_continuity() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut wal = WalWriter::create(&path, 1, 5).unwrap();
        wal.append(&WalRecord::put(6, "a", DOC_A.to_vec())).unwrap();
        let size = fs::metadata(&path).unwrap().len();

        let error = wal
            .append(&WalRecord::put(8, "b", DOC_B.to_vec()))
            .unwrap_err();
        assert!(error.to_string().contains("does not follow"), "{error}");
        assert_eq!(fs::metadata(&path).unwrap().len(), size);
        assert_eq!(wal.last_seq(), 6);

        let error = wal
            .append_batch(&[
                WalRecord::put(7, "b", DOC_B.to_vec()),
                WalRecord::put(9, "c", DOC_C.to_vec()),
            ])
            .unwrap_err();
        assert!(error.to_string().contains("does not follow"), "{error}");
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            size,
            "a rejected batch writes nothing"
        );
    }

    #[test]
    fn wal_rejects_invalid_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut wal = WalWriter::create(&path, 1, 0).unwrap();

        let mut delete_with_payload = WalRecord::delete(1, "a");
        delete_with_payload.payload = b"x".to_vec();
        assert!(wal.append(&delete_with_payload).is_err());

        let bad_json = WalRecord::put(1, "a", b"{not json".to_vec());
        assert!(wal.append(&bad_json).is_err());

        let bad_id = WalRecord::put(1, "", DOC_A.to_vec());
        assert!(wal.append(&bad_id).is_err());
        assert_eq!(wal.last_seq(), 0);
    }

    #[test]
    fn wal_writer_open_recovers_the_last_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        {
            let mut wal = WalWriter::create(&path, 1, 0).unwrap();
            wal.append_batch(&[
                WalRecord::put(1, "a", DOC_A.to_vec()),
                WalRecord::put(2, "b", DOC_B.to_vec()),
            ])
            .unwrap();
        }
        let mut wal = WalWriter::open(&path).unwrap();
        assert_eq!(wal.last_seq(), 2);
        wal.append(&WalRecord::put(3, "c", DOC_C.to_vec())).unwrap();

        let replay = replay_wal(&path, 0, false).unwrap();
        assert_eq!(replay.records.len(), 3);
        assert_eq!(replay.last_seq, 3);
    }

    #[test]
    fn wal_empty_generation_replays_to_its_base_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let wal = WalWriter::create(&path, 1, 17).unwrap();
        assert_eq!(wal.last_seq(), 17);

        let replay = replay_wal(&path, 17, false).unwrap();
        assert!(replay.records.is_empty());
        assert_eq!(replay.last_seq, 17);
        assert_eq!(replay.good_bytes, WAL_HEADER_LEN as u64);
        assert!(scan_wal_since(&path, 17, 0).unwrap().is_empty());
    }

    #[test]
    fn wal_torn_tail_is_truncated_and_later_appends_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        {
            let mut wal = WalWriter::create(&path, 1, 0).unwrap();
            wal.append_batch(&[
                WalRecord::put(1, "a", DOC_A.to_vec()),
                WalRecord::put(2, "b", DOC_B.to_vec()),
                WalRecord::put(3, "c", DOC_C.to_vec()),
            ])
            .unwrap();
        }
        let full_len = fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(full_len - 5).unwrap();
        drop(file);

        let message = corruption_message(replay_wal(&path, 0, false).unwrap_err());
        assert!(message.contains("torn WAL tail"), "{message}");

        let replay = replay_wal(&path, 0, true).unwrap();
        assert_eq!(replay.records.len(), 2);
        assert_eq!(replay.last_seq, 2);
        let third_frame_len = encode_frame(&WalRecord::put(3, "c", DOC_C.to_vec()))
            .unwrap()
            .len() as u64;
        assert_eq!(replay.truncated_tail_bytes, third_frame_len - 5);
        assert_eq!(replay.good_bytes, full_len - third_frame_len);
        assert_eq!(fs::metadata(&path).unwrap().len(), replay.good_bytes);

        let mut wal = WalWriter::open(&path).unwrap();
        assert_eq!(wal.last_seq(), 2);
        wal.append(&WalRecord::put(3, "c", DOC_C.to_vec())).unwrap();

        let replay = replay_wal(&path, 0, false).unwrap();
        assert_eq!(replay.records.len(), 3);
        assert_eq!(replay.last_seq, 3);
        assert_eq!(replay.records[2], WalRecord::put(3, "c", DOC_C.to_vec()));
    }

    #[test]
    fn wal_truncated_length_prefix_is_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        {
            let mut wal = WalWriter::create(&path, 1, 0).unwrap();
            wal.append_batch(&[
                WalRecord::put(1, "a", DOC_A.to_vec()),
                WalRecord::put(2, "b", DOC_B.to_vec()),
            ])
            .unwrap();
        }
        let frame = encode_frame(&WalRecord::put(2, "b", DOC_B.to_vec())).unwrap();
        let full_len = fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(full_len - frame.len() as u64 + 2).unwrap();
        drop(file);

        let replay = replay_wal(&path, 0, true).unwrap();
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.truncated_tail_bytes, 2);
    }

    #[test]
    fn wal_mid_frame_bit_flip_is_corruption_with_the_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        {
            let mut wal = WalWriter::create(&path, 1, 0).unwrap();
            wal.append_batch(&[
                WalRecord::put(1, "a", DOC_A.to_vec()),
                WalRecord::put(2, "b", DOC_B.to_vec()),
                WalRecord::put(3, "c", DOC_C.to_vec()),
            ])
            .unwrap();
        }
        let first = encode_frame(&WalRecord::put(1, "a", DOC_A.to_vec())).unwrap();
        tamper(&path, (WAL_HEADER_LEN + first.len() + 16) as usize, 0x01);

        let message = corruption_message(replay_wal(&path, 0, false).unwrap_err());
        assert!(message.contains("frame 1 at byte"), "{message}");
        assert!(message.contains("bytes follow it"), "{message}");

        assert!(scan_wal_since(&path, 0, 0).is_err());
    }

    #[test]
    fn repair_wal_file_quarantines_then_truncates_a_corrupt_middle_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        {
            let mut wal = WalWriter::create(&path, 1, 0).unwrap();
            wal.append_batch(&[
                WalRecord::put(1, "a", DOC_A.to_vec()),
                WalRecord::put(2, "b", DOC_B.to_vec()),
                WalRecord::put(3, "c", DOC_C.to_vec()),
            ])
            .unwrap();
        }
        let clean = fs::read(&path).unwrap();
        let first = encode_frame(&WalRecord::put(1, "a", DOC_A.to_vec())).unwrap();
        tamper(&path, (WAL_HEADER_LEN + first.len() + 16) as usize, 0x01);
        let corrupted = fs::read(&path).unwrap();

        let stats = repair_wal_file(&path, 0).unwrap();
        assert_eq!(stats.wal_gen, 1);
        assert_eq!(stats.base_seq, 0);
        assert_eq!(stats.frames_kept, 1);
        assert_eq!(stats.last_seq, 1);
        assert!(stats.truncated_bytes > 0);
        assert!(stats.issue.as_deref().unwrap().contains("frame 1"));
        assert_eq!(stats.good_bytes, WAL_HEADER_LEN as u64 + first.len() as u64);

        let backup = stats.backup.expect("repair creates a backup");
        assert!(backup.exists());
        assert!(backup.to_string_lossy().contains(".corrupt."));
        assert_eq!(
            fs::read(&backup).unwrap(),
            corrupted,
            "the backup is byte-identical to the pre-repair WAL"
        );
        assert_ne!(fs::read(&backup).unwrap(), clean);

        let replay = replay_wal(&path, 0, false).unwrap();
        assert_eq!(replay.records.len(), 1);
        assert_eq!(replay.last_seq, 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), stats.good_bytes);
    }

    #[test]
    fn repair_wal_file_on_a_clean_wal_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut wal = WalWriter::create(&path, 1, 0).unwrap();
        wal.append(&WalRecord::put(1, "a", DOC_A.to_vec())).unwrap();
        let size = fs::metadata(&path).unwrap().len();

        let stats = repair_wal_file(&path, 0).unwrap();
        assert_eq!(stats.truncated_bytes, 0);
        assert_eq!(stats.frames_kept, 1);
        assert!(stats.issue.is_none());
        assert!(stats.backup.is_none());
        assert_eq!(fs::metadata(&path).unwrap().len(), size);
    }

    #[test]
    fn scan_wal_since_returns_only_newer_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut wal = WalWriter::create(&path, 1, 0).unwrap();
        wal.append_batch(&[
            WalRecord::put(1, "a", DOC_A.to_vec()),
            WalRecord::put(2, "b", DOC_B.to_vec()),
            WalRecord::delete(3, "a"),
        ])
        .unwrap();

        assert_eq!(scan_wal_since(&path, 0, 0).unwrap().len(), 3);
        assert_eq!(scan_wal_since(&path, 0, 1).unwrap().len(), 2);
        let tail = scan_wal_since(&path, 0, 2).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 3);
        assert!(scan_wal_since(&path, 0, 3).unwrap().is_empty());
    }

    #[test]
    fn scan_wal_since_excludes_an_in_flight_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut wal = WalWriter::create(&path, 1, 0).unwrap();
        wal.append(&WalRecord::put(1, "a", DOC_A.to_vec())).unwrap();
        let clean_len = fs::metadata(&path).unwrap().len();
        let frame = encode_frame(&WalRecord::put(2, "b", DOC_B.to_vec())).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(&frame[..frame.len() - 3]);
        fs::write(&path, &bytes).unwrap();
        assert!(fs::metadata(&path).unwrap().len() > clean_len);

        let records = scan_wal_since(&path, 0, 0).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].seq, 1);
    }

    #[test]
    fn wal_rejects_a_base_sequence_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        WalWriter::create(&path, 1, 5).unwrap();
        let message = corruption_message(replay_wal(&path, 4, false).unwrap_err());
        assert!(message.contains("base_seq is 5"), "{message}");
    }

    #[test]
    fn wal_detects_header_crc_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        WalWriter::create(&path, 1, 0).unwrap();
        tamper(&path, 20, 0x01);
        let message = corruption_message(replay_wal(&path, 0, false).unwrap_err());
        assert!(message.contains("WAL header CRC mismatch"), "{message}");
    }

    #[test]
    fn wal_detects_a_sequence_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut bytes = encode_wal_header(&WalHeader {
            wal_gen: 1,
            base_seq: 0,
        })
        .to_vec();
        bytes.extend_from_slice(&encode_frame(&WalRecord::put(1, "a", DOC_A.to_vec())).unwrap());
        bytes.extend_from_slice(&encode_frame(&WalRecord::put(3, "c", DOC_C.to_vec())).unwrap());
        fs::write(&path, &bytes).unwrap();

        let message = corruption_message(replay_wal(&path, 0, false).unwrap_err());
        assert!(message.contains("frame 1 at byte"), "{message}");
        assert!(message.contains("does not follow 1"), "{message}");
    }

    #[test]
    fn wal_detects_a_first_frame_that_skips_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut bytes = encode_wal_header(&WalHeader {
            wal_gen: 1,
            base_seq: 5,
        })
        .to_vec();
        bytes.extend_from_slice(&encode_frame(&WalRecord::put(7, "a", DOC_A.to_vec())).unwrap());
        fs::write(&path, &bytes).unwrap();

        let message = corruption_message(replay_wal(&path, 5, false).unwrap_err());
        assert!(message.contains("does not follow 5"), "{message}");
    }

    #[test]
    fn wal_detects_an_unknown_op_tag() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        let mut frame = encode_frame(&WalRecord::put(1, "a", DOC_A.to_vec())).unwrap();
        frame[4] = 9;
        let crc = crc32(&frame[..frame.len() - 4]);
        let len = frame.len();
        frame[len - 4..].copy_from_slice(&crc.to_le_bytes());
        let mut bytes = encode_wal_header(&WalHeader {
            wal_gen: 1,
            base_seq: 0,
        })
        .to_vec();
        bytes.extend_from_slice(&frame);
        fs::write(&path, &bytes).unwrap();

        let message = corruption_message(replay_wal(&path, 0, false).unwrap_err());
        assert!(message.contains("unknown op tag 9"), "{message}");
    }

    #[test]
    fn wal_rejects_a_truncated_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = wal_file_path(dir.path(), 1);
        fs::write(&path, b"NDB2").unwrap();
        let message = corruption_message(replay_wal(&path, 0, false).unwrap_err());
        assert!(message.contains("shorter than its"), "{message}");
    }

    // ── fsck ───────────────────────────────────────────────────────

    #[test]
    fn fsck_accepts_a_clean_directory() {
        let (dir, manifest) = clean_v2_dir();
        let report = fsck_v2_dir(dir.path()).unwrap();
        assert_eq!(report.base_gen, manifest.base_gen);
        assert_eq!(report.wal_gen, manifest.wal_gen);
        assert_eq!(report.base_seq, 7);
        assert_eq!(report.doc_count, 3);
        assert_eq!(report.index_count, 2);
        assert_eq!(report.wal_frames, 2);
        assert_eq!(report.last_seq, 9);
        assert_eq!(report.min_available_seq, 1);
        assert!(report.base_bytes > BASE_HEADER_LEN as u64);
        assert!(report.wal_bytes > WAL_HEADER_LEN as u64);
    }

    #[test]
    fn fsck_rejects_a_manifest_base_sequence_mismatch() {
        let (dir, mut manifest) = clean_v2_dir();
        manifest.base_seq = 6;
        write_manifest_atomic(dir.path(), &manifest).unwrap();
        let message = corruption_message(fsck_v2_dir(dir.path()).unwrap_err());
        assert!(message.contains("base_seq 7"), "{message}");
    }

    #[test]
    fn fsck_rejects_a_document_count_mismatch() {
        let (dir, mut manifest) = clean_v2_dir();
        manifest.doc_count = 9;
        write_manifest_atomic(dir.path(), &manifest).unwrap();
        let message = corruption_message(fsck_v2_dir(dir.path()).unwrap_err());
        assert!(message.contains("documents"), "{message}");
    }

    #[test]
    fn fsck_rejects_an_index_missing_from_the_base() {
        let (dir, mut manifest) = clean_v2_dir();
        // Same count as the base, but one declared field does not exist there.
        manifest.indexes[1].field = "missing".to_string();
        write_manifest_atomic(dir.path(), &manifest).unwrap();
        let message = corruption_message(fsck_v2_dir(dir.path()).unwrap_err());
        assert!(message.contains("missing from the base"), "{message}");
    }

    #[test]
    fn fsck_rejects_a_document_that_is_no_longer_json() {
        let (dir, _manifest) = clean_v2_dir();
        let path = base_file_path(dir.path(), 1);
        // The first document in the data section is DOC_A (ids sort a, b, c);
        // turning ':' into ';' keeps every offset and CRC intact.
        tamper(&path, BASE_HEADER_LEN + 4, 0x01);
        let message = corruption_message(fsck_v2_dir(dir.path()).unwrap_err());
        assert!(message.contains("not valid JSON"), "{message}");
    }

    #[test]
    fn fsck_rejects_a_missing_wal_generation() {
        let (dir, _manifest) = clean_v2_dir();
        fs::remove_file(wal_file_path(dir.path(), 1)).unwrap();
        let message = corruption_message(fsck_v2_dir(dir.path()).unwrap_err());
        assert!(message.contains("does not exist"), "{message}");
    }

    #[test]
    fn fsck_rejects_a_corrupt_wal() {
        let (dir, _manifest) = clean_v2_dir();
        let path = wal_file_path(dir.path(), 1);
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"garbage tail");
        fs::write(&path, &bytes).unwrap();
        assert!(fsck_v2_dir(dir.path()).is_err());
    }

    #[test]
    fn fsck_rejects_a_wal_header_generation_mismatch() {
        let (dir, _manifest) = clean_v2_dir();
        let path = wal_file_path(dir.path(), 1);
        let mut bytes = fs::read(&path).unwrap();
        bytes[12..20].copy_from_slice(&7u64.to_le_bytes());
        let crc = crc32(&bytes[..WAL_HEADER_FIELDS_LEN]);
        bytes[WAL_HEADER_FIELDS_LEN..WAL_HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        let message = corruption_message(fsck_v2_dir(dir.path()).unwrap_err());
        assert!(message.contains("generation 7"), "{message}");
    }
}

