//! Phase 2 (Slice 2A) — non-blocking compaction, pinned snapshots and the full
//! crash/fault matrix.
//!
//! Every row of the contract's §4 crash matrix is exercised against the real
//! durable files, and the two properties that cannot be observed from a single
//! process — "the process died here" and "nothing after this point ran" — are
//! simulated out-of-process by `support::crash_child`, exactly as the Phase 17
//! harness does.
//!
//! The non-blocking claim is tested the only way it can be: readers and writers
//! run continuously while `compact()` executes, and the reopened database must
//! match every acknowledged write.

#![cfg(feature = "fault-io")]

mod support;

use ndb::io::{FaultAction, FaultIo, FaultPlan, Io, IoOp, RealIo};
use ndb::ram_format::{
    base_file_path, read_ram_manifest, repair_ram_wal, scan_ram_wal, wal_file_path,
};
use ndb::{Database, Error, QueryOptions, RamConfig};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The env-gated child payload. With no role set (a normal test run) this is a
/// no-op; with a role it never returns.
#[test]
fn crash_child_entrypoint() {
    if let Some(role) = support::crash_child::role() {
        support::crash_child::run(&role);
    }
}

// ─── Harness ────────────────────────────────────────────────────────────────

fn truncating_config() -> RamConfig {
    RamConfig {
        change_ring_cap_events: 1_000_000,
        change_ring_cap_bytes: 64 * 1024 * 1024,
        ..RamConfig::default()
    }
}

fn open(dir: &Path) -> Database {
    Database::open_ram_with_config(dir, truncating_config()).expect("open a RAM database")
}

fn open_with(dir: &Path, io: &Arc<FaultIo>) -> Database {
    let io_dyn: Arc<dyn Io> = io.clone();
    Database::open_ram_with_io(dir, truncating_config(), io_dyn).expect("open over FaultIo")
}

fn open_expect_err(result: ndb::Result<Database>, what: &str) -> Error {
    match result {
        Ok(_) => panic!("{what}: the open must fail"),
        Err(error) => error,
    }
}

fn generations(dir: &Path) -> (u64, u64, Option<u64>) {
    let manifest = read_ram_manifest(dir).expect("read the manifest");
    (
        manifest.base_gen,
        manifest.wal_gen,
        manifest.pending_wal_gen,
    )
}

fn ids_of(stdout: &str) -> Vec<String> {
    stdout
        .split_whitespace()
        .collect::<Vec<&str>>()
        .windows(2)
        .filter(|pair| pair[0] == "ID")
        .map(|pair| pair[1].to_string())
        .collect()
}

fn assert_no_duplicate_seqs(db: &Database) {
    let lineage = db.lineage();
    assert_eq!(
        db.len() as u64,
        lineage.last_seq,
        "one document per sequence in this fixture"
    );
}

// ─── R0: torn tail ──────────────────────────────────────────────────────────

#[test]
fn r0_torn_tail_is_truncated_and_the_next_write_succeeds() {
    let dir = tempfile::tempdir().expect("temp dir");
    let wal_path = {
        let db = open(dir.path());
        db.insert(json!({"n": 1})).expect("insert");
        db.insert(json!({"n": 2})).expect("insert");
        let manifest = read_ram_manifest(dir.path()).expect("read the manifest");
        wal_file_path(dir.path(), manifest.wal_gen)
    };

    // An op frame with no terminating `Commit`: a plausible length prefix
    // followed by fewer bytes than it declares.
    let before = std::fs::metadata(&wal_path).expect("stat WAL").len();
    let mut bytes = 64u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0u8; 20]);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .expect("open the WAL for a torn append");
    file.write_all(&bytes).expect("write the torn tail");
    file.sync_all().expect("sync the torn tail");
    drop(file);
    assert_eq!(
        std::fs::metadata(&wal_path).expect("stat WAL").len(),
        before + bytes.len() as u64
    );

    let db = open(dir.path());
    assert_eq!(db.len(), 2, "earlier acknowledged batches are intact");
    assert_eq!(
        std::fs::metadata(&wal_path).expect("stat WAL").len(),
        before,
        "recovery truncates the torn tail back to the last intact Commit"
    );
    let scan = scan_ram_wal(&wal_path).expect("scan after recovery");
    assert_eq!(scan.torn_tail_offset, None, "the tail is clean again");

    db.insert(json!({"n": 3}))
        .expect("a write after truncation succeeds");
    drop(db);
    let db = open(dir.path());
    assert_eq!(db.len(), 3);
    assert_eq!(db.lineage().last_seq, 3);
}

// ─── R1: durable but unpublished batch ──────────────────────────────────────

#[test]
fn r1_crash_after_the_batch_write_before_the_sync_replays_the_batch() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = support::crash_child::spawn("ram-durable-not-synced", dir.path());
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(support::crash_child::EXIT_CODE),
        "the child must die at the batch sync; stdout={stdout} stderr={stderr}"
    );
    let acknowledged = ids_of(&stdout);
    assert_eq!(acknowledged.len(), 2, "stdout={stdout}");

    let db = open(dir.path());
    assert_eq!(
        db.len(),
        3,
        "the batch's bytes reached the file before the process died, so recovery replays it"
    );
    assert_eq!(db.lineage().last_seq, 3);
    for id in &acknowledged {
        db.get(id).expect("every acknowledged document survives");
    }
    db.insert(json!({"n": 3}))
        .expect("writes work again after recovery");
}

// ─── C1 … C5: the compaction crash matrix ───────────────────────────────────

fn compaction_crash(point: &str) -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = support::crash_child::spawn_at("ram-compaction-crash", dir.path(), Some(point));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(support::crash_child::EXIT_CODE),
        "child '{point}' must die at the armed compaction step; stdout={stdout} stderr={stderr}"
    );
    let ids = ids_of(&stdout);
    assert!(ids.len() >= 3, "the child must report its acknowledged ids: {stdout}");
    (dir, ids)
}

#[test]
fn c1_crash_before_the_pending_manifest_rename_keeps_the_old_generations() {
    let (dir, ids) = compaction_crash("rotate-manifest");
    assert_eq!(
        generations(dir.path()),
        (1, 1, None),
        "the old manifest is still authoritative"
    );
    assert!(
        wal_file_path(dir.path(), 2).exists(),
        "the rotated WAL header was created before the manifest named it"
    );

    let db = open(dir.path());
    assert_eq!(db.len(), 3, "zero lost writes");
    assert_eq!(db.lineage().last_seq, 3);
    for id in &ids {
        db.get(id).expect("document survives");
    }
    assert_eq!(
        generations(dir.path()),
        (1, 1, None),
        "the compaction did not happen"
    );
    assert!(
        !wal_file_path(dir.path(), 2).exists(),
        "the unreferenced empty generation is swept as an orphan"
    );
    db.insert(json!({"n": 3})).expect("writes still work");
}

#[test]
fn c2_crash_after_the_pending_manifest_rename_before_the_base_replays_three_generations() {
    let (dir, ids) = compaction_crash("base-create");
    assert_eq!(
        generations(dir.path()),
        (1, 1, Some(2)),
        "the manifest names the pending generation"
    );
    assert!(!base_file_path(dir.path(), 2).exists());

    let db = open(dir.path());
    assert_eq!(db.len(), 3, "zero lost writes across base + wal + pending");
    assert_eq!(db.lineage().last_seq, 3);
    for id in &ids {
        db.get(id).expect("document survives");
    }
    assert_eq!(
        generations(dir.path()),
        (1, 1, None),
        "recovery folds the pending generation into the active WAL"
    );
    assert!(
        !wal_file_path(dir.path(), 2).exists(),
        "the folded generation is swept"
    );
}

#[test]
fn c2_stalled_compaction_keeps_writes_acked_into_the_pending_generation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let io = Arc::new(FaultIo::new());
    let db = open_with(dir.path(), &io);
    for n in 0..3 {
        db.insert(json!({"n": n})).expect("insert");
    }

    // CreateNew 1 = wal.gen_2, 2 = manifest temp, 3 = base temp.
    let trigger_at = io.calls(IoOp::CreateNew) + 3;
    io.arm(FaultPlan {
        op: IoOp::CreateNew,
        trigger_at,
        action: FaultAction::Error(std::io::ErrorKind::Other),
    });
    let error = db.compact().expect_err("the injected base write must fail");
    assert!(matches!(error, Error::Io { .. }), "got {error:?}");
    io.clear_plans();
    assert_eq!(
        generations(dir.path()),
        (1, 1, Some(2)),
        "a failed compaction leaves the pending generation durable"
    );

    // Writers keep going, and their batches land in wal.gen_2.
    for n in 3..6 {
        db.insert(json!({"n": n}))
            .expect("writes continue into the pending generation");
    }
    assert_eq!(db.lineage().last_seq, 6);
    drop(db);

    let db = open(dir.path());
    assert_eq!(db.len(), 6, "zero lost writes");
    assert_eq!(db.lineage().last_seq, 6);
    assert_eq!(
        generations(dir.path()),
        (1, 1, None),
        "recovery folds the pending generation"
    );

    // The next compaction reuses the folded generation and publishes cleanly.
    db.compact().expect("compaction after a stall");
    assert_eq!(generations(dir.path()), (2, 2, None));
    assert_eq!(db.len(), 6);
    drop(db);
    assert_eq!(open(dir.path()).len(), 6);
}

#[test]
fn c2_a_stalled_compaction_plus_post_freeze_writes_survives_an_abrupt_exit() {
    let (dir, ids) = compaction_crash("stalled-then-crash");
    assert_eq!(ids.len(), 6, "three writes landed after the freeze");
    assert_eq!(
        generations(dir.path()),
        (1, 1, Some(2)),
        "the durable state is the contract's C2 row"
    );

    let db = open(dir.path());
    assert_eq!(db.len(), 6, "zero lost writes");
    assert_eq!(db.lineage().last_seq, 6);
    for id in &ids {
        db.get(id).expect("document survives");
    }
    assert_eq!(generations(dir.path()), (1, 1, None));
    assert!(!wal_file_path(dir.path(), 2).exists());
}

#[test]
fn c4_a_failed_publish_plus_post_freeze_writes_survives_an_abrupt_exit() {
    let (dir, ids) = compaction_crash("publish-failed-then-crash");
    assert_eq!(ids.len(), 6);
    assert_eq!(
        generations(dir.path()),
        (1, 1, Some(2)),
        "the publish swap never happened"
    );
    assert!(
        base_file_path(dir.path(), 2).exists(),
        "the new base is complete on disk"
    );

    let db = open(dir.path());
    assert_eq!(db.len(), 6, "zero lost writes");
    assert_eq!(db.lineage().last_seq, 6);
    for id in &ids {
        db.get(id).expect("document survives");
    }
    assert_eq!(generations(dir.path()), (1, 1, None));
    assert!(!base_file_path(dir.path(), 2).exists());
    assert!(!wal_file_path(dir.path(), 2).exists());
}

#[test]
fn c3_a_truncated_base_is_discarded_and_the_wal_generations_win() {
    let dir = tempfile::tempdir().expect("temp dir");
    let io = Arc::new(FaultIo::new());
    let db = open_with(dir.path(), &io);
    for n in 0..3 {
        db.insert(json!({"n": n})).expect("insert");
    }
    let trigger_at = io.calls(IoOp::CreateNew) + 3;
    io.arm(FaultPlan {
        op: IoOp::CreateNew,
        trigger_at,
        action: FaultAction::Error(std::io::ErrorKind::Other),
    });
    db.compact().expect_err("the injected base write must fail");
    io.clear_plans();
    for n in 3..5 {
        db.insert(json!({"n": n})).expect("insert into the pending WAL");
    }
    drop(db);

    // The worst case: a partial base under the final name, with a valid-looking
    // header (it is a truncated copy of a real base).
    let real = std::fs::read(base_file_path(dir.path(), 1)).expect("read base.gen_1");
    std::fs::write(base_file_path(dir.path(), 2), &real[..40]).expect("write a truncated base");
    assert!(base_file_path(dir.path(), 2).exists());

    let db = open(dir.path());
    assert_eq!(db.len(), 5, "the pending WAL's writes are replayed");
    assert_eq!(db.lineage().last_seq, 5);
    assert_no_duplicate_seqs(&db);
    assert!(
        !base_file_path(dir.path(), 2).exists(),
        "an incomplete compaction artefact is removed, never repaired"
    );
    assert_eq!(generations(dir.path()), (1, 1, None));
}

#[test]
fn c4_crash_before_the_publish_rename_replays_and_discards_the_complete_base() {
    let (dir, ids) = compaction_crash("publish-manifest");
    assert_eq!(
        generations(dir.path()),
        (1, 1, Some(2)),
        "the publish swap never happened"
    );
    assert!(
        base_file_path(dir.path(), 2).exists()
            && wal_file_path(dir.path(), 2).exists(),
        "the new base and WAL are complete on disk"
    );

    let db = open(dir.path());
    assert_eq!(db.len(), 3, "zero lost writes");
    assert_eq!(db.lineage().last_seq, 3);
    for id in &ids {
        db.get(id).expect("document survives");
    }
    assert_eq!(
        generations(dir.path()),
        (1, 1, None),
        "correctness does not depend on adopting the complete new base"
    );
    assert!(
        !base_file_path(dir.path(), 2).exists() && !wal_file_path(dir.path(), 2).exists(),
        "the unused generation is discarded"
    );
}

#[test]
fn c5_crash_after_the_publish_rename_loads_the_new_generation_and_sweeps_the_old() {
    let (dir, ids) = compaction_crash("sweep");
    assert_eq!(
        generations(dir.path()),
        (2, 2, None),
        "the publish swap is durable"
    );
    assert!(base_file_path(dir.path(), 2).exists());
    assert!(
        base_file_path(dir.path(), 1).exists() && wal_file_path(dir.path(), 1).exists(),
        "the sweep had not run yet"
    );

    let db = open(dir.path());
    assert_eq!(db.len(), 3, "zero lost writes");
    assert_eq!(db.lineage().last_seq, 3);
    for id in &ids {
        db.get(id).expect("document survives");
    }
    assert_eq!(generations(dir.path()), (2, 2, None));
    assert!(
        !base_file_path(dir.path(), 1).exists() && !wal_file_path(dir.path(), 1).exists(),
        "the superseded generation is swept on the next boot"
    );
}

// ─── C6 / C7: recovery is idempotent ────────────────────────────────────────

fn write_torn_tail(dir: &Path) {
    let manifest = read_ram_manifest(dir).expect("read the manifest");
    let path = wal_file_path(dir, manifest.wal_gen);
    let mut bytes = 64u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0u8; 20]);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open the WAL for a torn append");
    file.write_all(&bytes).expect("write the torn tail");
    file.sync_all().expect("sync the torn tail");
}

#[test]
fn c6_a_failed_tail_truncation_is_retried_and_recovers_cleanly() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let db = open(dir.path());
        db.insert(json!({"n": 1})).expect("insert");
        db.insert(json!({"n": 2})).expect("insert");
    }
    write_torn_tail(dir.path());
    let manifest = read_ram_manifest(dir.path()).expect("manifest");
    let path = wal_file_path(dir.path(), manifest.wal_gen);
    let before = std::fs::metadata(&path).expect("stat").len();

    let io = Arc::new(FaultIo::new());
    let trigger_at = io.calls(IoOp::TruncateAndSync) + 1;
    io.arm(FaultPlan {
        op: IoOp::TruncateAndSync,
        trigger_at,
        action: FaultAction::Error(std::io::ErrorKind::Other),
    });
    let io_dyn: Arc<dyn Io> = io.clone();
    let error = open_expect_err(
        Database::open_ram_with_io(dir.path(), truncating_config(), io_dyn),
        "a failed truncation must fail the open loudly",
    );
    assert!(matches!(error, Error::Io { .. }), "got {error:?}");
    assert_eq!(
        std::fs::metadata(&path).expect("stat").len(),
        before,
        "the failed truncation left the file untouched"
    );
    io.clear_plans();

    let db = open(dir.path());
    assert_eq!(db.len(), 2);
    assert!(
        std::fs::metadata(&path).expect("stat").len() < before,
        "the retried truncation removed the torn bytes"
    );
    db.insert(json!({"n": 3})).expect("writes work again");
}

#[test]
fn c7_two_reopens_over_a_stalled_compaction_agree_exactly() {
    let dir = tempfile::tempdir().expect("temp dir");
    let io = Arc::new(FaultIo::new());
    {
        let db = open_with(dir.path(), &io);
        for n in 0..3 {
            db.insert(json!({"n": n})).expect("insert");
        }
        let trigger_at = io.calls(IoOp::CreateNew) + 3;
        io.arm(FaultPlan {
            op: IoOp::CreateNew,
            trigger_at,
            action: FaultAction::Error(std::io::ErrorKind::Other),
        });
        db.compact().expect_err("stalled compaction");
        io.clear_plans();
        for n in 3..5 {
            db.insert(json!({"n": n})).expect("insert into the pending WAL");
        }
    }

    // The first reopen folds the pending generation (an on-disk side effect);
    // the second reruns pure replay over the folded state.
    let first = {
        let db = open(dir.path());
        (db.lineage(), db.iter(), db.get_all_ids())
    };
    let second = {
        let db = open(dir.path());
        (db.lineage(), db.iter(), db.get_all_ids())
    };
    assert_eq!(first.0, second.0, "identical lineage both times");
    assert_eq!(first.1, second.1, "identical documents both times");
    assert_eq!(first.2, second.2, "identical ids both times");
    assert_eq!(first.1.len(), 5, "zero lost writes");
}

// ─── Mid-log corruption: loud failure, quarantined repair ───────────────────

#[test]
fn mid_log_corruption_fails_loudly_and_repair_recovers_the_clean_prefix() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let db = open(dir.path());
        db.insert(json!({"n": 1})).expect("insert");
        db.insert(json!({"n": 2})).expect("insert");
        db.insert(json!({"n": 3})).expect("insert");
    }
    let manifest = read_ram_manifest(dir.path()).expect("manifest");
    let path = wal_file_path(dir.path(), manifest.wal_gen);
    let scan = scan_ram_wal(&path).expect("scan the clean WAL");
    assert_eq!(scan.batches.len(), 3, "one batch per insert");
    let corrupt_at =
        ((scan.batches[0].end_offset + scan.batches[1].end_offset) / 2) as usize;

    let mut bytes = std::fs::read(&path).expect("read the WAL");
    bytes[corrupt_at] ^= 0xFF;
    std::fs::write(&path, &bytes).expect("write the bit flip");

    let error = open_expect_err(
        Database::open_ram(dir.path()),
        "corruption must fail the open",
    );
    match &error {
        Error::Corruption { file, message } => {
            assert_eq!(file, &path, "the error names the damaged file");
            assert!(
                message.contains("CRC") || message.contains("declares"),
                "the error must localise the damage: {message}"
            );
        }
        other => panic!("expected Error::Corruption, got {other:?}"),
    }

    let report =
        repair_ram_wal(&path, &RealIo::new()).expect("repair quarantines and truncates");
    assert!(report.was_corrupt, "mid-log damage, not a torn tail");
    assert!(report.truncated_bytes > 0);
    assert!(
        report
            .quarantine_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false),
        "no byte is destroyed: the pre-repair file is kept"
    );

    let db = open(dir.path());
    assert_eq!(
        db.lineage().last_seq,
        report.last_valid_seq,
        "the clean prefix survives"
    );
    assert_eq!(db.len() as u64, report.last_valid_seq);
    db.insert(json!({"n": 99}))
        .expect("writes work after the repair");
}

#[test]
fn a_damaged_active_base_fails_loudly() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let db = open(dir.path());
        db.insert(json!({"n": 1})).expect("insert");
        db.compact().expect("compact");
        db.insert(json!({"n": 2})).expect("insert");
    }
    let (base_gen, _, _) = generations(dir.path());
    let base = base_file_path(dir.path(), base_gen);
    let mut bytes = std::fs::read(&base).expect("read the base");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&base, &bytes).expect("write the bit flip");

    let error = open_expect_err(
        Database::open_ram(dir.path()),
        "a damaged base must fail the open",
    );
    assert!(
        matches!(error, Error::Corruption { .. }),
        "got {error:?}"
    );
}

// ─── Non-blocking compaction under live load ────────────────────────────────

const SEEDED: usize = 40;
const PER_WRITER: usize = 60;
const WRITERS: usize = 2;
const READERS: usize = 4;

#[test]
fn compaction_runs_non_blocking_under_live_readers_and_writers() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Arc::new(open(dir.path()));
    for n in 0..SEEDED {
        db.insert(json!({"seeded": true, "n": n}))
            .expect("seed");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for reader in 0..READERS {
        let db = Arc::clone(&db);
        let stop = Arc::clone(&stop);
        readers.push(std::thread::spawn(move || {
            let mut observed = 0usize;
            while !stop.load(Ordering::SeqCst) {
                let page = db
                    .query_with(json!({"seeded": true}), QueryOptions::default())
                    .expect("query while compacting");
                assert_eq!(page.len(), SEEDED, "reader {reader}: the seeded set is stable");
                for doc in page.iter().take(3) {
                    let id = doc["_id"].as_str().expect("_id");
                    db.get(id).expect("a returned document stays readable");
                }
                observed += 1;
                std::thread::yield_now();
            }
            observed
        }));
    }

    let written = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let mut writers = Vec::new();
    for writer in 0..WRITERS {
        let db = Arc::clone(&db);
        let written = Arc::clone(&written);
        writers.push(std::thread::spawn(move || {
            for n in 0..PER_WRITER {
                let id = db
                    .insert(json!({"writer": writer, "n": n}))
                    .expect("insert while compacting");
                written.lock().push(id);
            }
        }));
    }

    db.compact().expect("first compaction under load");
    db.compact().expect("second compaction under load");
    for writer in writers {
        writer.join().expect("writer thread");
    }
    stop.store(true, Ordering::SeqCst);
    let mut observations = 0usize;
    for reader in readers {
        observations += reader.join().expect("reader thread");
    }
    assert!(observations > 0, "the readers must have observed something");

    let ids = written.lock().clone();
    assert_eq!(ids.len(), WRITERS * PER_WRITER);
    let last_seq = db.lineage().last_seq;
    assert_eq!(last_seq, (SEEDED + ids.len()) as u64);
    drop(db);

    let db = open(dir.path());
    assert_eq!(db.len(), SEEDED + ids.len(), "zero lost writes");
    assert_eq!(db.lineage().last_seq, last_seq);
    for id in &ids {
        db.get(id).expect("every acknowledged write survives reopen");
    }
}

// ─── Pinned snapshots: retention and backpressure ───────────────────────────

#[test]
fn a_pinned_snapshot_survives_compaction_and_blocks_the_memory_budget() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Database::open_ram_with_config(
        dir.path(),
        RamConfig {
            max_memory_bytes: Some(512 * 1024),
            change_ring_cap_events: 1_000_000,
            change_ring_cap_bytes: 64 * 1024 * 1024,
            auto_compact_wal_bytes: None,
        },
    )
    .expect("open with a budget");

    let id = db
        .insert(json!({"pad": "p".repeat(2_000)}))
        .expect("insert the pinned document");
    let (pin_id, lineage, docs) = db
        .pin_snapshot(Duration::from_secs(120))
        .expect("pin")
        .expect("a RAM database pins");
    assert_eq!(docs.len(), 1);
    assert_eq!(lineage.last_seq, 1);
    let cut_seq = lineage.last_seq;

    db.compact().expect("compaction under a pin");
    assert!(
        db.lineage().min_available_seq <= cut_seq,
        "the pin's ring reservation outranks the compaction floor"
    );
    let batch = db.changes_since(cut_seq - 1).expect("the pinned seq is retained");
    assert!(
        batch.events.iter().any(|event| event.seq == cut_seq),
        "compaction never evicts a pinned ring entry"
    );

    // Updates are patch events: each retains a bounded old root, so a ring with
    // a pin at its front grows until the write is refused.
    let mut refusal = None;
    for n in 0..2_000 {
        match db.set(&id, "n", json!(n)) {
            Ok(_) => {}
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }
    let error = refusal.expect("a pinned ring must eventually exhaust the budget");
    assert!(
        matches!(error, Error::MemoryLimitExceeded { .. }),
        "got {error:?}"
    );
    assert!(
        db.changes_since(cut_seq - 1).is_ok(),
        "the pinned history is still readable under pressure"
    );
    assert_eq!(
        db.ram_stats().expect("stats").active_pins,
        1,
        "the pin is still alive"
    );

    // The frozen page keeps the pre-update image, while live state moved on.
    let page = db.pinned_page(pin_id, 0, Some(10)).expect("page the pin");
    assert_eq!(page.total, 1);
    assert_eq!(page.lineage.last_seq, cut_seq);
    assert!(
        page.docs[0].get("n").is_none(),
        "a pin is frozen: later writes do not leak into it"
    );
    assert!(db.get(&id).expect("live doc").get("n").is_some());

    // Expiry is reaped on the timer's behalf, not on access.
    db.pin_snapshot(Duration::from_millis(20))
        .expect("pin")
        .expect("a RAM database pins");
    std::thread::sleep(Duration::from_millis(60));
    assert!(
        db.reap_expired_pins() >= 1,
        "an expired pin is reaped without being accessed"
    );
    assert!(
        db.pinned_page(pin_id, 0, Some(10)).is_some(),
        "the long-lived pin is untouched"
    );

    db.release_pin(pin_id).expect("release the pin");
    assert!(db.pinned_page(pin_id, 0, Some(10)).is_none());
    db.set(&id, "last", json!(true))
        .expect("the freed ring makes room for the write again");
    assert_eq!(db.get(&id).expect("doc")["last"], json!(true));
}

// ─── HTTP: pinned snapshots on a RAM-first database ─────────────────────────

mod http {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;

    fn request(port: u16, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let bytes = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        let mut req = format!(
            "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
            method,
            path,
            bytes.len()
        );
        if body.is_some() {
            req.push_str("Content-Type: application/json\r\n");
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).expect("write head");
        stream.write_all(&bytes).expect("write body");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("read response");
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header/body separator");
        let head = String::from_utf8_lossy(&response[..split]).to_string();
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .expect("status code")
            .parse()
            .expect("numeric status");
        (
            status,
            serde_json::from_slice(&response[split + 4..]).unwrap_or(Value::Null),
        )
    }

    pub fn get(port: u16, path: &str) -> (u16, Value) {
        request(port, "GET", path, None)
    }

    pub fn put(port: u16, path: &str, body: &Value) -> (u16, Value) {
        request(port, "PUT", path, Some(body))
    }
}

#[test]
fn http_snapshot_pages_a_pinned_arc_snapshot_on_a_ram_database() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let db = open(dir.path());
        for n in 0..5 {
            db.insert(json!({"n": n})).expect("insert");
        }
    }
    let registry = ndb::server::DbRegistry::new(
        None,
        ndb::server::PersistenceMode::Immediate,
        5000,
    );
    registry.mount("ram", dir.path()).expect("mount");
    assert!(registry.get("ram").expect("mounted").is_ram());
    let handle = ndb::server::serve_multi_in_background(
        Arc::new(registry),
        ndb::server::ServerConfig {
            port: 0,
            ..ndb::server::ServerConfig::default()
        },
    )
    .expect("start the server");
    let port = handle.addr.port();

    let (status, full) = http::get(port, "/v2/dbs/ram/snapshot");
    assert_eq!(status, 200, "{full}");
    assert_eq!(full["total"], 5);
    assert_eq!(full["snapshot_id"], Value::Null, "an unpaginated read is not pinned");

    let (status, first) = http::get(port, "/v2/dbs/ram/snapshot?offset=0&limit=2");
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["total"], 5);
    assert_eq!(first["count"], 2);
    let snapshot_id = first["snapshot_id"].as_str().expect("pinned").to_string();
    let watermark = first["watermark_seq"].clone();

    // A write lands between the pages; the pin must not move.
    let (status, _) = http::put(port, "/v2/dbs/ram/doc", &json!({"n": 99}));
    assert_eq!(status, 201);

    let (status, second) = http::get(
        port,
        &format!("/v2/dbs/ram/snapshot?snapshot_id={snapshot_id}&offset=2&limit=2"),
    );
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["total"], 5, "the pinned set is frozen");
    assert_eq!(second["watermark_seq"], watermark);
    assert_eq!(second["count"], 2);

    // Pages do not overlap, and the write that landed after the pin is absent.
    let (status, third) = http::get(
        port,
        &format!("/v2/dbs/ram/snapshot?snapshot_id={snapshot_id}&offset=4&limit=2"),
    );
    assert_eq!(status, 200, "{third}");
    assert_eq!(third["count"], 1);
    let mut seen: Vec<String> = Vec::new();
    for body in [&first, &second, &third] {
        for doc in body["docs"].as_array().expect("docs array") {
            assert_ne!(doc["n"], json!(99), "the post-pin write is not in the snapshot");
            seen.push(doc["_id"].as_str().expect("_id").to_string());
        }
    }
    assert_eq!(seen.len(), 5);
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 5, "the pinned pages are disjoint and complete");

    let (status, expired) =
        http::get(port, "/v2/dbs/ram/snapshot?snapshot_id=not-a-snapshot&offset=0&limit=1");
    assert_eq!(status, 410, "{expired}");
    assert_eq!(expired["error"], "snapshot_expired");

    handle.shutdown();
}

#[test]
fn the_registry_reaper_releases_expired_engine_pins() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let db = open(dir.path());
        db.insert(json!({"n": 1})).expect("insert");
    }
    let registry =
        ndb::server::DbRegistry::new(None, ndb::server::PersistenceMode::Immediate, 5000);
    registry.mount("ram", dir.path()).expect("mount");
    let db = registry.get("ram").expect("mounted");
    assert!(db.is_ram());

    // A pin the timer must release without anyone touching it. The HTTP route
    // pins for 300 s; this stand-in proves the reap path the timer drives.
    db.pin_snapshot(Duration::from_millis(20))
        .expect("pin")
        .expect("a RAM database pins");
    assert_eq!(db.ram_stats().expect("stats").active_pins, 1);
    std::thread::sleep(Duration::from_millis(60));

    registry.reap_expired_pins();
    assert_eq!(
        db.ram_stats().expect("stats").active_pins,
        0,
        "an idle server's reaper drops an expired pin"
    );
}

// ─── WAL-growth auto-compaction ─────────────────────────────────────────────
#[test]
fn wal_growth_triggers_background_compaction() {
    for round in 0..4 {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = Database::open_ram_with_config(
            dir.path(),
            RamConfig {
                auto_compact_wal_bytes: Some(2 * 1024),
                ..truncating_config()
            },
        )
        .expect("open with background compaction");

        for n in 0..100 {
            db.insert(json!({"n": n, "text": "x".repeat(64)}))
                .expect("insert");
        }

        let deadline = Instant::now() + Duration::from_secs(30);
        while read_ram_manifest(dir.path())
            .expect("read the manifest")
            .base_gen
            <= 1
        {
            assert!(
                Instant::now() < deadline,
                "round {round}: the background compactor never rotated the base generation"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(db.len(), 100, "compaction never loses a document");
        assert_eq!(db.lineage().last_seq, 100);

        // Drop while the compactor may be holding its own `Arc`: the teardown
        // must stop the thread, from whichever thread observes the last
        // reference, without deadlocking on a self-join.
        for n in 100..140 {
            db.insert(json!({"n": n})).expect("insert");
        }
        drop(db);

        let db = open(dir.path());
        assert_eq!(db.len(), 140, "round {round}: zero lost writes");
        assert_eq!(db.lineage().last_seq, 140);
        assert_eq!(db.iter().len(), 140);
    }
}
