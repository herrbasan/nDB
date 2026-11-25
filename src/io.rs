//! Filesystem boundary for the RAM-first engine.
//!
//! Every step that carries a durability guarantee — WAL append, fsync,
//! manifest rename, parent-directory fsync, torn-tail truncation — is an
//! [`Io`] call. Production code uses [`RealIo`]; tests drive the exact same
//! code path through [`FaultIo`], which fails or kills the process at the
//! N-th call of a chosen operation.
//!
//! The fault matrix is only meaningful if the engine cannot reach the
//! filesystem any other way, so the trait deliberately covers the whole
//! surface the write protocol needs and nothing else.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// The durable operations a fault plan can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IoOp {
    WriteAll,
    SyncAll,
    SyncParentDir,
    Rename,
    RemoveFile,
    CreateNew,
    TruncateAndSync,
}

/// What a fault plan does when its trigger is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultAction {
    /// Return a deterministic `io::Error` of this kind.
    Error(io::ErrorKind),
    /// Terminate the process immediately. Out-of-process crash simulation:
    /// nothing runs afterwards, exactly like a power cut or an abort.
    Exit(i32),
}

/// One armed fault: the `trigger_at`-th (1-based) call of `op` in the wrapped
/// `FaultIo`'s lifetime fires `action`.
#[derive(Debug, Clone, Copy)]
pub struct FaultPlan {
    pub op: IoOp,
    pub trigger_at: usize,
    pub action: FaultAction,
}

/// The durable filesystem surface.
pub trait Io: Send + Sync {
    /// Create a file that must not already exist, open for read + write.
    fn create_new(&self, path: &Path) -> io::Result<File>;
    /// Append `buf` to `file` in one call.
    fn write_all(&self, file: &mut File, buf: &[u8]) -> io::Result<()>;
    /// Flush the file's data and metadata to stable storage.
    fn sync_all(&self, file: &File) -> io::Result<()>;
    /// fsync the directory holding `path` (a rename/creation is only durable
    /// once the directory entry itself is on disk).
    fn sync_parent_dir(&self, path: &Path) -> io::Result<()>;
    /// Atomically replace `to` with `from`.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Remove one file.
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Truncate `path` to `len` bytes and fsync it — recovery's torn-tail clear.
    fn truncate_and_sync(&self, path: &Path, len: u64) -> io::Result<()>;
}

/// The real filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealIo;

/// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`: a database file is
/// opened for append while readers and the maintenance tooling hold their own
/// handles, so every open must share.
#[cfg(windows)]
pub(crate) const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;

impl RealIo {
    pub fn new() -> Self {
        RealIo
    }
}

fn open_for_read_write(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(crate::io::FILE_SHARE_ALL);
    }
    options.open(path)
}

impl Io for RealIo {
    fn create_new(&self, path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(crate::io::FILE_SHARE_ALL);
        }
        options.open(path)
    }

    fn write_all(&self, file: &mut File, buf: &[u8]) -> io::Result<()> {
        file.write_all(buf)
    }

    fn sync_all(&self, file: &File) -> io::Result<()> {
        file.sync_all()
    }

    fn sync_parent_dir(&self, path: &Path) -> io::Result<()> {
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        sync_dir(dir)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn truncate_and_sync(&self, path: &Path, len: u64) -> io::Result<()> {
        let file = open_for_read_write(path)?;
        file.set_len(len)?;
        file.sync_all()
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot `sync_all` a directory through a read-only handle; it needs
/// write access plus `FILE_FLAG_BACKUP_SEMANTICS`. Same convention as
/// `storage::sync_parent_dir`, proven by its `windows_directory_fsync_is_honest`
/// test.
#[cfg(windows)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?;
    file.sync_all()
}

#[cfg(not(any(unix, windows)))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

// ─── Fault injection (test builds and `--features fault-io` only) ───────────

#[cfg(any(test, feature = "fault-io"))]
pub use fault::FaultIo;

#[cfg(any(test, feature = "fault-io"))]
mod fault {
    use super::{FaultAction, FaultPlan, Io, IoOp, RealIo};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    /// Wraps [`RealIo`] and fires a [`FaultPlan`] at the exact call it names.
    ///
    /// Counts are per operation kind and per instance, so a plan armed before
    /// a test step fires deterministically inside that step — no timing, no
    /// ambient state. A plan is consumed only when its trigger matches; the
    /// error returned is always `Injected... (call N of Kind)`.
    pub struct FaultIo {
        inner: RealIo,
        plans: Mutex<Vec<FaultPlan>>,
        calls: Mutex<HashMap<IoOp, usize>>,
    }

    impl Default for FaultIo {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FaultIo {
        pub fn new() -> Self {
            FaultIo {
                inner: RealIo::new(),
                plans: Mutex::new(Vec::new()),
                calls: Mutex::new(HashMap::new()),
            }
        }

        /// Arm one fault. Several plans may target different ops (or different
        /// call indices of one op).
        pub fn arm(&self, plan: FaultPlan) {
            self.plans.lock().push(plan);
        }

        /// Drop every armed plan. Call counts are kept.
        pub fn clear_plans(&self) {
            self.plans.lock().clear();
        }

        /// How many times `op` has been called on this instance.
        pub fn calls(&self, op: IoOp) -> usize {
            self.calls.lock().get(&op).copied().unwrap_or(0)
        }

        fn hit(&self, op: IoOp) -> io::Result<()> {
            let call = {
                let mut calls = self.calls.lock();
                let count = calls.entry(op).or_insert(0);
                *count += 1;
                *count
            };
            let action = self
                .plans
                .lock()
                .iter()
                .find(|plan| plan.op == op && plan.trigger_at == call)
                .map(|plan| plan.action);
            match action {
                None => Ok(()),
                Some(FaultAction::Error(kind)) => Err(io::Error::new(
                    kind,
                    format!("FaultIo: injected {op:?} failure at call {call}"),
                )),
                Some(FaultAction::Exit(code)) => std::process::exit(code),
            }
        }
    }

    impl Io for FaultIo {
        fn create_new(&self, path: &Path) -> io::Result<File> {
            self.hit(IoOp::CreateNew)?;
            self.inner.create_new(path)
        }

        fn write_all(&self, file: &mut File, buf: &[u8]) -> io::Result<()> {
            self.hit(IoOp::WriteAll)?;
            self.inner.write_all(file, buf)
        }

        fn sync_all(&self, file: &File) -> io::Result<()> {
            self.hit(IoOp::SyncAll)?;
            self.inner.sync_all(file)
        }

        fn sync_parent_dir(&self, path: &Path) -> io::Result<()> {
            self.hit(IoOp::SyncParentDir)?;
            self.inner.sync_parent_dir(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.hit(IoOp::Rename)?;
            self.inner.rename(from, to)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.hit(IoOp::RemoveFile)?;
            self.inner.remove_file(path)
        }

        fn truncate_and_sync(&self, path: &Path, len: u64) -> io::Result<()> {
            self.hit(IoOp::TruncateAndSync)?;
            self.inner.truncate_and_sync(path, len)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    fn write_via(io: &dyn Io, path: &Path, bytes: &[u8]) {
        let mut file = io.create_new(path).expect("create file");
        io.write_all(&mut file, bytes).expect("write bytes");
        io.sync_all(&file).expect("sync file");
        io.sync_parent_dir(path).expect("sync parent dir");
    }

    fn read(path: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        File::open(path)
            .expect("open file")
            .read_to_end(&mut bytes)
            .expect("read file");
        bytes
    }

    #[test]
    fn real_io_writes_syncs_and_truncates() {
        let dir = temp_dir();
        let path = dir.path().join("wal.gen_0.ndb");
        write_via(&RealIo, &path, b"first-batch");
        assert_eq!(read(&path), b"first-batch");

        RealIo
            .truncate_and_sync(&path, 5)
            .expect("truncate and sync");
        assert_eq!(read(&path), b"first");
    }

    #[test]
    fn real_io_renames_and_removes() {
        let dir = temp_dir();
        let from = dir.path().join("manifest.tmp");
        let to = dir.path().join("manifest.json");
        write_via(&RealIo, &from, b"{}");
        RealIo.rename(&from, &to).expect("rename");
        RealIo.sync_parent_dir(&to).expect("sync dir after rename");
        assert!(!from.exists());
        assert_eq!(read(&to), b"{}");
        RealIo.remove_file(&to).expect("remove");
        assert!(!to.exists());
    }

    #[test]
    fn real_io_create_new_refuses_an_existing_path() {
        let dir = temp_dir();
        let path = dir.path().join("base.gen_0.ndb");
        write_via(&RealIo, &path, b"x");
        let err = RealIo
            .create_new(&path)
            .expect_err("create_new over an existing file must fail");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn real_io_truncate_missing_file_fails_loud() {
        let dir = temp_dir();
        let err = RealIo
            .truncate_and_sync(&dir.path().join("absent.ndb"), 0)
            .expect_err("truncating a missing file must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn fault_io_injects_write_sync_rename_dir_sync_truncate_and_create() {
        let dir = temp_dir();
        let cases = [
            (IoOp::CreateNew, "create"),
            (IoOp::WriteAll, "write"),
            (IoOp::SyncAll, "sync"),
            (IoOp::SyncParentDir, "dir-sync"),
            (IoOp::Rename, "rename"),
            (IoOp::TruncateAndSync, "truncate"),
        ];
        for (op, label) in cases {
            let io = FaultIo::new();
            io.arm(FaultPlan {
                op,
                trigger_at: 1,
                action: FaultAction::Error(io::ErrorKind::Other),
            });
            let from = dir.path().join(format!("{label}.tmp"));
            let to = dir.path().join(format!("{label}.final"));
            let result = match op {
                IoOp::CreateNew => io.create_new(&from).map(|_| ()),
                IoOp::WriteAll => {
                    let mut file = RealIo.create_new(&from).expect("setup file");
                    io.write_all(&mut file, b"payload")
                }
                IoOp::SyncAll => {
                    let file = RealIo.create_new(&from).expect("setup file");
                    io.sync_all(&file)
                }
                IoOp::SyncParentDir => io.sync_parent_dir(&from),
                IoOp::Rename => io.rename(&from, &to),
                IoOp::TruncateAndSync => io.truncate_and_sync(&from, 0),
                IoOp::RemoveFile => io.remove_file(&from),
            };
            let err = result.expect_err(label);
            assert_eq!(err.kind(), io::ErrorKind::Other, "{label}");
            assert!(
                err.to_string().contains(&format!("injected {op:?} failure at call 1")),
                "{label}: {err}"
            );
            assert_eq!(io.calls(op), 1, "{label}");
        }
    }

    #[test]
    fn fault_io_fires_only_at_the_planned_call() {
        let dir = temp_dir();
        let io = FaultIo::new();
        io.arm(FaultPlan {
            op: IoOp::WriteAll,
            trigger_at: 2,
            action: FaultAction::Error(io::ErrorKind::BrokenPipe),
        });
        let path = dir.path().join("wal.gen_1.ndb");
        let mut file = io.create_new(&path).expect("create");
        io.write_all(&mut file, b"first").expect("first write passes");
        let err = io
            .write_all(&mut file, b"second")
            .expect_err("second write fails");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        io.clear_plans();
        io.write_all(&mut file, b"third").expect("cleared plan");
        assert_eq!(io.calls(IoOp::WriteAll), 3);
        assert_eq!(read(&path), b"firstthird");
    }

    #[test]
    fn fault_io_does_not_fire_when_no_plan_is_armed() {
        let dir = temp_dir();
        let io = FaultIo::new();
        let path = dir.path().join("base.gen_0.ndb");
        write_via(&io, &path, b"clean");
        assert_eq!(read(&path), b"clean");
        assert_eq!(io.calls(IoOp::CreateNew), 1);
        assert_eq!(io.calls(IoOp::WriteAll), 1);
        assert_eq!(io.calls(IoOp::SyncAll), 1);
        assert_eq!(io.calls(IoOp::SyncParentDir), 1);
    }

    #[test]
    fn fault_io_remove_file_injection_is_reachable() {
        let dir = temp_dir();
        let path = dir.path().join("stale.gen_0.ndb");
        write_via(&RealIo, &path, b"x");
        let io = FaultIo::new();
        io.arm(FaultPlan {
            op: IoOp::RemoveFile,
            trigger_at: 1,
            action: FaultAction::Error(io::ErrorKind::PermissionDenied),
        });
        let err = io.remove_file(&path).expect_err("remove fails");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(path.exists(), "the file must survive a failed remove");
    }

    #[test]
    fn fault_io_exit_action_terminates_out_of_process() {
        // In-process this would kill the test runner, so the plan is only
        // constructed here; `tests/phase17_fault_injection_tests.rs` proves
        // the exit path in a child process.
        let io = FaultIo::new();
        io.arm(FaultPlan {
            op: IoOp::SyncAll,
            trigger_at: 1,
            action: FaultAction::Exit(17),
        });
        assert_eq!(io.calls(IoOp::SyncAll), 0);
    }

    #[test]
    fn io_is_object_safe_and_usable_behind_a_box() {
        let dir = temp_dir();
        let io: Box<dyn Io> = Box::new(RealIo::new());
        let path = dir.path().join("probe.ndb");
        write_via(io.as_ref(), &path, b"object-safe");
        assert_eq!(read(&path), b"object-safe");
    }
}
