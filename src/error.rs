//! Error types for ndb.

use std::path::PathBuf;
use thiserror::Error;

/// All errors that can occur in ndb operations.
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
    #[error("corruption in {file}: {message}")]
    Corruption {
        file: PathBuf,
        message: String,
    },

    /// Document not found.
    #[error("not found: {id}")]
    NotFound { id: String },

    /// Invalid argument.
    #[error("invalid argument: {reason}")]
    InvalidArgument { reason: String },

    /// Serialization/deserialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Database already open or locked.
    #[error("database locked: {path}")]
    DatabaseLocked { path: PathBuf },

    /// Index error.
    #[error("index error for field '{field}': {reason}")]
    IndexError { field: String, reason: String },

    /// File bucket error.
    #[error("file bucket error: {reason}")]
    BucketError { reason: String },
}

impl Error {
    /// Create an I/O error with context.
    pub fn io_err(
        path: impl Into<PathBuf>,
        context: impl Into<String>,
    ) -> impl FnOnce(std::io::Error) -> Self {
        move |e: std::io::Error| Error::Io {
            source: e,
            path: path.into(),
            context: context.into(),
        }
    }

    /// Create a corruption error.
    pub fn corruption(file: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Error::Corruption {
            file: file.into(),
            message: message.into(),
        }
    }

    /// Create a not-found error.
    pub fn not_found(id: impl Into<String>) -> Self {
        Error::NotFound { id: id.into() }
    }

    /// Create an invalid argument error.
    pub fn invalid_arg(reason: impl Into<String>) -> Self {
        Error::InvalidArgument {
            reason: reason.into(),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serialization(e.to_string())
    }
}

/// Result type alias for ndb operations.
pub type Result<T> = std::result::Result<T, Error>;
