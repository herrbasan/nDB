//! Collection manifest for tracking state.
//!
//! The manifest tracks:
//! - Active segments (immutable, memory-mapped)
//! - Last WAL sequence number applied
//! - Collection configuration
//!
//! Updates are atomic via write-to-temp + fsync + rename.

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::CollectionConfig;

/// Manifest file name
pub const MANIFEST_FILE_NAME: &str = "MANIFEST";

/// Manifest temporary file name (for atomic writes)
pub const MANIFEST_TEMP_NAME: &str = "MANIFEST.tmp";

/// Collection manifest.
///
/// Tracks the state of a collection: segments, WAL position, and configuration.
/// Updated atomically via write-to-temp + fsync + rename.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Collection configuration (immutable after creation)
    pub config: CollectionConfig,
    /// Active segments (sorted by creation order)
    pub segments: Vec<SegmentEntry>,
    /// Last WAL sequence number applied to this collection
    pub last_wal_seq: u64,
    /// Manifest format version
    pub version: u32,
    /// Path to HNSW index file (relative to collection root), if exists
    #[serde(default)]
    pub index_file: Option<String>,
    /// HNSW index generation (incremented on rebuild)
    #[serde(default)]
    pub index_generation: u32,
}

/// Entry for an active segment in the manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentEntry {
    /// Segment file name (not full path)
    pub filename: String,
    /// Number of documents in this segment
    pub doc_count: u64,
    /// Internal ID range in this segment [start, end)
    pub id_range: (u32, u32),
}

impl Manifest {
    /// Current manifest format version
    pub const CURRENT_VERSION: u32 = 1;

    /// Create a new manifest for a collection
    pub fn new(config: CollectionConfig) -> Self {
        Self {
            config,
            segments: Vec::new(),
            last_wal_seq: 0,
            version: Self::CURRENT_VERSION,
            index_file: None,
            index_generation: 0,
        }
    }

    /// Load manifest from a file
    pub fn load(path: impl AsRef<Path>) -> Result<Option<Self>> {
        let path = path.as_ref();

        if !path.exists() {
            return Ok(None);
        }

        let mut file = File::open(path)
            .map_err(Error::io_err(path, "failed to open manifest"))?;

        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .map_err(Error::io_err(path, "failed to read manifest"))?;

        let manifest: Manifest = serde_json::from_slice(&contents).map_err(|e| {
            Error::corruption(path, 0, format!("invalid manifest JSON: {}", e))
        })?;

        // Version check
        if manifest.version != Self::CURRENT_VERSION {
            return Err(Error::corruption(
                path,
                0,
                format!(
                    "unsupported manifest version: expected {}, got {}",
                    Self::CURRENT_VERSION,
                    manifest.version
                ),
            ));
        }

        Ok(Some(manifest))
    }

    /// Save manifest atomically
    ///
    /// Uses write-to-temp + fsync + rename for atomicity.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let temp_path = path.with_extension("tmp");

        // Serialize to JSON
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Serialization(format!("failed to serialize manifest: {}", e)))?;

        // Write to temp file
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)
            .map_err(Error::io_err(&temp_path, "failed to create temp manifest"))?;

        temp_file
            .write_all(&json)
            .map_err(Error::io_err(&temp_path, "failed to write temp manifest"))?;

        temp_file
            .sync_all()
            .map_err(Error::io_err(&temp_path, "failed to sync temp manifest"))?;

        drop(temp_file);

        // Atomic rename
        std::fs::rename(&temp_path, path)
            .map_err(Error::io_err(path, "failed to rename manifest"))?;

        // Sync parent directory to ensure rename is durable
        Self::sync_parent_dir(path)?;

        Ok(())
    }

    /// Sync the parent directory (ensures rename is durable)
    fn sync_parent_dir(_path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let parent = path.parent().ok_or_else(|| {
                Error::io_err(path, "manifest has no parent directory")(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "no parent",
                ))
            })?;

            let dir = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(parent)
                .map_err(Error::io_err(parent, "failed to open parent directory"))?;

            dir.sync_all()
                .map_err(Error::io_err(parent, "failed to sync parent directory"))?;
        }

        #[cfg(windows)]
        {
            // On Windows, fs::rename is atomic enough for our purposes
            // Directory sync is not standard practice on Windows
        }

        Ok(())
    }

    /// Add a segment to the manifest
    pub fn add_segment(&mut self, entry: SegmentEntry) {
        self.segments.push(entry);
    }

    /// Remove segments by filename
    pub fn remove_segments(&mut self, filenames: &[String]) {
        self.segments.retain(|s| !filenames.contains(&s.filename));
    }

    /// Get segment filenames
    pub fn segment_filenames(&self) -> Vec<String> {
        self.segments.iter().map(|s| s.filename.clone()).collect()
    }

    /// Update the last WAL sequence number
    pub fn set_last_wal_seq(&mut self, seq: u64) {
        self.last_wal_seq = seq;
    }

    /// Get total document count across all segments
    pub fn total_doc_count(&self) -> u64 {
        self.segments.iter().map(|s| s.doc_count).sum()
    }

    /// Set the index file path
    pub fn set_index_file(&mut self, path: Option<String>) {
        self.index_file = path;
    }

    /// Get the index file path
    pub fn index_file(&self) -> Option<&str> {
        self.index_file.as_deref()
    }

    /// Increment index generation (call after rebuild)
    pub fn increment_index_generation(&mut self) {
        self.index_generation += 1;
    }

    /// Get the index generation
    pub fn index_generation(&self) -> u32 {
        self.index_generation
    }
}

/// Manifest manager handles atomic updates and cleanup.
pub struct ManifestManager {
    path: PathBuf,
    manifest: Manifest,
}

impl ManifestManager {
    /// Open or create a manifest at the given path
    pub fn open(path: impl AsRef<Path>, default_config: Option<CollectionConfig>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let manifest = if let Some(m) = Manifest::load(&path)? {
            m
        } else if let Some(config) = default_config {
            Manifest::new(config)
        } else {
            return Err(Error::NotFound {
                id: path.to_string_lossy().to_string(),
            });
        };

        Ok(Self { path, manifest })
    }

    /// Get a reference to the manifest
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Get a mutable reference to the manifest
    pub fn manifest_mut(&mut self) -> &mut Manifest {
        &mut self.manifest
    }

    /// Save the current manifest state atomically
    pub fn save(&self) -> Result<()> {
        self.manifest.save(&self.path)
    }

    /// Update the manifest with a closure and save atomically
    pub fn update<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Manifest),
    {
        f(&mut self.manifest);
        self.save()
    }

    /// Get the manifest path
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the last WAL sequence
    pub fn last_wal_seq(&self) -> u64 {
        self.manifest.last_wal_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_manifest_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = temp_dir.path().join("MANIFEST");

        let config = CollectionConfig::new(768);
        let mut manifest = Manifest::new(config);
        manifest.add_segment(SegmentEntry {
            filename: "0001.ndb".to_string(),
            doc_count: 100,
            id_range: (0, 100),
        });
        manifest.set_last_wal_seq(42);

        manifest.save(&manifest_path).unwrap();

        let loaded = Manifest::load(&manifest_path).unwrap().unwrap();
        assert_eq!(loaded.config.dim, 768);
        assert_eq!(loaded.segments.len(), 1);
        assert_eq!(loaded.segments[0].filename, "0001.ndb");
        assert_eq!(loaded.last_wal_seq, 42);
    }

    #[test]
    fn test_manifest_atomic_update() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = temp_dir.path().join("MANIFEST");

        let config = CollectionConfig::new(768);
        let manifest = Manifest::new(config);
        manifest.save(&manifest_path).unwrap();

        // Simulate concurrent update by creating temp file first
        let temp_path = manifest_path.with_extension("tmp");
        std::fs::write(&temp_path, b"incomplete data").unwrap();

        // Complete the update
        let mut manifest2 = Manifest::load(&manifest_path).unwrap().unwrap();
        manifest2.add_segment(SegmentEntry {
            filename: "0002.ndb".to_string(),
            doc_count: 50,
            id_range: (100, 150),
        });
        manifest2.save(&manifest_path).unwrap();

        // Temp file should be gone
        assert!(!temp_path.exists());

        // Manifest should be valid
        let loaded = Manifest::load(&manifest_path).unwrap().unwrap();
        assert_eq!(loaded.segments.len(), 1);
    }

    #[test]
    fn test_manifest_manager() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = temp_dir.path().join("MANIFEST");

        let config = CollectionConfig::new(768);
        let mut manager = ManifestManager::open(&manifest_path, Some(config)).unwrap();

        manager
            .update(|m| {
                m.add_segment(SegmentEntry {
                    filename: "0001.ndb".to_string(),
                    doc_count: 100,
                    id_range: (0, 100),
                });
                m.set_last_wal_seq(10);
            })
            .unwrap();

        let manager2 = ManifestManager::open(&manifest_path, None).unwrap();
        assert_eq!(manager2.manifest().segments.len(), 1);
        assert_eq!(manager2.last_wal_seq(), 10);
    }

    #[test]
    fn test_manifest_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = temp_dir.path().join("NONEXISTENT");

        let result = ManifestManager::open(&manifest_path, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_remove_segments() {
        let temp_dir = TempDir::new().unwrap();
        let manifest_path = temp_dir.path().join("MANIFEST");

        let config = CollectionConfig::new(768);
        let mut manifest = Manifest::new(config);
        manifest.add_segment(SegmentEntry {
            filename: "0001.ndb".to_string(),
            doc_count: 100,
            id_range: (0, 100),
        });
        manifest.add_segment(SegmentEntry {
            filename: "0002.ndb".to_string(),
            doc_count: 50,
            id_range: (100, 150),
        });

        manifest.remove_segments(&["0001.ndb".to_string()]);

        assert_eq!(manifest.segments.len(), 1);
        assert_eq!(manifest.segments[0].filename, "0002.ndb");
    }

    #[test]
    fn test_total_doc_count() {
        let config = CollectionConfig::new(768);
        let mut manifest = Manifest::new(config);
        manifest.add_segment(SegmentEntry {
            filename: "0001.ndb".to_string(),
            doc_count: 100,
            id_range: (0, 100),
        });
        manifest.add_segment(SegmentEntry {
            filename: "0002.ndb".to_string(),
            doc_count: 50,
            id_range: (100, 150),
        });

        assert_eq!(manifest.total_doc_count(), 150);
    }
}
