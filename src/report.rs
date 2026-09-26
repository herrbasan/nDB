//! Reporting for failures that have no result to travel back in.
//!
//! nDB does not grow a logging framework: it is embedded by services whose
//! stderr already lands in the lab's log monitor, so stderr *is* the sink.
//!
//! What this module exists to forbid is the bare `let _ =`. Discarding a
//! failure does not make it go away, it makes it invisible for the lifetime of
//! the process — a delete whose trash record was never written, a GC that
//! counted a file it never moved, a flush at shutdown that never happened.
//! Every such site names the operation and says so here instead.

/// A failure in work ancillary to the operation the caller asked for.
///
/// The primary operation's outcome stands and its return value is not the
/// place to report this — the caller asked to delete a document, and the
/// document *was* deleted. But the file it referenced could not be moved to
/// trash, and that must not pass unremarked. Logged, never swallowed.
pub fn suppressed(op: &str, detail: impl std::fmt::Display) {
    eprintln!("ndb: suppressed failure: {op}: {detail}");
}

/// A failure on a path with no caller to return to — a background sweep, a
/// `Drop` impl. There is no result to carry it, so stderr is the only channel.
pub fn detached(op: &str, detail: impl std::fmt::Display) {
    eprintln!("ndb: detached failure: {op}: {detail}");
}
