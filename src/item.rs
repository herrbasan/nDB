//! Item buckets (#9): a second bucket kind keyed by item identity.
//!
//! A `hash` bucket keys blobs by content (SHA-256, dedup, refcounted).
//! An `items` bucket keys *folders* by an engine-issued item id: the
//! application streams bytes to the folder path itself — nDB is never in
//! the ingest path and never holds a buffer — and commits one record of
//! facts (`name`, `size`, `sha256`, plus caller fields) when the bytes
//! are at rest. The engine treats the folder as a unit: it moves,
//! trashes, restores and purges folders whole and never parses their
//! contents beyond the committed original's integrity check in `verify`.
//!
//! Records live in an append-only journal at `_files/<bucket>/items.jsonl`
//! — full-record lines, last write wins, tombstones mark deletions.
//! Ordering mirrors the database's own contract (#7): the journal append
//! lands before any in-memory or on-disk state moves; a folder that fails
//! to move after the commit is under-cleaning — named, reported, swept
//! later, never silent.

use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bucket::sha256_file_hex;
use crate::error::{Error, Result};
use crate::id::generate_unique_with_prefix;
use crate::report;
use crate::storage;
use crate::Persistence;

/// Default reserved-item TTL: 30 minutes (#9). An abandoned reservation
/// must not be an undeletable empty folder.
pub const DEFAULT_RESERVED_TTL_SECS: u64 = 1800;

/// Lifecycle state of an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemState {
    /// Created, not yet committed. Swept by `reserved_ttl_seconds`.
    Reserved,
    /// Committed. Deletable.
    Live,
}

impl ItemState {
    fn as_str(&self) -> &'static str {
        match self {
            ItemState::Reserved => "reserved",
            ItemState::Live => "live",
        }
    }
}

/// One item record. Serialized as a full journal line per transition;
/// replay is last-write-wins per id.
#[derive(Clone, Debug)]
pub struct ItemRecord {
    pub id: String,
    pub state: ItemState,
    /// Committed facts: `name`, `size`, `sha256` required at commit,
    /// `mime` and anything else caller-supplied.
    pub facts: Option<Value>,
    pub created: u64,
    pub committed: Option<u64>,
    /// Tombstone timestamp. A deleted record keeps its prior state so a
    /// restore returns the item to exactly what it was.
    pub deleted: Option<u64>,
}

impl ItemRecord {
    fn to_json(&self) -> Value {
        json!({
            "_item": self.id,
            "state": self.state.as_str(),
            "facts": self.facts,
            "created": self.created,
            "committed": self.committed,
            "deleted": self.deleted,
        })
    }

    fn from_json(v: &Value, journal: &Path, line_no: usize) -> Result<ItemRecord> {
        let bad = |what: &str| Error::corruption(journal, format!("line {line_no}: {what}"));
        let id = v
            .get("_item")
            .and_then(|x| x.as_str())
            .ok_or_else(|| bad("missing '_item' id"))?
            .to_string();
        let state = match v.get("state").and_then(|x| x.as_str()) {
            Some("reserved") => ItemState::Reserved,
            Some("live") => ItemState::Live,
            _ => return Err(bad("'state' must be \"reserved\" or \"live\"")),
        };
        let facts = match v.get("facts") {
            Some(f) if f.is_object() => Some(f.clone()),
            _ => None,
        };
        let created = v
            .get("created")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| bad("missing 'created' timestamp"))?;
        let committed = v.get("committed").and_then(|x| x.as_u64());
        let deleted = v.get("deleted").and_then(|x| x.as_u64());
        Ok(ItemRecord {
            id,
            state,
            facts,
            created,
            committed,
            deleted,
        })
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Validate caller-committed facts. `name`, `size` and `sha256` are the
/// engine's integrity contract with `verify`; everything else passes
/// through untouched.
fn validate_facts(facts: &Value) -> Result<()> {
    let obj = facts
        .as_object()
        .ok_or_else(|| Error::invalid_arg("item facts must be a JSON object"))?;
    match obj.get("name").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => {}
        _ => return Err(Error::invalid_arg("item facts require a non-empty 'name'")),
    }
    if obj.get("size").and_then(|v| v.as_u64()).is_none() {
        return Err(Error::invalid_arg(
            "item facts require a non-negative integer 'size'",
        ));
    }
    match obj.get("sha256").and_then(|v| v.as_str()) {
        Some(s) if s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()) => {}
        _ => {
            return Err(Error::invalid_arg(
                "item facts require a 64-char hex 'sha256'",
            ))
        }
    }
    Ok(())
}

/// An item bucket: `_files/<bucket>/<itemId>/` folders plus the
/// `items.jsonl` record journal. Constructed by `Database::open` for
/// buckets declared `kind: "items"` in `meta.json`.
pub struct ItemBucket {
    name: String,
    base_dir: PathBuf,
    reserved_ttl: Duration,
    records: RwLock<HashMap<String, ItemRecord>>,
    journal: Mutex<Option<fs::File>>,
}

impl ItemBucket {
    /// Open (and replay) an item bucket. A missing journal is a new
    /// bucket; a malformed journal line is corruption and fails loudly —
    /// this file is engine-owned, so its integrity is an invariant.
    pub fn open(name: &str, base_dir: &Path, reserved_ttl: Duration) -> Result<Self> {
        let bucket = ItemBucket {
            name: name.to_string(),
            base_dir: base_dir.to_path_buf(),
            reserved_ttl,
            records: RwLock::new(HashMap::new()),
            journal: Mutex::new(None),
        };
        let journal_path = bucket.journal_path();
        if journal_path.exists() {
            let raw = fs::read_to_string(&journal_path)
                .map_err(Error::io_err(&journal_path, "read item journal"))?;
            let mut records = bucket.records.write();
            for (i, line) in raw.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let v: Value = serde_json::from_str(line).map_err(|e| {
                    Error::corruption(&journal_path, format!("line {}: invalid JSON: {e}", i + 1))
                })?;
                let record = ItemRecord::from_json(&v, &journal_path, i + 1)?;
                records.insert(record.id.clone(), record);
            }
        }
        Ok(bucket)
    }

    fn dir(&self) -> PathBuf {
        self.base_dir.join("_files").join(&self.name)
    }

    fn journal_path(&self) -> PathBuf {
        self.dir().join("items.jsonl")
    }

    fn trash_dir(&self) -> PathBuf {
        self.base_dir.join("_trash").join("files").join(&self.name)
    }

    /// The folder path of one item. Engine-issued — callers never
    /// assemble it themselves.
    pub fn item_path(&self, item_id: &str) -> PathBuf {
        self.dir().join(item_id)
    }

    fn append(&self, record: &ItemRecord, persistence: Persistence) -> Result<()> {
        let line = serde_json::to_string(&record.to_json())?;
        let mut handle = self.journal.lock();
        if handle.is_none() {
            let dir = self.dir();
            fs::create_dir_all(&dir).map_err(Error::io_err(&dir, "create item bucket dir"))?;
            // The item journal is engine-owned and headerless; create on
            // first append (storage::open_for_append requires existing).
            let journal_path = self.journal_path();
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&journal_path)
                .map_err(Error::io_err(&journal_path, "open item journal for append"))?;
            *handle = Some(file);
        }
        let journal_path = self.journal_path();
        if let Some(ref mut file) = *handle {
            match persistence {
                Persistence::Immediate => {
                    storage::append_line_sync(file, &journal_path, &line)?;
                }
                _ => {
                    storage::append_line(file, &journal_path, &line)?;
                }
            }
        }
        Ok(())
    }

    /// Reserve an item: create the folder, then journal the reserved
    /// record. If the journal write fails the empty folder is removed —
    /// the operation is all-or-nothing from the caller's side, and every
    /// other crash prefix is sweepable or verify-visible by design.
    pub fn create(&self, persistence: Persistence) -> Result<(String, PathBuf)> {
        let existing: HashSet<String> = self.records.read().keys().cloned().collect();
        let item_id = generate_unique_with_prefix("itm", &existing);
        let folder = self.item_path(&item_id);
        fs::create_dir_all(&folder).map_err(Error::io_err(&folder, "create item folder"))?;

        let record = ItemRecord {
            id: item_id.clone(),
            state: ItemState::Reserved,
            facts: None,
            created: now_secs(),
            committed: None,
            deleted: None,
        };
        if let Err(e) = self.append(&record, persistence) {
            if let Err(cleanup) = fs::remove_dir_all(&folder) {
                report::suppressed("create_item: remove folder after failed journal write", cleanup);
            }
            return Err(e);
        }
        self.records.write().insert(item_id.clone(), record);
        Ok((item_id, folder))
    }

    /// Commit an item: one journal write carrying the caller's facts.
    /// Only a reserved item can be committed.
    pub fn commit(&self, item_id: &str, facts: Value, persistence: Persistence) -> Result<()> {
        validate_facts(&facts)?;
        let record = {
            let records = self.records.read();
            let current = records
                .get(item_id)
                .filter(|r| r.deleted.is_none())
                .ok_or_else(|| Error::not_found(item_id))?;
            if current.state != ItemState::Reserved {
                return Err(Error::invalid_arg(format!(
                    "item '{item_id}' is already committed"
                )));
            }
            ItemRecord {
                state: ItemState::Live,
                facts: Some(facts),
                committed: Some(now_secs()),
                ..current.clone()
            }
        };
        self.append(&record, persistence)?;
        self.records.write().insert(item_id.to_string(), record);
        Ok(())
    }

    /// Read one live or reserved item. Tombstoned and unknown ids are
    /// not found.
    pub fn get(&self, item_id: &str) -> Result<ItemRecord> {
        self.records
            .read()
            .get(item_id)
            .filter(|r| r.deleted.is_none())
            .cloned()
            .ok_or_else(|| Error::not_found(item_id))
    }

    /// List non-deleted items, optionally filtered to one state.
    pub fn list(&self, state: Option<ItemState>) -> Vec<ItemRecord> {
        self.records
            .read()
            .values()
            .filter(|r| r.deleted.is_none())
            .filter(|r| state.map_or(true, |s| r.state == s))
            .cloned()
            .collect()
    }

    /// Delete an item: tombstone first, then move the folder to trash.
    /// A folder that fails to move after the commit stays where it is
    /// and is named — under-cleaning is recoverable (verify flags it,
    /// the sweep/purge paths take folders whole), silence would strand
    /// it forever.
    pub fn delete(&self, item_id: &str, persistence: Persistence) -> Result<()> {
        let record = {
            let records = self.records.read();
            let current = records
                .get(item_id)
                .filter(|r| r.deleted.is_none())
                .ok_or_else(|| Error::not_found(item_id))?;
            ItemRecord {
                deleted: Some(now_secs()),
                ..current.clone()
            }
        };
        self.append(&record, persistence)?;
        self.records.write().insert(item_id.to_string(), record);

        let folder = self.item_path(item_id);
        if folder.exists() {
            let trash = self.trash_dir();
            if let Err(e) = fs::create_dir_all(&trash)
                .map_err(Error::io_err(&trash, "create item trash dir"))
                .and_then(|_| {
                    fs::rename(folder.clone(), trash.join(item_id))
                        .map_err(Error::io_err(&folder, "move item folder to trash"))
                })
            {
                report::suppressed("delete_item: move folder to trash", e);
            }
        }
        Ok(())
    }

    /// Restore a tombstoned item to its pre-delete state, moving the
    /// folder back if it is still in trash. Mirrors document restore:
    /// the record comes back even when its folder no longer can.
    pub fn restore(&self, item_id: &str, persistence: Persistence) -> Result<()> {
        let record = {
            let records = self.records.read();
            let current = records.get(item_id).ok_or_else(|| Error::not_found(item_id))?;
            if current.deleted.is_none() {
                return Err(Error::invalid_arg(format!(
                    "item '{item_id}' is not deleted"
                )));
            }
            ItemRecord {
                deleted: None,
                ..current.clone()
            }
        };
        self.append(&record, persistence)?;
        self.records.write().insert(item_id.to_string(), record);

        let trashed = self.trash_dir().join(item_id);
        if trashed.exists() {
            let folder = self.item_path(item_id);
            if let Err(e) = fs::rename(&trashed, &folder)
                .map_err(Error::io_err(&trashed, "restore item folder from trash"))
            {
                report::suppressed("restore_item: move folder back from trash", e);
            }
        }
        Ok(())
    }

    /// Sweep reserved items older than the reserved TTL: each is
    /// tombstoned and its folder moved to trash, exactly as delete().
    pub fn sweep_reserved(&self, persistence: Persistence) -> Result<usize> {
        let cutoff = now_secs().saturating_sub(self.reserved_ttl.as_secs());
        let stale: Vec<String> = self
            .records
            .read()
            .values()
            .filter(|r| r.deleted.is_none() && r.state == ItemState::Reserved && r.created <= cutoff)
            .map(|r| r.id.clone())
            .collect();
        let mut swept = 0;
        for item_id in stale {
            self.delete(&item_id, persistence)?;
            swept += 1;
        }
        Ok(swept)
    }

    /// Verify the bucket: every live item's original exists and matches
    /// its committed `size`/`sha256`; reserved items have their folder;
    /// folders without a live record are orphans (sweepable). Returns
    /// human-readable anomalies; empty means clean. Hashing is streaming
    /// — originals are never read whole.
    pub fn verify(&self) -> Vec<String> {
        let mut anomalies = Vec::new();
        let records = self.records.read();

        for record in records.values() {
            // Tombstoned items are covered by the folder scan below —
            // flagging them here too would double-report.
            if record.deleted.is_some() {
                continue;
            }
            let folder = self.item_path(&record.id);
            if !folder.is_dir() {
                anomalies.push(format!("{}: record has no folder", record.id));
                continue;
            }
            if record.state == ItemState::Reserved {
                continue;
            }
            // Live: the committed original is the integrity contract.
            let facts = record.facts.as_ref().expect("live item has facts");
            let name = facts["name"].as_str().unwrap();
            let original = folder.join(name);
            if !original.is_file() {
                anomalies.push(format!("{}: committed original '{name}' missing", record.id));
                continue;
            }
            let expected_size = facts["size"].as_u64().unwrap();
            let actual_size = fs::metadata(&original).map(|m| m.len()).unwrap_or(0);
            if actual_size != expected_size {
                anomalies.push(format!(
                    "{}: size mismatch (committed {expected_size}, on disk {actual_size})",
                    record.id
                ));
                continue;
            }
            match sha256_file_hex(&original) {
                Ok(hash) => {
                    let expected = facts["sha256"].as_str().unwrap();
                    if !hash.eq_ignore_ascii_case(expected) {
                        anomalies.push(format!("{}: sha256 mismatch", record.id));
                    }
                }
                Err(e) => anomalies.push(format!("{}: cannot hash original: {e}", record.id)),
            }
        }

        // Folders with no live record are orphans.
        if let Ok(entries) = fs::read_dir(self.dir()) {
            for entry in entries.flatten() {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if !is_dir {
                    continue;
                }
                let folder_name = entry.file_name().to_string_lossy().to_string();
                match records.get(&folder_name) {
                    Some(r) if r.deleted.is_none() => {}
                    Some(_) => anomalies.push(format!(
                        "{folder_name}: folder belongs to a tombstoned item (sweepable)"
                    )),
                    None => anomalies.push(format!("{folder_name}: orphan folder (sweepable)")),
                }
            }
        }
        anomalies
    }
}
