//! Read-only memory-mapped files.
//!
//! A compacted base file is immutable once a generation is committed, so every
//! reader can share one mapping instead of copying bytes into the heap. The
//! mapping is opened with `FILE_SHARE_DELETE` on Windows: compaction unlinks a
//! superseded `base.gen_<N>.ndb` while live readers still hold an
//! `Arc<MmapFile>`; their view stays valid until the last handle closes.
//!
//! Both platforms use a minimal hand-written FFI (no `libc` / `windows-sys`
//! dependency). Every fallible call is checked and converted through
//! `io::Error::last_os_error()`; there is no fallback path.

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(not(any(unix, windows)))]
compile_error!("nDB's mmap layer supports Windows and Unix only");

/// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` (Win32 share flags).
///
/// Every v2 file is opened with all three: compaction must be able to unlink a
/// superseded base generation while readers are mapped onto it, and a WAL is
/// read by change-feed consumers while the writer appends to it.
#[cfg(windows)]
pub(crate) const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    pub type Handle = *mut c_void;

    /// `PAGE_READONLY`
    pub const PAGE_READONLY: u32 = 0x02;
    /// `FILE_MAP_READ`
    pub const FILE_MAP_READ: u32 = 0x04;

    #[allow(non_snake_case)]
    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateFileMappingW(
            h_file: Handle,
            lp_file_mapping_attributes: *mut c_void,
            fl_protect: u32,
            dw_maximum_size_high: u32,
            dw_maximum_size_low: u32,
            lp_name: *const u16,
        ) -> Handle;
        pub fn MapViewOfFile(
            h_file_mapping_object: Handle,
            dw_desired_access: u32,
            dw_file_offset_high: u32,
            dw_file_offset_low: u32,
            dw_number_of_bytes_to_map: usize,
        ) -> *mut c_void;
        pub fn UnmapViewOfFile(lp_base_address: *const c_void) -> i32;
        pub fn CloseHandle(h_object: Handle) -> i32;
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::c_void;
    use std::os::raw::c_int;

    /// `PROT_READ`
    pub const PROT_READ: c_int = 0x1;
    /// `MAP_PRIVATE`
    pub const MAP_PRIVATE: c_int = 0x2;
    /// `MAP_FAILED` — `(void *)-1`
    pub const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

    extern "C" {
        // `off_t` is 64-bit on every Unix target nDB builds for; a 32-bit
        // target without large-file support would truncate the offset, which
        // these files never use (offset is always 0).
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }
}

fn os_error(path: &Path, context: &str) -> Error {
    Error::io_err(path, context)(std::io::Error::last_os_error())
}

/// A read-only mapping of an entire file.
///
/// The bytes are immutable for the lifetime of the mapping: the struct exposes
/// no mutable access, and the OS keeps the backing pages alive even after the
/// file is unlinked (Windows) or unlinked (Unix) by another process.
pub struct MmapFile {
    ptr: *const u8,
    len: usize,
    path: PathBuf,
    #[cfg(windows)]
    mapping: win::Handle,
    /// Kept alive so the mapped file cannot be closed under the view.
    _file: File,
}

// SAFETY: the mapping is read-only and immutable for the lifetime of the
// struct. No method hands out a mutable view, and `Drop` is the only writer of
// `ptr`/`len`.
unsafe impl Send for MmapFile {}
unsafe impl Sync for MmapFile {}

impl std::fmt::Debug for MmapFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MmapFile")
            .field("path", &self.path)
            .field("len", &self.len)
            .finish()
    }
}

impl MmapFile {
    /// Open `path` and map its whole length read-only.
    ///
    /// A 0-byte file is valid and maps to an empty slice without ever calling
    /// the OS mapping API (mapping zero bytes is an error on both platforms).
    pub fn open(path: &Path) -> Result<MmapFile> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(FILE_SHARE_ALL);
        }
        let file = options
            .open(path)
            .map_err(Error::io_err(path, "open file for read-only mmap"))?;
        let file_len = file
            .metadata()
            .map_err(Error::io_err(path, "stat file for read-only mmap"))?
            .len();
        let len = usize::try_from(file_len).map_err(|_| {
            Error::invalid_arg(format!(
                "{} is {file_len} bytes, larger than this platform's address space",
                path.display()
            ))
        })?;

        if len == 0 {
            return Ok(MmapFile {
                ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(),
                len: 0,
                path: path.to_path_buf(),
                #[cfg(windows)]
                mapping: std::ptr::null_mut(),
                _file: file,
            });
        }

        #[cfg(windows)]
        {
            let mapping = unsafe {
                win::CreateFileMappingW(
                    file.as_raw_handle(),
                    std::ptr::null_mut(),
                    win::PAGE_READONLY,
                    0,
                    0,
                    std::ptr::null(),
                )
            };
            if mapping.is_null() {
                return Err(os_error(path, "CreateFileMappingW failed"));
            }
            let view = unsafe { win::MapViewOfFile(mapping, win::FILE_MAP_READ, 0, 0, 0) };
            if view.is_null() {
                let error = os_error(path, "MapViewOfFile failed");
                unsafe {
                    win::CloseHandle(mapping);
                }
                return Err(error);
            }
            return Ok(MmapFile {
                ptr: view as *const u8,
                len,
                path: path.to_path_buf(),
                mapping,
                _file: file,
            });
        }

        #[cfg(unix)]
        {
            let view = unsafe {
                unix::mmap(
                    std::ptr::null_mut(),
                    len,
                    unix::PROT_READ,
                    unix::MAP_PRIVATE,
                    file.as_raw_fd(),
                    0,
                )
            };
            if view == unix::MAP_FAILED {
                return Err(os_error(path, "mmap failed"));
            }
            return Ok(MmapFile {
                ptr: view as *const u8,
                len,
                path: path.to_path_buf(),
                _file: file,
            });
        }
    }

    /// The mapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` addresses `len` readable bytes held alive by `_file`
        // and the mapping handle. For `len == 0` it is a dangling-but-aligned
        // non-null pointer, which is valid for a zero-length slice.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// Mapped length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapped file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The path this mapping was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MmapFile {
    fn drop(&mut self) {
        if self.len == 0 {
            // Nothing was mapped: `mapping` (Windows) is null and there is no
            // view to unmap.
            return;
        }
        #[cfg(windows)]
        unsafe {
            // Both calls are unmapping/cleanup: a failure here cannot be
            // reported from `Drop`, and the OS reclaims the view at process
            // exit regardless.
            win::UnmapViewOfFile(self.ptr as *const std::ffi::c_void);
            win::CloseHandle(self.mapping);
        }
        #[cfg(unix)]
        unsafe {
            unix::munmap(self.ptr as *mut std::ffi::c_void, self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(path: &Path, bytes: &[u8]) {
        let mut file = File::create(path).expect("create test file");
        file.write_all(bytes).expect("write test file");
        file.sync_all().expect("sync test file");
    }

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn mmap_file_is_send_and_sync() {
        assert_send_sync::<MmapFile>();
    }

    #[test]
    fn maps_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exact.bin");
        let bytes: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        write_file(&path, &bytes);

        let mapped = MmapFile::open(&path).unwrap();
        assert_eq!(mapped.len(), bytes.len());
        assert!(!mapped.is_empty());
        assert_eq!(mapped.as_slice(), bytes.as_slice());
        assert_eq!(mapped.path(), path.as_path());
    }

    #[test]
    fn maps_multi_page_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        let bytes: Vec<u8> = (0..1_048_576u32).map(|i| (i % 251) as u8).collect();
        write_file(&path, &bytes);

        let mapped = MmapFile::open(&path).unwrap();
        assert_eq!(mapped.len(), 1_048_576);
        assert_eq!(mapped.as_slice(), bytes.as_slice());
        assert_eq!(mapped.as_slice()[0], 0);
        assert_eq!(mapped.as_slice()[700_001], (700_001u32 % 251) as u8);
        assert_eq!(mapped.as_slice()[1_048_575], (1_048_575u32 % 251) as u8);
    }

    #[test]
    fn maps_zero_byte_file_without_calling_os_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        write_file(&path, b"");

        let mapped = MmapFile::open(&path).unwrap();
        assert_eq!(mapped.len(), 0);
        assert!(mapped.is_empty());
        assert!(mapped.as_slice().is_empty());
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = MmapFile::open(&dir.path().join("nope.bin")).unwrap_err();
        match error {
            Error::Io { path, .. } => assert!(path.ends_with("nope.bin")),
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_unlink_while_mapped_keeps_the_view_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("superseded.gen_1.ndb");
        let bytes: Vec<u8> = b"base generation one payload".repeat(64);
        write_file(&path, &bytes);

        let mapped = MmapFile::open(&path).unwrap();
        assert_eq!(mapped.as_slice(), bytes.as_slice());

        // Compaction unlinks the superseded generation while a reader is live.
        std::fs::remove_file(&path).expect("unlink while mapped must succeed with FILE_SHARE_DELETE");
        assert!(!path.exists(), "directory entry must be gone");

        // The mapping still sees the original bytes: the file is not released
        // until the last handle closes.
        assert_eq!(mapped.as_slice(), bytes.as_slice());

        drop(mapped);
        assert!(!path.exists(), "path stays gone after the mapping is dropped");
    }

    #[cfg(unix)]
    #[test]
    fn unix_unlink_while_mapped_keeps_the_view_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("superseded.gen_1.ndb");
        let bytes: Vec<u8> = b"base generation one payload".repeat(64);
        write_file(&path, &bytes);

        let mapped = MmapFile::open(&path).unwrap();
        std::fs::remove_file(&path).expect("unlink while mapped must succeed");
        assert_eq!(mapped.as_slice(), bytes.as_slice());
        drop(mapped);
        assert!(!path.exists());
    }
}
