//! Phase 1A Integration Tests
//!
//! Tests for file format, mmap, validation, and multi-process locking.
//!
//! Created: 2026-02-14 17:16:48+01:00
//! Phase: 1A — File Format, Mmap & Validation

use ndb::{Document, Segment, SegmentBuilder};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_doc(id: &str, dim: usize, value: f32) -> Document {
    Document {
        id: id.to_string(),
        vector: vec![value; dim],
        payload: Some(serde_json::json!({"id": id, "value": value})),
    }
}

#[test]
fn test_segment_create_and_reopen() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("test.segment");

    // Create segment with multiple documents
    let dim = 768;
    let mut builder = SegmentBuilder::new(dim);

    for i in 0..100 {
        builder
            .add(create_test_doc(&format!("doc{}", i), dim, i as f32))
            .unwrap();
    }

    builder.build(&path).unwrap();

    // Reopen and verify
    let segment = Segment::open(&path).unwrap();

    assert_eq!(segment.dimension(), dim);
    assert_eq!(segment.doc_count(), 100);

    // Verify all vectors
    for i in 0..100 {
        let vector = segment.get_vector(i).unwrap();
        assert_eq!(vector.len(), dim);
        assert!(vector.iter().all(|&v| v == i as f32));

        let external_id = segment.get_external_id(i).unwrap();
        assert_eq!(external_id, format!("doc{}", i));

        let internal_id = segment.get_internal_id(&format!("doc{}", i)).unwrap();
        assert_eq!(internal_id, i as u32);

        let payload = segment.get_payload(i).unwrap();
        assert_eq!(payload["id"], format!("doc{}", i));
        assert_eq!(payload["value"], i as f32);
    }
}

#[test]
fn test_segment_header_format() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("header_test.segment");

    // Create a minimal segment
    let mut builder = SegmentBuilder::new(4);
    builder.add(create_test_doc("doc1", 4, 1.0)).unwrap();
    builder.build(&path).unwrap();

    // Read raw header bytes
    let mut file = OpenOptions::new().read(true).open(&path).unwrap();

    let mut header_bytes = [0u8; 64];
    file.read_exact(&mut header_bytes).unwrap();

    // Verify magic bytes
    assert_eq!(&header_bytes[0..4], b"nDB\0");

    // Verify version (little-endian u16 at offset 4)
    let version = u16::from_le_bytes([header_bytes[4], header_bytes[5]]);
    assert_eq!(version, 1);

    // Verify dimension (little-endian u32 at offset 8)
    let dimension = u32::from_le_bytes([
        header_bytes[8],
        header_bytes[9],
        header_bytes[10],
        header_bytes[11],
    ]);
    assert_eq!(dimension, 4);

    // Verify doc_count (little-endian u64 at offset 12)
    let doc_count = u64::from_le_bytes([
        header_bytes[12],
        header_bytes[13],
        header_bytes[14],
        header_bytes[15],
        header_bytes[16],
        header_bytes[17],
        header_bytes[18],
        header_bytes[19],
    ]);
    assert_eq!(doc_count, 1);
}

#[test]
fn test_segment_corruption_detection() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("corrupt.segment");

    // Create segment
    let mut builder = SegmentBuilder::new(4);
    builder.add(create_test_doc("doc1", 4, 1.0)).unwrap();
    builder.build(&path).unwrap();

    // Read the file
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();

    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();

    // Corrupt a byte in the vector region (after header)
    data[100] = 0xff;

    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&data).unwrap();
    drop(file);

    // Should fail checksum verification
    let result = Segment::open(&path);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("checksum"));
}

#[test]
fn test_segment_with_empty_payloads() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("no_payload.segment");

    // Create segment with documents that have no payloads
    let mut builder = SegmentBuilder::new(4);

    let doc = Document {
        id: "doc1".to_string(),
        vector: vec![1.0, 2.0, 3.0, 4.0],
        payload: None,
    };
    builder.add(doc).unwrap();

    builder.build(&path).unwrap();

    // Verify
    let segment = Segment::open(&path).unwrap();
    assert_eq!(segment.get_payload(0), None);
}

#[test]
fn test_segment_iterators() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("iter.segment");

    let mut builder = SegmentBuilder::new(4);
    builder.add(create_test_doc("doc1", 4, 1.0)).unwrap();
    builder.add(create_test_doc("doc2", 4, 2.0)).unwrap();
    builder.add(create_test_doc("doc3", 4, 3.0)).unwrap();
    builder.build(&path).unwrap();

    let segment = Segment::open(&path).unwrap();

    // Test vector iterator
    let vectors: Vec<_> = segment.iter_vectors().collect();
    assert_eq!(vectors.len(), 3);
    assert!(vectors[0].iter().all(|&v| v == 1.0));
    assert!(vectors[1].iter().all(|&v| v == 2.0));
    assert!(vectors[2].iter().all(|&v| v == 3.0));

    // Test full iterator
    let docs: Vec<_> = segment.iter().collect();
    assert_eq!(docs.len(), 3);
    assert_eq!(docs[0].0, 0);
    assert_eq!(docs[0].1, "doc1");
}

#[test]
fn test_large_dimension_vectors() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("large_dim.segment");

    // Test common embedding dimensions
    for dim in [384, 768, 1536] {
        let mut builder = SegmentBuilder::new(dim);
        builder.add(create_test_doc("doc", dim, 1.0)).unwrap();
        builder.build(&path).unwrap();

        let segment = Segment::open(&path).unwrap();
        assert_eq!(segment.dimension(), dim);
        assert_eq!(segment.get_vector(0).unwrap().len(), dim);

        // Clean up for next iteration
        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn test_segment_sharing_across_threads() {
    use std::thread;

    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("shared.segment");

    // Create segment
    let mut builder = SegmentBuilder::new(4);
    for i in 0..10 {
        builder
            .add(create_test_doc(&format!("doc{}", i), 4, i as f32))
            .unwrap();
    }
    builder.build(&path).unwrap();

    // Share across threads
    let segment: Arc<Segment> = Segment::open(&path).unwrap();

    let handles: Vec<_> = (0..4)
        .map(|thread_id| {
            let seg = segment.clone();
            thread::spawn(move || {
                for i in 0..10 {
                    let vector = seg.get_vector(i).unwrap();
                    assert_eq!(vector[0], i as f32);
                    let external_id = seg.get_external_id(i).unwrap();
                    assert_eq!(external_id, format!("doc{}", i));
                }
                thread_id
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn test_empty_segment_rejected() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("empty.segment");

    let builder = SegmentBuilder::new(4);
    let result = builder.build(&path);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("empty"));
}

#[test]
fn test_dimension_mismatch_detected() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("mismatch.segment");

    let mut builder = SegmentBuilder::new(4);

    // Add correct dimension
    builder.add(create_test_doc("doc1", 4, 1.0)).unwrap();

    // Try to add wrong dimension
    let result = builder.add(create_test_doc("doc2", 8, 2.0));
    assert!(result.is_err());

    // First doc should still be buildable
    builder.build(&path).unwrap();
    let segment = Segment::open(&path).unwrap();
    assert_eq!(segment.doc_count(), 1);
}
