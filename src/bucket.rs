//! File buckets for binary storage with hash-based deduplication.
//!
//! Files are stored by SHA-256 content hash. Same content = same file = deduplication.
//! Each bucket is a folder. Trash is per-bucket.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Reference to a stored file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRef {
    /// Bucket name (folder).
    pub bucket: String,
    /// Content hash (first 8 chars of SHA-256).
    pub id: String,
    /// File extension (preserved from original).
    pub ext: String,
}

impl FileRef {
    /// Full hash used for storage filename.
    pub fn filename(&self) -> String {
        format!("{}.{}", self.id, self.ext)
    }

    /// Compact string form: `bucket:hash.ext`
    pub fn to_string_compact(&self) -> String {
        format!("{}:{}.{}", self.bucket, self.id, self.ext)
    }

    /// Parse from compact string form.
    pub fn from_compact(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.splitn(2, ':').collect();
        if parts.len() != 2 {
            return None;
        }
        let bucket = parts[0].to_string();
        let file_part = parts[1];
        let dot_pos = file_part.rfind('.')?;
        let id = file_part[..dot_pos].to_string();
        let ext = file_part[dot_pos + 1..].to_string();
        Some(FileRef { bucket, id, ext })
    }
}

/// Full file metadata stored in documents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    /// File reference (bucket, hash, ext).
    pub _file: FileRef,
    /// Original filename.
    pub name: String,
    /// File size in bytes.
    pub size: usize,
    /// MIME type.
    #[serde(rename = "type")]
    pub type_: String,
    /// Creation timestamp (UNIX epoch seconds).
    pub created: u64,
}

/// A file bucket for storing binary data.
pub struct FileBucket {
    /// Bucket name.
    name: String,
    /// Base directory for all files.
    base_dir: PathBuf,
}

impl FileBucket {
    /// Create a new file bucket reference.
    pub fn new(name: &str, base_dir: &Path) -> Self {
        FileBucket {
            name: name.to_string(),
            base_dir: base_dir.to_path_buf(),
        }
    }

    /// Get the directory path for this bucket.
    fn dir(&self) -> PathBuf {
        if self.name == "_files" {
            self.base_dir.join("_files")
        } else {
            self.base_dir.join("_files").join(&self.name)
        }
    }

    /// Get the trash directory for this bucket.
    pub fn trash_dir(&self) -> PathBuf {
        self.base_dir.join("_trash").join("files").join(&self.name)
    }

    /// Compute SHA-256 hash of data. Returns full hex string.
    fn sha256(data: &[u8]) -> String {
        // Simple SHA-256 implementation (no external crate)
        // Using the standard crypto hash algorithm
        let mut hash: [u8; 32] = [0; 32];
        sha256_raw(data, &mut hash);
        hex_encode(&hash)
    }

    /// Store a file. Returns FileMeta with hash reference.
    /// If file already exists (same hash), just returns the reference (dedup).
    pub fn store(&self, name: &str, data: &[u8], mime_type: &str) -> Result<FileMeta> {
        let dir = self.dir();
        fs::create_dir_all(&dir).map_err(Error::io_err(&dir, "create bucket directory"))?;

        let full_hash = Self::sha256(data);
        let hash_id = &full_hash[..8];
        let ext = Path::new(name)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();

        let file_path = dir.join(format!("{}.{}", hash_id, ext));

        // Only write if file doesn't exist (dedup)
        if !file_path.exists() {
            // Write atomically: temp file then rename
            let tmp_path = file_path.with_extension("tmp");
            let mut file =
                fs::File::create(&tmp_path).map_err(Error::io_err(&tmp_path, "create temp file"))?;
            file.write_all(data)
                .map_err(Error::io_err(&tmp_path, "write file data"))?;
            file.flush()
                .map_err(Error::io_err(&tmp_path, "flush file data"))?;
            file.sync_all()
                .map_err(Error::io_err(&tmp_path, "fsync file data"))?;
            fs::rename(&tmp_path, &file_path)
                .map_err(Error::io_err(&file_path, "atomic rename file"))?;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Ok(FileMeta {
            _file: FileRef {
                bucket: self.name.clone(),
                id: hash_id.to_string(),
                ext: ext.clone(),
            },
            name: name.to_string(),
            size: data.len(),
            type_: mime_type.to_string(),
            created: now,
        })
    }

    /// Get file bytes by FileRef.
    pub fn get(&self, file_ref: &FileRef) -> Result<Vec<u8>> {
        let path = self.dir().join(file_ref.filename());
        fs::read(&path).map_err(Error::io_err(&path, "read file"))
    }

    /// Get file bytes by hash string.
    pub fn get_by_hash(&self, hash: &str, ext: &str) -> Result<Vec<u8>> {
        let path = self.dir().join(format!("{}.{}", hash, ext));
        fs::read(&path).map_err(Error::io_err(&path, "read file by hash"))
    }

    /// Check if a file exists.
    pub fn exists(&self, file_ref: &FileRef) -> bool {
        self.dir().join(file_ref.filename()).exists()
    }

    /// Delete a file (move to trash).
    pub fn delete(&self, file_ref: &FileRef) -> Result<()> {
        let src = self.dir().join(file_ref.filename());
        if !src.exists() {
            return Err(Error::not_found(file_ref.filename()));
        }

        let trash_dir = self.trash_dir();
        fs::create_dir_all(&trash_dir)
            .map_err(Error::io_err(&trash_dir, "create file trash directory"))?;

        let dst = trash_dir.join(file_ref.filename());
        fs::rename(&src, &dst).map_err(Error::io_err(&src, "move file to trash"))?;

        Ok(())
    }

    /// List all files in this bucket.
    pub fn list(&self) -> Result<Vec<String>> {
        let dir = self.dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for entry in fs::read_dir(&dir).map_err(Error::io_err(&dir, "list bucket files"))? {
            let entry = entry.map_err(Error::io_err(&dir, "read dir entry"))?;
            if entry.file_type().unwrap().is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".tmp") {
                    files.push(name);
                }
            }
        }
        Ok(files)
    }

    /// Restore a file from trash.
    pub fn restore(&self, hash: &str, ext: &str) -> Result<()> {
        let trash_dir = self.trash_dir();
        let filename = format!("{}.{}", hash, ext);
        let src = trash_dir.join(&filename);
        let dst = self.dir().join(&filename);

        if !src.exists() {
            return Err(Error::not_found(filename));
        }

        fs::rename(&src, &dst).map_err(Error::io_err(&src, "restore file from trash"))?;
        Ok(())
    }

    /// Purge trashed files older than given duration.
    pub fn purge_trash_ttl(&self, older_than: std::time::Duration) -> Result<usize> {
        let trash_dir = self.trash_dir();
        if !trash_dir.exists() {
            return Ok(0);
        }

        let now = std::time::SystemTime::now();
        let check_ttl = older_than > std::time::Duration::ZERO;
        let mut count = 0;
        for entry in fs::read_dir(&trash_dir).map_err(Error::io_err(&trash_dir, "read trash dir"))? {
            let entry = entry.map_err(Error::io_err(&trash_dir, "read trash entry"))?;
            
            if check_ttl {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(modified) = meta.modified() {
                        if let Ok(age) = now.duration_since(modified) {
                            if age <= older_than {
                                continue;
                            }
                        }
                    }
                }
            }
            
            if let Err(e) = fs::remove_file(entry.path()) {
                // Log but continue
                eprintln!("purge warning: {}", e);
            } else {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Complete manual purge.
    pub fn purge_trash(&self) -> Result<usize> {
        self.purge_trash_ttl(std::time::Duration::ZERO)
    }

    /// Clear all trashed files (alias for manual purge).
    pub fn clear_trash(&self) -> Result<usize> {
        self.purge_trash_ttl(std::time::Duration::ZERO)
    }

    /// Get the bucket name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

// ─── SHA-256 Implementation ─────────────────────────────────────────
// Minimal SHA-256 for dedup hashing. No external dependencies.

fn sha256_raw(data: &[u8], out: &mut [u8; 32]) {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
        0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
        0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
        0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    // Padding
    let bit_len = (data.len() as u64) * 8;
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    // Process 64-byte blocks
    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(k[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&state[i].to_be_bytes());
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_bucket(name: &str) -> (FileBucket, TempDir) {
        let dir = TempDir::new().unwrap();
        let bucket = FileBucket::new(name, dir.path());
        (bucket, dir)
    }

    #[test]
    fn store_and_get() {
        let (bucket, _) = test_bucket("attachments");
        let data = b"hello world";
        let meta = bucket.store("test.txt", data, "text/plain").unwrap();

        assert_eq!(meta.name, "test.txt");
        assert_eq!(meta.size, 11);
        assert_eq!(meta.type_, "text/plain");
        assert_eq!(meta._file.bucket, "attachments");
        assert!(!meta._file.id.is_empty());
        assert_eq!(meta._file.ext, "txt");

        let retrieved = bucket.get(&meta._file).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn store_deduplicates() {
        let (bucket, _) = test_bucket("files");
        let data = b"same content";

        let ref1 = bucket.store("file1.txt", data, "text/plain").unwrap();
        let ref2 = bucket.store("file2.txt", data, "text/plain").unwrap();

        assert_eq!(ref1._file.id, ref2._file.id);
        assert_eq!(bucket.list().unwrap().len(), 1);
    }

    #[test]
    fn delete_moves_to_trash() {
        let (bucket, _) = test_bucket("files");
        let meta = bucket.store("delete_me.txt", b"data", "text/plain").unwrap();

        bucket.delete(&meta._file).unwrap();
        assert!(!bucket.exists(&meta._file));

        // Should be in trash
        let trash_dir = bucket.trash_dir();
        assert!(trash_dir.join(meta._file.filename()).exists());
    }

    #[test]
    fn restore_from_trash() {
        let (bucket, _) = test_bucket("files");
        let meta = bucket.store("restore.txt", b"data", "text/plain").unwrap();

        bucket.delete(&meta._file).unwrap();
        bucket.restore(&meta._file.id, &meta._file.ext).unwrap();

        let data = bucket.get(&meta._file).unwrap();
        assert_eq!(data, b"data");
    }

    #[test]
    fn list_files() {
        let (bucket, _) = test_bucket("files");
        bucket.store("a.txt", b"a", "text/plain").unwrap();
        bucket.store("b.txt", b"b", "text/plain").unwrap();

        let files = bucket.list().unwrap();
        assert_eq!(files.len(), 2);
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
    fn sha256_known_value() {
        // SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let hash = FileBucket::sha256(b"hello");
        assert_eq!(&hash[..8], "2cf24dba");
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn sha256_empty() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let hash = FileBucket::sha256(b"");
        assert_eq!(&hash[..8], "e3b0c442");
    }

    #[test]
    fn get_nonexistent_file() {
        let (bucket, _) = test_bucket("files");
        let fr = FileRef {
            bucket: "files".to_string(),
            id: "nonexist".to_string(),
            ext: "txt".to_string(),
        };
        assert!(bucket.get(&fr).is_err());
    }

    #[test]
    fn delete_nonexistent_file() {
        let (bucket, _) = test_bucket("files");
        let fr = FileRef {
            bucket: "files".to_string(),
            id: "nonexist".to_string(),
            ext: "txt".to_string(),
        };
        assert!(bucket.delete(&fr).is_err());
    }
}
