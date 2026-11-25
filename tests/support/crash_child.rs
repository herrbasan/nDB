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
/// Exit code every crashing role uses, so the parent can tell "died where we
/// aimed" from "died for another reason".
pub const EXIT_CODE: i32 = 42;

pub fn role() -> Option<String> {
    std::env::var(ROLE_ENV).ok()
}

pub fn dir() -> PathBuf {
    PathBuf::from(
        std::env::var(DIR_ENV).expect("crash child started without NDB_CRASH_CHILD_DIR"),
    )
}

/// Spawn this test binary as a crash child. The caller must have created `dir`.
pub fn spawn(role: &str, dir: &Path) -> Output {
    let exe = std::env::current_exe().expect("resolve the current test executable");
    Command::new(exe)
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
        .stderr(Stdio::piped())
        .output()
        .expect("spawn the crash child")
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
            use ndb::{Database, OpenOptions, Persistence, StorageFormat};
            use std::io::Write;
            use serde_json::json;

            let db = Database::open_v2_with_options(
                dir(),
                OpenOptions {
                    format: StorageFormat::V2,
                    persistence: Persistence::Immediate,
                },
            )
            .expect("open the child database");
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
        other => panic!("unknown crash child role '{other}'"),
    }
}
