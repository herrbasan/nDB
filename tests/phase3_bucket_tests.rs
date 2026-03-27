//! Integration tests for nDB Phase 3: File Buckets
//!
//! Tests binary storage, SHA-256 hashing, deduplication, and trash.

use ndb::{Database, FileRef};
use serde_json::json;
use tempfile::TempDir;

fn setup() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("buckets.jsonl");
    let db = Database::open(&path).unwrap();
    (db, dir)
}

#[test]
fn store_and_retrieve_file() {
    let (db, _dir) = setup();
    let bucket = db.bucket("attachments");

    let data = b"Hello, file storage!";
    let meta = bucket.store("hello.txt", data, "text/plain").unwrap();

    assert_eq!(meta.name, "hello.txt");
    assert_eq!(meta.size, 20);
    assert_eq!(meta.type_, "text/plain");
    assert_eq!(meta._file.bucket, "attachments");
    assert!(!meta._file.id.is_empty());
    assert_eq!(meta._file.ext, "txt");

    let retrieved = bucket.get(&meta._file).unwrap();
    assert_eq!(retrieved, data);
}

#[test]
fn file_deduplication() {
    let (db, _dir) = setup();
    let bucket = db.bucket("files");

    let data = b"identical content";
    let meta1 = bucket.store("original.txt", data, "text/plain").unwrap();
    let meta2 = bucket.store("copy.txt", data, "text/plain").unwrap();

    // Same hash, different original names
    assert_eq!(meta1._file.id, meta2._file.id);
    assert_eq!(meta1.name, "original.txt");
    assert_eq!(meta2.name, "copy.txt");

    // Only one file on disk
    let files = bucket.list().unwrap();
    assert_eq!(files.len(), 1);
}

#[test]
fn file_delete_and_trash() {
    let (db, _dir) = setup();
    let bucket = db.bucket("uploads");

    let meta = bucket.store("photo.png", b"image data", "image/png").unwrap();
    assert!(bucket.exists(&meta._file));

    bucket.delete(&meta._file).unwrap();
    assert!(!bucket.exists(&meta._file));

    // File should be in trash
    let trash_path = db.bucket("uploads").trash_dir();
    assert!(trash_path.exists());
}

#[test]
fn file_restore_from_trash() {
    let (db, _dir) = setup();
    let bucket = db.bucket("restorable");

    let meta = bucket.store("doc.pdf", b"pdf content", "application/pdf").unwrap();
    bucket.delete(&meta._file).unwrap();

    bucket.restore(&meta._file.id, &meta._file.ext).unwrap();

    let data = bucket.get(&meta._file).unwrap();
    assert_eq!(data, b"pdf content");
}

#[test]
fn multiple_buckets() {
    let (db, _dir) = setup();

    let avatars = db.bucket("avatars");
    let uploads = db.bucket("uploads");

    let avatar_meta = avatars.store("face.jpg", b"avatar", "image/jpeg").unwrap();
    let upload_meta = uploads.store("file.txt", b"upload", "text/plain").unwrap();

    assert_eq!(avatar_meta._file.bucket, "avatars");
    assert_eq!(upload_meta._file.bucket, "uploads");

    // Each bucket has its own files
    assert_eq!(avatars.list().unwrap().len(), 1);
    assert_eq!(uploads.list().unwrap().len(), 1);
}

#[test]
fn file_ref_in_document() {
    let (db, _dir) = setup();
    let bucket = db.bucket("attachments");

    let file_data = b"attachment content";
    let file_meta = bucket.store("report.pdf", file_data, "application/pdf").unwrap();

    // Store file reference in a document
    let doc = json!({
        "title": "Report",
        "attachment": {
            "_file": {
                "bucket": file_meta._file.bucket,
                "id": file_meta._file.id,
                "ext": file_meta._file.ext
            },
            "name": file_meta.name,
            "size": file_meta.size,
            "type": file_meta.type_
        }
    });

    let doc_id = db.insert(doc).unwrap();
    let retrieved_doc = db.get(&doc_id).unwrap();

    // Retrieve file using document reference
    let file_ref = FileRef {
        bucket: retrieved_doc["attachment"]["_file"]["bucket"].as_str().unwrap().to_string(),
        id: retrieved_doc["attachment"]["_file"]["id"].as_str().unwrap().to_string(),
        ext: retrieved_doc["attachment"]["_file"]["ext"].as_str().unwrap().to_string(),
    };

    let file_content = db.bucket(&file_ref.bucket).get(&file_ref).unwrap();
    assert_eq!(file_content, file_data);
}

#[test]
fn file_ref_compact_string() {
    let fr = FileRef {
        bucket: "attachments".to_string(),
        id: "a3f5c2d1".to_string(),
        ext: "png".to_string(),
    };

    let compact = fr.to_string_compact();
    assert_eq!(compact, "attachments:a3f5c2d1.png");

    let parsed = FileRef::from_compact(&compact).unwrap();
    assert_eq!(parsed, fr);
}

#[test]
fn list_bucket_files() {
    let (db, _dir) = setup();
    let bucket = db.bucket("files");

    bucket.store("a.txt", b"a", "text/plain").unwrap();
    bucket.store("b.txt", b"b", "text/plain").unwrap();
    bucket.store("c.txt", b"c", "text/plain").unwrap();

    let files = bucket.list().unwrap();
    assert_eq!(files.len(), 3);
}

#[test]
fn clear_trash() {
    let (db, _dir) = setup();
    let bucket = db.bucket("temp");

    let meta1 = bucket.store("f1.txt", b"1", "text/plain").unwrap();
    let meta2 = bucket.store("f2.txt", b"2", "text/plain").unwrap();

    bucket.delete(&meta1._file).unwrap();
    bucket.delete(&meta2._file).unwrap();

    let count = bucket.clear_trash().unwrap();
    assert_eq!(count, 2);
}

#[test]
fn large_file_storage() {
    let (db, _dir) = setup();
    let bucket = db.bucket("large");

    // 1MB file
    let data = vec![0xAB_u8; 1024 * 1024];
    let meta = bucket.store("large.bin", &data, "application/octet-stream").unwrap();
    assert_eq!(meta.size, 1024 * 1024);

    let retrieved = bucket.get(&meta._file).unwrap();
    assert_eq!(retrieved.len(), 1024 * 1024);
    assert_eq!(retrieved, data);
}
