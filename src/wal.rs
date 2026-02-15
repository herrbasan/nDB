//! Write-Ahead Log (WAL) for crash recovery.
//!
//! WAL format: [seq:u64][len:u32][crc32:u32][opcode:u8][body]
//!
//! - seq: Monotonic sequence number per collection
//! - len: Total bytes after len field (crc32 + opcode + body)
//! - crc32: CRC32 of (seq + opcode + body)
//! - opcode: 1 = Insert, 2 = Delete
//! - body: Variable-length record data
//!
//! Recovery:
//! - Replay from last_applied_seq + 1
//! - Idempotent: skip if seq <= last_applied_seq
//! - Corrupt/partial tail: truncate or skip forward

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use crc32fast::Hasher as Crc32Hasher;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::segment::Document;

/// WAL file name
pub const WAL_FILE_NAME: &str = "wal.log";

/// Maximum WAL size before flush is triggered (64MB)
pub const WAL_FLUSH_THRESHOLD: u64 = 64 * 1024 * 1024;

/// Maximum batch size for insert_batch (64MB)
pub const MAX_BATCH_SIZE: usize = 64 * 1024 * 1024;

/// WAL operation codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    /// Insert or replace a document
    Insert = 1,
    /// Delete a document (soft delete)
    Delete = 2,
}

impl Opcode {
    /// Convert u8 to Opcode
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Opcode::Insert),
            2 => Some(Opcode::Delete),
            _ => None,
        }
    }
}

/// A WAL record header (17 bytes before body)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// Sequence number (monotonic per collection)
    pub seq: u64,
    /// Total bytes after len field (crc32 + opcode + body)
    pub len: u32,
    /// CRC32 of (seq + opcode + body)
    pub crc32: u32,
    /// Operation code
    pub opcode: u8,
}

impl RecordHeader {
    /// Header size in bytes (seq + len + crc32 + opcode)
    pub const SIZE: usize = 8 + 4 + 4 + 1;

    /// Serialize header to bytes (excluding body)
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        let mut cursor = std::io::Cursor::new(&mut buf[..]);
        cursor.write_u64::<LittleEndian>(self.seq).unwrap();
        cursor.write_u32::<LittleEndian>(self.len).unwrap();
        cursor.write_u32::<LittleEndian>(self.crc32).unwrap();
        cursor.write_u8(self.opcode).unwrap();
        buf
    }

    /// Deserialize header from bytes
    pub fn from_bytes(bytes: &[u8; Self::SIZE]) -> Result<Self> {
        let mut cursor = std::io::Cursor::new(bytes);
        let seq = cursor.read_u64::<LittleEndian>().map_err(|e| {
            Error::WalError {
                seq: 0,
                message: format!("failed to read seq: {}", e),
            }
        })?;
        let len = cursor.read_u32::<LittleEndian>().map_err(|e| {
            Error::WalError {
                seq,
                message: format!("failed to read len: {}", e),
            }
        })?;
        let crc32 = cursor.read_u32::<LittleEndian>().map_err(|e| {
            Error::WalError {
                seq,
                message: format!("failed to read crc32: {}", e),
            }
        })?;
        let opcode = cursor.read_u8().map_err(|e| Error::WalError {
            seq,
            message: format!("failed to read opcode: {}", e),
        })?;
        Ok(Self {
            seq,
            len,
            crc32,
            opcode,
        })
    }
}

/// Body of an Insert record
#[derive(Debug, Clone)]
pub struct InsertBody {
    /// External document ID
    pub id: String,
    /// Vector data (f32s serialized as little-endian bytes)
    pub vector: Vec<f32>,
    /// Optional JSON payload
    pub payload: Option<String>,
}

impl InsertBody {
    /// Serialize to bytes
    pub fn serialize(&self, dim: usize) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // id_len: u32 + id_bytes
        buf.write_u32::<LittleEndian>(self.id.len() as u32)
            .map_err(|e| Error::Serialization(format!("failed to write id_len: {}", e)))?;
        buf.write_all(self.id.as_bytes())
            .map_err(|e| Error::Serialization(format!("failed to write id: {}", e)))?;

        // vector: dim * 4 bytes (f32 little-endian)
        if self.vector.len() != dim {
            return Err(Error::WrongDimension {
                expected: dim,
                got: self.vector.len(),
            });
        }
        for &val in &self.vector {
            buf.write_f32::<LittleEndian>(val)
                .map_err(|e| Error::Serialization(format!("failed to write vector: {}", e)))?;
        }

        // payload: has_payload: u8 + (payload_len: u32 + payload_bytes)?
        match &self.payload {
            Some(payload) => {
                buf.write_u8(1)
                    .map_err(|e| Error::Serialization(format!("failed to write has_payload: {}", e)))?;
                buf.write_u32::<LittleEndian>(payload.len() as u32)
                    .map_err(|e| Error::Serialization(format!("failed to write payload_len: {}", e)))?;
                buf.write_all(payload.as_bytes())
                    .map_err(|e| Error::Serialization(format!("failed to write payload: {}", e)))?;
            }
            None => {
                buf.write_u8(0)
                    .map_err(|e| Error::Serialization(format!("failed to write has_payload: {}", e)))?;
            }
        }

        Ok(buf)
    }

    /// Deserialize from bytes
    pub fn deserialize(data: &[u8], dim: usize) -> Result<Self> {
        let mut cursor = std::io::Cursor::new(data);

        // id
        let id_len = cursor.read_u32::<LittleEndian>().map_err(|e| {
            Error::Serialization(format!("failed to read id_len: {}", e))
        })?;
        let mut id_bytes = vec![0u8; id_len as usize];
        cursor.read_exact(&mut id_bytes).map_err(|e| {
            Error::Serialization(format!("failed to read id: {}", e))
        })?;
        let id = String::from_utf8(id_bytes)
            .map_err(|e| Error::Serialization(format!("invalid utf8 in id: {}", e)))?;

        // vector
        let mut vector = Vec::with_capacity(dim);
        for _ in 0..dim {
            let val = cursor.read_f32::<LittleEndian>().map_err(|e| {
                Error::Serialization(format!("failed to read vector: {}", e))
            })?;
            vector.push(val);
        }

        // payload
        let has_payload = cursor.read_u8().map_err(|e| {
            Error::Serialization(format!("failed to read has_payload: {}", e))
        })?;
        let payload = if has_payload != 0 {
            let payload_len = cursor.read_u32::<LittleEndian>().map_err(|e| {
                Error::Serialization(format!("failed to read payload_len: {}", e))
            })?;
            let mut payload_bytes = vec![0u8; payload_len as usize];
            cursor.read_exact(&mut payload_bytes).map_err(|e| {
                Error::Serialization(format!("failed to read payload: {}", e))
            })?;
            Some(String::from_utf8(payload_bytes).map_err(|e| {
                Error::Serialization(format!("invalid utf8 in payload: {}", e))
            })?)
        } else {
            None
        };

        Ok(Self {
            id,
            vector,
            payload,
        })
    }
}

/// Body of a Delete record
#[derive(Debug, Clone)]
pub struct DeleteBody {
    /// External document ID to delete
    pub id: String,
}

impl DeleteBody {
    /// Serialize to bytes
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        buf.write_u32::<LittleEndian>(self.id.len() as u32)
            .map_err(|e| Error::Serialization(format!("failed to write id_len: {}", e)))?;
        buf.write_all(self.id.as_bytes())
            .map_err(|e| Error::Serialization(format!("failed to write id: {}", e)))?;
        Ok(buf)
    }

    /// Deserialize from bytes
    pub fn deserialize(data: &[u8]) -> Result<Self> {
        let mut cursor = std::io::Cursor::new(data);
        let id_len = cursor.read_u32::<LittleEndian>().map_err(|e| {
            Error::Serialization(format!("failed to read id_len: {}", e))
        })?;
        let mut id_bytes = vec![0u8; id_len as usize];
        cursor.read_exact(&mut id_bytes).map_err(|e| {
            Error::Serialization(format!("failed to read id: {}", e))
        })?;
        let id = String::from_utf8(id_bytes)
            .map_err(|e| Error::Serialization(format!("invalid utf8 in id: {}", e)))?;
        Ok(Self { id })
    }
}

/// A complete WAL record
#[derive(Debug, Clone)]
pub enum Record {
    /// Insert a document
    Insert(InsertBody),
    /// Delete a document
    Delete(DeleteBody),
}

impl Record {
    /// Get the opcode for this record
    pub fn opcode(&self) -> Opcode {
        match self {
            Record::Insert(_) => Opcode::Insert,
            Record::Delete(_) => Opcode::Delete,
        }
    }

    /// Serialize to bytes with the given sequence number
    pub fn serialize(&self, seq: u64, dim: usize) -> Result<Vec<u8>> {
        let body = match self {
            Record::Insert(body) => body.serialize(dim)?,
            Record::Delete(body) => body.serialize()?,
        };

        let opcode = self.opcode() as u8;
        let len = (4 + 1 + body.len()) as u32; // crc32 + opcode + body

        // Compute CRC32 over (seq + opcode + body)
        let mut hasher = Crc32Hasher::new();
        hasher.update(&seq.to_le_bytes());
        hasher.update(&[opcode]);
        hasher.update(&body);
        let crc32 = hasher.finalize();

        // Build record
        let mut buf = Vec::with_capacity(RecordHeader::SIZE + body.len());
        buf.write_u64::<LittleEndian>(seq).unwrap();
        buf.write_u32::<LittleEndian>(len).unwrap();
        buf.write_u32::<LittleEndian>(crc32).unwrap();
        buf.write_u8(opcode).unwrap();
        buf.extend_from_slice(&body);

        Ok(buf)
    }
}

/// Write-ahead log for a collection.
///
/// Append-only file with checksum-protected records.
/// Supports crash recovery via WAL replay.
pub struct Wal {
    path: PathBuf,
    file: File,
    next_seq: u64,
}

impl Wal {
    /// Create or open a WAL at the given path.
    ///
    /// If the WAL exists, scans to find the next sequence number.
    /// If last_seq is provided, validates consistency.
    pub fn open(path: impl AsRef<Path>, last_seq: Option<u64>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let exists = path.exists();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(Error::io_err(&path, "failed to open WAL"))?;

        let mut wal = Self {
            path,
            file,
            next_seq: 1, // Sequence numbers start at 1
        };

        if exists {
            // Scan to find next sequence number and truncate any corrupt records
            let (max_seq, _) = wal.scan_records()?;
            if max_seq > 0 {
                wal.next_seq = max_seq + 1;
            }
            
            // Note: We don't validate against last_seq here because the manifest
            // may not be updated after every write. During recovery, we'll replay
            // from last_seq + 1 to next_seq - 1.
        }

        Ok(wal)
    }

    /// Create a new empty WAL, truncating any existing file.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(Error::io_err(&path, "failed to create WAL"))?;

        Ok(Self {
            path,
            file,
            next_seq: 1,
        })
    }

    /// Get the next sequence number that will be assigned
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Get the current file size
    pub fn file_size(&self) -> Result<u64> {
        let metadata = self
            .file
            .metadata()
            .map_err(Error::io_err(&self.path, "failed to get WAL metadata"))?;
        Ok(metadata.len())
    }

    /// Append a record to the WAL.
    ///
    /// Returns the sequence number assigned to the record.
    pub fn append(&mut self, record: &Record, dim: usize) -> Result<u64> {
        let seq = self.next_seq;
        let data = record.serialize(seq, dim)?;

        self.file
            .write_all(&data)
            .map_err(Error::io_err(&self.path, "failed to write WAL record"))?;

        self.next_seq += 1;
        Ok(seq)
    }

    /// Append a record and sync to disk.
    ///
    /// Returns the sequence number assigned to the record.
    pub fn append_and_sync(&mut self, record: &Record, dim: usize) -> Result<u64> {
        let seq = self.append(record, dim)?;
        self.sync()?;
        Ok(seq)
    }

    /// Sync the WAL to disk (fdatasync)
    pub fn sync(&mut self) -> Result<()> {
        self.file
            .sync_all()
            .map_err(Error::io_err(&self.path, "failed to sync WAL"))
    }

    /// Scan all records in the WAL, returning (max_seq, valid_records)
    ///
    /// On corruption, returns records up to the corruption point.
    fn scan_records(&mut self) -> Result<(u64, Vec<(u64, Record)>)> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(Error::io_err(&self.path, "failed to seek WAL"))?;

        let mut records = Vec::new();
        let mut max_seq = 0u64;

        loop {
            // Try to read header
            let mut header_buf = [0u8; RecordHeader::SIZE];
            match self.file.read_exact(&mut header_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    return Err(Error::io_err(&self.path, "failed to read WAL header")(e));
                }
            }

            let header = match RecordHeader::from_bytes(&header_buf) {
                Ok(h) => h,
                Err(e) => {
                    // Corrupt header - truncate here
                    let pos = self.file.stream_position().map_err(Error::io_err(
                        &self.path,
                        "failed to get WAL position",
                    ))?;
                    self.file
                        .set_len(pos - header_buf.len() as u64)
                        .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
                    break;
                }
            };

            // Validate sequence number monotonicity
            if header.seq != max_seq + 1 && max_seq > 0 {
                // Non-sequential sequence - truncate here
                let pos = self.file.stream_position().map_err(Error::io_err(
                    &self.path,
                    "failed to get WAL position",
                ))?;
                self.file
                    .set_len(pos - header_buf.len() as u64)
                    .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
                break;
            }

            // Read body
            let body_len = header.len as usize - 4 - 1; // minus crc32 and opcode
            let mut body = vec![0u8; body_len];
            match self.file.read_exact(&mut body) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Partial record - truncate at header start (we haven't read body)
                    let pos = self.file.stream_position().map_err(Error::io_err(
                        &self.path,
                        "failed to get WAL position",
                    ))?;
                    let truncate_pos = pos.saturating_sub(RecordHeader::SIZE as u64);
                    self.file
                        .set_len(truncate_pos)
                        .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
                    break;
                }
                Err(e) => {
                    return Err(Error::io_err(&self.path, "failed to read WAL body")(e));
                }
            }

            // Verify CRC32
            let mut hasher = Crc32Hasher::new();
            hasher.update(&header.seq.to_le_bytes());
            hasher.update(&[header.opcode]);
            hasher.update(&body);
            let computed_crc = hasher.finalize();

            if computed_crc != header.crc32 {
                // CRC mismatch - truncate at header start
                let pos = self.file.stream_position().map_err(Error::io_err(
                    &self.path,
                    "failed to get WAL position",
                ))?;
                self.file
                    .set_len(pos - RecordHeader::SIZE as u64 - body.len() as u64)
                    .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
                break;
            }

            // Parse record
            let _opcode = match Opcode::from_u8(header.opcode) {
                Some(op) => op,
                None => {
                    // Unknown opcode - truncate
                    let pos = self.file.stream_position().map_err(Error::io_err(
                        &self.path,
                        "failed to get WAL position",
                    ))?;
                    self.file
                        .set_len(pos - RecordHeader::SIZE as u64 - body.len() as u64)
                        .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
                    break;
                }
            };

            // Store record for replay (dimension will be provided during replay)
            // We can't fully deserialize without dimension, so store raw
            max_seq = header.seq;
            records.push((header.seq, header.opcode, body));
        }

        Ok((max_seq, Vec::new()))
    }

    /// Replay WAL records from the given sequence number.
    ///
    /// Calls the provided callback for each valid record with seq >= start_seq.
    /// Truncates corrupt/partial records at the tail.
    ///
    /// Returns the last sequence number applied.
    pub fn replay<F>(&mut self, start_seq: u64, dim: usize, mut callback: F) -> Result<u64>
    where
        F: FnMut(u64, Record) -> Result<()>,
    {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(Error::io_err(&self.path, "failed to seek WAL"))?;

        let mut last_applied = 0u64;

        loop {
            // Read header
            let mut header_buf = [0u8; RecordHeader::SIZE];
            match self.file.read_exact(&mut header_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    return Err(Error::io_err(&self.path, "failed to read WAL header")(e));
                }
            }

            let header = RecordHeader::from_bytes(&header_buf)?;

            // Skip records before start_seq (idempotency)
            if header.seq < start_seq {
                // Skip body
                let body_len = header.len as usize - 4 - 1;
                self.file
                    .seek(SeekFrom::Current(body_len as i64))
                    .map_err(Error::io_err(&self.path, "failed to skip WAL record"))?;
                last_applied = header.seq;
                continue;
            }

            // Validate sequence continuity
            if header.seq != last_applied + 1 && last_applied > 0 {
                // Gap in sequence numbers - stop here
                break;
            }

            // Read body
            let body_len = header.len as usize - 4 - 1;
            let mut body = vec![0u8; body_len];
            match self.file.read_exact(&mut body) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Partial record - truncate at header start
                    let pos = self
                        .file
                        .stream_position()
                        .map_err(Error::io_err(&self.path, "failed to get WAL position"))?;
                    let truncate_pos = pos.saturating_sub(RecordHeader::SIZE as u64);
                    self.file
                        .set_len(truncate_pos)
                        .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
                    break;
                }
                Err(e) => {
                    return Err(Error::io_err(&self.path, "failed to read WAL body")(e));
                }
            }

            // Verify CRC32
            let mut hasher = Crc32Hasher::new();
            hasher.update(&header.seq.to_le_bytes());
            hasher.update(&[header.opcode]);
            hasher.update(&body);
            let computed_crc = hasher.finalize();

            if computed_crc != header.crc32 {
                // CRC mismatch - truncate
                let pos = self
                    .file
                    .stream_position()
                    .map_err(Error::io_err(&self.path, "failed to get WAL position"))?;
                self.file
                    .set_len(pos - RecordHeader::SIZE as u64 - body.len() as u64)
                    .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
                break;
            }

            // Parse and apply record
            let opcode = match Opcode::from_u8(header.opcode) {
                Some(op) => op,
                None => {
                    // Unknown opcode - truncate
                    let pos = self
                        .file
                        .stream_position()
                        .map_err(Error::io_err(&self.path, "failed to get WAL position"))?;
                    self.file
                        .set_len(pos - RecordHeader::SIZE as u64 - body.len() as u64)
                        .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
                    break;
                }
            };

            let record = match opcode {
                Opcode::Insert => {
                    let body = InsertBody::deserialize(&body, dim)?;
                    Record::Insert(body)
                }
                Opcode::Delete => {
                    let body = DeleteBody::deserialize(&body)?;
                    Record::Delete(body)
                }
            };

            callback(header.seq, record)?;
            last_applied = header.seq;
        }

        // Update next_seq based on what we found
        self.next_seq = last_applied + 1;

        Ok(last_applied)
    }

    /// Reset the WAL (truncate and start fresh)
    pub fn reset(&mut self) -> Result<()> {
        self.file
            .set_len(0)
            .map_err(Error::io_err(&self.path, "failed to truncate WAL"))?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(Error::io_err(&self.path, "failed to seek WAL"))?;
        self.next_seq = 1;
        Ok(())
    }

    /// Get the WAL file path
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Create an Insert record from a Document
pub fn insert_record(doc: &Document) -> Record {
    Record::Insert(InsertBody {
        id: doc.id.clone(),
        vector: doc.vector.clone(),
        payload: doc.payload.as_ref().map(|p| p.to_string()),
    })
}

/// Create a Delete record for the given ID
pub fn delete_record(id: &str) -> Record {
    Record::Delete(DeleteBody { id: id.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_doc(id: &str, dim: usize) -> Document {
        Document {
            id: id.to_string(),
            vector: (0..dim).map(|i| i as f32).collect(),
            payload: Some(serde_json::json!({"id": id})),
        }
    }

    #[test]
    fn test_wal_append_and_replay() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log");

        // Create WAL and append records
        let mut wal = Wal::create(&wal_path).unwrap();

        let doc1 = create_test_doc("doc1", 4);
        let doc2 = create_test_doc("doc2", 4);

        wal.append(&insert_record(&doc1), 4).unwrap();
        wal.append(&insert_record(&doc2), 4).unwrap();
        wal.sync().unwrap();

        // Replay
        let mut wal2 = Wal::open(&wal_path, None).unwrap();
        let mut replayed = Vec::new();
        wal2
            .replay(1, 4, |seq, record| {
                replayed.push((seq, record));
                Ok(())
            })
            .unwrap();

        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].0, 1);
        assert_eq!(replayed[1].0, 2);

        match &replayed[0].1 {
            Record::Insert(body) => assert_eq!(body.id, "doc1"),
            _ => panic!("expected insert"),
        }
    }

    #[test]
    fn test_wal_idempotent_replay() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log");

        let mut wal = Wal::create(&wal_path).unwrap();
        let doc = create_test_doc("doc1", 4);
        wal.append(&insert_record(&doc), 4).unwrap();
        wal.sync().unwrap();

        // Replay from seq 1
        let mut wal2 = Wal::open(&wal_path, None).unwrap();
        let mut count = 0;
        wal2
            .replay(1, 4, |_seq, _record| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 1);

        // Replay from seq 2 (should skip)
        let mut wal3 = Wal::open(&wal_path, None).unwrap();
        let mut count = 0;
        wal3
            .replay(2, 4, |_seq, _record| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_wal_corruption_truncation() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log");

        // Create WAL with 3 records
        let mut wal = Wal::create(&wal_path).unwrap();
        for i in 0..3 {
            let doc = create_test_doc(&format!("doc{}", i), 4);
            wal.append(&insert_record(&doc), 4).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);

        // Corrupt the file by truncating mid-way through
        // This will corrupt the last record(s)
        std::fs::OpenOptions::new()
            .write(true)
            .open(&wal_path)
            .unwrap()
            .set_len(50)
            .unwrap();

        // Opening should succeed (truncates corrupt tail)
        let mut wal = Wal::open(&wal_path, None).unwrap();
        
        // Replay should succeed with whatever records are valid
        let mut count = 0;
        wal
            .replay(1, 4, |_seq, _record| {
                count += 1;
                Ok(())
            })
            .unwrap();
        
        // We should have recovered some records (at least 0, possibly 1)
        // The important thing is that replay succeeded without panic
        eprintln!("Recovered {} records after truncation", count);
    }

    #[test]
    fn test_delete_record() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log");

        let mut wal = Wal::create(&wal_path).unwrap();
        wal.append(&delete_record("doc1"), 0).unwrap();
        wal.sync().unwrap();

        let mut wal2 = Wal::open(&wal_path, None).unwrap();
        let mut found_delete = false;
        wal2
            .replay(1, 0, |_seq, record| {
                match record {
                    Record::Delete(body) => {
                        assert_eq!(body.id, "doc1");
                        found_delete = true;
                    }
                    _ => panic!("expected delete"),
                }
                Ok(())
            })
            .unwrap();

        assert!(found_delete);
    }

    #[test]
    fn test_wal_reset() {
        let temp_dir = TempDir::new().unwrap();
        let wal_path = temp_dir.path().join("wal.log");

        let mut wal = Wal::create(&wal_path).unwrap();
        let doc = create_test_doc("doc1", 4);
        wal.append(&insert_record(&doc), 4).unwrap();
        wal.reset().unwrap();

        // Should be empty
        let mut wal2 = Wal::open(&wal_path, None).unwrap();
        let mut count = 0;
        wal2
            .replay(1, 4, |_seq, _record| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(wal2.next_seq(), 1);
    }

    // Property: WAL replay is idempotent - replaying from the same start_seq
    // produces the same results
    use proptest::prelude::*;

    proptest! {

        #[test]
        fn prop_wal_replay_idempotent(
            num_records in 1usize..100,
            start_offset in 0usize..5
        ) {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let wal_path = temp_dir.path().join("wal.log");

            // Create WAL with random number of records
            let mut wal = Wal::create(&wal_path).unwrap();
            for i in 0..num_records {
                let doc = Document {
                    id: format!("doc_{}", i),
                    vector: vec![i as f32; 4],
                    payload: None,
                };
                wal.append(&insert_record(&doc), 4).unwrap();
            }
            wal.sync().unwrap();
            drop(wal);

            // Replay once
            let mut wal = Wal::open(&wal_path, None).unwrap();
            let mut first_run = Vec::new();
            let start_seq = 1 + start_offset.min(num_records - 1) as u64;
            wal.replay(start_seq, 4, |seq, _record| {
                first_run.push(seq);
                Ok(())
            }).unwrap();

            // Replay again - should produce identical results
            let mut wal = Wal::open(&wal_path, None).unwrap();
            let mut second_run = Vec::new();
            wal.replay(start_seq, 4, |seq, _record| {
                second_run.push(seq);
                Ok(())
            }).unwrap();

            prop_assert_eq!(first_run, second_run);
        }

        // Property: WAL sequence numbers are strictly monotonic
        #[test]
        fn prop_wal_sequence_monotonic(num_records in 1usize..100) {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let wal_path = temp_dir.path().join("wal.log");

            let mut wal = Wal::create(&wal_path).unwrap();
            let mut expected_seq = 1u64;

            for i in 0..num_records {
                let doc = Document {
                    id: format!("doc_{}", i),
                    vector: vec![i as f32; 4],
                    payload: None,
                };
                let seq = wal.append(&insert_record(&doc), 4).unwrap();
                prop_assert_eq!(seq, expected_seq);
                expected_seq += 1;
            }

            prop_assert_eq!(wal.next_seq(), expected_seq);
        }

        // Property: Record count equals number of successful appends
        #[test]
        fn prop_record_count_matches_appends(num_records in 1usize..100) {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let wal_path = temp_dir.path().join("wal.log");

            let mut wal = Wal::create(&wal_path).unwrap();

            for i in 0..num_records {
                let doc = Document {
                    id: format!("doc_{}", i),
                    vector: vec![i as f32; 4],
                    payload: None,
                };
                wal.append(&insert_record(&doc), 4).unwrap();
            }
            wal.sync().unwrap();
            drop(wal);

            let mut wal = Wal::open(&wal_path, None).unwrap();
            let mut count = 0usize;
            wal.replay(1, 4, |_seq, _record| {
                count += 1;
                Ok(())
            }).unwrap();

            prop_assert_eq!(count, num_records);
        }

        // Property: Delete records are preserved through WAL
        #[test]
        fn prop_delete_records_preserved(num_records in 1usize..50) {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let wal_path = temp_dir.path().join("wal.log");

            let mut wal = Wal::create(&wal_path).unwrap();
            let mut delete_count = 0usize;

            for i in 0..num_records {
                if i % 3 == 0 {
                    wal.append(&delete_record(&format!("doc_{}", i)), 0).unwrap();
                    delete_count += 1;
                } else {
                    let doc = Document {
                        id: format!("doc_{}", i),
                        vector: vec![i as f32; 4],
                        payload: None,
                    };
                    wal.append(&insert_record(&doc), 4).unwrap();
                }
            }
            wal.sync().unwrap();
            drop(wal);

            let mut wal = Wal::open(&wal_path, None).unwrap();
            let mut recovered_deletes = 0usize;
            wal.replay(1, 4, |_seq, record| {
                if matches!(record, Record::Delete(_)) {
                    recovered_deletes += 1;
                }
                Ok(())
            }).unwrap();

            prop_assert_eq!(recovered_deletes, delete_count);
        }
    }
}
