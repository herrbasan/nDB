//! Error types for ndb.

use std::path::PathBuf;
use thiserror::Error;

use serde_json::Value;

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

    /// Bucket policy from meta.json forbids the operation.
    #[error("bucket policy violation: {reason}")]
    PolicyViolation { reason: String },

    /// A sync cursor points before the oldest replayable sequence.
    #[error("cursor expired: requested seq {requested_seq}, oldest available {min_available_seq} (last {last_seq})")]
    CursorExpired {
        requested_seq: u64,
        min_available_seq: u64,
        last_seq: u64,
    },

    /// A sync cursor points past the last committed sequence.
    #[error("cursor ahead of journal: requested seq {requested_seq}, last {last_seq}")]
    CursorAhead { requested_seq: u64, last_seq: u64 },

    /// Optimistic-concurrency check failed: the document moved since the
    /// caller read it. Carries the current state so the caller can retry
    /// without a second round trip.
    #[error("conflict: document is at seq {current_seq}")]
    Conflict { current_seq: u64, doc: Value },

    /// The database is poisoned: a WAL write or fsync failed in a way that
    /// leaves the durable tail's state unknown. Nothing was published, and
    /// every later mutation is refused until the database is reopened and
    /// recovered. Reads keep working — RAM is authoritative up to `last_seq`.
    #[error("database is poisoned after an uncertain durable write ({reason}); reopen and recover before writing again")]
    Poisoned { reason: String },

    /// A write would cross the database's own RAM budget. Nothing was staged
    /// and nothing was written: the caller may retry after freeing room.
    #[error(
        "memory limit exceeded: {current_bytes} bytes in use, limit {limit_bytes}, this write needs {needed_bytes} more"
    )]
    MemoryLimitExceeded {
        current_bytes: usize,
        limit_bytes: usize,
        needed_bytes: usize,
    },
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

    /// Create a bucket-policy violation error.
    pub fn policy_violation(reason: impl Into<String>) -> Self {
        Error::PolicyViolation {
            reason: reason.into(),
        }
    }

    /// Create a poisoned-database error.
    pub fn poisoned(reason: impl Into<String>) -> Self {
        Error::Poisoned {
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
