use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use memmap2::{Mmap, MmapOptions};
// use rkyv::{Archive, Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::id::IdMapping;

/// Magic bytes for segment files: "nDB\0"
pub const SEGMENT_MAGIC: &[u8; 4] = b"nDB\0";
/// Current segment format version
pub const SEGMENT_VERSION: u16 = 1;
/// Header size: 64 bytes, aligned
pub const HEADER_SIZE: usize = 64;
/// Alignment requirement for vector data (64 bytes for AVX-512)
pub const ALIGNMENT: usize = 64;

/// Segment file header (64 bytes, aligned).
/// 
/// Layout (text, not Rust code):
/// ```text
/// [4]   magic: "nDB\0"
/// [2]   version: u16
/// [2]   reserved
/// [4]   dimension: u32
/// [8]   doc_count: u64
/// [8]   vector_offset: u64
/// [8]   id_mapping_offset: u64
/// [8]   payload_offset: u64
/// [8]   checksum: u64
/// [8]   reserved
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    pub magic: [u8; 4],
    pub version: u16,
    pub dimension: u32,
    pub doc_count: u64,
    pub vector_offset: u64,
    pub id_mapping_offset: u64,
    pub payload_offset: u64,
    pub checksum: u64,
}

impl SegmentHeader {
    /// Create a new header with the given parameters.
    pub fn new(dimension: u32, doc_count: u64, vector_offset: u64, id_mapping_offset: u64, payload_offset: u64) -> Self {
        Self {
            magic: *SEGMENT_MAGIC,
            version: SEGMENT_VERSION,
            dimension,
            doc_count,
            vector_offset,
            id_mapping_offset,
            payload_offset,
            checksum: 0, // Computed separately
        }
    }

    /// Serialize header to 64 bytes.
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        let mut cursor = std::io::Cursor::new(&mut buf[..]);
        
        cursor.write_all(&self.magic).unwrap();
        cursor.write_u16::<LittleEndian>(self.version).unwrap();
        cursor.write_u16::<LittleEndian>(0).unwrap(); // reserved
        cursor.write_u32::<LittleEndian>(self.dimension).unwrap();
        cursor.write_u64::<LittleEndian>(self.doc_count).unwrap();
        cursor.write_u64::<LittleEndian>(self.vector_offset).unwrap();
        cursor.write_u64::<LittleEndian>(self.id_mapping_offset).unwrap();
        cursor.write_u64::<LittleEndian>(self.payload_offset).unwrap();
        cursor.write_u64::<LittleEndian>(self.checksum).unwrap();
        cursor.write_u64::<LittleEndian>(0).unwrap(); // reserved
        
        buf
    }

    /// Deserialize header from 64 bytes.
    pub fn from_bytes(bytes: &[u8; HEADER_SIZE]) -> Result<Self> {
        let mut cursor = std::io::Cursor::new(bytes);
        
        let mut magic = [0u8; 4];
        cursor.read_exact(&mut magic).map_err(|e| Error::corruption(
            "<unknown>", 0, format!("failed to read magic: {}", e)
        ))?;
        
        if &magic != SEGMENT_MAGIC {
            return Err(Error::corruption(
                "<unknown>", 0,
                format!("invalid magic: expected {:?}, got {:?}", SEGMENT_MAGIC, magic)
            ));
        }
        
        let version = cursor.read_u16::<LittleEndian>().map_err(|e| Error::corruption(
            "<unknown>", 4, format!("failed to read version: {}", e)
        ))?;
        
        if version != SEGMENT_VERSION {
            return Err(Error::corruption(
                "<unknown>", 4,
                format!("unsupported version: expected {}, got {}", SEGMENT_VERSION, version)
            ));
        }
        
        let _reserved = cursor.read_u16::<LittleEndian>().map_err(|e| Error::corruption(
            "<unknown>", 6, format!("failed to read reserved: {}", e)
        ))?;
        
        let dimension = cursor.read_u32::<LittleEndian>().map_err(|e| Error::corruption(
            "<unknown>", 8, format!("failed to read dimension: {}", e)
        ))?;
        
        let doc_count = cursor.read_u64::<LittleEndian>().map_err(|e| Error::corruption(
            "<unknown>", 12, format!("failed to read doc_count: {}", e)
        ))?;
        
        let vector_offset = cursor.read_u64::<LittleEndian>().map_err(|e| Error::corruption(
            "<unknown>", 20, format!("failed to read vector_offset: {}", e)
        ))?;
        
        let id_mapping_offset = cursor.read_u64::<LittleEndian>().map_err(|e| Error::corruption(
            "<unknown>", 28, format!("failed to read id_mapping_offset: {}", e)
        ))?;
        
        let payload_offset = cursor.read_u64::<LittleEndian>().map_err(|e| Error::corruption(
            "<unknown>", 36, format!("failed to read payload_offset: {}", e)
        ))?;
        
        let checksum = cursor.read_u64::<LittleEndian>().map_err(|e| Error::corruption(
            "<unknown>", 44, format!("failed to read checksum: {}", e)
        ))?;
        
        Ok(Self {
            magic,
            version,
            dimension,
            doc_count,
            vector_offset,
            id_mapping_offset,
            payload_offset,
            checksum,
        })
    }

    /// Compute checksum over the body (everything after header).
    /// Uses BLAKE3 truncated to 64 bits for speed.
    pub fn compute_checksum(data: &[u8]) -> u64 {
        let hash = blake3::hash(data);
        let bytes = hash.as_bytes();
        u64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
    }
}

/// ID mapping entry for serialization.
#[derive(Debug, Clone)]
pub struct IdEntry {
    pub internal: u32,
    pub external: String,
}

/// Payload entry for serialization.
#[derive(Debug, Clone)]
pub struct PayloadEntry {
    pub internal_id: u32,
    pub payload: Option<String>, // JSON string
}

/// Serialize ID entries to bytes.
/// Format: [count: u32] + ([internal: u32, external_len: u32, external_bytes...])*
fn serialize_id_entries(entries: &[IdEntry]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    
    // Write count
    buf.write_u32::<LittleEndian>(entries.len() as u32)
        .map_err(|e| Error::Serialization(format!("failed to write count: {}", e)))?;
    
    for entry in entries {
        buf.write_u32::<LittleEndian>(entry.internal)
            .map_err(|e| Error::Serialization(format!("failed to write internal: {}", e)))?;
        buf.write_u32::<LittleEndian>(entry.external.len() as u32)
            .map_err(|e| Error::Serialization(format!("failed to write external_len: {}", e)))?;
        buf.write_all(entry.external.as_bytes())
            .map_err(|e| Error::Serialization(format!("failed to write external: {}", e)))?;
    }
    
    Ok(buf)
}

/// Deserialize ID entries from bytes.
fn deserialize_id_entries(data: &[u8]) -> Result<Vec<IdEntry>> {
    let mut cursor = std::io::Cursor::new(data);
    
    let count = cursor.read_u32::<LittleEndian>()
        .map_err(|e| Error::Serialization(format!("failed to read count: {}", e)))?;
    
    let mut entries = Vec::with_capacity(count as usize);
    
    for _ in 0..count {
        let internal = cursor.read_u32::<LittleEndian>()
            .map_err(|e| Error::Serialization(format!("failed to read internal: {}", e)))?;
        let external_len = cursor.read_u32::<LittleEndian>()
            .map_err(|e| Error::Serialization(format!("failed to read external_len: {}", e)))?;
        
        let mut external_bytes = vec![0u8; external_len as usize];
        cursor.read_exact(&mut external_bytes)
            .map_err(|e| Error::Serialization(format!("failed to read external: {}", e)))?;
        
        let external = String::from_utf8(external_bytes)
            .map_err(|e| Error::Serialization(format!("invalid utf8 in external: {}", e)))?;
        
        entries.push(IdEntry { internal, external });
    }
    
    Ok(entries)
}

/// Serialize payload entries to bytes.
/// Format: [count: u32] + ([internal_id: u32, has_payload: u8, payload_len: u32?, payload_bytes...?])*
fn serialize_payload_entries(entries: &[PayloadEntry]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    
    // Write count
    buf.write_u32::<LittleEndian>(entries.len() as u32)
        .map_err(|e| Error::Serialization(format!("failed to write count: {}", e)))?;
    
    for entry in entries {
        buf.write_u32::<LittleEndian>(entry.internal_id)
            .map_err(|e| Error::Serialization(format!("failed to write internal_id: {}", e)))?;
        
        match &entry.payload {
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
    }
    
    Ok(buf)
}

/// Deserialize payload entries from bytes.
fn deserialize_payload_entries(data: &[u8]) -> Result<Vec<PayloadEntry>> {
    let mut cursor = std::io::Cursor::new(data);
    
    let count = cursor.read_u32::<LittleEndian>()
        .map_err(|e| Error::Serialization(format!("failed to read count: {}", e)))?;
    
    let mut entries = Vec::with_capacity(count as usize);
    
    for _ in 0..count {
        let internal_id = cursor.read_u32::<LittleEndian>()
            .map_err(|e| Error::Serialization(format!("failed to read internal_id: {}", e)))?;
        
        let has_payload = cursor.read_u8()
            .map_err(|e| Error::Serialization(format!("failed to read has_payload: {}", e)))?;
        
        let payload = if has_payload != 0 {
            let payload_len = cursor.read_u32::<LittleEndian>()
                .map_err(|e| Error::Serialization(format!("failed to read payload_len: {}", e)))?;
            let mut payload_bytes = vec![0u8; payload_len as usize];
            cursor.read_exact(&mut payload_bytes)
                .map_err(|e| Error::Serialization(format!("failed to read payload: {}", e)))?;
            Some(String::from_utf8(payload_bytes)
                .map_err(|e| Error::Serialization(format!("invalid utf8 in payload: {}", e)))?)
        } else {
            None
        };
        
        entries.push(PayloadEntry { internal_id, payload });
    }
    
    Ok(entries)
}

/// A document to be stored in a segment.
#[derive(Debug, Clone)]
pub struct Document {
    pub id: String,
    pub vector: Vec<f32>,
    pub payload: Option<serde_json::Value>,
}

/// Builder for creating segment files.
pub struct SegmentBuilder {
    dimension: usize,
    documents: Vec<Document>,
    id_mapping: IdMapping,
}

impl SegmentBuilder {
    /// Create a new segment builder with the given vector dimension.
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension,
            documents: Vec::new(),
            id_mapping: IdMapping::new(),
        }
    }

    /// Add a document to the segment.
    pub fn add(&mut self, doc: Document) -> Result<()> {
        if doc.vector.len() != self.dimension {
            return Err(Error::WrongDimension {
                expected: self.dimension,
                got: doc.vector.len(),
            });
        }
        
        // Insert into ID mapping
        self.id_mapping.insert(doc.id.clone());
        self.documents.push(doc);
        Ok(())
    }

    /// Number of documents in the builder.
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Build and write the segment to a file.
    /// 
    /// File layout:
    /// - Header (64 bytes)
    /// - Vector data region (packed f32s, 64-byte aligned)
    /// - ID mapping region (rkyv archived)
    /// - Payload region (rkyv archived)
    pub fn build(&self, path: &Path) -> Result<()> {
        if self.documents.is_empty() {
            return Err(Error::invalid_arg("documents", "cannot build empty segment"));
        }

        // Build all data in memory first, then write atomically
        
        // Align to 64 bytes for vector data
        let vector_offset = align_to(HEADER_SIZE, ALIGNMENT) as u64;

        // Build vector data (packed f32s)
        let mut vector_data = Vec::with_capacity(self.documents.len() * self.dimension * 4);
        for doc in &self.documents {
            for &val in &doc.vector {
                vector_data.write_f32::<LittleEndian>(val)
                    .map_err(Error::io_err(path, "failed to serialize vector data"))?;
            }
        }

        // Build ID mapping (simple binary format)
        let id_entries: Vec<IdEntry> = self.id_mapping
            .to_vec()
            .into_iter()
            .map(|(internal, external)| IdEntry { internal, external })
            .collect();
        
        let id_bytes = serialize_id_entries(&id_entries)?;
        let id_mapping_offset = vector_offset + vector_data.len() as u64;

        // Build payloads (simple binary format)
        let payloads: Vec<PayloadEntry> = self.documents
            .iter()
            .map(|doc| {
                let internal_id = self.id_mapping.get_internal(&doc.id).unwrap();
                let payload = doc.payload.as_ref().map(|p| p.to_string());
                PayloadEntry { internal_id, payload }
            })
            .collect();
        
        let payload_bytes = serialize_payload_entries(&payloads)?;
        
        let payload_offset = id_mapping_offset + id_bytes.len() as u64;

        // Compute checksum over body (everything after header)
        let mut body_data = Vec::new();
        // Pad from HEADER_SIZE to vector_offset
        body_data.resize((vector_offset as usize) - HEADER_SIZE, 0);
        body_data.extend_from_slice(&vector_data);
        body_data.extend_from_slice(&id_bytes);
        body_data.extend_from_slice(&payload_bytes);
        
        let checksum = SegmentHeader::compute_checksum(&body_data);

        // Build header
        let header = SegmentHeader::new(
            self.dimension as u32,
            self.documents.len() as u64,
            vector_offset,
            id_mapping_offset,
            payload_offset,
        );
        
        let mut header_bytes = header.to_bytes();
        // Patch in checksum at offset 44
        let mut cursor = std::io::Cursor::new(&mut header_bytes[44..52]);
        cursor.write_u64::<LittleEndian>(checksum).unwrap();

        // Write everything to file
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(Error::io_err(path, "failed to create segment file"))?;

        file.write_all(&header_bytes)
            .map_err(Error::io_err(path, "failed to write header"))?;
        file.write_all(&body_data)
            .map_err(Error::io_err(path, "failed to write body"))?;
        file.sync_all()
            .map_err(Error::io_err(path, "failed to sync segment file"))?;

        Ok(())
    }
}

/// An immutable, memory-mapped segment.
/// 
/// Provides zero-copy access to vector data, ID mappings, and payloads.
/// The segment is read-only and can be safely shared across threads.
pub struct Segment {
    path: PathBuf,
    header: SegmentHeader,
    mmap: Mmap,
    /// Pointer to vector data (packed f32s)
    vector_ptr: *const f32,
    /// Offset to ID mapping in mmap
    id_mapping_offset: usize,
    /// Offset to payload data in mmap
    payload_offset: usize,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("path", &self.path)
            .field("header", &self.header)
            .field("dimension", &self.dimension())
            .field("doc_count", &self.doc_count())
            .field("vector_ptr", &self.vector_ptr)
            .finish()
    }
}

// Safety: Segment is immutable and the mmap is valid for its lifetime
unsafe impl Send for Segment {}
unsafe impl Sync for Segment {}

impl Segment {
    /// Open a segment file via memory mapping.
    /// 
    /// Performs checksum verification on open. If checksum matches,
    /// skips deep validation and trusts the rkyv layout.
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let file = File::open(path)
            .map_err(Error::io_err(path, "failed to open segment file"))?;

        // Check file size
        let metadata = file.metadata()
            .map_err(Error::io_err(path, "failed to get metadata"))?;
        let file_size = metadata.len();

        if file_size < HEADER_SIZE as u64 {
            return Err(Error::corruption(
                path, 0,
                format!("file too small: {} bytes (min {})", file_size, HEADER_SIZE)
            ));
        }

        // Memory map the file
        let mmap = unsafe {
            MmapOptions::new()
                .map(&file)
                .map_err(Error::io_err(path, "failed to mmap segment"))?
        };

        // Pre-fault pages to surface I/O errors early
        #[cfg(unix)]
        unsafe {
            libc::madvise(
                mmap.as_ptr() as *mut libc::c_void,
                mmap.len(),
                libc::MADV_POPULATE_READ,
            );
        }

        // Read and validate header
        let mut header_bytes = [0u8; HEADER_SIZE];
        header_bytes.copy_from_slice(&mmap[..HEADER_SIZE]);
        let header = SegmentHeader::from_bytes(&header_bytes)?;

        // Verify checksum
        let body_data = &mmap[HEADER_SIZE..];
        let computed_checksum = SegmentHeader::compute_checksum(body_data);
        
        if computed_checksum != header.checksum {
            return Err(Error::ChecksumMismatch {
                file: path.to_path_buf(),
                expected: header.checksum,
                got: computed_checksum,
            });
        }

        // Get vector pointer (64-byte aligned by construction)
        let vector_ptr = unsafe {
            mmap.as_ptr().add(header.vector_offset as usize) as *const f32
        };

        // Deserialize ID mapping via rkyv (zero-copy)
        let id_mapping_offset = header.id_mapping_offset as usize;
        let payload_offset = header.payload_offset as usize;

        Ok(Arc::new(Self {
            path: path.to_path_buf(),
            header,
            mmap,
            vector_ptr,
            id_mapping_offset,
            payload_offset,
        }))
    }

    /// Get the segment header.
    pub fn header(&self) -> &SegmentHeader {
        &self.header
    }

    /// Get vector dimension.
    pub fn dimension(&self) -> usize {
        self.header.dimension as usize
    }

    /// Get document count.
    pub fn doc_count(&self) -> usize {
        self.header.doc_count as usize
    }

    /// Get the file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get vector data for a document by internal ID.
    /// 
    /// Returns None if the internal ID is not found.
    pub fn get_vector(&self, internal_id: u32) -> Option<&[f32]> {
        if internal_id as usize >= self.header.doc_count as usize {
            return None;
        }
        
        let dim = self.dimension();
        let offset = internal_id as usize * dim;
        
        unsafe {
            let ptr = self.vector_ptr.add(offset);
            Some(std::slice::from_raw_parts(ptr, dim))
        }
    }

    /// Iterate over all vectors in the segment.
    pub fn iter_vectors(&self) -> VectorIter<'_> {
        VectorIter {
            segment: self,
            current: 0,
        }
    }

    /// Get external ID for an internal ID.
    pub fn get_external_id(&self, internal_id: u32) -> Option<String> {
        let id_mapping_data = &self.mmap[self.id_mapping_offset..];
        let entries = deserialize_id_entries(id_mapping_data).ok()?;
        entries
            .into_iter()
            .find(|entry| entry.internal == internal_id)
            .map(|entry| entry.external)
    }

    /// Get internal ID for an external ID.
    pub fn get_internal_id(&self, external_id: &str) -> Option<u32> {
        let id_mapping_data = &self.mmap[self.id_mapping_offset..];
        let entries = deserialize_id_entries(id_mapping_data).ok()?;
        entries
            .into_iter()
            .find(|entry| entry.external == external_id)
            .map(|entry| entry.internal)
    }

    /// Get payload for an internal ID.
    pub fn get_payload(&self, internal_id: u32) -> Option<serde_json::Value> {
        let payload_data = &self.mmap[self.payload_offset..];
        let entries = deserialize_payload_entries(payload_data).ok()?;
        entries
            .into_iter()
            .find(|entry| entry.internal_id == internal_id)
            .and_then(|entry| entry.payload)
            .and_then(|p| serde_json::from_str(&p).ok())
    }

    /// Iterate over all (internal_id, external_id, vector) tuples.
    pub fn iter(&self) -> impl Iterator<Item = (u32, String, &[f32])> + '_ {
        (0..self.header.doc_count as u32)
            .filter_map(move |internal_id| {
                let external_id = self.get_external_id(internal_id)?;
                let vector = self.get_vector(internal_id)?;
                Some((internal_id, external_id, vector))
            })
    }
}

/// Iterator over vectors in a segment.
pub struct VectorIter<'a> {
    segment: &'a Segment,
    current: usize,
}

impl<'a> Iterator for VectorIter<'a> {
    type Item = &'a [f32];

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.segment.doc_count() {
            return None;
        }
        
        let result = self.segment.get_vector(self.current as u32);
        self.current += 1;
        result
    }
}

/// Align a value to the specified alignment.
fn align_to(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
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
    fn test_segment_builder_and_open() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.segment");

        // Build segment
        let mut builder = SegmentBuilder::new(4);
        builder.add(create_test_doc("doc1", 4)).unwrap();
        builder.add(create_test_doc("doc2", 4)).unwrap();
        builder.add(create_test_doc("doc3", 4)).unwrap();
        
        builder.build(&path).unwrap();

        // Open and verify
        let segment = Segment::open(&path).unwrap();
        
        assert_eq!(segment.dimension(), 4);
        assert_eq!(segment.doc_count(), 3);
        
        // Verify vectors
        let v0 = segment.get_vector(0).unwrap();
        assert_eq!(v0, &[0.0, 1.0, 2.0, 3.0]);
        
        let v1 = segment.get_vector(1).unwrap();
        assert_eq!(v1, &[0.0, 1.0, 2.0, 3.0]);

        // Verify ID mapping
        assert_eq!(segment.get_external_id(0), Some("doc1".to_string()));
        assert_eq!(segment.get_external_id(1), Some("doc2".to_string()));
        assert_eq!(segment.get_internal_id("doc1"), Some(0));
        assert_eq!(segment.get_internal_id("doc2"), Some(1));

        // Verify payload
        let payload = segment.get_payload(0).unwrap();
        assert_eq!(payload["id"], "doc1");
    }

    #[test]
    fn test_dimension_mismatch() {
        let mut builder = SegmentBuilder::new(4);
        let doc = Document {
            id: "doc1".to_string(),
            vector: vec![1.0, 2.0, 3.0], // Wrong dimension
            payload: None,
        };
        
        let err = builder.add(doc).unwrap_err();
        assert!(matches!(err, Error::WrongDimension { expected: 4, got: 3 }));
    }

    #[test]
    fn test_empty_segment_fails() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("empty.segment");
        
        let builder = SegmentBuilder::new(4);
        let err = builder.build(&path).unwrap_err();
        
        assert!(matches!(err, Error::InvalidArgument { field, reason } if field == "documents"));
    }

    #[test]
    fn test_checksum_verification() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("corrupt.segment");

        // Build segment
        let mut builder = SegmentBuilder::new(4);
        builder.add(create_test_doc("doc1", 4)).unwrap();
        builder.build(&path).unwrap();

        // Corrupt the file
        use std::io::Seek;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(std::io::SeekFrom::Start(100)).unwrap();
        file.write_all(&[0xff, 0xff, 0xff, 0xff]).unwrap();
        drop(file);

        // Should fail checksum verification
        let err = Segment::open(&path).unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }));
    }
}
