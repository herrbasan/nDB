//! Phase 12 — CLI regression tests (B1).
//!
//! The CLI binary and the library share the meta.json contract; these tests
//! spawn the real `ndb` binary and verify what it writes actually opens with
//! `Database::open` — the exact failure B1 described (`ndb init` wrote a
//! `buckets` shape the core refused to load).

use std::fs;
use std::process::Command;

use ndb::Database;
use serde_json::Value;
use tempfile::TempDir;

fn ndb(args: &[&str], cwd: Option<&std::path::Path>) -> (bool, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ndb"));
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().expect("failed to spawn ndb binary");
    let ok = out.status.success();
    (
        ok,
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn meta_of(target: &std::path::Path) -> Value {
    serde_json::from_str(&fs::read_to_string(target.join("meta.json")).unwrap())
        .expect("meta.json must be valid JSON")
}

/// The B1 regression, whole class: whatever `ndb init` writes, the library
/// must be able to open — with buckets and without.
#[test]
fn init_creates_a_database_the_library_can_open() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("fresh");

    let (ok, text) = ndb(
        &["init", target.to_str().unwrap(), "--buckets", "avatars,docs"],
        None,
    );
    assert!(ok, "ndb init failed: {text}");

    // buckets are the object form the policy loader expects
    let buckets = meta_of(&target).get("buckets").cloned().unwrap();
    assert!(
        buckets.is_object(),
        "init must write object-form buckets, got {buckets}"
    );
    assert!(buckets.get("avatars").is_some());
    assert!(buckets.get("docs").is_some());

    Database::open(target.join("data.jsonl"))
        .unwrap_or_else(|e| panic!("CLI-created database must open, got {e}"));
}

#[test]
fn init_without_buckets_opens_cleanly() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("fresh");

    let (ok, text) = ndb(&["init", target.to_str().unwrap()], None);
    assert!(ok, "ndb init failed: {text}");

    let buckets = meta_of(&target).get("buckets").cloned().unwrap();
    assert!(buckets.is_object(), "empty buckets must be an object, got {buckets}");

    Database::open(target.join("data.jsonl"))
        .unwrap_or_else(|e| panic!("CLI-created database must open, got {e}"));
}

/// `ndb config set buckets` (runs inside the DB folder) must write the
/// object form too — including the single-name case, which previously
/// wrote a bare string the loader rejects.
#[test]
fn config_set_buckets_writes_openable_object_form() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("fresh");

    let (ok, text) = ndb(&["init", target.to_str().unwrap()], None);
    assert!(ok, "ndb init failed: {text}");

    // single name — the case that used to write `"buckets": "media"`
    let (ok, text) = ndb(&["config", "set", "buckets", "media"], Some(&target));
    assert!(ok, "config set failed: {text}");

    let buckets = meta_of(&target).get("buckets").cloned().unwrap();
    assert!(
        buckets.is_object(),
        "config set must write object-form buckets, got {buckets}"
    );
    assert!(buckets.get("media").is_some(), "media must be declared");

    Database::open(target.join("data.jsonl"))
        .unwrap_or_else(|e| panic!("post-config database must open, got {e}"));
}

/// `ndb merge` unions buckets from both metas and preserves policy blocks.
#[test]
fn merge_writes_object_form_and_preserves_policies() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().join("base");
    let other = dir.path().join("other");
    let merged = dir.path().join("merged");

    let (ok, text) = ndb(&["init", base.to_str().unwrap(), "--buckets", "avatars"], None);
    assert!(ok, "init base failed: {text}");
    let (ok, text) = ndb(&["init", other.to_str().unwrap(), "--buckets", "media"], None);
    assert!(ok, "init other failed: {text}");

    // A library-shaped policy on the base bucket must survive the merge.
    let base_meta = serde_json::json!({
        "version": 1,
        "buckets": { "avatars": { "onDocumentDelete": "restrict" } }
    });
    fs::write(
        base.join("meta.json"),
        serde_json::to_string_pretty(&base_meta).unwrap(),
    )
    .unwrap();

    let (ok, text) = ndb(
        &["merge", base.to_str().unwrap(), other.to_str().unwrap(), "--output", merged.to_str().unwrap()],
        None,
    );
    assert!(ok, "ndb merge failed: {text}");

    let buckets = meta_of(&merged).get("buckets").cloned().unwrap();
    assert!(buckets.is_object(), "merge must write object-form buckets, got {buckets}");
    assert_eq!(
        buckets.get("avatars").and_then(|b| b.get("onDocumentDelete")),
        Some(&serde_json::json!("restrict")),
        "policy blocks must survive the merge"
    );
    assert!(buckets.get("media").is_some(), "union must include the merge-in bucket");

    Database::open(merged.join("data.jsonl"))
        .unwrap_or_else(|e| panic!("merged database must open, got {e}"));
}
