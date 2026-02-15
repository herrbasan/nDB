use std::path::PathBuf;
use thiserror::Error;

/// All errors that can occur in nDB operations.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// I/O error with context.
    #[error("I/O error at {path}: {context} ({source})")]
    Io {
        #[source]
        source: std::io::Error,
        path: PathBuf,
        context: String,
    },

    /// Data corruption detected.
    #[error("corruption in {file} at offset {offset}: {message}")]
    Corruption {
        file: PathBuf,
        offset: u64,
        message: String,
    },

    /// Invalid argument provided.
    #[error("invalid argument for field '{field}': {reason}")]
    InvalidArgument { field: String, reason: String },

    /// Document or resource not found.
    #[error("not found: {id}")]
    NotFound { id: String },

    /// Vector dimension mismatch.
    #[error("dimension mismatch: expected {expected}, got {got}")]
    WrongDimension { expected: usize, got: usize },

    /// Collection is locked by another writer.
    #[error("collection '{name}' is locked by another process")]
    CollectionLocked { name: String },

    /// Collection already exists.
    #[error("collection '{name}' already exists")]
    CollectionExists { name: String },

    /// Collection not found.
    #[error("collection '{name}' not found")]
    CollectionNotFound { name: String },

    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// WAL corruption or format error.
    #[error("WAL error at sequence {seq}: {message}")]
    WalError { seq: u64, message: String },

    /// Checksum mismatch.
    #[error("checksum mismatch in {file}: expected {expected:016x}, got {got:016x}")]
    ChecksumMismatch {
        file: PathBuf,
        expected: u64,
        got: u64,
    },
}

impl Error {
    /// Create an I/O error with context.
    pub fn io_err(path: impl Into<PathBuf>, context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        move |e: std::io::Error| Error::Io {
            source: e,
            path: path.into(),
            context: context.into(),
        }
    }

    /// Create a corruption error.
    pub fn corruption(file: impl Into<PathBuf>, offset: u64, message: impl Into<String>) -> Self {
        Error::Corruption {
            file: file.into(),
            offset,
            message: message.into(),
        }
    }

    /// Create an invalid argument error.
    pub fn invalid_arg(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::InvalidArgument {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

/// Result type alias for nDB operations.
pub type Result<T> = std::result::Result<T, Error>;
