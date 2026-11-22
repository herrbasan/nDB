//! Phase 3 — Slice 3C: exhaustive Phase 16 verification of the v2 binary/mmap
//! storage engine against every Phase 3 exit gate.
//!
//! Every test works on a `tempfile::TempDir()`; nothing under
//! `D:\DEV\mcp_server\data` is ever opened for writing (the real-corpus test
//! copies the live journals first).
//!
//! Gates covered:
//!   1. torn WAL tail truncation + recovery, then a new write, then reopen
//!   2. mid-WAL corruption → loud `Error::Corruption` → `repair_wal_file` /
//!      `ndb repair` quarantine + truncate → reopen with exact survivors, `fsck`
//!   3. 8 group-commit writers + 2 readers + mid-flight non-blocking compaction
//!   4. bounded `change_ring` (>1,100 events) answered from the on-disk WAL
//!   5. persisted Hash + BTree indexes across a cold reopen (no rebuild)
//!   6. resumable `ndb migrate` (leftover `.migrating/`) + real Badkid corpus
//!      parity, lineage preservation and `fsck`
//!   7. multi-DB `ndb serve` HTTP end-to-end on two v2 databases

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use ndb::{
    Database, Error, OpenOptions, Persistence, StorageFormat, V2IndexKind, V2Manifest,
    V2_CHANGE_RING_CAP as CHANGE_RING_CAP,
};
use serde_json::{json, Value};
use tempfile::TempDir;

// ─── Fixtures ──────────────────────────────────────────────────────────────

fn open_v2(dir: &Path) -> Database {
    Database::open_v2_with_options(
        dir,
        OpenOptions {
            format: StorageFormat::V2,
            persistence: Persistence::Immediate,
        },
    )
    .expect("open v2")
}

fn manifest(dir: &Path) -> V2Manifest {
    ndb::read_manifest(dir).expect("read manifest")
}

fn wal_path(dir: &Path) -> PathBuf {
    ndb::wal_file_path(dir, manifest(dir).wal_gen)
}

fn fsck_clean(dir: &Path) -> ndb::FsckReport {
    let report = ndb::fsck_v2_dir(dir).unwrap_or_else(|e| panic!("fsck {}: {e}", dir.display()));
    assert_eq!(
        report.last_seq,
        report.base_seq + report.wal_frames as u64,
        "fsck must see one contiguous sequence per frame"
    );
    report
}

fn generation_files(dir: &Path) -> (Vec<String>, Vec<String>) {
    let mut bases = Vec::new();
    let mut wals = Vec::new();
    for entry in fs::read_dir(dir).expect("read db dir").flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("base.gen_") {
            bases.push(name);
        } else if name.starts_with("wal.gen_") {
            wals.push(name);
        }
    }
    bases.sort();
    wals.sort();
    (bases, wals)
}

fn ndb_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ndb"))
}

fn run_cli(args: &[&str]) -> Output {
    let output = Command::new(ndb_exe())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn ndb {args:?}: {e}"));
    if !output.status.success() {
        panic!(
            "ndb {args:?} failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

/// One decoded WAL frame: enough to corrupt the middle of a payload and to
/// locate every frame boundary without a second parser in the test.
struct Frame {
    offset: usize,
    body_len: usize,
    seq: u64,
}

fn wal_frames(path: &Path) -> Vec<Frame> {
    let bytes = fs::read(path).expect("read wal");
    let mut offset = ndb::v2_format::WAL_HEADER_LEN;
    let mut frames = Vec::new();
    while offset + 4 <= bytes.len() {
        let body_len =
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("len prefix")) as usize;
        if body_len < ndb::v2_format::MIN_FRAME_BODY as usize
            || offset + 4 + body_len + 4 > bytes.len()
        {
            break;
        }
        let body = &bytes[offset + 4..offset + 4 + body_len];
        let seq = u64::from_le_bytes(body[1..9].try_into().expect("frame seq"));
        frames.push(Frame {
            offset,
            body_len,
            seq,
        });
        offset += 4 + body_len + 4;
    }
    assert_eq!(offset, bytes.len(), "every byte of a clean WAL is one frame");
    frames
}

/// Flip one byte in the middle of a frame's payload — the CRC no longer
/// matches, which is exactly the mid-WAL corruption `ndb repair` hunts.
fn corrupt_frame_payload(path: &Path, index: usize) {
    let frames = wal_frames(path);
    let frame = frames.get(index).unwrap_or_else(|| {
        panic!("frame {index} does not exist (wal has {} frames)", frames.len())
    });
    let mut bytes = fs::read(path).expect("read wal");
    let payload_start = frame.offset + 4 + 1 + 8 + 2;
    let payload_len = frame.body_len - (1 + 8 + 2);
    assert!(payload_len > 0, "frame {index} carries no payload");
    let target = payload_start + payload_len / 2;
    bytes[target] ^= 0xFF;
    fs::write(path, bytes).expect("write corrupted wal");
}

// ─── HTTP client (std only, keep-alive) ────────────────────────────────────

struct Http {
    stream: TcpStream,
    pending: Vec<u8>,
}

impl Http {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connect ndb serve");
        stream.set_nodelay(true).expect("nodelay");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        Http {
            stream,
            pending: Vec::new(),
        }
    }

    fn call(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> (u16, Vec<u8>) {
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: ndb-test\r\nConnection: keep-alive\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str("\r\n");
        self.stream.write_all(head.as_bytes()).expect("write head");
        self.stream.write_all(body).expect("write body");
        self.stream.flush().expect("flush");

        let mut buf = std::mem::take(&mut self.pending);
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let status: u16 = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .and_then(|s| s.parse().ok())
                    .expect("status line");
                let length: usize = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.trim().parse().expect("content-length"))
                    .expect("response content-length");
                while buf.len() < end + 4 + length {
                    let mut chunk = [0u8; 64 * 1024];
                    let n = self.stream.read(&mut chunk).expect("read body");
                    assert!(n > 0, "connection closed mid-body");
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = buf[end + 4..end + 4 + length].to_vec();
                self.pending = buf[end + 4 + length..].to_vec();
                return (status, body);
            }
            let mut chunk = [0u8; 64 * 1024];
            let n = self.stream.read(&mut chunk).expect("read head");
            assert!(n > 0, "connection closed while reading head");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn json(&mut self, method: &str, path: &str, body: &Value) -> (u16, Value) {
        let encoded = serde_json::to_vec(body).expect("encode request");
        let (status, raw) = self.call(method, path, &encoded, &[]);
        let value: Value = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&raw).unwrap_or_else(|e| {
                panic!(
                    "{method} {path} -> {status}: non-JSON body {e}: {}",
                    String::from_utf8_lossy(&raw)
                )
            })
        };
        (status, value)
    }
}

// ─── Gate 1: torn WAL tail ─────────────────────────────────────────────────

#[test]
fn torn_wal_tail_truncates_and_the_next_write_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let mut ids = Vec::new();
    {
        let db = open_v2(dir.path());
        for i in 0..5 {
            ids.push(db.insert(json!({"kind": "torn", "i": i})).unwrap());
        }
    }

    let wal = wal_path(dir.path());
    let clean_len = fs::metadata(&wal).unwrap().len();
    // A partial frame: a length prefix followed by half a record and no CRC.
    let mut bytes = fs::read(&wal).unwrap();
    bytes.extend_from_slice(&[0x40, 0x00, 0x00, 0x00, 0x01, 0x07, 0x00, 0x00]);
    fs::write(&wal, &bytes).unwrap();
    assert!(fs::metadata(&wal).unwrap().len() > clean_len);

    let db = open_v2(dir.path());
    assert_eq!(db.len(), 5, "a torn tail never hides a committed document");
    assert_eq!(
        fs::metadata(&wal).unwrap().len(),
        clean_len,
        "reopening truncated the torn tail back to the last frame boundary"
    );
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(db.get(id).unwrap()["i"], json!(i));
    }

    // Immediately write again: the append must not land on corrupt bytes.
    let fresh = db.insert(json!({"kind": "fresh", "i": 99})).unwrap();
    drop(db);

    // The tail was truncated, so the WAL is now structurally clean.
    let report = fsck_clean(dir.path());
    assert_eq!(report.wal_frames, 6);

    let reopened = open_v2(dir.path());
    assert_eq!(reopened.len(), 6);
    assert_eq!(reopened.get(&fresh).unwrap()["i"], json!(99));
    assert_eq!(reopened.lineage().last_seq, 6);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(reopened.get(id).unwrap()["i"], json!(i));
    }
}

// ─── Gate 2: mid-WAL corruption, repair, fsck ──────────────────────────────

#[test]
fn mid_wal_corruption_fails_loud_then_repair_quarantines_and_recovers() {
    let dir = TempDir::new().unwrap();
    let mut ids = Vec::new();
    {
        let db = open_v2(dir.path());
        for i in 0..10 {
            ids.push(
                db.insert(json!({"kind": "survivor", "i": i, "body": format!("doc-{i}")}))
                    .unwrap(),
            );
        }
    }

    let wal = wal_path(dir.path());
    let base_seq = manifest(dir.path()).base_seq;
    let frames = wal_frames(&wal);
    assert_eq!(frames.len(), 10);
    assert_eq!(
        frames.iter().map(|frame| frame.seq).collect::<Vec<u64>>(),
        (1..=10).collect::<Vec<u64>>(),
        "frames are stamped with a contiguous sequence"
    );
    corrupt_frame_payload(&wal, 5);
    let backup_glob = || -> Vec<String> {
        fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".corrupt.") && n.ends_with(".bak"))
            .collect()
    };
    assert!(backup_glob().is_empty());

    // Opening must refuse: a hole in the middle of the WAL is exactly the
    // silent-data-loss shape the engine is not allowed to absorb.
    let err = match Database::open_v2(dir.path()) {
        Ok(_) => panic!("opening a mid-WAL-corrupt database must fail"),
        Err(err) => err,
    };
    match &err {
        Error::Corruption { message, .. } => {
            assert!(
                message.contains("frame") || message.contains("WAL"),
                "corruption message names the frame: {message}"
            );
        }
        other => panic!("expected Error::Corruption, got {other:?}"),
    }

    let stats = ndb::repair_wal_file(&wal, base_seq).expect("repair the corrupt WAL");
    assert_eq!(stats.frames_kept, 5, "frames before the corruption survive");
    assert!(stats.issue.is_some(), "the repair names the failing frame");
    assert!(stats.truncated_bytes > 0);
    let backup = stats.backup.expect("a quarantined backup");
    assert!(backup.exists(), "the original bytes are preserved");
    assert!(backup
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains(".corrupt."));
    assert!(backup
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(".bak"));

    // Reopen: exactly the frames before the corruption, byte-for-byte.
    let db = open_v2(dir.path());
    assert_eq!(db.len(), 5, "only the pre-corruption commits survive");
    assert_eq!(db.lineage().last_seq, 5);
    for (i, id) in ids.iter().enumerate().take(5) {
        let doc = db.get(id).expect("survivor present");
        assert_eq!(doc["i"], json!(i));
        assert_eq!(doc["body"], json!(format!("doc-{i}")));
    }
    for id in &ids[5..] {
        assert!(
            matches!(db.get(id).unwrap_err(), Error::NotFound { .. }),
            "documents past the corrupt frame are gone, not silently resurrected"
        );
    }
    drop(db);
    let report = fsck_clean(dir.path());
    assert_eq!(report.wal_frames, 5);

    // The published `ndb fsck` agrees.
    let fsck = run_cli(&["fsck", dir.path().to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&fsck.stdout);
    assert!(stdout.contains("Format: v2"), "{stdout}");
    assert!(stdout.contains("OK"), "{stdout}");

    // A second corruption round must be recoverable through the CLI tool.
    {
        let db = open_v2(dir.path());
        for i in 10..15 {
            db.insert(json!({"kind": "second-round", "i": i})).unwrap();
        }
    }
    assert_eq!(wal_frames(&wal).len(), 10);
    corrupt_frame_payload(&wal, 7);
    assert!(Database::open_v2(dir.path()).is_err());

    let repaired = run_cli(&["repair", dir.path().to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&repaired.stdout);
    assert!(stdout.contains("Original preserved at:"), "{stdout}");
    assert!(stdout.contains(".corrupt."), "{stdout}");
    assert!(!backup_glob().is_empty() || stdout.contains(".corrupt."));

    let db = open_v2(dir.path());
    assert_eq!(db.len(), 7, "5 first-round + 2 before the second corruption");
    assert_eq!(db.lineage().last_seq, 7);
    drop(db);
    fsck_clean(dir.path());
    run_cli(&["fsck", dir.path().to_str().unwrap()]);
}

// ─── Gate 3: group commit + readers + non-blocking compaction ──────────────

#[test]
fn concurrent_writers_readers_and_compaction_lose_nothing() {
    const WRITERS: usize = 8;
    const WRITES_PER_WRITER: usize = 50;
    let dir = TempDir::new().unwrap();
    let db = Arc::new(open_v2(dir.path()));
    // A seed document so readers always have something to fetch.
    db.insert(json!({"seed": true})).unwrap();

    let barrier = Arc::new(Barrier::new(WRITERS + 2 + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let patch_seqs: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

    let mut writers = Vec::new();
    for writer in 0..WRITERS {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let failures = Arc::clone(&failures);
        let patch_seqs = Arc::clone(&patch_seqs);
        writers.push(std::thread::spawn(move || {
            barrier.wait();
            for i in 0..WRITES_PER_WRITER {
                let id = match db.insert(json!({"writer": writer, "i": i})) {
                    Ok(id) => id,
                    Err(e) => {
                        failures.lock().unwrap().push(format!("writer {writer} insert {i}: {e}"));
                        return;
                    }
                };
                let result = db.apply_ops(
                    &id,
                    &[
                        ndb::PatchOp::Set {
                            path: "patched".to_string(),
                            value: json!(i),
                        },
                        ndb::PatchOp::ArrayPush {
                            path: "history".to_string(),
                            value: json!(writer),
                        },
                    ],
                    None,
                );
                match result {
                    Ok(applied) => patch_seqs.lock().unwrap().push(applied.seq),
                    Err(e) => {
                        failures
                            .lock()
                            .unwrap()
                            .push(format!("writer {writer} patch {i}: {e}"));
                        return;
                    }
                }
            }
        }));
    }

    let mut readers = Vec::new();
    for reader in 0..2 {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let failures = Arc::clone(&failures);
        let stop = Arc::clone(&stop);
        readers.push(std::thread::spawn(move || {
            barrier.wait();
            let mut rounds = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let ids = db.get_all_ids();
                match db.query(json!({})) {
                    Ok(all) => {
                        // Two separate calls, so a concurrent commit may add
                        // documents between them — never remove one.
                        if all.len() < ids.len() {
                            failures.lock().unwrap().push(format!(
                                "reader {reader}: query returned {} of {} documents",
                                all.len(),
                                ids.len()
                            ));
                            return;
                        }
                        for doc in &all {
                            let id = doc["_id"].as_str().expect("_id on every result");
                            if let Err(e) = db.get(id) {
                                failures
                                    .lock()
                                    .unwrap()
                                    .push(format!("reader {reader}: query result {id} unreadable: {e}"));
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        failures.lock().unwrap().push(format!("reader {reader} query: {e}"));
                        return;
                    }
                }
                if let Some(id) = ids.get(rounds % ids.len().max(1)) {
                    if let Err(e) = db.get(id) {
                        failures.lock().unwrap().push(format!("reader {reader} get: {e}"));
                        return;
                    }
                    if db.doc_seq(id).is_none() {
                        failures
                            .lock()
                            .unwrap()
                            .push(format!("reader {reader}: no doc_seq for live id"));
                        return;
                    }
                }
                rounds += 1;
            }
        }));
    }

    let compactor = {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        let failures = Arc::clone(&failures);
        std::thread::spawn(move || -> Option<(Duration, u64)> {
            barrier.wait();
            // Let the writers build a WAL backlog, then compact under load.
            std::thread::sleep(Duration::from_millis(25));
            let started = Instant::now();
            if let Err(e) = db.compact() {
                failures.lock().unwrap().push(format!("compact: {e}"));
                return None;
            }
            Some((started.elapsed(), db.lineage().last_seq))
        })
    };

    let (compact_elapsed, compact_seq) = compactor
        .join()
        .expect("compactor thread")
        .expect("compaction reported no error");
    for writer in writers {
        writer.join().expect("writer thread");
    }
    // Writers are done: let the readers observe their final state and exit.
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader.join().expect("reader thread");
    }

    let errors = failures.lock().unwrap().clone();
    assert!(errors.is_empty(), "concurrent run reported errors: {errors:#?}");
    assert!(compact_elapsed < Duration::from_secs(60));

    let total_docs = 1 + WRITERS * WRITES_PER_WRITER;
    let total_commits = 1 + WRITERS * WRITES_PER_WRITER * 2;
    assert_eq!(db.len(), total_docs, "no write lost");
    assert_eq!(
        db.lineage().last_seq,
        total_commits as u64,
        "one sequence per commit, no gaps and no duplicates"
    );
    assert!(compact_seq <= total_commits as u64);

    // Every patch was stamped with a distinct, in-range sequence.
    let mut seqs = patch_seqs.lock().unwrap().clone();
    seqs.sort_unstable();
    assert_eq!(seqs.len(), WRITERS * WRITES_PER_WRITER);
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "patch sequences are strictly monotonic and unique"
    );
    assert!(seqs[0] >= 1 && *seqs.last().unwrap() <= total_commits as u64);

    let ids = db.get_all_ids();
    assert_eq!(ids.len(), total_docs);
    let mut patched_docs = 0usize;
    for id in &ids {
        let doc = db.get(id).expect("live document");
        if doc.get("writer").is_some() {
            assert_eq!(doc["patched"], doc["i"], "the patch landed on its own doc");
            assert_eq!(doc["history"], json!([doc["writer"]]));
            patched_docs += 1;
        }
    }
    assert_eq!(patched_docs, WRITERS * WRITES_PER_WRITER);

    drop(db);
    let reopened = open_v2(dir.path());
    assert_eq!(reopened.len(), total_docs, "all final documents survive reopen");
    assert_eq!(reopened.lineage().last_seq, total_commits as u64);
    let report = fsck_clean(dir.path());
    assert_eq!(report.last_seq, total_commits as u64);
    assert!(
        report.doc_count <= total_docs as u64,
        "base doc_count counts the frozen base; live WAL overlays add the rest"
    );

    // Windows-safe unlink: only the current generation files remain.
    let manifest = manifest(dir.path());
    let (bases, wals) = generation_files(dir.path());
    assert_eq!(
        bases,
        vec![ndb::base_file_name(manifest.base_gen)],
        "superseded base generations are unlinked"
    );
    assert_eq!(
        wals,
        vec![ndb::wal_file_name(manifest.wal_gen)],
        "superseded WAL generations are unlinked"
    );
    assert!(manifest.base_gen > 1, "compaction published a new base");
}

// ─── Gate 4: bounded change ring + WAL fallback ────────────────────────────

#[test]
fn bounded_change_ring_answers_more_than_its_capacity_from_the_wal() {
    assert_eq!(CHANGE_RING_CAP, 1024);
    let dir = TempDir::new().unwrap();
    let db = open_v2(dir.path());

    let mut ids = Vec::new();
    for i in 0..400 {
        ids.push(db.insert(json!({"i": i})).unwrap());
    }
    // 800 further mutations on the same documents: 1,200 commits total, well
    // past the in-memory ring, and never compacted.
    for (n, id) in ids.iter().cycle().take(800).enumerate() {
        db.set(id, "counter", json!(n)).unwrap();
    }
    assert_eq!(db.lineage().last_seq, 1200);

    let batch = db.changes_since(0).unwrap();
    assert_eq!(batch.events.len(), 1200, "the WAL answers the full history");
    for (index, event) in batch.events.iter().enumerate() {
        assert_eq!(
            event.seq,
            index as u64 + 1,
            "events arrive in exact seq = 1..=1200 order"
        );
    }
    assert_eq!(batch.events[0].seq, 1);
    assert_eq!(batch.events[1199].seq, 1200);
    assert_eq!(batch.lineage.last_seq, 1200);

    // Cursors inside and after the ring both work.
    assert_eq!(db.changes_since(1023).unwrap().events.len(), 177);
    assert_eq!(db.changes_since(1199).unwrap().events.len(), 1);
    assert!(matches!(
        db.changes_since(1201).unwrap_err(),
        Error::CursorAhead { .. }
    ));

    // And the history survives a reopen: the ring is RAM, the WAL is truth.
    drop(db);
    let db = open_v2(dir.path());
    let batch = db.changes_since(0).unwrap();
    assert_eq!(batch.events.len(), 1200);
    assert_eq!(batch.events[1199].seq, 1200);
    assert_eq!(db.changes_since(0).unwrap().events[1].seq, 2);
}

// ─── Gate 5: persisted indexes across a cold reopen ────────────────────────

fn indexes_of(manifest: &V2Manifest) -> Vec<(String, V2IndexKind)> {
    manifest
        .indexes
        .iter()
        .map(|spec| (spec.field.clone(), spec.kind))
        .collect()
}

#[test]
fn persisted_hash_and_btree_indexes_survive_cold_reopen_without_rebuild() {
    let dir = TempDir::new().unwrap();
    let plain_dir = TempDir::new().unwrap();

    let mut ids = Vec::new();
    {
        let db = open_v2(dir.path());
        for i in 0..500 {
            ids.push(
                db.insert(json!({
                    "kind": format!("k{}", i % 10),
                    "score": i,
                    "bucket": (i * 7) % 23,
                    "name": format!("document-{i:03}"),
                    "even": i % 2 == 0,
                }))
                .unwrap(),
            );
        }
        db.create_index("kind").unwrap();
        db.create_btree_index("score").unwrap();
        db.compact().unwrap();
        assert_eq!(
            indexes_of(&manifest(dir.path())),
            vec![
                ("kind".to_string(), V2IndexKind::Hash),
                ("score".to_string(), V2IndexKind::BTree),
            ],
            "the manifest declares both index kinds"
        );
    }

    // The unindexed twin holds the same documents so results can be compared.
    {
        let plain = open_v2(plain_dir.path());
        for i in 0..500 {
            plain
                .insert(json!({
                    "kind": format!("k{}", i % 10),
                    "score": i,
                    "bucket": (i * 7) % 23,
                    "name": format!("document-{i:03}"),
                    "even": i % 2 == 0,
                }))
                .unwrap();
        }
    }

    // Cold reopen: no `create_index` call anywhere below.
    let indexed = open_v2(dir.path());
    assert!(indexed.has_index("kind"), "hash index is live on open");
    assert!(indexed.has_index("score"), "btree index is live on open");
    assert_eq!(indexed.len(), 500);
    let report = fsck_clean(dir.path());
    assert_eq!(report.index_count, 2);
    assert_eq!(
        indexes_of(&manifest(dir.path())),
        vec![
            ("kind".to_string(), V2IndexKind::Hash),
            ("score".to_string(), V2IndexKind::BTree),
        ]
    );

    let plain = open_v2(plain_dir.path());
    assert!(!plain.has_index("kind"));
    assert!(!plain.has_index("score"));

    // Ids are generated per database, so result sets are compared by their
    // content alone: `_id` is the only field that can differ.
    let sorted = |mut docs: Vec<Value>| -> Vec<Value> {
        for doc in docs.iter_mut() {
            doc.as_object_mut().expect("object").remove("_id");
        }
        docs.sort_by_key(|doc| doc["name"].as_str().unwrap_or("").to_string());
        docs
    };
    for kind in ["k0", "k3", "k9"] {
        let a = sorted(indexed.find("kind", &json!(kind)));
        let b = sorted(plain.find("kind", &json!(kind)));
        assert_eq!(a, b, "indexed find({kind}) matches the unindexed scan");
        assert_eq!(a.len(), 50);
    }
    for (lo, hi) in [(0, 49), (100, 249), (450, 499)] {
        let a = sorted(indexed.find_range("score", &json!(lo), &json!(hi)));
        let b = sorted(plain.find_range("score", &json!(lo), &json!(hi)));
        assert_eq!(a, b, "indexed find_range({lo},{hi}) matches the scan");
        assert_eq!(a.len(), hi - lo + 1);
    }

    // The planner path (`query`) must agree too, including a BTree range.
    let filter = json!({"$and": [{"kind": "k4"}, {"score": {"$gte": 200, "$lte": 399}}]});
    let a = sorted(indexed.query(filter.clone()).unwrap());
    let b = sorted(plain.query(filter).unwrap());
    assert_eq!(a, b);
    assert_eq!(a.len(), 20);

    // Writable after reopen: the restored indexes track new documents.
    let new_id = indexed.insert(json!({"kind": "k0", "score": 5000})).unwrap();
    assert_eq!(indexed.find("kind", &json!("k0")).len(), 51);
    assert_eq!(
        indexed.find_range("score", &json!(5000), &json!(5000)).len(),
        1
    );
    drop(indexed);
    let reopened = open_v2(dir.path());
    assert_eq!(reopened.len(), 501);
    assert!(reopened.get(&new_id).is_ok());
    // Chained through `ids` so the earlier list is not dead weight.
    assert_eq!(ids.len(), 500);
    fsck_clean(dir.path());
}

// ─── Gate 6a: resumable migration ──────────────────────────────────────────

#[test]
fn resumable_migration_clears_a_leftover_staging_dir() {
    let dir = TempDir::new().unwrap();
    let journal = dir.path().join("data.jsonl");
    let (epoch, last_seq, docs) = {
        let db = Database::open(&journal).expect("create v1 journal");
        for i in 0..50 {
            db.insert(json!({"kind": "v1-doc", "i": i, "nested": {"even": i % 2 == 0}}))
                .unwrap();
        }
        db.set(&db.get_all_ids()[0], "mutated", json!(true))
            .unwrap();
        let snapshot = db.snapshot_at();
        let lineage = db.lineage();
        (lineage.source_epoch_uuid, lineage.last_seq, snapshot.docs)
    };
    assert!(last_seq >= 51);

    // Simulate a crash mid-migration: a staging folder holding a half-written
    // base/WAL pair and a stale manifest.
    let staging = dir.path().join(".migrating");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("base.gen_1.ndb"), b"NDB2BASE-half-written").unwrap();
    fs::write(staging.join("wal.gen_1.ndb"), b"NDB2WAL\0half").unwrap();
    fs::write(staging.join("manifest.json"), b"{\"format_version\":2}").unwrap();

    let output = run_cli(&["migrate", dir.path().to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Migrated"), "{stdout}");
    assert!(stdout.contains(&format!("last_seq: {last_seq}")), "{stdout}");

    assert!(!staging.exists(), "the staging folder is cleared");
    assert!(
        dir.path().join("data.jsonl.v1.bak").exists(),
        "the v1 journal is retired, not deleted"
    );
    assert!(dir.path().join("manifest.json").exists());
    assert!(!journal.exists());

    let migrated = manifest(dir.path());
    assert_eq!(migrated.source_epoch_uuid, epoch, "lineage survives migration");
    assert_eq!(migrated.base_seq, last_seq);
    assert_eq!(migrated.doc_count, docs.len() as u64);

    let db = Database::open_v2(dir.path()).unwrap();
    assert_eq!(db.len(), docs.len());
    assert_eq!(db.lineage().source_epoch_uuid, epoch);
    assert_eq!(db.lineage().last_seq, last_seq);
    for (id, expected) in &docs {
        assert_eq!(
            &db.get(id).unwrap(),
            expected,
            "document {id} round-trips through migration"
        );
    }
    drop(db);

    let fsck = run_cli(&["fsck", dir.path().to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&fsck.stdout).contains("OK"));
    let report = fsck_clean(dir.path());
    assert_eq!(report.doc_count, docs.len() as u64);
    assert_eq!(report.last_seq, last_seq);

    // A second `ndb migrate` must refuse: the folder is v2 now.
    let again = Command::new(ndb_exe())
        .args(["migrate", dir.path().to_str().unwrap()])
        .output()
        .expect("spawn second migrate");
    assert!(!again.status.success(), "re-migrating a v2 folder must fail");
    assert!(String::from_utf8_lossy(&again.stderr).contains("already a v2 database"));
}

// ─── Gate 6b: real Badkid corpus parity ────────────────────────────────────

fn live_corpora() -> Vec<(&'static str, PathBuf)> {
    let root = PathBuf::from(
        std::env::var("NDB_TEST_MCP_DATA")
            .unwrap_or_else(|_| r"D:\DEV\mcp_server\data".to_string()),
    );
    [
        ("memories", r"memories\data.jsonl"),
        ("notepad", r"notepad\data.jsonl"),
        ("codereview", r"codereview\db\data.jsonl"),
    ]
    .into_iter()
    .map(|(name, rel)| (name, root.join(rel)))
    .collect()
}

#[test]
fn real_badkid_corpora_migrate_with_document_by_document_parity() {
    let corpora = live_corpora();
    let available: Vec<_> = corpora
        .iter()
        .filter(|(_, path)| path.is_file())
        .collect();
    if available.is_empty() {
        // Loud, but not a failure: the corpora live on the Badkid workstation.
        println!("SKIP: no live corpus found under D:\\DEV\\mcp_server\\data");
        return;
    }

    for (name, source) in available {
        let dir = TempDir::new().unwrap();
        let journal = dir.path().join("data.jsonl");
        fs::copy(source, &journal)
            .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", source.display(), journal.display()));
        let source_bytes = fs::metadata(&journal).unwrap().len();

        // The v1 truth, read once.
        let (epoch, last_seq, v1_docs) = {
            let db = Database::open(&journal).expect("open v1 corpus");
            let snapshot = db.snapshot_at();
            let lineage = db.lineage();
            (lineage.source_epoch_uuid, lineage.last_seq, snapshot.docs)
        };

        let started = Instant::now();
        let output = run_cli(&["migrate", dir.path().to_str().unwrap()]);
        let migrate_ms = started.elapsed().as_secs_f64() * 1000.0;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("Documents: {}", v1_docs.len())),
            "{name}: {stdout}"
        );
        println!(
            "{name}: {} docs, {:.1} MB v1 → migrated in {:.1} ms",
            v1_docs.len(),
            source_bytes as f64 / (1024.0 * 1024.0),
            migrate_ms
        );

        assert!(dir.path().join("data.jsonl.v1.bak").exists());
        let migrated = manifest(dir.path());
        assert_eq!(migrated.source_epoch_uuid, epoch, "{name}: epoch preserved");
        assert_eq!(migrated.base_seq, last_seq, "{name}: last_seq preserved");
        assert_eq!(migrated.doc_count, v1_docs.len() as u64);

        let db = Database::open_v2(dir.path())
            .unwrap_or_else(|e| panic!("{name}: open migrated v2: {e}"));
        assert_eq!(db.len(), v1_docs.len(), "{name}: doc count parity");
        let lineage = db.lineage();
        assert_eq!(lineage.source_epoch_uuid, epoch, "{name}: lineage epoch");
        assert_eq!(lineage.last_seq, last_seq, "{name}: lineage last_seq");

        let v2_ids: Vec<String> = db.get_all_ids();
        assert_eq!(v2_ids.len(), v1_docs.len());
        for (id, expected) in &v1_docs {
            let actual = db
                .get(id)
                .unwrap_or_else(|e| panic!("{name}: migrated db missing {id:?}: {e}"));
            assert_eq!(
                &actual, expected,
                "{name}: document {id:?} differs after migration"
            );
        }
        // Reopening keeps every document too (base, not a one-shot replay).
        drop(db);
        let reopened = Database::open_v2(dir.path()).unwrap();
        assert_eq!(reopened.len(), v1_docs.len(), "{name}: reopen parity");
        for (id, expected) in &v1_docs {
            assert_eq!(&reopened.get(id).unwrap(), expected, "{name}: reopen {id:?}");
        }
        drop(reopened);

        let fsck = run_cli(&["fsck", dir.path().to_str().unwrap()]);
        assert!(
            String::from_utf8_lossy(&fsck.stdout).contains("OK"),
            "{name}: fsck"
        );
        let report = fsck_clean(dir.path());
        assert_eq!(report.doc_count, v1_docs.len() as u64, "{name}: fsck doc count");
        assert_eq!(report.last_seq, last_seq, "{name}: fsck last_seq");
    }
}

// ─── Gate 7: multi-DB `ndb serve` HTTP E2E on v2 databases ─────────────────

#[test]
fn multi_db_v2_serve_http_end_to_end() {
    use ndb::server::{serve_multi_in_background, DbRegistry, PersistenceMode, ServerConfig};

    let root = TempDir::new().unwrap();
    for name in ["alpha", "beta"] {
        let db = open_v2(&root.path().join(name));
        db.insert(json!({"seeded": name})).unwrap();
    }

    let registry = Arc::new(DbRegistry::new(None, PersistenceMode::Immediate, 5_000));
    registry
        .mount_with("alpha", root.path().join("alpha"), PersistenceMode::Immediate)
        .expect("mount alpha");
    registry
        .mount_with("beta", root.path().join("beta"), PersistenceMode::Immediate)
        .expect("mount beta");
    let probe = Arc::clone(&registry);
    let handle = serve_multi_in_background(
        registry,
        ServerConfig {
            bind: "127.0.0.1".to_string(),
            port: 0,
            ..ServerConfig::default()
        },
    )
    .expect("start ndb serve");
    let addr = handle.addr;
    let mut http = Http::connect(addr);

    // Health lists both mounted v2 databases.
    let (status, body) = http.call("GET", "/health", b"", &[]);
    assert_eq!(status, 200);
    let health: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(health["dbs"], json!(["alpha", "beta"]));
    for name in ["alpha", "beta"] {
        let (status, db) = http.json("GET", &format!("/v2/dbs/{name}"), &Value::Null);
        assert_eq!(status, 200);
        assert_eq!(db["db"]["format"], json!("v2"), "{name} is served as v2");
        assert_eq!(db["db"]["documents"], json!(1));
    }

    // Idempotent index creation (repeated calls return created:false without
    // writing WAL frames or failing).
    let (status, idx1) = http.json("POST", "/v2/dbs/alpha/index", &json!({"field": "n", "type": "hash"}));
    assert_eq!(status, 200, "{idx1}");
    assert_eq!(idx1["created"], json!(true));
    let (status, idx2) = http.json("POST", "/v2/dbs/alpha/index", &json!({"field": "n", "type": "hash"}));
    assert_eq!(status, 200, "{idx2}");
    assert_eq!(idx2["created"], json!(false));
    let (status, idx3) = http.json("POST", "/v2/dbs/alpha/index", &json!({"field": "score", "type": "btree"}));
    assert_eq!(status, 200, "{idx3}");
    assert_eq!(idx3["created"], json!(true));
    let (status, idx4) = http.json("POST", "/v2/dbs/alpha/index", &json!({"field": "score", "type": "btree"}));
    assert_eq!(status, 200, "{idx4}");
    assert_eq!(idx4["created"], json!(false));

    // CRUD on alpha.
    let (status, created) = http.json("PUT", "/v2/dbs/alpha/doc", &json!({"n": 1, "kind": "a"}));
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let (status, fetched) = http.json(
        "GET",
        &format!("/v2/dbs/alpha/doc/{id}"),
        &Value::Null,
    );
    assert_eq!(status, 200);
    assert_eq!(fetched["doc"]["n"], json!(1));
    let doc_seq = created["seq"]
        .as_u64()
        .expect("insert reports the document seq");

    // Atomic multi-op PATCH guarded by if_seq.
    let patch = json!({"ops": [
        {"op": "set", "path": "n", "value": 2},
        {"op": "array_push", "path": "tags", "value": "x"}
    ]});
    let (status, applied) = http.json(
        "PATCH",
        &format!("/v2/dbs/alpha/doc/{id}?if_seq={doc_seq}"),
        &patch,
    );
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["doc"]["n"], json!(2));
    assert_eq!(applied["doc"]["tags"], json!(["x"]));
    let patched = applied["seq"].as_u64().unwrap();
    assert!(patched > doc_seq);
    let (status, conflict) = http.json(
        "PATCH",
        &format!("/v2/dbs/alpha/doc/{id}?if_seq={doc_seq}"),
        &patch,
    );
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(conflict["error"], json!("conflict"));
    assert_eq!(conflict["current_seq"], json!(patched));

    // Insert a second document and read both with mget.
    let (status, second) = http.json(
        "PUT",
        "/v2/dbs/alpha/doc",
        &json!({"n": 3, "kind": "a"}),
    );
    assert_eq!(status, 201);
    let second_id = second["id"].as_str().unwrap().to_string();
    let (status, mget) = http.json(
        "POST",
        "/v2/dbs/alpha/docs/mget",
        &json!({"ids": [id.clone(), second_id.clone(), "missing"]}),
    );
    assert_eq!(status, 200);
    assert_eq!(mget["docs"].as_object().unwrap().len(), 2);
    assert_eq!(mget["missing"], json!(["missing"]));

    // Rust-side query, with a projection.
    let (status, queried) = http.json(
        "POST",
        "/v2/dbs/alpha/query",
        &json!({"filter": {"kind": "a"}, "fields": ["n"], "sort": {"field": "n", "dir": "desc"}}),
    );
    assert_eq!(status, 200, "{queried}");
    assert_eq!(queried["total"], json!(2));
    let rows = queried["results"].as_array().unwrap();
    assert_eq!(rows[0]["n"], json!(3));
    assert_eq!(rows[1]["n"], json!(2));

    // Change feed from the beginning: 2 idempotent-checked IndexAdd frames
    // advanced last_seq to 6 without emitting bogus document delete events.
    let (status, changes) = http.json("GET", "/v2/dbs/alpha/changes?since=0", &Value::Null);
    assert_eq!(status, 200);
    assert_eq!(changes["ok"], json!(true));
    assert_eq!(changes["last_seq"], json!(6));
    assert_eq!(changes["changes"].as_array().unwrap().len(), 4);

    // Pinned snapshot pages come from one frozen state.
    let (status, first_page) = http.json("GET", "/v2/dbs/alpha/snapshot?limit=1", &Value::Null);
    assert_eq!(status, 200);
    let snapshot_id = first_page["snapshot_id"].as_str().expect("snapshot id").to_string();
    assert_eq!(first_page["total"], json!(3));
    let (status, second_page) = http.json(
        "GET",
        &format!("/v2/dbs/alpha/snapshot?snapshot_id={snapshot_id}&offset=1&limit=1"),
        &Value::Null,
    );
    assert_eq!(status, 200);
    assert_eq!(second_page["snapshot_id"], json!(snapshot_id));
    let (status, expired) = http.json(
        "GET",
        "/v2/dbs/alpha/snapshot?snapshot_id=does-not-exist&limit=1",
        &Value::Null,
    );
    assert_eq!(status, 410, "{expired}");

    // Files bucket: put, list, read, then GC an unreferenced blob.
    let (status, stored) = http.call(
        "PUT",
        "/v2/dbs/alpha/files/attachments?filename=note.txt",
        b"hello-bucket",
        &[("X-Filename", "note.txt")],
    );
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&stored));
    let stored: Value = serde_json::from_slice(&stored).unwrap();
    let ref_str = stored["ref"].as_str().unwrap().to_string();
    let hash = stored["hash"].as_str().unwrap().to_string();
    let ext = stored["ext"].as_str().unwrap().to_string();
    assert_eq!(stored["size"], json!(12));
    assert_eq!(ref_str, format!("attachments:{hash}.{ext}"));

    let (status, listed) = http.json("GET", "/v2/dbs/alpha/files/attachments", &Value::Null);
    assert_eq!(status, 200);
    assert_eq!(listed["files"], json!([format!("{hash}.{ext}")]));
    let (status, bytes) = http.call(
        "GET",
        &format!("/v2/dbs/alpha/files/attachments/{hash}.{ext}"),
        b"",
        &[],
    );
    assert_eq!(status, 200);
    assert_eq!(bytes, b"hello-bucket");

    let (status, swept) = http.json("POST", "/v2/dbs/alpha/files/gc", &Value::Null);
    assert_eq!(status, 200, "{swept}");
    assert_eq!(swept["moved"], json!(1), "the unreferenced blob is swept");
    let (status, _) = http.call(
        "GET",
        &format!("/v2/dbs/alpha/files/attachments/{hash}.{ext}"),
        b"",
        &[],
    );
    assert_eq!(status, 404);

    // Per-DB isolation: beta never sees alpha's documents.
    let (status, beta_docs) = http.json("POST", "/v2/dbs/beta/query", &json!({"filter": {}}));
    assert_eq!(status, 200);
    assert_eq!(beta_docs["total"], json!(1));
    let (status, alpha_docs) = http.json("POST", "/v2/dbs/alpha/query", &json!({"filter": {}}));
    assert_eq!(status, 200);
    assert_eq!(alpha_docs["total"], json!(3));

    // Compaction over HTTP keeps everything.
    let (status, compacted) = http.json("POST", "/v2/dbs/alpha/compact", &Value::Null);
    assert_eq!(status, 200, "{compacted}");
    let (status, stats) = http.json("GET", "/v2/dbs/alpha/stats", &Value::Null);
    assert_eq!(status, 200);
    assert_eq!(stats["documents"], json!(3));
    let (status, after) = http.json("GET", &format!("/v2/dbs/alpha/doc/{id}"), &Value::Null);
    assert_eq!(status, 200);
    assert_eq!(after["doc"]["n"], json!(2));

    handle.shutdown();

    // Everything above is durable on disk and fsck-clean (audited without the
    // writer lock the still-mounted registry holds).
    let alpha_report = fsck_clean(&root.path().join("alpha"));
    let beta_report = fsck_clean(&root.path().join("beta"));
    assert_eq!(alpha_report.last_seq, 6);
    assert_eq!(beta_report.last_seq, 1);
    // The registry's own handle sees the final state, and the on-disk base +
    // WAL carry it: 3 live documents for alpha, 1 for beta.
    let alpha = probe.get("alpha").unwrap();
    assert_eq!(alpha.len(), 3);
    assert_eq!(alpha.get(&id).unwrap()["tags"], json!(["x"]));
    assert_eq!(probe.get("beta").unwrap().len(), 1);
}
