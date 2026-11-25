//! Phase 20: v3 (RAM-first) CLI tooling and the open maintenance issues.
//!
//! Covers `ndb init --ram`, `ndb migrate --ram` from v1 and v2 (doc-by-doc
//! parity, lineage, indexes, source backup), `ndb fsck`, `ndb repair` and
//! `ndb export --jsonl` on v3 folders, plus the regressions for
//! #11 (`gc_buckets` vs `items.jsonl`), #12 (`ndb verify` and compact refs) and
//! #14 (trash TTL uses residence time, not file mtime).

use ndb::ram_format::{
    base_file_name, base_file_path, read_ram_manifest, wal_file_name, wal_file_path,
    RAM_STORAGE_VERSION,
};
use ndb::Database;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

/// sha256("hello world") — a known vector, so the test never hashes through the
/// engine it is checking.
const HELLO_SHA256: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

// ─── CLI harness ────────────────────────────────────────────────────────────

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

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("utf-8 temp path")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs()
}

fn set_age(path: &Path, age: Duration) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    file.set_modified(SystemTime::now() - age)
        .unwrap_or_else(|e| panic!("set mtime of {}: {e}", path.display()));
}

// ─── `ndb init --ram` ───────────────────────────────────────────────────────

#[test]
fn init_ram_creates_a_complete_v3_folder() {
    let parent = TempDir::new().unwrap();
    let dir = parent.path().join("ram_db");


    let out = run_cli(&["init", path_str(&dir), "--ram"]);
    assert!(
        stdout_of(&out).contains("v3 (RAM-first)"),
        "{}",
        stdout_of(&out)
    );

    assert!(dir.join("manifest.json").is_file());
    assert!(dir.join(base_file_name(1)).is_file(), "base.gen_1.ndb");
    assert!(dir.join(wal_file_name(1)).is_file(), "wal.gen_1.ndb");
    assert!(dir.join("meta.json").is_file());
    assert!(dir.join("_files").is_dir());
    assert!(dir.join("_trash").is_dir());
    assert!(
        !dir.join("data.jsonl").exists(),
        "a v3 folder never writes a v1 journal"
    );

    let manifest = read_ram_manifest(&dir).expect("read v3 manifest");
    assert_eq!(manifest.version, RAM_STORAGE_VERSION);
    assert_eq!(manifest.base_gen, 1);
    assert_eq!(manifest.wal_gen, 1);
    assert_eq!(manifest.last_seq, 0);
    assert_eq!(manifest.min_available_seq, 1);
    assert!(manifest.indexes.is_empty());
    assert!(!manifest.source_epoch_uuid.is_empty());

    // It opens, accepts a write, and survives a reopen.
    {
        let db = Database::open_ram(&dir).expect("open the fresh v3 database");
        db.insert(json!({"boot": true})).unwrap();
    }
    let db = Database::open_ram(&dir).expect("reopen the v3 database");
    assert_eq!(db.len(), 1);

    // fsck and info both understand v3.
    let fsck = run_cli(&["fsck", path_str(&dir)]);
    let text = stdout_of(&fsck);
    assert!(text.contains("Format: v3 (RAM-first)"), "{text}");
    assert!(text.contains("OK"), "{text}");
    let info = run_cli(&["info", path_str(&dir)]);
    assert!(stdout_of(&info).contains("Format: v3 (RAM-first)"));
}

#[test]
fn init_accepts_v3_as_an_alias_and_refuses_the_retired_v2_flag() {
    let parent = TempDir::new().unwrap();
    let dir = parent.path().join("alias_db");
    run_cli(&["init", path_str(&dir), "--v3"]);
    assert_eq!(
        read_ram_manifest(&dir).expect("v3 manifest").version,
        RAM_STORAGE_VERSION
    );

    let other = parent.path().join("retired_db");
    let out = Command::new(ndb_exe())
        .args(["init", path_str(&other), "--v2"])
        .output()
        .expect("spawn ndb init");
    assert!(!out.status.success());
    let stderr = stderr_of(&out);
    assert!(stderr.contains("retired binary/mmap backend"), "{stderr}");
    assert!(!other.exists(), "a refused init creates nothing");
}

// ─── `ndb migrate --ram` from v1 ────────────────────────────────────────────

/// A v1 fixture with updates, deletes (so seq gaps exist) and a bucket file.
fn build_v1_fixture(dir: &Path) -> (String, u64, Vec<(String, Value)>) {
    fs::write(
        dir.join("meta.json"),
        serde_json::to_string(&json!({"buckets": {"attachments": {}}})).unwrap(),
    )
    .unwrap();
    let db = Database::open(dir.join("data.jsonl")).expect("create v1 journal");
    let stored = db
        .bucket("attachments")
        .store("note.txt", b"bucket payload", "text/plain")
        .unwrap();
    for i in 0..40 {
        db.insert(json!({
            "kind": if i % 2 == 0 { "even" } else { "odd" },
            "i": i,
            "nested": {"tags": ["a", "b"], "flag": i % 3 == 0},
        }))
        .unwrap();
    }
    let ids = db.get_all_ids();
    db.set(&ids[0], "mutated", json!(true)).unwrap();
    db.update(&ids[1], json!({"replaced": true, "i": 1})).unwrap();
    db.delete(&ids[2]).unwrap();
    db.delete(&ids[3]).unwrap();
    db.insert(json!({
        "attachment": stored._file.to_string_compact(),
        "url_form": format!("/files/attachments/{}", stored._file.filename()),
    }))
    .unwrap();

    let lineage = db.lineage();
    let snapshot = db.snapshot_at();
    assert!(lineage.last_seq >= 45);
    (
        lineage.source_epoch_uuid,
        lineage.last_seq,
        snapshot.docs,
    )
}

#[test]
fn migrate_v1_to_ram_preserves_every_document_and_backs_up_the_journal() {
    let dir = TempDir::new().unwrap();
    let (epoch, last_seq, docs) = build_v1_fixture(dir.path());

    let out = run_cli(&["migrate", path_str(dir.path()), "--ram"]);
    let text = stdout_of(&out);
    assert!(text.contains("to v3 (RAM-first)"), "{text}");
    assert!(text.contains(&format!("last_seq: {last_seq}")), "{text}");

    // v3 artefacts, v1 journal retired but preserved.
    assert!(dir.path().join("manifest.json").is_file());
    assert!(dir.path().join(base_file_name(1)).is_file());
    assert!(dir.path().join(wal_file_name(1)).is_file());
    assert!(!dir.path().join("data.jsonl").exists());
    assert!(dir.path().join("data.jsonl.v1.bak").is_file());

    let manifest = read_ram_manifest(dir.path()).expect("v3 manifest");
    assert_eq!(manifest.version, RAM_STORAGE_VERSION);
    assert_eq!(manifest.source_epoch_uuid, epoch, "epoch survives migration");
    assert_eq!(manifest.last_seq, last_seq);
    assert_eq!(manifest.base_gen, 1);
    assert_eq!(manifest.wal_gen, 1);
    assert!(manifest.indexes.is_empty(), "v1 persists no index definitions");

    // Doc-by-doc parity through the library.
    let db = Database::open_ram(dir.path()).expect("open the migrated database");
    assert_eq!(db.len(), docs.len());
    assert_eq!(db.lineage().source_epoch_uuid, epoch);
    assert_eq!(db.lineage().last_seq, last_seq);
    for (id, expected) in &docs {
        assert_eq!(
            &db.get(id).unwrap_or_else(|e| panic!("missing {id:?}: {e}")),
            expected,
            "document {id} round-trips through the v1 → v3 migration"
        );
    }
    drop(db);

    // The bucket payload survived untouched.
    assert!(dir
        .path()
        .join("_files")
        .join("attachments")
        .read_dir()
        .expect("bucket dir")
        .any(|e| e.expect("entry").path().is_file()));

    let fsck = run_cli(&["fsck", path_str(dir.path())]);
    let fsck_text = stdout_of(&fsck);
    assert!(fsck_text.contains("Format: v3 (RAM-first)"), "{fsck_text}");
    assert!(fsck_text.contains(&format!("last_seq: {last_seq}")), "{fsck_text}");
    assert!(fsck_text.contains("OK"), "{fsck_text}");

    // Re-running against the now-v3 folder is a clean refusal.
    let again = Command::new(ndb_exe())
        .args(["migrate", path_str(dir.path()), "--ram"])
        .output()
        .expect("spawn second migrate");
    assert!(!again.status.success());
    assert!(
        stderr_of(&again).contains("already a v3 (RAM-first) database"),
        "{}",
        stderr_of(&again)
    );
}

#[test]
fn migrate_to_ram_accepts_the_to_flag_and_rejects_impossible_targets() {
    let dir = TempDir::new().unwrap();
    let (epoch, last_seq, docs) = build_v1_fixture(dir.path());

    let out = run_cli(&["migrate", path_str(dir.path()), "--to", "v3"]);
    assert!(stdout_of(&out).contains("to v3 (RAM-first)"));
    let manifest = read_ram_manifest(dir.path()).unwrap();
    assert_eq!(manifest.source_epoch_uuid, epoch);
    assert_eq!(manifest.last_seq, last_seq);
    let db = Database::open_ram(dir.path()).unwrap();
    assert_eq!(db.len(), docs.len());

    let other = TempDir::new().unwrap();
    build_v1_fixture(other.path());
    let bad = Command::new(ndb_exe())
        .args(["migrate", path_str(other.path()), "--to", "auto"])
        .output()
        .expect("spawn migrate");
    assert!(!bad.status.success());
    assert!(stderr_of(&bad).contains("--to accepts ram (v3) only"));
    assert!(
        other.path().join("data.jsonl").exists(),
        "a refused migration leaves the source untouched"
    );
}

// ─── v2 mmap retirement ───────────────────────────────────────────

#[test]
fn migrate_refuses_a_v2_mmap_folder_and_points_at_the_transition_binary() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_string_pretty(&json!({
            "format_version": 2,
            "base_gen": 1,
            "wal_gen": 1,
            "source_epoch_uuid": "00000000-0000-4000-8000-000000000000",
            "base_seq": 0,
            "min_available_seq": 1,
            "doc_count": 0,
            "indexes": [],
        }))
        .unwrap(),
    )
    .unwrap();

    let out = Command::new(ndb_exe())
        .args(["migrate", path_str(dir.path())])
        .output()
        .expect("spawn migrate");
    assert!(!out.status.success());
    let stderr = stderr_of(&out);
    assert!(stderr.contains("engine was retired"), "{stderr}");
    assert!(stderr.contains("v2/transition binary"), "{stderr}");

    // The library refuses the same folder instead of degrading silently.
    let error = match Database::open(dir.path()) {
        Ok(_) => panic!("a v2 folder must not open"),
        Err(e) => e,
    };
    assert!(
        error.to_string().contains("engine was retired"),
        "unexpected error: {error}"
    );
}

#[test]
fn migrate_without_a_flag_targets_v3_ram_first() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    for i in 0..5 {
        db.insert(json!({"i": i})).unwrap();
    }
    drop(db);

    let out = run_cli(&["migrate", path_str(dir.path())]);
    assert!(
        stdout_of(&out).contains("to v3 (RAM-first)"),
        "{}",
        stdout_of(&out)
    );
    // A v3 manifest names the field `version`, not `format_version`.
    let raw = fs::read_to_string(dir.path().join("manifest.json")).unwrap();
    assert!(raw.contains("\"version\": 3"), "{raw}");
    assert!(Database::open_ram(dir.path()).is_ok());
}

// ─── `ndb fsck` / `ndb repair` on v3 ────────────────────────────────────────

#[test]
fn fsck_and_repair_recover_a_corrupt_v3_wal() {
    let parent = TempDir::new().unwrap();
    let dir = parent.path().join("wal_db");
    run_cli(&["init", path_str(&dir), "--ram"]);
    {
        let db = Database::open_ram(&dir).unwrap();
        db.insert(json!({"n": 1})).unwrap();
        db.insert(json!({"n": 2})).unwrap();
    }
    let manifest = read_ram_manifest(&dir).unwrap();
    assert_eq!(manifest.last_seq, 0, "last_seq is published by compaction");
    let wal_scan = ndb::ram_format::scan_ram_wal(&wal_file_path(&dir, manifest.wal_gen)).unwrap();
    assert_eq!(wal_scan.batches.len(), 2, "two separate commit batches");
    assert_eq!(wal_scan.last_seq, 2);

    // An intact WAL audits clean (the lagging manifest is a warning, not damage).
    let clean = run_cli(&["fsck", path_str(&dir)]);
    assert!(stdout_of(&clean).contains("OK"));
    assert!(stdout_of(&clean).contains("warning: manifest last_seq 0 lags"));
    let no_op = run_cli(&["repair", path_str(&dir)]);
    assert!(
        stdout_of(&no_op).contains("Nothing to repair"),
        "{}",
        stdout_of(&no_op)
    );

    // Corrupt the final commit frame's payload and leave bytes after it, so the
    // scanner sees mid-log damage rather than an interrupted append.
    let wal = wal_file_path(&dir, manifest.wal_gen);
    let mut bytes = fs::read(&wal).unwrap();
    let at = bytes.len() - 6;
    bytes[at] ^= 0xFF;
    bytes.extend_from_slice(&[0xAA; 8]);
    fs::write(&wal, &bytes).unwrap();

    let broken = Command::new(ndb_exe())
        .args(["fsck", path_str(&dir)])
        .output()
        .expect("spawn fsck");
    assert!(!broken.status.success(), "fsck must fail on a corrupt WAL");
    assert_eq!(broken.status.code(), Some(2), "corruption exit code");

    let repaired = run_cli(&["repair", path_str(&dir)]);
    let text = stdout_of(&repaired);
    assert!(text.contains("Preserved:"), "{text}");
    assert!(text.contains("Original preserved at:"), "{text}");
    assert!(text.contains("Lost seq range: from 2 upward"), "{text}");

    // The quarantine copy holds the pre-repair bytes; the live WAL is truncated.
    let quarantined: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().contains(".corrupt."))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(quarantined.len(), 1, "exactly one quarantine copy");
    assert_eq!(fs::read(&quarantined[0]).unwrap().len(), bytes.len());

    let fixed = ndb::ram_format::scan_ram_wal(&wal).expect("the repaired WAL scans");
    assert_eq!(fixed.batches.len(), 1, "only the damaged batch was removed");
    assert_eq!(fixed.last_seq, 1);
}

// ─── `ndb export --jsonl` from v3 ───────────────────────────────────────────

#[test]
fn export_jsonl_from_v3_is_sorted_and_carries_lineage() {
    let parent = TempDir::new().unwrap();
    let dir = parent.path().join("export_db");
    run_cli(&["init", path_str(&dir), "--ram"]);
    let (epoch, last_seq, mut expected_ids) = {
        let db = Database::open_ram(&dir).unwrap();
        // Inserted out of id order on purpose: the export must sort.
        for n in [7, 3, 9, 1, 5] {
            db.insert(json!({"n": n})).unwrap();
        }
        let lineage = db.lineage();
        (lineage.source_epoch_uuid, lineage.last_seq, db.get_all_ids())
    };
    expected_ids.sort();

    let out_path = dir.join("export.jsonl");
    let out = run_cli(&[
        "export",
        path_str(&dir),
        path_str(&out_path),
        "--jsonl",
    ]);
    assert!(stdout_of(&out).contains("Exported 5 document(s)"));

    let text = fs::read_to_string(&out_path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 6, "one _meta header plus five documents");

    let meta: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(meta["_meta"]["source_epoch_uuid"], json!(epoch));
    assert_eq!(meta["_meta"]["last_seq"], json!(last_seq));
    assert_eq!(meta["_meta"]["min_available_seq"], json!(1));

    let exported_ids: Vec<String> = lines[1..]
        .iter()
        .map(|line| {
            let doc: Value = serde_json::from_str(line).expect("valid JSON line");
            doc["_id"].as_str().expect("_id").to_string()
        })
        .collect();
    assert_eq!(exported_ids, expected_ids, "export is id-sorted");
}

// ─── #11: gc_buckets must not touch an items bucket ─────────────────────────

#[test]
fn gc_buckets_leaves_items_buckets_and_their_journal_alone() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("meta.json"),
        serde_json::to_string(&json!({
            "buckets": {
                "avatars": {"kind": "hash"},
                "media": {"kind": "items"},
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();

    // One orphaned hash blob and one committed item, coexisting.
    let orphan = db
        .bucket("avatars")
        .store("orphan.png", b"orphan bytes", "image/png")
        .unwrap();
    let (item_id, folder) = db.create_item("media").unwrap();
    fs::write(folder.join("photo.png"), b"hello world").unwrap();
    db.commit_item(
        "media",
        &item_id,
        json!({"name": "photo.png", "size": 11, "sha256": HELLO_SHA256}),
    )
    .unwrap();

    let journal = dir.path().join("_files").join("media").join("items.jsonl");
    assert!(journal.is_file());

    let swept = db.gc_buckets().expect("gc_buckets");
    assert_eq!(swept, 1, "only the orphaned hash blob moves to trash");

    assert!(!db.bucket("avatars").exists(&orphan._file));
    assert!(
        journal.is_file(),
        "the item journal is engine-owned, never an orphaned hash"
    );
    assert!(
        db.read_item("media", &item_id).is_ok(),
        "the live item survived the sweep"
    );
    assert_eq!(
        fs::read(folder.join("photo.png")).unwrap(),
        b"hello world",
        "item bytes were never touched"
    );

    // A referenced hash blob is still protected, as before.
    let kept = db
        .bucket("avatars")
        .store("kept.png", b"kept bytes", "image/png")
        .unwrap();
    db.insert(json!({"avatar": kept._file.to_string_compact()}))
        .unwrap();
    assert_eq!(db.gc_buckets().unwrap(), 0);
    assert!(db.bucket("avatars").exists(&kept._file));
}

// ─── #12: `ndb verify` must see compact / URL refs ─────────────────────────

#[test]
fn verify_detects_a_missing_file_referenced_by_a_compact_string() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("meta.json"),
        serde_json::to_string(&json!({"buckets": {"attachments": {}}})).unwrap(),
    )
    .unwrap();
    let (compact, url_form, filename) = {
        let db = Database::open(dir.path().join("data.jsonl")).unwrap();
        let stored = db
            .bucket("attachments")
            .store("report.pdf", b"pdf bytes", "application/pdf")
            .unwrap();
        let compact = stored._file.to_string_compact();
        let url_form = format!("/files/attachments/{}", stored._file.filename());
        db.insert(json!({"title": "compact", "file": compact.clone()}))
            .unwrap();
        db.insert(json!({"title": "url", "file": url_form.clone()}))
            .unwrap();
        (compact, url_form, stored._file.filename())
    };
    assert_ne!(compact, url_form);

    // Both ref forms resolve: verify is green.
    let clean = run_cli(&["verify", path_str(dir.path())]);
    assert!(
        stdout_of(&clean).contains("verified"),
        "{}",
        stdout_of(&clean)
    );

    // Remove the blob the documents point at; a checker blind to string refs
    // would still call this database clean.
    fs::remove_file(dir.path().join("_files").join("attachments").join(&filename)).unwrap();

    let broken = Command::new(ndb_exe())
        .args(["verify", path_str(dir.path())])
        .output()
        .expect("spawn verify");
    assert!(!broken.status.success(), "verify must detect the missing blob");
    assert_eq!(broken.status.code(), Some(2), "corruption exit code");
    let stderr = stderr_of(&broken);
    assert!(
        stderr.contains(&format!("attachments:{filename}")),
        "the missing compact ref is named: {stderr}"
    );
}

// ─── #14: trash TTL uses residence time, not mtime ─────────────────────────

#[test]
fn hash_trash_ttl_uses_residence_time_and_fails_safe_without_a_stamp() {
    let dir = TempDir::new().unwrap();
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    let bucket = db.bucket("avatars");
    let meta = bucket
        .store("ancient.png", b"ancient bytes", "image/png")
        .unwrap();
    let live = dir
        .path()
        .join("_files")
        .join("avatars")
        .join(meta._file.filename());

    // The content is days old. Before #14 that meant it aged out of trash the
    // instant it arrived.
    set_age(&live, Duration::from_secs(3 * 24 * 3600));
    bucket.delete(&meta._file).expect("move to trash");

    let trashed = bucket.trash_dir().join(meta._file.filename());
    let stamp = bucket
        .trash_dir()
        .join(format!("{}.trashed_at", meta._file.filename()));
    assert!(trashed.is_file());
    assert!(stamp.is_file(), "an explicit residence record is written");
    let recorded: u64 = fs::read_to_string(&stamp).unwrap().trim().parse().unwrap();
    assert!(now_secs().saturating_sub(recorded) <= 5);

    assert_eq!(
        bucket.purge_trash_ttl(Duration::from_secs(60)).unwrap(),
        0,
        "a freshly trashed file survives its TTL, whatever its mtime says"
    );
    assert!(trashed.is_file());

    // Restore takes the residence record with it.
    bucket.restore(&meta._file.id, &meta._file.ext).unwrap();
    assert!(bucket.exists(&meta._file));
    assert!(!stamp.exists(), "restore removes the residence record");

    // A repeated delete starts a new residence window.
    bucket.delete(&meta._file).unwrap();
    assert!(stamp.is_file());
    let recorded: u64 = fs::read_to_string(&stamp).unwrap().trim().parse().unwrap();
    assert!(now_secs().saturating_sub(recorded) <= 5);

    // Missing record: retained, never purged on a guess.
    fs::remove_file(&stamp).unwrap();
    assert_eq!(bucket.purge_trash_ttl(Duration::from_secs(60)).unwrap(), 0);
    assert!(trashed.is_file(), "no record means retain");

    // Corrupt record: retained too.
    fs::write(&stamp, b"not-a-number").unwrap();
    assert_eq!(bucket.purge_trash_ttl(Duration::from_secs(60)).unwrap(), 0);
    assert!(trashed.is_file(), "an unreadable record means retain");

    // A genuinely old residence window is purged.
    fs::write(&stamp, format!("{}\n", now_secs() - 7200)).unwrap();
    assert_eq!(bucket.purge_trash_ttl(Duration::from_secs(60)).unwrap(), 1);
    assert!(!trashed.exists());
    assert!(!stamp.exists(), "the residence record goes with the entry");
}

#[test]
fn item_trash_ttl_uses_residence_time_and_survives_restore() {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("meta.json"),
        serde_json::to_string(&json!({"buckets": {"media": {"kind": "items"}}})).unwrap(),
    )
    .unwrap();
    let db = Database::open(dir.path().join("data.jsonl")).unwrap();
    let bucket = db.bucket("media");
    let (item_id, folder) = db.create_item("media").unwrap();
    let original = folder.join("photo.png");
    fs::write(&original, b"hello world").unwrap();
    db.commit_item(
        "media",
        &item_id,
        json!({"name": "photo.png", "size": 11, "sha256": HELLO_SHA256}),
    )
    .unwrap();
    set_age(&original, Duration::from_secs(5 * 24 * 3600));

    db.delete_item("media", &item_id).unwrap();
    let trashed = bucket.trash_dir().join(&item_id);
    let stamp = bucket.trash_dir().join(format!("{item_id}.trashed_at"));
    assert!(trashed.is_dir());
    assert!(stamp.is_file());

    assert_eq!(
        bucket.purge_trash_ttl(Duration::from_secs(60)).unwrap(),
        0,
        "an old item folder survives its first TTL window"
    );
    assert!(trashed.is_dir());

    // Restore moves the folder back and clears the record.
    db.restore_item("media", &item_id).unwrap();
    assert!(original.is_file());
    assert!(!stamp.exists());

    // Missing record on a re-delete: retained.
    db.delete_item("media", &item_id).unwrap();
    assert!(stamp.is_file());
    fs::remove_file(&stamp).unwrap();
    assert_eq!(bucket.purge_trash_ttl(Duration::from_secs(60)).unwrap(), 0);
    assert!(trashed.is_dir(), "no record means retain");

    // An aged record purges the folder as one unit.
    fs::write(&stamp, format!("{}\n", now_secs() - 7200)).unwrap();
    assert_eq!(bucket.purge_trash_ttl(Duration::from_secs(60)).unwrap(), 1);
    assert!(!trashed.exists());
    assert!(!stamp.exists());
}

// ─── Sanity: the v3 folder layout the CLI produces ─────────────────────────

#[test]
fn v3_layout_uses_generation_one_for_base_and_wal() {
    let parent = TempDir::new().unwrap();
    let dir = parent.path().join("layout_db");
    run_cli(&["init", path_str(&dir), "--ram"]);
    assert!(base_file_path(&dir, 1).is_file());
    assert!(wal_file_path(&dir, 1).is_file());
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(names.contains(&base_file_name(1)));
    assert!(names.contains(&wal_file_name(1)));
}
