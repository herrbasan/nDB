//! Out-of-process crash simulation.
//!
//! A crash that leaves the process running proves nothing — the interesting
//! failure is the one where nothing after the crash point ever executes. The
//! release profile builds with `panic = "abort"`, and a test binary cannot
//! observe its own death, so the only honest simulation is a child process
//! that dies and a parent that inspects the wreckage.
//!
//! The child is the *same test executable*: `current_exe()` is spawned with
//! `--exact crash_child_entrypoint`, and the role arrives in an environment
//! variable. The entrypoint test in `tests/phase17_fault_injection_tests.rs`
//! reads the role and runs the payload; without the variable it returns
//! immediately, so a normal test run is unaffected.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Which payload the child runs.
pub const ROLE_ENV: &str = "NDB_CRASH_CHILD_ROLE";
/// Directory the payload may write into.
pub const DIR_ENV: &str = "NDB_CRASH_CHILD_DIR";
/// Where inside a role the crash is aimed (role-specific point name).
pub const POINT_ENV: &str = "NDB_CRASH_CHILD_POINT";
/// Exit code every crashing role uses, so the parent can tell "died where we
/// aimed" from "died for another reason".
pub const EXIT_CODE: i32 = 42;

pub fn role() -> Option<String> {
    std::env::var(ROLE_ENV).ok()
}

pub fn point() -> String {
    std::env::var(POINT_ENV).expect("crash child started without NDB_CRASH_CHILD_POINT")
}

pub fn dir() -> PathBuf {
    PathBuf::from(
        std::env::var(DIR_ENV).expect("crash child started without NDB_CRASH_CHILD_DIR"),
    )
}

/// Spawn this test binary as a crash child. The caller must have created `dir`.
pub fn spawn(role: &str, dir: &Path) -> Output {
    spawn_at(role, dir, None)
}

/// Spawn a crash child aimed at one point inside its role.
pub fn spawn_at(role: &str, dir: &Path, point: Option<&str>) -> Output {
    let exe = std::env::current_exe().expect("resolve the current test executable");
    let mut command = Command::new(exe);
    command
        .args([
            "--exact",
            "crash_child_entrypoint",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(ROLE_ENV, role)
        .env(DIR_ENV, dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(point) = point {
        command.env(POINT_ENV, point);
    }
    command.output().expect("spawn the crash child")
}

/// Run one role to completion. Never returns: every role ends in
/// `std::process::exit` or a panic.
pub fn run(role: &str) -> ! {
    match role {
        // FaultIo kills the process at the armed operation. Reaching the line
        // after `sync_all` would mean the injection silently did nothing.
        #[cfg(feature = "fault-io")]
        "fault-io-exit" => {
            use ndb::io::{FaultAction, FaultIo, FaultPlan, Io, IoOp};

            let io = FaultIo::new();
            io.arm(FaultPlan {
                op: IoOp::SyncAll,
                trigger_at: 1,
                action: FaultAction::Exit(EXIT_CODE),
            });
            let path = dir().join("fault-probe.ndb");
            let mut file = io.create_new(&path).expect("create the probe file");
            io.write_all(&mut file, b"durable-bytes")
                .expect("write the probe bytes");
            io.sync_all(&file).expect("sync the probe file");
            panic!("child survived an armed FaultAction::Exit");
        }
        // Abrupt death after acknowledged writes: the parent must find every
        // acknowledged document after reopening the database.
        "db-abrupt-exit" => {
            use std::io::Write;
            use serde_json::json;

            let db = ndb::Database::open_ram(dir()).expect("open the child database");
            // The test harness writes "test crash_child_entrypoint ... "
            // without a newline, so start the id stream on a clean line.
            println!();
            for n in 0..5 {
                let id = db.insert(json!({"n": n, "text": format!("doc-{n}")}))
                    .expect("insert an acknowledged document");
                println!("ID {id}");
            }
            std::io::stdout().flush().expect("flush child stdout");
            std::process::exit(EXIT_CODE);
        }
        // A batch whose bytes reached the file but whose fsync never ran: the
        // process dies at the sync, so no caller was acked. Recovery must find
        // the complete batch and replay it (contract §2.2: an unacknowledged
        // write may appear; what must never happen is a half-applied batch).
        #[cfg(feature = "fault-io")]
        "ram-durable-not-synced" => {
            use ndb::io::{FaultAction, FaultIo, FaultPlan, Io, IoOp};
            use ndb::{Database, RamConfig};
            use serde_json::json;
            use std::io::Write;
            use std::sync::Arc;

            let io = Arc::new(FaultIo::new());
            let io_dyn: Arc<dyn Io> = io.clone();
            let db = Database::open_ram_with_io(dir(), RamConfig::default(), io_dyn)
                .expect("open the RAM database");
            println!();
            for n in 0..2 {
                let id = db
                    .insert(json!({"n": n, "text": format!("acked-{n}")}))
                    .expect("insert an acknowledged document");
                println!("ID {id}");
            }
            std::io::stdout().flush().expect("flush child stdout");

            let call = io.calls(IoOp::SyncAll) + 1;
            io.arm(FaultPlan {
                op: IoOp::SyncAll,
                trigger_at: call,
                action: FaultAction::Exit(EXIT_CODE),
            });
            // Its frames are written, then the process dies at the fsync.
            db.insert(json!({"n": 2, "text": "unacked-2"}))
                .expect("the child must die inside this insert, not return");
            panic!("child survived the armed RAM batch sync");
        }
        // A compaction interrupted at one exact durable step. The parent checks
        // the recovery outcome of the contract's C1/C2/C4/C5 rows.
        #[cfg(feature = "fault-io")]
        "ram-compaction-crash" => {
            use ndb::io::{FaultAction, FaultIo, FaultPlan, Io, IoOp};
            use ndb::{Database, RamConfig};
            use serde_json::json;
            use std::io::Write;
            use std::sync::Arc;

            let io = Arc::new(FaultIo::new());
            let io_dyn: Arc<dyn Io> = io.clone();
            let db = Database::open_ram_with_io(dir(), RamConfig::default(), io_dyn)
                .expect("open the RAM database");
            println!();
            for n in 0..3 {
                let id = db
                    .insert(json!({"n": n}))
                    .expect("insert an acknowledged document");
                println!("ID {id}");
            }
            std::io::stdout().flush().expect("flush child stdout");

            // Call offsets from the moment the plan is armed, in the exact order
            // `compact()` issues them:
            //   CreateNew  1 wal.gen_<N+1> · 2 manifest temp · 3 base temp
            //   Rename     1 pending manifest · 2 base · 3 publish manifest
            let (op, offset) = match point().as_str() {
                "rotate-manifest" => (IoOp::Rename, 1),
                "base-create" => (IoOp::CreateNew, 3),
                "publish-manifest" => (IoOp::Rename, 3),
                "sweep" => (IoOp::RemoveFile, 1),
                // The two "stalled" points fail the step instead of dying in
                // it, so the child can keep acknowledging writes into the
                // pending generation and *then* die. That is the durable state
                // the C2/C4 rows describe: base + WAL + a pending WAL that
                // holds writes no other generation knows about.
                "stalled-then-crash" => (IoOp::CreateNew, 3),
                "publish-failed-then-crash" => (IoOp::Rename, 3),
                other => panic!("unknown compaction crash point '{other}'"),
            };
            let stalled = point().ends_with("-then-crash");
            let trigger_at = io.calls(op) + offset;
            io.arm(FaultPlan {
                op,
                trigger_at,
                action: if stalled {
                    FaultAction::Error(std::io::ErrorKind::Other)
                } else {
                    FaultAction::Exit(EXIT_CODE)
                },
            });
            let outcome = db.compact();
            if !stalled {
                panic!("child survived the armed compaction crash at {op:?}: {outcome:?}");
            }
            outcome.expect_err("the stalled compaction must report its failure");
            io.clear_plans();
            for n in 3..6 {
                let id = db
                    .insert(json!({"n": n}))
                    .expect("insert into the pending WAL generation");
                println!("ID {id}");
            }
            std::io::stdout().flush().expect("flush child stdout");
            std::process::exit(EXIT_CODE);
        }
        other => panic!("unknown crash child role '{other}'"),
    }
}
