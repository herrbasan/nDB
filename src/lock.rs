//! Multi-process locking using advisory file locks.
//!
//! This module provides exclusive locks for collection writers using `flock(2)`
//! on Unix and `LockFile` on Windows. Readers do not require locks.

use crate::error::{Error, Result};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// An exclusive lock on a collection.
///
/// The lock is released when this handle is dropped. The lock file
/// remains on disk but is no longer locked.
#[derive(Debug)]
pub struct CollectionLock {
    #[cfg(unix)]
    _file: File,
    #[cfg(windows)]
    _file: File,
    path: PathBuf,
    collection_name: String,
}

impl CollectionLock {
    /// Acquire an exclusive lock on a collection.
    ///
    /// Creates the lock file if it doesn't exist. Returns an error if
    /// another process already holds the lock.
    pub fn acquire(collection_path: &Path, collection_name: &str) -> Result<Self> {
        let lock_path = collection_path.join("LOCK");
        
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(Error::io_err(&lock_path, "failed to open lock file"))?;

        Self::try_lock(&file, &lock_path, collection_name)?;

        Ok(Self {
            #[cfg(unix)]
            _file: file,
            #[cfg(windows)]
            _file: file,
            path: lock_path,
            collection_name: collection_name.to_string(),
        })
    }

    #[cfg(unix)]
    fn try_lock(file: &File, lock_path: &Path, collection_name: &str) -> Result<()> {
        use std::os::unix::io::AsRawFd;

        let fd = file.as_raw_fd();
        
        // Try to acquire exclusive lock, non-blocking
        let result = unsafe {
            libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB)
        };

        if result != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                return Err(Error::CollectionLocked {
                    name: collection_name.to_string(),
                });
            }
            return Err(Error::Io {
                source: err,
                path: lock_path.to_path_buf(),
                context: format!("flock failed: {}", err),
            });
        }

        Ok(())
    }

    #[cfg(windows)]
    fn try_lock(file: &File, lock_path: &Path, collection_name: &str) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::FALSE;
        use windows_sys::Win32::Storage::FileSystem::LockFileEx;
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let handle = file.as_raw_handle();
        
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        
        // Try to acquire exclusive lock, non-blocking
        // LOCKFILE_EXCLUSIVE_LOCK = 2, LOCKFILE_FAIL_IMMEDIATELY = 1
        let flags = 0x00000002 | 0x00000001; // EXCLUSIVE | FAIL_IMMEDIATELY
        
        let result = unsafe {
            LockFileEx(
                handle as _,
                flags,
                0,
                0xFFFFFFFF, // dwNumberOfBytesToLockLow
                0xFFFFFFFF, // dwNumberOfBytesToLockHigh
                &mut overlapped,
            )
        };

        if result == FALSE {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(33) { // ERROR_LOCK_VIOLATION
                return Err(Error::CollectionLocked {
                    name: collection_name.to_string(),
                });
            }
            let err_msg = err.to_string();
            return Err(Error::Io {
                source: err,
                path: lock_path.to_path_buf(),
                context: format!("LockFileEx failed: {}", err_msg),
            });
        }

        Ok(())
    }

    /// Get the path to the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the collection name.
    pub fn collection_name(&self) -> &str {
        &self.collection_name
    }
}

impl Drop for CollectionLock {
    #[cfg(unix)]
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        
        let fd = self._file.as_raw_fd();
        unsafe {
            // Release the lock
            libc::flock(fd, libc::LOCK_UN);
        }
    }

    #[cfg(windows)]
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let handle = self._file.as_raw_handle();
        
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        
        unsafe {
            UnlockFileEx(
                handle as _,
                0,
                0xFFFFFFFF,
                0xFFFFFFFF,
                &mut overlapped,
            );
        }
    }
}

/// Check if a collection is locked without acquiring the lock.
///
/// Returns `Ok(true)` if locked, `Ok(false)` if not locked, or an error
/// if the check failed.
pub fn is_locked(collection_path: &Path) -> Result<bool> {
    let lock_path = collection_path.join("LOCK");
    
    // If lock file doesn't exist, collection is not locked
    if !lock_path.exists() {
        return Ok(false);
    }

    let file = OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .map_err(Error::io_err(&lock_path, "failed to open lock file for check"))?;

    // Try to acquire lock non-blocking, immediately release
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        
        let fd = file.as_raw_fd();
        let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        
        if result == 0 {
            // Acquired lock, so it wasn't locked - release it
            unsafe { libc::flock(fd, libc::LOCK_UN) };
            Ok(false)
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                Ok(true)
            } else {
                Err(Error::Io {
                    source: err,
                    path: lock_path,
                    context: format!("flock check failed: {}", err),
                })
            }
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::LockFileEx;
        use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
        use windows_sys::Win32::System::IO::OVERLAPPED;

        let handle = file.as_raw_handle();
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        
        // Try exclusive lock with fail immediately
        let flags = 0x00000002 | 0x00000001;
        
        let result = unsafe {
            LockFileEx(
                handle as _,
                flags,
                0,
                0xFFFFFFFF,
                0xFFFFFFFF,
                &mut overlapped,
            )
        };

        if result == windows_sys::Win32::Foundation::TRUE {
            // Acquired lock, release it
            unsafe {
                UnlockFileEx(handle as _, 0, 0xFFFFFFFF, 0xFFFFFFFF, &mut overlapped);
            }
            Ok(false)
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(33) { // ERROR_LOCK_VIOLATION
                Ok(true)
            } else {
                let err_msg = err.to_string();
                Err(Error::Io {
                    source: err,
                    path: lock_path,
                    context: format!("LockFileEx check failed: {}", err_msg),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_lock_acquire_and_release() {
        let temp_dir = TempDir::new().unwrap();
        let collection_path = temp_dir.path();

        // Acquire lock
        let lock = CollectionLock::acquire(collection_path, "test_collection");
        assert!(lock.is_ok());

        // Verify lock is held
        assert!(is_locked(collection_path).unwrap());

        // Drop the lock
        drop(lock);

        // Verify lock is released
        assert!(!is_locked(collection_path).unwrap());
    }

    #[test]
    fn test_double_lock_fails() {
        let temp_dir = TempDir::new().unwrap();
        let collection_path = temp_dir.path();

        // First lock should succeed
        let lock1 = CollectionLock::acquire(collection_path, "test_collection").unwrap();

        // Second lock should fail
        let result = CollectionLock::acquire(collection_path, "test_collection");
        assert!(matches!(result, Err(Error::CollectionLocked { name }) if name == "test_collection"));

        // Release first lock
        drop(lock1);

        // Now second lock should succeed
        let lock2 = CollectionLock::acquire(collection_path, "test_collection");
        assert!(lock2.is_ok());
    }

    #[test]
    fn test_lock_file_created() {
        let temp_dir = TempDir::new().unwrap();
        let collection_path = temp_dir.path();
        let lock_path = collection_path.join("LOCK");

        assert!(!lock_path.exists());

        let _lock = CollectionLock::acquire(collection_path, "test").unwrap();

        assert!(lock_path.exists());
    }
}
