use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

// Core Philosophy Exit Codes
const EXIT_SUCCESS: i32 = 0;
const EXIT_GENERAL_ERROR: i32 = 1;
const EXIT_CORRUPTION: i32 = 2;
const EXIT_LOCKED: i32 = 3;

/// Report a fatal CLI error and exit non-zero.
///
/// Maintenance commands exist to produce a correct artifact; a step that could
/// not be performed means the artifact is not the one that was asked for, so
/// stop rather than print success over an incomplete result.
fn fatal(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    process::exit(EXIT_GENERAL_ERROR);
}

// ─── Graceful shutdown (M3) ─────────────────────────────────────────

/// The OS-level hook for `ndb serve`: a first Ctrl-C / SIGINT sets the
/// process shutdown latch, which stops the accept loop, drains in-flight
/// workers and flushes every mounted database. A second signal falls through
/// to the OS default (an operator who really wants the process gone can have
/// it) — but the first one always drains first.
#[cfg(windows)]
mod shutdown {
    use std::sync::atomic::{AtomicUsize, Ordering};

    type HandlerRoutine = Option<unsafe extern "system" fn(u32) -> i32>;

    extern "system" {
        fn SetConsoleCtrlHandler(handler: HandlerRoutine, add: i32) -> i32;
    }

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_SHUTDOWN_EVENT: u32 = 5;

    static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

    // Async-signal-safe by construction: an atomic bump and a flag store.
    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        match ctrl_type {
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_SHUTDOWN_EVENT => {
                if INTERRUPTS.fetch_add(1, Ordering::SeqCst) == 0 {
                    ndb::server::request_process_shutdown();
                    return 1; // handled: the serve loop drains and exits
                }
                0 // second signal: OS default termination
            }
            _ => 0,
        }
    }

    pub fn install() -> bool {
        (unsafe { SetConsoleCtrlHandler(Some(handler), 1) }) != 0
    }
}

/// POSIX twin of the Windows handler (SIGINT + SIGTERM).
#[cfg(unix)]
mod shutdown {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }

    static INTERRUPTS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn handler(_sig: i32) {
        if INTERRUPTS.fetch_add(1, Ordering::SeqCst) == 0 {
            ndb::server::request_process_shutdown();
        }
    }

    pub fn install() -> bool {
        let h = handler as usize;
        (unsafe { signal(SIGINT, h) }) != usize::MAX
            && (unsafe { signal(SIGTERM, h) }) != usize::MAX
    }
}

#[cfg(not(any(windows, unix)))]
mod shutdown {
    pub fn install() -> bool {
        false
    }
}

/// Read the `buckets` block in either form: the library's object form
/// (`{"name": {policy}}`, #10) or the legacy CLI name array (`["name"]`).
/// Legacy names map to an empty policy. The CLI writes the object form;
/// the array form is read-compat for databases created by older binaries
/// (B1).
fn buckets_map(meta: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    match meta.get("buckets") {
        Some(serde_json::Value::Object(obj)) => {
            for (name, block) in obj {
                out.insert(name.clone(), block.clone());
            }
        }
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                if let Some(name) = item.as_str() {
                    out.insert(name.to_string(), serde_json::json!({}));
                }
            }
        }
        _ => {}
    }
    out
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        process::exit(EXIT_GENERAL_ERROR);
    }

    let command = args[1].as_str();

    match command {
        "init" => handle_init(&args[2..]),
        "migrate" => handle_migrate(&args[2..]),
        "destroy" | "drop" => handle_destroy(&args[2..]),
        "info" => handle_info(&args[2..]),
        "compact" => handle_compact(&args[2..]),
        "export" => handle_export(&args[2..]),
        "import" => handle_import(&args[2..]),
        "merge" => handle_merge(&args[2..]),
        "verify" | "check" => handle_verify(&args[2..]),
        "fsck" => handle_fsck(&args[2..]),
        "repair" => handle_repair(&args[2..]),
        "recover" => handle_recover(&args[2..]),
        "dump" => handle_dump(&args[2..]),
        "config" => handle_config(&args[2..]),
        "query" => handle_query(&args[2..]),
        "serve" => handle_serve(&args[2..]),
        _ => {
            eprintln!("Unknown command: {}", command);
            print_usage();
            process::exit(EXIT_GENERAL_ERROR);
        }
    }
}

fn print_usage() {
    eprintln!("nDB Command Line Interface");
    eprintln!("Usage: ndb <command> [args...]");
    eprintln!("");
    eprintln!("Commands:");
    eprintln!("  init <path> [--buckets a,b] [--v2]  Initialize a new database");
    eprintln!("  migrate <path>                Migrate a v1 journal to the v2 binary backend");
    eprintln!("  destroy <path> --force        Safely delete a database");
    eprintln!("  info <path>                   Show database statistics");
    eprintln!("  compact <path>                Compact the database in-place");
    eprintln!("  export <path> <dest>          Create a portable snapshot");
    eprintln!("  export <path> <out.jsonl> --jsonl   Export human-readable JSONL");
    eprintln!("  import <src> <path>           Restore a snapshot");
    eprintln!("  merge <base> <merge-in>       Combine databases");
    eprintln!("  verify <path>                 Check for corruptions");
    eprintln!("  fsck <path>                   Full integrity check (v1 journal or v2 generations)");
    eprintln!("  repair <path>                 Quarantine/truncate a corrupt v1 journal or v2 WAL");
    eprintln!("  recover <src> <dest>          Recover corrupted data");
    eprintln!("  dump <path>                   Export JSON Lines to stdout");
    eprintln!("  config <get|set> ...          Manage metadata/config");
    eprintln!("  query <path> <query_ast>      Run a raw JSON AST query");
    eprintln!("  serve <path> [options]         Run as resident HTTP daemon");
    eprintln!("  serve --db <name>=<path> ...   Serve several databases by name");
    eprintln!("  serve --dir <root>             Serve every child DB folder under <root>");
    eprintln!("  serve --config <file.json>     Service configuration (CLI wins)");
    eprintln!("");
    eprintln!("Serve options:");
    eprintln!("  --bind <addr>    Bind address (default 127.0.0.1; 0.0.0.0 for LAN)");
    eprintln!("  --port <n>       Port (default 8323)");
    eprintln!("  --token <secret> Require 'Authorization: Bearer <secret>' (recommended for LAN)");
    eprintln!("  --db <name>=<path>  Mount a database under a name (repeatable;");
    eprintln!("                     path is a data.jsonl file or a database folder)");
    eprintln!("  --dir <root>     Mount every child folder holding data.jsonl or meta.json,");
    eprintln!("                     and allow POST /v2/dbs to create new databases under <root>");
    eprintln!("  --config <file>  JSON config (bind/port/token/persistence/scheduled_ms/");
    eprintln!("                     dir/max_limit/default_limit/dbs)");
    eprintln!("  --persistence <immediate|scheduled|lazy>  Service default (immediate)");
    eprintln!("  --format <auto|v1|v2>       Format for databases created via POST /v2/dbs (default auto)");
    eprintln!("  --scheduled-ms <n>  Flush interval for persistence=scheduled (default 5000)");
    eprintln!("  --max-limit <n>     Hard cap for an explicit query limit (default 50000)");
    eprintln!("  --default-limit <n> Page size when a query omits limit (default 1000)");
    eprintln!("  --text-index <f>  Build a full-text index on field <f> at boot (repeatable;");
    eprintln!("                     cached on disk - unchanged journal boots instantly)");
    eprintln!("  --threads <n>     Worker threads: connections + index builds (default: all cores)");
}

fn handle_serve(args: &[String]) {
    use ndb::server::{
        serve_registry, DbRegistry, PersistenceMode, ServiceConfig, ServerConfig,
        DEFAULT_SCHEDULED_MS,
    };

    let mut config = ServerConfig::default();
    let mut text_indexes: Vec<String> = Vec::new();
    let mut positional: Option<std::path::PathBuf> = None;
    let mut config_file: Option<std::path::PathBuf> = None;
    let mut cli_dbs: Vec<(String, String)> = Vec::new();
    let mut cli_dir: Option<std::path::PathBuf> = None;
    let mut cli_persistence: Option<PersistenceMode> = None;
    let mut cli_scheduled_ms: Option<u64> = None;
    let mut cli_max_limit: Option<usize> = None;
    let mut cli_default_limit: Option<usize> = None;
    let mut cli_format: Option<ndb::StorageFormat> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut value = |name: &str| -> String {
            if i + 1 >= args.len() {
                fatal(&format!("{name} requires a value"));
            }
            i += 1;
            args[i].clone()
        };
        match arg {
            "--bind" => config.bind = value("--bind"),
            "--port" => {
                let raw = value("--port");
                config.port = match raw.parse::<u16>() {
                    Ok(p) => p,
                    Err(_) => fatal(&format!("invalid port: {raw}")),
                };
            }
            "--token" => config.token = Some(value("--token")),
            "--format" => {
                let raw = value("--format");
                match ndb::StorageFormat::parse(&raw) {
                    Ok(format) => cli_format = Some(format),
                    Err(e) => fatal(&e.to_string()),
                }
            }
            "--text-index" => text_indexes.push(value("--text-index")),
            "--threads" => {
                let raw = value("--threads");
                config.threads = match raw.parse::<usize>() {
                    Ok(t) => t,
                    Err(_) => fatal(&format!("invalid thread count: {raw}")),
                };
            }
            "--config" => config_file = Some(std::path::PathBuf::from(value("--config"))),
            "--db" => {
                let raw = value("--db");
                match raw.split_once('=') {
                    Some((name, path)) if !name.is_empty() && !path.is_empty() => {
                        cli_dbs.push((name.to_string(), path.to_string()));
                    }
                    _ => fatal(&format!(
                        "--db expects <name>=<path>, got '{raw}'"
                    )),
                }
            }
            "--dir" => cli_dir = Some(std::path::PathBuf::from(value("--dir"))),
            "--persistence" => {
                let raw = value("--persistence");
                cli_persistence = Some(
                    PersistenceMode::parse(&raw)
                        .unwrap_or_else(|e| fatal(&e.to_string())),
                );
            }
            "--scheduled-ms" => {
                let raw = value("--scheduled-ms");
                cli_scheduled_ms = Some(
                    raw.parse::<u64>()
                        .unwrap_or_else(|_| fatal(&format!("invalid --scheduled-ms: {raw}"))),
                );
            }
            "--max-limit" => {
                let raw = value("--max-limit");
                cli_max_limit = Some(
                    raw.parse::<usize>()
                        .unwrap_or_else(|_| fatal(&format!("invalid --max-limit: {raw}"))),
                );
            }
            "--default-limit" => {
                let raw = value("--default-limit");
                cli_default_limit = Some(
                    raw.parse::<usize>()
                        .unwrap_or_else(|_| fatal(&format!("invalid --default-limit: {raw}"))),
                );
            }
            other if other.starts_with("--") => {
                fatal(&format!("unknown serve option: {other}"));
            }
            other => {
                if positional.is_some() {
                    fatal(&format!(
                        "unexpected extra argument '{other}' (one database path may be given positionally)"
                    ));
                }
                positional = Some(std::path::PathBuf::from(other));
            }
        }
        i += 1;
    }

    // The config file supplies defaults; explicit CLI flags win.
    let mut declared_dbs = Vec::new();
    let mut file_dir: Option<std::path::PathBuf> = None;
    let mut file_persistence: Option<PersistenceMode> = None;
    let mut file_scheduled_ms: Option<u64> = None;
    let mut file_format: Option<ndb::StorageFormat> = None;
    if let Some(path) = &config_file {
        let loaded = ServiceConfig::load(path)
            .unwrap_or_else(|e| fatal(&format!("{}: {e}", path.display())));
        declared_dbs = loaded
            .apply_to(&mut config)
            .unwrap_or_else(|e| fatal(&e.to_string()));
        file_persistence = loaded
            .persistence()
            .unwrap_or_else(|e| fatal(&e.to_string()));
        file_dir = loaded.dir.clone();
        file_scheduled_ms = loaded.scheduled_ms;
        file_format = loaded.format().unwrap_or_else(|e| fatal(&e.to_string()));
    }

    if let Some(m) = cli_max_limit {
        config.max_limit = m;
    }
    if let Some(d) = cli_default_limit {
        config.default_limit = d;
    }
    if config.default_limit > config.max_limit {
        fatal(&format!(
            "default-limit {} exceeds max-limit {}",
            config.default_limit, config.max_limit
        ));
    }
    config.text_indexes = text_indexes;

    let persistence = cli_persistence
        .or(file_persistence)
        .unwrap_or(PersistenceMode::Immediate);
    let scheduled_ms = cli_scheduled_ms
        .or(file_scheduled_ms)
        .unwrap_or(DEFAULT_SCHEDULED_MS);
    let root_dir = cli_dir.or(file_dir);

    if positional.is_none() && cli_dbs.is_empty() && declared_dbs.is_empty() && root_dir.is_none() {
        eprintln!("Error: nothing to serve. Give a database path, --db, --dir or --config with dbs.");
        print_usage();
        process::exit(EXIT_GENERAL_ERROR);
    }
    if let Some(root) = &root_dir {
        if !root.is_dir() {
            fatal(&format!(
                "--dir root does not exist or is not a directory: {}",
                root.display()
            ));
        }
    }

    let registry = DbRegistry::new(root_dir.clone(), persistence, scheduled_ms);
    registry.set_create_format(cli_format.or(file_format).unwrap_or(ndb::StorageFormat::Auto));

    if let Some(path) = &positional {
        // Legacy contract: a positional database must already exist.
        if !path.exists() {
            eprintln!("Error: database file does not exist: {}", path.display());
            process::exit(EXIT_GENERAL_ERROR);
        }
        registry
            .mount_resolved("default", path, persistence.to_persistence(scheduled_ms))
            .unwrap_or_else(|e| fatal(&format!("mounting 'default': {e}")));
    }

    for (name, path) in &cli_dbs {
        registry
            .mount_resolved(name, path, persistence.to_persistence(scheduled_ms))
            .unwrap_or_else(|e| fatal(&format!("mounting '{name}': {e}")));
    }

    for (name, path, mode, sched) in &declared_dbs {
        let mode = mode.unwrap_or(persistence);
        let p = mode.to_persistence(sched.unwrap_or(scheduled_ms));
        registry
            .mount_resolved(name, path, p)
            .unwrap_or_else(|e| fatal(&format!("mounting '{name}': {e}")));
    }

    // Discovery: every direct child of --dir that holds a journal or meta.json
    // is a database, mounted by folder name. Sorted for deterministic boots.
    if let Some(root) = &root_dir {
        let mut children: Vec<std::path::PathBuf> = fs::read_dir(root)
            .unwrap_or_else(|e| fatal(&format!("reading --dir {}: {e}", root.display())))
            .filter_map(|entry| match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if path.is_dir() {
                        Some(path)
                    } else {
                        None
                    }
                }
                Err(e) => fatal(&format!("reading --dir {}: {e}", root.display())),
            })
            .collect();
        children.sort();
        for child in children {
            if !(child.join("data.jsonl").exists()
                || child.join("manifest.json").exists()
                || child.join("meta.json").exists())
            {
                continue;
            }
            let Some(name) = child.file_name().map(|n| n.to_string_lossy().to_string()) else {
                fatal(&format!("cannot derive a database name from {}", child.display()));
            };
            registry
                .mount_resolved(&name, &child, persistence.to_persistence(scheduled_ms))
                .unwrap_or_else(|e| fatal(&format!("mounting discovered DB '{name}': {e}")));
        }
    }

    if registry.is_empty() {
        fatal("no database was mounted");
    }

    // Ctrl-C / SIGINT must flush, not kill mid-write. A platform without a
    // hook still serves; it just says so instead of pretending.
    if !shutdown::install() {
        eprintln!(
            "warning: could not install the shutdown handler on this platform; \
             Ctrl-C will not flush mounted databases"
        );
    }

    if let Err(e) = serve_registry(std::sync::Arc::new(registry), config) {
        eprintln!("Server error: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }
}

fn handle_init(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ndb init <path> [--buckets a,b] [--v2]");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let path = Path::new(&args[0]);
    if path.exists() {
        eprintln!("Error: Path already exists. Must be an empty directory.");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    // Parse buckets and the v2 flag
    let mut buckets = Vec::new();
    let mut v2 = false;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--buckets" && i + 1 < args.len() {
            buckets = args[i + 1].split(',').map(|s| s.trim().to_string()).collect();
            i += 2;
            continue;
        }
        if args[i] == "--v2" {
            v2 = true;
            i += 1;
            continue;
        }
        eprintln!("Error: unknown init argument '{}'", args[i]);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Create directory structure
    if let Err(e) = fs::create_dir_all(path) {
        eprintln!("Failed to create directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Create meta.json. Buckets are written in the object form the
    // library's policy loader expects (B1): name → empty policy block.
    let buckets_obj: serde_json::Map<String, serde_json::Value> =
        buckets.iter().map(|b| (b.clone(), serde_json::json!({}))).collect();
    let meta = serde_json::json!({
        "version": 1,
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        "buckets": buckets_obj,
    });
    let meta_json = serde_json::to_string_pretty(&meta)
        .unwrap_or_else(|e| fatal(&format!("serialize meta.json: {e}")));
    if let Err(e) = fs::write(path.join("meta.json"), meta_json) {
        eprintln!("Failed to write meta.json: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Create library-compatible bucket home (_files) and per-bucket subdirs
    let files_dir = path.join("_files");
    if let Err(e) = fs::create_dir_all(&files_dir) {
        eprintln!("Failed to create _files directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }
    for bucket in &buckets {
        if let Err(e) = fs::create_dir(files_dir.join(bucket)) {
            eprintln!("Failed to create bucket dir '{}': {}", bucket, e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }

    // Create the trash root (the library manages _trash/docs/ on demand)
    if let Err(e) = fs::create_dir_all(path.join("_trash")) {
        eprintln!("Failed to create _trash directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Write the empty journal with a lineage header: a new database gets a fresh
    // epoch, nothing committed, full history. A v2 database initialises a
    // manifest + empty base/WAL generation instead; it never writes data.jsonl.
    if v2 {
        match ndb::Database::open_v2(path) {
            Ok(db) => drop(db),
            Err(e) => {
                eprintln!("Failed to initialise the v2 database: {e}");
                process::exit(EXIT_GENERAL_ERROR);
            }
        }
        println!("Initialized v2 database at {}", path.display());
        process::exit(EXIT_SUCCESS);
    }

    if let Err(e) = ndb::storage::init_file(
        &path.join("data.jsonl"),
        &ndb::storage::SourceLineage::fresh(),
    ) {
        eprintln!("Failed to write data.jsonl: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    println!("Initialized database at {}", path.display());
    process::exit(EXIT_SUCCESS);
}

/// `ndb migrate <path>` — convert a v1 JSONL database folder to v2.
///
/// Staged in `<dir>/.migrating/` so a crash mid-run leaves the live database
/// untouched and a rerun starts clean. The source journal is renamed to
/// `data.jsonl.v1.bak` only after the staged v2 generation has been verified
/// byte-for-byte at the document level and moved into place.
fn handle_migrate(args: &[String]) {
    if args.len() != 1 {
        eprintln!("Usage: ndb migrate <path>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let input = Path::new(&args[0]);
    let dir = if input.is_dir() {
        input.to_path_buf()
    } else {
        input
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Path::new(".").to_path_buf())
    };
    let journal = dir.join("data.jsonl");

    if dir.join(ndb::MANIFEST_FILE).exists() {
        fatal(&format!(
            "{} is already a v2 database (manifest.json exists)",
            dir.display()
        ));
    }
    if !journal.exists() {
        fatal(&format!("no v1 journal at {}", journal.display()));
    }

    // Opening takes the OS writer lock: a database served by another process
    // is refused here, which is exactly the maintenance guarantee needed
    // before rewriting any of its artefacts.
    let db = match ndb::Database::open(&journal) {
        Ok(db) => db,
        Err(e) => fatal(&format!(
            "cannot open {}: {e} (is a service still running on this database?)",
            journal.display()
        )),
    };
    let lineage = db.lineage();
    let snapshot = db.snapshot_at();
    let mut docs: Vec<(String, u64, Vec<u8>)> = Vec::with_capacity(snapshot.docs.len());
    for (id, doc) in &snapshot.docs {
        let doc_seq = db.doc_seq(id).unwrap_or(lineage.last_seq);
        let bytes = match serde_json::to_vec(doc) {
            Ok(bytes) => bytes,
            Err(e) => fatal(&format!("serialize v1 document {id:?}: {e}")),
        };
        docs.push((id.clone(), doc_seq, bytes));
    }
    let v1_lineage = db.lineage();
    drop(db);

    let staging = dir.join(".migrating");
    if staging.exists() {
        if let Err(e) = fs::remove_dir_all(&staging) {
            fatal(&format!(
                "cannot clear the previous partial migration at {}: {e}",
                staging.display()
            ));
        }
    }
    if let Err(e) = fs::create_dir_all(&staging) {
        fatal(&format!("create {}: {e}", staging.display()));
    }

    let base_path = staging.join(ndb::base_file_name(1));
    let wal_path = staging.join(ndb::wal_file_name(1));
    let inputs: Vec<ndb::BaseDocInput<'_>> = docs
        .iter()
        .map(|(id, doc_seq, bytes)| ndb::BaseDocInput::new(id, *doc_seq, bytes))
        .collect();
    if let Err(e) = ndb::write_base_file(&base_path, v1_lineage.last_seq, &inputs, &[]) {
        cleanup_staging(&staging);
        fatal(&format!("write v2 base generation: {e}"));
    }
    if let Err(e) = ndb::WalWriter::create(&wal_path, 1, v1_lineage.last_seq) {
        cleanup_staging(&staging);
        fatal(&format!("write v2 WAL generation: {e}"));
    }
    let manifest = ndb::V2Manifest {
        format_version: ndb::STORAGE_VERSION,
        base_gen: 1,
        wal_gen: 1,
        source_epoch_uuid: v1_lineage.source_epoch_uuid.clone(),
        base_seq: v1_lineage.last_seq,
        min_available_seq: v1_lineage.last_seq.saturating_add(1),
        doc_count: docs.len() as u64,
        indexes: Vec::new(),
    };
    if let Err(e) = ndb::write_manifest_atomic(&staging, &manifest) {
        cleanup_staging(&staging);
        fatal(&format!("write v2 manifest: {e}"));
    }

    // Verify before publishing: the staged folder must open, carry every
    // document, and reproduce each document's canonical JSON exactly.
    let staged = match ndb::Database::open_v2(&staging) {
        Ok(db) => db,
        Err(e) => {
            cleanup_staging(&staging);
            fatal(&format!("staged v2 database does not open: {e}"));
        }
    };
    if staged.len() != docs.len() {
        let actual = staged.len();
        drop(staged);
        cleanup_staging(&staging);
        fatal(&format!(
            "staged v2 database holds {actual} documents, expected {}",
            docs.len()
        ));
    }
    if staged.lineage().source_epoch_uuid != v1_lineage.source_epoch_uuid
        || staged.lineage().last_seq != v1_lineage.last_seq
    {
        drop(staged);
        cleanup_staging(&staging);
        fatal("staged v2 lineage does not match the v1 source");
    }
    for (id, _, bytes) in &docs {
        let expected: serde_json::Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(e) => {
                drop(staged);
                cleanup_staging(&staging);
                fatal(&format!("v1 document {id:?} is not valid JSON: {e}"));
            }
        };
        match staged.get(id) {
            Ok(actual) => {
                if serde_json::to_string(&actual).ok() != serde_json::to_string(&expected).ok() {
                    drop(staged);
                    cleanup_staging(&staging);
                    fatal(&format!(
                        "document {id:?} does not round-trip through the v2 base"
                    ));
                }
            }
            Err(e) => {
                drop(staged);
                cleanup_staging(&staging);
                fatal(&format!("staged v2 database is missing {id:?}: {e}"));
            }
        }
    }
    drop(staged);

    // Publish: generations first, then the manifest (the sole authority),
    // then retire the v1 journal.
    for name in [ndb::base_file_name(1), ndb::wal_file_name(1)] {
        let from = staging.join(&name);
        let to = dir.join(&name);
        if let Err(e) = fs::rename(&from, &to) {
            cleanup_staging(&staging);
            fatal(&format!("move {name} into {}: {e}", dir.display()));
        }
    }
    if let Err(e) = ndb::write_manifest_atomic(&dir, &manifest) {
        fatal(&format!("write {}: {e}", dir.join(ndb::MANIFEST_FILE).display()));
    }
    let backup = dir.join("data.jsonl.v1.bak");
    if let Err(e) = fs::rename(&journal, &backup) {
        fatal(&format!(
            "rename {} to {}: {e}",
            journal.display(),
            backup.display()
        ));
    }
    if let Err(e) = fs::remove_dir_all(&staging) {
        eprintln!("warning: could not remove {}: {e}", staging.display());
    }

    let v2_bytes = fs::metadata(dir.join(ndb::base_file_name(1)))
        .map(|m| m.len())
        .unwrap_or(0)
        + fs::metadata(dir.join(ndb::wal_file_name(1)))
            .map(|m| m.len())
            .unwrap_or(0);
    let v1_bytes = fs::metadata(&backup).map(|m| m.len()).unwrap_or(0);
    println!("Migrated {} to v2", dir.display());
    println!("Documents: {}", docs.len());
    println!("source_epoch_uuid: {}", v1_lineage.source_epoch_uuid);
    println!("last_seq: {}", v1_lineage.last_seq);
    println!("v1 journal: {v1_bytes} bytes (kept at data.jsonl.v1.bak)");
    println!("v2 base + WAL: {v2_bytes} bytes");
    process::exit(EXIT_SUCCESS);
}

fn cleanup_staging(staging: &Path) {
    if let Err(e) = fs::remove_dir_all(staging) {
        eprintln!(
            "warning: could not remove the partial migration at {}: {e}",
            staging.display()
        );
    }
}

/// `ndb fsck <path>` — full integrity audit of a v1 journal or a v2 folder.
fn handle_fsck(args: &[String]) {
    if args.len() != 1 {
        eprintln!("Usage: ndb fsck <path>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let input = Path::new(&args[0]);
    let dir = if input.is_dir() {
        input.to_path_buf()
    } else {
        input
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Path::new(".").to_path_buf())
    };

    if dir.join(ndb::MANIFEST_FILE).exists() {
        let report = match ndb::fsck_v2_dir(&dir) {
            Ok(report) => report,
            Err(e) => fatal(&format!("fsck {}: {e}", dir.display())),
        };
        println!("Database: {}", dir.display());
        println!("Format: v2");
        println!("base.gen_{}: {} documents, {} bytes", report.base_gen, report.doc_count, report.base_bytes);
        println!(
            "wal.gen_{}: {} frames, {} bytes",
            report.wal_gen, report.wal_frames, report.wal_bytes
        );
        println!("base_seq: {}", report.base_seq);
        println!("last_seq: {}", report.last_seq);
        println!("min_available_seq: {}", report.min_available_seq);
        println!("indexes: {}", report.index_count);
        match check_bucket_files(&dir) {
            Ok(count) => println!("bucket files: {count} verified"),
            Err(e) => fatal(&format!("fsck {}: {e}", dir.display())),
        }
        println!("OK");
        process::exit(EXIT_SUCCESS);
    }

    // v1: verify the journal and the doc trash without opening for write.
    let journal = dir.join("data.jsonl");
    if !journal.exists() {
        fatal(&format!(
            "{} holds neither manifest.json nor data.jsonl",
            dir.display()
        ));
    }
    let mut problems = 0usize;
    match ndb::storage::read_all_with_meta(&journal) {
        Ok((meta, docs)) => {
            println!("Database: {}", dir.display());
            println!("Format: v1");
            println!("documents replayed: {}", docs.len());
            if let Some(meta) = meta {
                println!("source_epoch_uuid: {}", meta.source_epoch_uuid.as_deref().unwrap_or("<legacy>"));
                println!("last_seq: {}", meta.last_seq);
            }
        }
        Err(e) => {
            eprintln!("journal {}: {e}", journal.display());
            problems += 1;
        }
    }
    let trash = dir.join("_trash").join("docs").join("data.jsonl");
    if trash.exists() {
        if let Err(e) = ndb::storage::read_trash(&trash) {
            eprintln!("trash {}: {e}", trash.display());
            problems += 1;
        }
    }
    if problems > 0 {
        eprintln!("fsck failed with {problems} problem(s)");
        process::exit(EXIT_CORRUPTION);
    }
    println!("OK");
    process::exit(EXIT_SUCCESS);
}

/// Stat every file under `_files/`; an unreadable or zero-length blob is a
/// bucket-integrity failure (dedup never writes one).
fn check_bucket_files(dir: &Path) -> Result<usize, String> {
    let files = dir.join("_files");
    if !files.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    let buckets = fs::read_dir(&files).map_err(|e| format!("{}: {e}", files.display()))?;
    for bucket in buckets {
        let bucket = bucket.map_err(|e| format!("{}: {e}", files.display()))?;
        if !bucket.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let entries = fs::read_dir(bucket.path())
            .map_err(|e| format!("{}: {e}", bucket.path().display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", bucket.path().display()))?;
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let meta = entry
                .metadata()
                .map_err(|e| format!("{}: {e}", entry.path().display()))?;
            if meta.len() == 0 {
                return Err(format!("{} is a zero-length blob", entry.path().display()));
            }
            count += 1;
        }
    }
    Ok(count)
}

fn handle_destroy(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ndb destroy <path> [--force]");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    let path = Path::new(&args[0]);
    let force = args.len() > 1 && args[1] == "--force";

    if !force {
        print!("Are you sure you want to permanently destroy the database at {}? Type 'yes' to confirm: ", path.display());
        std::io::stdout().flush().unwrap();
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        if input.trim() != "yes" {
            eprintln!("Aborted.");
            process::exit(EXIT_GENERAL_ERROR);
        }
    }
    if !path.join("meta.json").exists() {
        eprintln!("Error: Target is not a valid nDB folder.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    if path.join(".lock").exists() {
        eprintln!("Error: Database is currently active and locked. Please stop the database before destroying.");
        process::exit(EXIT_LOCKED);
    }

    if let Err(e) = fs::remove_dir_all(path) {
        eprintln!("Failed to destroy database: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    println!("Destroyed database at {}", path.display());
    process::exit(EXIT_SUCCESS);
}
fn handle_info(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ndb info <path>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    let path = Path::new(&args[0]);
    let meta_path = path.join("meta.json");

    if path.join(ndb::MANIFEST_FILE).exists() {
        let manifest = match ndb::read_manifest(path) {
            Ok(manifest) => manifest,
            Err(e) => fatal(&format!("read {}: {e}", path.display())),
        };
        let base_size = fs::metadata(ndb::base_file_path(path, manifest.base_gen))
            .map(|m| m.len())
            .unwrap_or(0);
        let wal_size = fs::metadata(ndb::wal_file_path(path, manifest.wal_gen))
            .map(|m| m.len())
            .unwrap_or(0);
        let meta: serde_json::Value = fs::read_to_string(&meta_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(serde_json::Value::Null);
        println!("Status: Valid");
        println!("Format: v2");
        println!("Database Path: {}", path.display());
        println!("Schema Version: {}", meta.get("version").and_then(|v| v.as_i64()).unwrap_or(0));
        println!("Active Documents: {}", manifest.doc_count);
        println!("base.gen_{}: {} bytes", manifest.base_gen, base_size);
        println!("wal.gen_{}: {} bytes", manifest.wal_gen, wal_size);
        println!("base_seq: {}", manifest.base_seq);
        println!("min_available_seq: {}", manifest.min_available_seq);
        println!("source_epoch_uuid: {}", manifest.source_epoch_uuid);
        if manifest.indexes.is_empty() {
            println!("Indexes: None");
        } else {
            println!("Indexes:");
            for spec in &manifest.indexes {
                println!("  - {} ({:?})", spec.field, spec.kind);
            }
        }
        process::exit(EXIT_SUCCESS);
    }

    if !meta_path.exists() {
        eprintln!("Status: Invalid\nReason: Missing meta.json in {}", path.display());
        process::exit(EXIT_GENERAL_ERROR);
    }

    let meta_content = fs::read_to_string(&meta_path).unwrap_or_default();
    let meta: serde_json::Value = serde_json::from_str(&meta_content).unwrap_or_else(|_| serde_json::Value::Null);

    let active_size = fs::metadata(path.join("data.jsonl")).map(|m| m.len()).unwrap_or(0);
    let trash_size = fs::metadata(path.join("_trash").join("docs").join("data.jsonl")).map(|m| m.len()).unwrap_or(0);
    let total_size = active_size + trash_size;

    let mut doc_count = 0;
    if let Ok(file) = fs::File::open(path.join("data.jsonl")) {
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(file);
        // Exclude the _meta header line
        doc_count = reader.lines().count().saturating_sub(1);
    }

    // Both forms are read: object blocks from the library (#10), legacy
    // name arrays from older CLIs (B1).
    let buckets = buckets_map(&meta);

    println!("Status: Valid");
    println!("Database Path: {}", path.display());
    println!("Schema Version: {}", meta.get("version").and_then(|v| v.as_i64()).unwrap_or(0));
    println!("Active Documents: {}", doc_count);
    println!("Disk Usage:");
    println!("  - Active (data.jsonl): {} bytes", active_size);
    println!("  - Trash (_trash/docs/data.jsonl): {} bytes", trash_size);

    if total_size > 0 {
        let frag = (trash_size as f64 / total_size as f64) * 100.0;
        println!("Fragmentation: {:.2}%", frag);
    } else {
        println!("Fragmentation: 0.00%");
    }

    if buckets.is_empty() {
        println!("Buckets: None configured");
    } else {
        println!("Buckets:");
        for bucket in buckets.keys() {
            let bucket_dir = path.join("_files").join(bucket);
            let mut bucket_size = 0;
            let mut file_count = 0;
            if let Ok(entries) = fs::read_dir(&bucket_dir) {
                for entry in entries.flatten() {
                    if let Ok(meta) = entry.metadata() {
                        if meta.is_file() {
                            bucket_size += meta.len();
                            file_count += 1;
                        }
                    }
                }
            }
            println!("  - {}: {} bytes ({} files)", bucket, bucket_size, file_count);
        }
    }
    
    process::exit(EXIT_SUCCESS);
}

fn handle_compact(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ndb compact <path>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let path = Path::new(&args[0]);
    let meta_path = path.join("meta.json");
    let db_path = path.join("data.jsonl");

    if !meta_path.exists() || !(db_path.exists() || path.join(ndb::MANIFEST_FILE).exists()) {
        eprintln!("Error: Target is not a valid nDB folder.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[1/2] Connecting to database...");
    
    // The folder path auto-detects v1 (data.jsonl) or v2 (manifest.json).
    // Opening takes the OS writer lock, so a database served elsewhere is
    // reported as locked rather than compacted underneath its writer.
    let db = match ndb::Database::open(path) {
        Ok(db) => db,
        Err(e @ ndb::Error::DatabaseLocked { .. }) => {
            eprintln!("Error: database is in use: {e}");
            process::exit(EXIT_LOCKED);
        }
        Err(e) => {
            eprintln!("Failed to open database: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    eprintln!("[2/2] Merging segments and removing tombstones...");
    if let Err(e) = db.compact() {
        eprintln!("Failed to compact database: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    println!("Compaction complete.");
    process::exit(EXIT_SUCCESS);
}

fn handle_export(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ndb export <path> <dest> [--consistent] [--jsonl]");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    let src_path = Path::new(&args[0]);
    let dest_path = Path::new(&args[1]);
    let db_path = src_path.join("data.jsonl");

    let consistent = args.iter().any(|a| a == "--consistent");
    let jsonl = args.iter().any(|a| a == "--jsonl")
        || (dest_path
            .extension()
            .map(|e| e.to_string_lossy().eq_ignore_ascii_case("jsonl"))
            .unwrap_or(false)
            && !dest_path.is_dir());

    if consistent {
        if !src_path.join(".readonly").exists() {
            eprintln!("Error: --consistent requested but .readonly marker not found. Database might be actively writing.");
            process::exit(EXIT_LOCKED);
        }
    }

    if dest_path.exists() {
        eprintln!("Error: Destination path already exists. Must be empty.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    if jsonl {
        eprintln!("Exporting active documents as JSON Lines...");
        let db = match ndb::Database::open(src_path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("Failed to open source database: {e}");
                process::exit(EXIT_GENERAL_ERROR);
            }
        };
        let count = match db.export_jsonl(dest_path, true) {
            Ok(count) => count,
            Err(e) => {
                eprintln!("Export failed: {e}");
                process::exit(EXIT_GENERAL_ERROR);
            }
        };
        println!(
            "Exported {} document(s) to {}",
            count,
            dest_path.display()
        );
        process::exit(EXIT_SUCCESS);
    }

    eprintln!("[1/4] Preparing snapshot directory...");
    if let Err(e) = fs::create_dir_all(dest_path) {
        eprintln!("Failed to create destination directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[2/4] Connecting to source database...");
    let db = match ndb::Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open source database: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    eprintln!("[3/4] Exporting active state (Crash-Consistent)...");
    if let Err(e) = db.export_snapshot(dest_path) {
         eprintln!("Export failed: {}", e);
         process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[4/4] Writing snapshot.json marker...");
    let snapshot_json = format!(
        "{{\n  \"type\": \"ndb\",\n  \"version\": 1,\n  \"timestamp\": {},\n  \"original_path\": \"{}\"\n}}\n",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
        src_path.display().to_string().replace('\\', "\\\\")
    );
    
    if let Err(e) = fs::write(dest_path.join("snapshot.json"), snapshot_json) {
        eprintln!("Failed to write snapshot.json: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    println!("Export complete. Snapshot ready at {}", dest_path.display());
    process::exit(EXIT_SUCCESS);
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let entry_path = entry.path();
        let dest_path = dst.join(entry.file_name());
        
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else {
            fs::copy(&entry_path, &dest_path)?;
        }
    }
    Ok(())
}

fn handle_import(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ndb import <snapshot> <dest> [--force]");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    let src_path = Path::new(&args[0]);
    let dest_path = Path::new(&args[1]);
    let force = args.len() > 2 && args[2] == "--force";

    let snapshot_file = src_path.join("snapshot.json");
    if !snapshot_file.exists() {
        eprintln!("Error: Source is not a valid snapshot (missing snapshot.json).");
        process::exit(EXIT_GENERAL_ERROR);
    }

    let snapshot_content = match fs::read_to_string(&snapshot_file) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error reading snapshot.json: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };
    
    let snapshot_json: serde_json::Value = match serde_json::from_str(&snapshot_content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing snapshot.json: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    if snapshot_json.get("type").and_then(|v| v.as_str()) != Some("ndb") {
        eprintln!("Error: Snapshot type is not 'ndb'.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    if snapshot_json.get("version").and_then(|v| v.as_u64()) != Some(1) {
        eprintln!("Error: Unsupported snapshot version.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    if dest_path.exists() {
        let is_empty = fs::read_dir(dest_path).map(|mut i| i.next().is_none()).unwrap_or(false);
        if !is_empty {
            if !force {
                eprintln!("Error: Target directory exists and is not empty. Use --force to overwrite.");
                process::exit(EXIT_GENERAL_ERROR);
            }
            if let Err(e) = fs::remove_dir_all(dest_path) {
                eprintln!("Error removing existing target directory: {}", e);
                process::exit(EXIT_GENERAL_ERROR);
            }
        }
    }

    if let Err(e) = fs::create_dir_all(dest_path) {
        eprintln!("Error creating target directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Copy meta.json
    let meta_src = src_path.join("meta.json");
    if meta_src.exists() {
        if let Err(e) = fs::copy(&meta_src, dest_path.join("meta.json")) {
            eprintln!("Error copying meta.json: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }

    // Copy data.jsonl
    let db_src = src_path.join("data.jsonl");
    if db_src.exists() {
        if let Err(e) = fs::copy(&db_src, dest_path.join("data.jsonl")) {
            eprintln!("Error copying data.jsonl: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }

    // Copy _files directory
    let files_src = src_path.join("_files");
    if files_src.exists() {
        if let Err(e) = copy_dir_recursive(&files_src, &dest_path.join("_files")) {
            eprintln!("Error copying files: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }

    println!("Import complete. Snapshot restored to {}", dest_path.display());
    process::exit(EXIT_SUCCESS);
}

fn handle_merge(args: &[String]) {
    if args.len() < 4 || args[2] != "--output" {
        eprintln!("Usage: ndb merge <base> <merge-in> --output <dest>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    let base_path = Path::new(&args[0]);
    let merge_path = Path::new(&args[1]);
    let dest_path = Path::new(&args[3]);

    if dest_path.exists() {
        eprintln!("Error: Destination path already exists. Must be empty.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[1/4] Connecting to source databases...");
    let base_db = match ndb::Database::open(base_path.join("data.jsonl")) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open base database: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };
    
    let merge_db = match ndb::Database::open(merge_path.join("data.jsonl")) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open merge-in database: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    eprintln!("[2/4] Resolving document collisions...");
    let mut resolved_docs: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();

    for id in base_db.get_all_ids() {
        // Skipping a document we cannot read would emit a merge that silently
        // lost it while still reporting success.
        let doc = base_db
            .get(&id)
            .unwrap_or_else(|e| fatal(&format!("read {id} from base: {e}")));
        resolved_docs.insert(id, doc);
    }

    for id in merge_db.get_all_ids() {
        let merge_doc = merge_db
            .get(&id)
            .unwrap_or_else(|e| fatal(&format!("read {id} from merge-in: {e}")));
        if let Some(base_doc) = resolved_docs.get(&id) {
            // Collision! Compare _modified
            let base_mod = base_doc.get("_modified").and_then(|v| v.as_u64()).unwrap_or(0);
            let merge_mod = merge_doc.get("_modified").and_then(|v| v.as_u64()).unwrap_or(0);
            if merge_mod >= base_mod {
                resolved_docs.insert(id, merge_doc);
            }
        } else {
            resolved_docs.insert(id.clone(), merge_doc);
        }
    }

    eprintln!("[3/4] Writing merged state to destination...");
    if let Err(e) = fs::create_dir_all(dest_path) {
        eprintln!("Failed to create destination directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Write data.jsonl
    let target_db = dest_path.join("data.jsonl");
    let active: Vec<&serde_json::Value> = resolved_docs.values().collect();
    if let Err(e) = ndb::storage::rewrite_atomic_with_meta(
        &target_db,
        &active,
        &ndb::storage::SourceLineage::fresh(),
    ) {
        eprintln!("Failed to write merged data.jsonl: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    // Create the trash root (the library manages _trash/docs/ on demand)
    if let Err(e) = fs::create_dir_all(dest_path.join("_trash")) {
        fatal(&format!("create trash directory: {e}"));
    }

    // Copy and merge meta.json: union their buckets, written in the
    // object form the library expects. Both forms are read — legacy name
    // arrays from old CLIs, object blocks with policies from the library
    // (#10) — and policy blocks are preserved. When both sides declare the
    // same bucket, the base policy wins (deterministic; B1).
    let base_meta_path = base_path.join("meta.json");
    let merge_meta_path = merge_path.join("meta.json");
    
    // A meta.json that exists but cannot be read or parsed must not quietly
    // become an empty one: the merge would drop every bucket it declared.
    let mut meta: serde_json::Value = if base_meta_path.exists() {
        let content = fs::read_to_string(&base_meta_path)
            .unwrap_or_else(|e| fatal(&format!("read {}: {e}", base_meta_path.display())));
        serde_json::from_str(&content)
            .unwrap_or_else(|e| fatal(&format!("parse {}: {e}", base_meta_path.display())))
    } else {
        serde_json::json!({})
    };

    let mut buckets = buckets_map(&meta);
    if merge_meta_path.exists() {
        let content = fs::read_to_string(&merge_meta_path)
            .unwrap_or_else(|e| fatal(&format!("read {}: {e}", merge_meta_path.display())));
        let merge_meta: serde_json::Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| fatal(&format!("parse {}: {e}", merge_meta_path.display())));
        for (name, block) in buckets_map(&merge_meta) {
            buckets.entry(name).or_insert(block);
        }
    }

    meta["buckets"] = serde_json::Value::Object(buckets);
    
    let meta_json = serde_json::to_string_pretty(&meta)
        .unwrap_or_else(|e| fatal(&format!("serialize meta.json: {e}")));
    if let Err(e) = fs::write(dest_path.join("meta.json"), meta_json) {
        eprintln!("Failed to write merged meta.json: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[4/4] Deduplicating and copying file buckets...");
    // Copy base buckets
    let base_buckets = base_path.join("_files");
    let dest_buckets = dest_path.join("_files");
    if base_buckets.exists() {
        if let Err(e) = copy_dir_recursive(&base_buckets, &dest_buckets) {
            eprintln!("Failed to copy base buckets: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }
    // Copy merge-in buckets (overwrites handle native deduplication naturally)
    let merge_buckets = merge_path.join("_files");
    if merge_buckets.exists() {
        if let Err(e) = copy_dir_recursive(&merge_buckets, &dest_buckets) {
            eprintln!("Failed to copy merge-in buckets: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }

    println!("Merge complete. Unified database at {}", dest_path.display());
    process::exit(EXIT_SUCCESS);
}

fn check_file_refs(val: &serde_json::Value, base_path: &Path) -> Result<(), String> {
    if let Some(obj) = val.as_object() {
        for v in obj.values() {
            if let Some(inner) = v.as_object() {
                if inner.contains_key("_file") {
                    if let Some(file_ref) = inner.get("_file").and_then(|f| f.as_object()) {
                        let bucket = file_ref.get("bucket").and_then(|x| x.as_str()).unwrap_or("");
                        let id = file_ref.get("id").and_then(|x| x.as_str()).unwrap_or("");
                        let ext = file_ref.get("ext").and_then(|x| x.as_str()).unwrap_or("");
                        
                        if !bucket.is_empty() && !id.is_empty() {
                            let path = base_path.join("_files").join(bucket).join(format!("{}.{}", id, ext));
                            if !path.exists() {
                                return Err(format!("{}:{}.{}", bucket, id, ext));
                            }
                        }
                    }
                } else {
                    check_file_refs(v, base_path)?;
                }
            } else if v.is_array() {
                check_file_refs(v, base_path)?;
            }
        }
    } else if let Some(arr) = val.as_array() {
        for v in arr {
            check_file_refs(v, base_path)?;
        }
    }
    Ok(())
}

fn handle_verify(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ndb verify <path>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    // A v2 folder has no JSONL journal: the full binary audit is `fsck`.
    if Path::new(&args[0]).join(ndb::MANIFEST_FILE).exists() {
        handle_fsck(args);
    }
    
    let path = Path::new(&args[0]);
    let db_path = path.join("data.jsonl");
    let trash_path = path.join("_trash").join("docs").join("data.jsonl");
    
    if !path.join("meta.json").exists() {
        eprintln!("Error: Target is not a valid nDB folder.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[1/3] Verifying data.jsonl syntax and references...");
    let mut corruptions = 0;
    
    if db_path.exists() {
        if let Ok(file) = fs::File::open(&db_path) {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(file);
            for (i, line_res) in reader.lines().enumerate() {
                if let Ok(line) = line_res {
                    if line.trim().is_empty() { continue; }
                    match serde_json::from_str::<serde_json::Value>(&line) {
                        Ok(val) => {
                            if let Err(e) = check_file_refs(&val, path) {
                                eprintln!("Line {}: Missing file ref: {}", i+1, e);
                                corruptions += 1;
                            }
                        },
                        Err(e) => {
                            eprintln!("Line {}: Invalid JSON: {}", i+1, e);
                            corruptions += 1;
                        }
                    }
                } else {
                    eprintln!("Line {}: Corrupted UTF-8 read", i+1);
                    corruptions += 1;
                }
            }
        }
    }
    
    eprintln!("[2/3] Verifying _trash/docs/data.jsonl syntax and references...");
    if trash_path.exists() {
        if let Ok(file) = fs::File::open(&trash_path) {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(file);
            for (i, line_res) in reader.lines().enumerate() {
                if let Ok(line) = line_res {
                    if line.trim().is_empty() { continue; }
                    if let Err(e) = serde_json::from_str::<serde_json::Value>(&line) {
                        eprintln!("Trash Line {}: Invalid JSON: {}", i+1, e);
                        corruptions += 1;
                    }
                }
            }
        }
    }
    
    // Item buckets (#9): for buckets declared `kind: "items"` in
    // meta.json, check live originals against their committed facts,
    // reserved folders, and orphan folders. Runs through the library so
    // the check and the engine share one implementation; if the journal
    // itself is unreadable the syntax pass above has already reported it.
    if db_path.exists() {
        match ndb::Database::open(&db_path) {
            Ok(db) if !db.item_bucket_names().is_empty() => {
                eprintln!("[3/3] Verifying item buckets...");
                for anomaly in db.verify_item_buckets() {
                    eprintln!("Item bucket: {anomaly}");
                    corruptions += 1;
                }
            }
            Ok(_) => {} // no items buckets declared
            Err(e) => {
                eprintln!("[3/3] Item bucket check skipped: database did not open cleanly ({e})");
            }
        }
    }

    if corruptions > 0 {
        eprintln!("Verification failed. Found {} corrupt rows/missing references.", corruptions);
        process::exit(EXIT_CORRUPTION); // Code 2
    }
    
    println!("Database integrity verified.");
    process::exit(EXIT_SUCCESS); // Code 0
}

/// `ndb repair <path>` — quarantine a corrupt journal and truncate it at the
/// last complete record.
///
/// Recovery is destructive by nature, so the pre-repair bytes are copied to
/// `<file>.corrupt.<timestamp>.bak` first and the exact preserved/lost counts
/// are printed. Nothing is silent.
fn handle_repair(args: &[String]) {
    if args.len() != 1 {
        eprintln!("Usage: ndb repair <path>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let input = Path::new(&args[0]);
    let dir = if input.is_dir() {
        input.to_path_buf()
    } else {
        input
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Path::new(".").to_path_buf())
    };

    // v2: quarantine and truncate the WAL's bad tail under the maintenance lock.
    if dir.join(ndb::MANIFEST_FILE).exists() {
        let manifest = match ndb::read_manifest(&dir) {
            Ok(manifest) => manifest,
            Err(e) => fatal(&format!("read {}: {e}", dir.display())),
        };
        let journal = dir.join("data.jsonl");
        let _lock = match ndb::MaintenanceLock::acquire(&journal) {
            Ok(lock) => lock,
            Err(e) => fatal(&format!(
                "cannot repair {}: {e} (is a service still running on this database?)",
                dir.display()
            )),
        };
        let wal_path = ndb::wal_file_path(&dir, manifest.wal_gen);
        let stats = match ndb::repair_wal_file(&wal_path, manifest.base_seq) {
            Ok(stats) => stats,
            Err(e) => fatal(&format!("repair {}: {e}", wal_path.display())),
        };
        println!("WAL: {}", wal_path.display());
        println!(
            "Preserved: {} frame(s), {} of {} byte(s)",
            stats.frames_kept, stats.good_bytes, stats.total_bytes
        );
        match &stats.backup {
            Some(backup) => {
                println!(
                    "Removed: {} byte(s) after frame {}",
                    stats.truncated_bytes,
                    stats.frames_kept
                );
                println!("Original preserved at: {}", backup.display());
            }
            None => println!("Nothing to repair: the WAL is intact."),
        }
        process::exit(EXIT_SUCCESS);
    }

    let journal: std::path::PathBuf = if input.is_dir() {
        input.join("data.jsonl")
    } else if input.exists() {
        input.to_path_buf()
    } else {
        fatal(&format!("no journal or database folder at {}", input.display()));
    };
    if !journal.exists() {
        fatal(&format!("journal not found: {}", journal.display()));
    }

    // Truncating a journal under a live writer destroys the data the writer is
    // piling up. Refuse instead of racing it.
    let _lock = match ndb::MaintenanceLock::acquire(&journal) {
        Ok(lock) => lock,
        Err(e) => fatal(&format!(
            "cannot repair {}: {e} (is a service still running on this database?)",
            journal.display()
        )),
    };

    let report = match ndb::storage::repair_journal(&journal) {
        Ok(report) => report,
        Err(e) => fatal(&format!("repair {}: {e}", journal.display())),
    };

    println!("Journal: {}", journal.display());
    println!(
        "Preserved: {} record(s), {} of {} byte(s)",
        report.kept_records, report.kept_bytes, report.total_bytes
    );
    match (&report.backup, report.removed_from_line) {
        (Some(backup), Some(line)) => {
            println!(
                "Removed: {} line(s) from line {} ({} byte(s))",
                report.removed_lines, line, report.removed_bytes
            );
            println!("Original preserved at: {}", backup.display());
        }
        _ => println!("Nothing to repair: the journal is intact."),
    }
    process::exit(EXIT_SUCCESS);
}

fn handle_recover(args: &[String]) {    if args.len() < 3 || args[1] != "--output" {
        eprintln!("Usage: ndb recover <src> --output <dest>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    let src_path = Path::new(&args[0]);
    let dest_path = Path::new(&args[2]);
    
    if dest_path.exists() {
        eprintln!("Error: Destination path already exists. Must be empty.");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    eprintln!("[1/2] Creating recover destination...");
    if let Err(e) = fs::create_dir_all(dest_path) {
        eprintln!("Failed to create destination dir: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    // Copy meta.json safely
    let meta_src = src_path.join("meta.json");
    let meta_dst = dest_path.join("meta.json");
    if meta_src.exists() {
        if let Err(e) = fs::copy(&meta_src, &meta_dst) {
            fatal(&format!("copy meta.json: {e}"));
        }
    } else {
        // Mock a meta if missing (object-form buckets; B1)
        let meta = serde_json::json!({
            "version": 1,
            "created": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            "buckets": {},
        });
        if let Err(e) = fs::write(&meta_dst, serde_json::to_string_pretty(&meta).unwrap()) {
            fatal(&format!("write meta.json: {e}"));
        }
    }
    
    // Attempt bucket recovery
    let buckets_src = src_path.join("_files");
    if buckets_src.exists() {
        if let Err(e) = copy_dir_recursive(&buckets_src, &dest_path.join("_files")) {
            fatal(&format!("recover file buckets: {e}"));
        }
    }
    
    eprintln!("[2/2] Scanning data.jsonl for surviving rows...");
    let db_src = src_path.join("data.jsonl");
    let target_db = dest_path.join("data.jsonl");
    
    // Create the trash root (the library manages _trash/docs/ on demand)
    if let Err(e) = fs::create_dir_all(dest_path.join("_trash")) {
        fatal(&format!("create trash directory: {e}"));
    }

    if db_src.exists() {
        use std::io::{BufRead, BufReader, Write};
        let src_file = match fs::File::open(&db_src) {
            Ok(file) => file,
            Err(e) => fatal(&format!("open source data.jsonl: {e}")),
        };
        let reader = BufReader::new(src_file);
        
        let mut dest_file = match fs::File::create(&target_db) {
            Ok(file) => file,
            Err(e) => fatal(&format!("create destination data.jsonl: {e}")),
        };
        
        let mut salvaged = 0;
        let mut skipped = 0;
        
        for (i, line_res) in reader.lines().enumerate() {
            if let Ok(line) = line_res {
                if line.trim().is_empty() { continue; }
                if serde_json::from_str::<serde_json::Value>(&line).is_ok() {
                    // A write that fails here would report the row as salvaged
                    // while leaving it out of the file.
                    if let Err(e) = writeln!(dest_file, "{}", line) {
                        fatal(&format!("Line {}: write failed: {e}", i + 1));
                    }
                    salvaged += 1;
                } else {
                    eprintln!("Line {}: Corrupted - Skipping", i+1);
                    skipped += 1;
                }
            } else {
                eprintln!("Line {}: I/O Read Error - Skipping", i+1);
                skipped += 1;
            }
        }
        
        if let Err(e) = dest_file.sync_all() {
            fatal(&format!("flush recovered data.jsonl: {e}"));
        }
        eprintln!("Recovered {} rows, skipped {} corrupt rows.", salvaged, skipped);
    }
    
    println!("Recovery complete. Rescued database ready at {}", dest_path.display());
    process::exit(EXIT_SUCCESS);
}

fn handle_dump(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ndb dump <path>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let path = Path::new(&args[0]);
    let db_path = path.join("data.jsonl");

    let db = match ndb::Database::open(if db_path.exists() { &db_path } else { path }) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open database: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    for id in db.get_all_ids() {
        if let Ok(doc) = db.get(&id) {
            println!("{}", serde_json::to_string(&doc).unwrap_or_default());
        }
    }
    process::exit(EXIT_SUCCESS);
}

fn handle_config(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ndb config <get|set> <key> [value]");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let _path = Path::new("."); // By default config usually has the path passed or assumes current directory? 
    // Wait, the spec says `ndb config get display.title`. That assumes the command needs to know the db path or runs in it.
    // Actually wait, let me look at `cli-spec.md` again for `config`:
    // `$ ndb config get display.title`
    // Wait, there's no path parameter in the spec example? Let's check `cli-spec.md` line 186.
    // The spec usually expects the path if not in the current dir. Let's assume args[0] is the DB path, missing from spec?
    // Let me check what `get_schema` in the cli-spec has. Ah, the "usage" inside ndb.rs says: `config <get|set> ...`
    // I am going to require the path as `args[0]` then `get|set` as `args[1]` for safety, or parse like `ndb config <path> get <key>`.
    
    let (action, key, val) = match args[0].as_str() {
        "get" | "set" => {
            if args.len() == 2 && args[0] == "get" {
                ("get", args[1].clone(), None)
            } else if args.len() == 3 && args[0] == "set" {
                ("set", args[1].clone(), Some(args[2].clone()))
            } else {
                eprintln!("Usage: ndb config <get|set> <key> [value]");
                process::exit(EXIT_GENERAL_ERROR);
            }
        },
        _ => {
            eprintln!("Usage: ndb config <get|set> <key> [value] (Run inside DB folder)");
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    let meta_path = Path::new("meta.json");
    if !meta_path.exists() {
        eprintln!("Error: meta.json not found in current directory.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    let mut meta: serde_json::Value = match fs::read_to_string(meta_path) {
        Ok(c) => serde_json::from_str::<serde_json::Value>(&c).unwrap_or_else(|e| {
            eprintln!("Error: meta.json is not valid JSON: {e}");
            process::exit(EXIT_GENERAL_ERROR);
        }),
        Err(e) => {
            eprintln!("Failed to read meta.json: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    if action == "get" {
        // Simple dot notation split for object traversal
        let mut current = &meta;
        for part in key.split('.') {
            if let Some(obj) = current.as_object() {
                current = obj.get(part).unwrap_or(&serde_json::Value::Null);
            } else {
                current = &serde_json::Value::Null;
                break;
            }
        }
        match current {
            serde_json::Value::String(s) => println!("{}", s),
            serde_json::Value::Null => println!("(null)"),
            other => println!("{}", other),
        }
    } else if action == "set" {
        if let Some(value_str) = val {
            // `buckets` is set as a comma-separated name list and stored in
            // the object form the library expects; a policy block already
            // present for a listed name is preserved (B1). Also covers the
            // single-name case, which previously wrote a bare string.
            let parsed_val: serde_json::Value = if key == "buckets" {
                let mut existing = buckets_map(&meta);
                let mut out = serde_json::Map::new();
                for name in value_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    let block = existing.remove(name).unwrap_or_else(|| serde_json::json!({}));
                    out.insert(name.to_string(), block);
                }
                serde_json::Value::Object(out)
            } else if let Ok(num) = value_str.parse::<i64>() {
                serde_json::Value::Number(num.into())
            } else if let Ok(v) = serde_json::from_str(&value_str) {
                v
            } else {
                serde_json::Value::String(value_str)
            };

            let parts: Vec<&str> = key.split('.').collect();
            let mut current = &mut meta;
            for (i, part) in parts.iter().enumerate() {
                if i == parts.len() - 1 {
                    if let Some(obj) = current.as_object_mut() {
                        obj.insert(part.to_string(), parsed_val.clone());
                    }
                } else {
                    if !current.as_object().map(|o| o.contains_key(*part)).unwrap_or(false) {
                        if let Some(obj) = current.as_object_mut() {
                            obj.insert(part.to_string(), serde_json::json!({}));
                        }
                    }
                    current = current.get_mut(*part).unwrap();
                }
            }
            if let Err(e) = fs::write(meta_path, serde_json::to_string_pretty(&meta).unwrap()) {
                eprintln!("Failed to write meta.json: {}", e);
                process::exit(EXIT_GENERAL_ERROR);
            }
            println!("Updated '{}'", key);
        }
    }
    process::exit(EXIT_SUCCESS);
}

fn handle_query(args: &[String]) {
    if args.len() < 2 {
        eprintln!("Usage: ndb query <path> <query_ast>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let path = Path::new(&args[0]);
    let query_str = &args[1];

    let query: std::collections::HashMap<String, serde_json::Value> = match serde_json::from_str(query_str) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("Invalid JSON query: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    let db_path = path.join("data.jsonl");
    let db = match ndb::Database::open(if db_path.exists() { &db_path } else { path }) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open database: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    let mut matches = 0;
    for id in db.get_all_ids() {
        if let Ok(doc) = db.get(&id) {
            let mut matched = true;
            for (k, v) in &query {
                if let Some(doc_val) = doc.get(k) {
                    if let Some(op_obj) = v.as_object() {
                        if let Some(eq_val) = op_obj.get("$eq") {
                            if doc_val != eq_val { matched = false; break; }
                        }
                    } else if doc_val != v {
                        matched = false; break;
                    }
                } else {
                    matched = false; break;
                }
            }
            if matched {
                println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
                matches += 1;
            }
        }
    }
    eprintln!("Found {} results.", matches);
    process::exit(EXIT_SUCCESS);
}
