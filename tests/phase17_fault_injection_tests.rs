//! Phase 17 harness smoke tests: the fault-injecting `Io` and the
//! out-of-process crash runner must be reliable on Windows before any engine
//! code depends on them.
#![cfg(feature = "fault-io")]

mod support;

use ndb::io::{FaultAction, FaultIo, FaultPlan, Io, IoOp};
use std::io::Read;
use std::path::Path;

/// The env-gated child payload. With no role set (a normal test run) this is
/// a no-op; with a role it never returns.
#[test]
fn crash_child_entrypoint() {
    if let Some(role) = support::crash_child::role() {
        support::crash_child::run(&role);
    }
}

fn read(path: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .expect("open file")
        .read_to_end(&mut bytes)
        .expect("read file");
    bytes
}

#[test]
fn fault_io_fails_every_durable_step_deterministically() {
    let dir = tempfile::tempdir().expect("temp dir");

    // Write: fails before any byte lands.
    let path = dir.path().join("wal.gen_0.ndb");
    let io = FaultIo::new();
    io.arm(FaultPlan {
        op: IoOp::WriteAll,
        trigger_at: 1,
        action: FaultAction::Error(std::io::ErrorKind::Other),
    });
    let mut file = io.create_new(&path).expect("create_new passes");
    let err = io
        .write_all(&mut file, b"never-written")
        .expect_err("write must fail");
    assert!(err.to_string().contains("injected WriteAll failure at call 1"));
    assert_eq!(read(&path), b"", "a failed write must leave no bytes");
    assert_eq!(io.calls(IoOp::WriteAll), 1);

    // Sync: the bytes are in the page cache but the durability call fails.
    let io = FaultIo::new();
    io.arm(FaultPlan {
        op: IoOp::SyncAll,
        trigger_at: 1,
        action: FaultAction::Error(std::io::ErrorKind::Other),
    });
    let mut file = io.create_new(&path.with_extension("sync")).expect("create");
    io.write_all(&mut file, b"cached").expect("write");
    let err = io.sync_all(&file).expect_err("sync must fail");
    assert!(err.to_string().contains("injected SyncAll failure at call 1"));

    // Rename.
    let io = FaultIo::new();
    io.arm(FaultPlan {
        op: IoOp::Rename,
        trigger_at: 1,
        action: FaultAction::Error(std::io::ErrorKind::Other),
    });
    let err = io
        .rename(&path.with_extension("sync"), &path.with_extension("renamed"))
        .expect_err("rename must fail");
    assert!(err.to_string().contains("injected Rename failure at call 1"));

    // Parent-directory fsync.
    let io = FaultIo::new();
    io.arm(FaultPlan {
        op: IoOp::SyncParentDir,
        trigger_at: 1,
        action: FaultAction::Error(std::io::ErrorKind::Other),
    });
    let err = io
        .sync_parent_dir(&path)
        .expect_err("directory sync must fail");
    assert!(err
        .to_string()
        .contains("injected SyncParentDir failure at call 1"));

    // Torn-tail truncation.
    let io = FaultIo::new();
    io.arm(FaultPlan {
        op: IoOp::TruncateAndSync,
        trigger_at: 1,
        action: FaultAction::Error(std::io::ErrorKind::Other),
    });
    let err = io
        .truncate_and_sync(&path, 0)
        .expect_err("truncate must fail");
    assert!(err
        .to_string()
        .contains("injected TruncateAndSync failure at call 1"));
}

#[test]
fn fault_io_passes_through_until_the_planned_call_and_after_clearing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("base.gen_0.ndb");
    let io = FaultIo::new();
    io.arm(FaultPlan {
        op: IoOp::WriteAll,
        trigger_at: 2,
        action: FaultAction::Error(std::io::ErrorKind::BrokenPipe),
    });
    let mut file = io.create_new(&path).expect("create");
    io.write_all(&mut file, b"first").expect("first write lands");
    let err = io
        .write_all(&mut file, b"second")
        .expect_err("second write fails");
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    io.clear_plans();
    io.write_all(&mut file, b"third").expect("cleared write lands");
    io.sync_all(&file).expect("sync passes");
    assert_eq!(read(&path), b"firstthird");
}

#[test]
fn child_dies_exactly_at_the_armed_sync() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = support::crash_child::spawn("fault-io-exit", dir.path());
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(support::crash_child::EXIT_CODE),
        "child must die at the armed exit; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("survived"),
        "the child must never reach the line after the armed operation: {stdout}"
    );
    assert_eq!(
        read(&dir.path().join("fault-probe.ndb")),
        b"durable-bytes",
        "the write before the crashed sync must really have happened"
    );
}

#[test]
fn abrupt_exit_keeps_every_acknowledged_write_recoverable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = support::crash_child::spawn("db-abrupt-exit", dir.path());
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(support::crash_child::EXIT_CODE),
        "child must exit abruptly, stdout={stdout} stderr={stderr}"
    );

    let acknowledged: Vec<String> = stdout
        .split_whitespace()
        .collect::<Vec<&str>>()
        .windows(2)
        .filter(|pair| pair[0] == "ID")
        .map(|pair| pair[1].to_string())
        .collect();
    assert_eq!(
        acknowledged.len(),
        5,
        "child must report all five acknowledged ids, got {acknowledged:?}"
    );

    // Recovery: a fresh process opens the same folder. The RAM engine fsyncs
    // every acknowledged batch, so none may be missing.
    let db = ndb::Database::open_ram(dir.path()).expect("reopen the crashed database");
    assert_eq!(db.len(), acknowledged.len());
    for (n, id) in acknowledged.iter().enumerate() {
        let doc = db.get(id).expect("acknowledged document survives the crash");
        assert_eq!(doc.get("n").and_then(|v| v.as_i64()), Some(n as i64));
        assert_eq!(
            doc.get("text").and_then(|v| v.as_str()),
            Some(format!("doc-{n}").as_str())
        );
    }
    assert_eq!(db.lineage().last_seq, 5);
    db.flush().expect("flush after recovery");
}
