use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
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

/// Report a failed integrity audit with the corruption exit code, so a caller
/// can tell "the database is damaged" from "the command could not run".
fn corrupt_fatal(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    process::exit(EXIT_CORRUPTION);
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
    eprintln!("  init <path> [--buckets a,b] [--ram]  Initialize a v3 (RAM-first) database");
    eprintln!("  migrate <path>                Migrate a v1 journal to v3 RAM-first");
    eprintln!("  destroy <path> --force        Safely delete a database");
    eprintln!("  info <path>                   Show database statistics");
    eprintln!("  compact <path>                Compact the database in-place");
    eprintln!("  export <path> <dest>          Create a portable snapshot");
    eprintln!("  export <path> <out.jsonl> --jsonl   Export human-readable JSONL");
    eprintln!("  import <src> <path>           Restore a snapshot");
    eprintln!("  merge <base> <merge-in>       Combine databases");
    eprintln!("  verify <path>                 Check for corruptions");
    eprintln!("  fsck <path>                   Full integrity check (v1 or v3 folder)");
    eprintln!("  repair <path>                 Quarantine/truncate a corrupt v1 journal or v3 WAL");
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
    eprintln!("  --format <auto|v1|ram>  Format for databases created via POST /v2/dbs or a --db mount of a not-yet-existing path (default ram)");
    eprintln!("  --scheduled-ms <n>  Flush interval for persistence=scheduled (default 5000)");
    eprintln!("  --max-limit <n>     Hard cap for an explicit query limit (default 50000)");
    eprintln!("  --default-limit <n> Page size when a query omits limit (default 1000)");
    eprintln!("  --threads <n>     Worker threads: connections + index builds (default: all cores)");
}

fn handle_serve(args: &[String]) {
    use ndb::server::{
        serve_registry, DbRegistry, PersistenceMode, ServiceConfig, ServerConfig,
        DEFAULT_SCHEDULED_MS,
    };

    let mut config = ServerConfig::default();
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
    let create_format = cli_format
        .or(file_format)
        .unwrap_or(ndb::StorageFormat::Ram);
    registry.set_create_format(create_format);

    // A mount of a path that does not exist yet creates the database, and it
    // must be created in the format this serve was told to use. An existing
    // folder keeps `Auto`, so detection (v1 `data.jsonl`, v3 `manifest.json`)
    // — never a format flag — decides what is opened.
    let mount_format = |path: &str| {
        if std::path::Path::new(path).exists() {
            ndb::StorageFormat::Auto
        } else {
            create_format
        }
    };

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
            .mount_resolved_format(
                name,
                path,
                persistence.to_persistence(scheduled_ms),
                mount_format(path),
            )
            .unwrap_or_else(|e| fatal(&format!("mounting '{name}': {e}")));
    }

    for (name, path, mode, sched) in &declared_dbs {
        let mode = mode.unwrap_or(persistence);
        let p = mode.to_persistence(sched.unwrap_or(scheduled_ms));
        registry
            .mount_resolved_format(name, path, p, mount_format(path))
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
        eprintln!("Usage: ndb init <path> [--buckets a,b] [--ram | --v3]");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let path = Path::new(&args[0]);
    if path.exists() {
        eprintln!("Error: Path already exists. Must be an empty directory.");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    // Parse buckets. `--ram` / `--v3` are accepted and redundant: v3 RAM-first is
    // the only backend `init` creates.
    let mut buckets = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--buckets" && i + 1 < args.len() {
            buckets = args[i + 1].split(',').map(|s| s.trim().to_string()).collect();
            i += 2;
            continue;
        }
        if args[i] == "--v2" {
            eprintln!(
                "Error: '--v2' names the retired binary/mmap backend. v3 (RAM-first) is the \
                 only binary format; use '--ram' (the default) instead."
            );
            process::exit(EXIT_GENERAL_ERROR);
        }
        if args[i] == "--ram" || args[i] == "--v3" {
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

    // Initialise the v3 RAM-first database: the engine writes `base.gen_1.ndb`,
    // `wal.gen_1.ndb` and a version-3 `manifest.json`. It also refuses a folder
    // holding a v1 journal, so this path can never shadow existing documents.
    match ndb::Database::open_ram(path) {
        Ok(db) => drop(db),
        Err(e) => {
            eprintln!("Failed to initialise the v3 (RAM-first) database: {e}");
            process::exit(EXIT_GENERAL_ERROR);
        }
    }
    println!("Initialized v3 (RAM-first) database at {}", path.display());
    process::exit(EXIT_SUCCESS);
}

/// `ndb migrate <path> [--ram | --v3]` — convert a v1 (`data.jsonl`) folder into
/// the v3 RAM-first backend.
///
/// **v3 (RAM-first) is the only target.** A folder already holding a v2 mmap
/// `manifest.json` is refused: that engine was retired, and the conversion must
/// run on the preserved v2/transition binary first.
fn handle_migrate(args: &[String]) {
    let mut path_index: Option<usize> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ram" | "--v3" => {}
            "--to" => {
                if i + 1 >= args.len() {
                    fatal("--to requires a value (ram)");
                }
                i += 1;
                match ndb::StorageFormat::parse(&args[i]) {
                    Ok(ndb::StorageFormat::Ram) => {}
                    Ok(other) => fatal(&format!(
                        "--to accepts ram (v3) only, not '{}'",
                        other.as_str()
                    )),
                    Err(e) => fatal(&e.to_string()),
                }
            }
            other if other.starts_with("--") => {
                fatal(&format!("unknown migrate argument '{other}'"))
            }
            _ => {
                if path_index.is_some() {
                    fatal("migrate takes exactly one path");
                }
                path_index = Some(i);
            }
        }
        i += 1;
    }
    let Some(path_index) = path_index else {
        eprintln!("Usage: ndb migrate <path> [--ram | --v3]");
        process::exit(EXIT_GENERAL_ERROR);
    };

    let input = Path::new(&args[path_index]);
    let dir = if input.is_dir() {
        input.to_path_buf()
    } else {
        input
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| Path::new(".").to_path_buf())
    };
    let journal = dir.join("data.jsonl");

    // Source format is read from the manifest, never guessed from a filename.
    let source_version = manifest_version(&dir);
    match source_version {
        Some(3) => fatal(&format!(
            "{} is already a v3 (RAM-first) database",
            dir.display()
        )),
        Some(2) => fatal(&format!(
            "{} is a v2 (binary/mmap) database, and that engine was retired. Convert it with the \
             preserved v2/transition binary first (`ndb migrate {} --ram`), then reopen it here",
            dir.display(),
            dir.display()
        )),
        Some(other) => fatal(&format!(
            "{} holds a manifest with unknown version {other}",
            dir.display()
        )),
        None => {
            if !journal.exists() {
                fatal(&format!("no v1 journal at {}", journal.display()));
            }
            migrate_to_ram(&dir);
        }
    }
}

/// Read a folder's `manifest.json` version without binding to either schema:
/// v2 names the field `format_version`, v3 names it `version`. `None` means the
/// folder holds no manifest (an absent v2/v3 database is a v1 journal).
fn manifest_version(dir: &Path) -> Option<u32> {
    let path = dir.join("manifest.json");
    if !path.exists() {
        return None;
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => fatal(&format!("read {}: {e}", path.display())),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => fatal(&format!("{} is not valid JSON: {e}", path.display())),
    };
    match value
        .get("version")
        .or_else(|| value.get("format_version"))
        .and_then(|v| v.as_u64())
    {
        Some(version) => match u32::try_from(version) {
            Ok(version) => Some(version),
            Err(_) => fatal(&format!(
                "{} declares an out-of-range version {version}",
                path.display()
            )),
        },
        None => fatal(&format!(
            "{} has neither a usable `version` nor `format_version` field",
            path.display()
        )),
    }
}

/// `migrate`: a v1 (`data.jsonl`) folder → v3 RAM-first.
///
/// Staged in `<dir>/.migrating_ram/` and verified document-by-document before
/// anything in the live folder moves. The source journal is preserved *before*
/// the v3 manifest is published, so v1 stays the sole authority until the folder
/// genuinely is a v3 database: `data.jsonl` becomes `data.jsonl.v1.bak`.
fn migrate_to_ram(dir: &Path) {
    let journal = dir.join("data.jsonl");
    let db = match ndb::Database::open(&journal) {
        Ok(db) => db,
        Err(e) => fatal(&format!(
            "cannot open {}: {e} (is a service still running on this database?)",
            dir.display()
        )),
    };
    let lineage = db.lineage();
    let snapshot = db.snapshot_at();
    let mut docs: Vec<(String, u64, serde_json::Value)> = Vec::with_capacity(snapshot.docs.len());
    for (id, doc) in &snapshot.docs {
        // A pre-lineage v1 journal carries no per-document seq; the source
        // `last_seq` is then the honest upper bound.
        let doc_seq = db.doc_seq(id).unwrap_or(lineage.last_seq);
        docs.push((id.clone(), doc_seq, doc.clone()));
    }
    // v1 never persists index definitions.
    let indexes: Vec<ndb::ram_format::IndexSpec> = Vec::new();
    drop(db);

    let staging = dir.join(".migrating_ram");
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

    let io = ndb::io::RealIo::new();
    let base_path = staging.join(ndb::ram_format::base_file_name(1));
    let wal_path = staging.join(ndb::ram_format::wal_file_name(1));
    let records: Vec<std::sync::Arc<ndb::docval::DocRecord>> = docs
        .iter()
        .map(|(id, doc_seq, doc)| {
            ndb::docval::DocRecord::new(
                std::sync::Arc::from(id.as_str()),
                *doc_seq,
                ndb::docval::DocVal::from_value(doc),
            )
        })
        .collect();

    if let Err(e) =
        ndb::ram_format::write_ram_base_file(&base_path, lineage.last_seq, &records, &io)
    {
        cleanup_staging(&staging);
        fatal(&format!("write v3 base generation: {e}"));
    }
    if let Err(e) = ndb::ram_format::RamWalWriter::create_or_open(
        &wal_path,
        1,
        std::sync::Arc::new(ndb::io::RealIo::new()),
    ) {
        cleanup_staging(&staging);
        fatal(&format!("write v3 WAL generation: {e}"));
    }
    let manifest = ndb::ram_format::RamManifest {
        version: ndb::ram_format::RAM_STORAGE_VERSION,
        source_epoch_uuid: lineage.source_epoch_uuid.clone(),
        base_gen: 1,
        wal_gen: 1,
        pending_wal_gen: None,
        last_seq: lineage.last_seq,
        // Every document is folded into `base.gen_1.ndb` and the WAL holds no
        // events, so nothing before `last_seq + 1` is replayable through the
        // change feed. Carrying a lower source value would advertise history
        // that no longer exists — and the engine reports `last_seq + 1` for
        // this folder anyway.
        min_available_seq: lineage.last_seq.saturating_add(1),
        indexes: indexes.clone(),
    };
    if let Err(e) = ndb::ram_format::write_ram_manifest_atomic(&staging, &manifest, &io) {
        cleanup_staging(&staging);
        fatal(&format!("write v3 manifest: {e}"));
    }

    // Verify the staged folder before anything live moves: it must open, carry
    // every document, reproduce each JSON value exactly, and keep the lineage
    // and every index declaration.
    let staged = match ndb::Database::open_ram(&staging) {
        Ok(db) => db,
        Err(e) => {
            cleanup_staging(&staging);
            fatal(&format!("staged v3 database does not open: {e}"));
        }
    };
    verify_ram_parity("staged v3 database", &staged, &staging, &lineage, &indexes, &docs);
    drop(staged);

    // Publish: generations first, then the manifest (the sole authority).
    for name in [
        ndb::ram_format::base_file_name(1),
        ndb::ram_format::wal_file_name(1),
        ndb::MANIFEST_FILE.to_string(),
    ] {
        let from = staging.join(&name);
        let to = dir.join(&name);
        if let Err(e) = fs::rename(&from, &to) {
            fatal(&format!(
                "move {name} into {}: {e} — the source journal is preserved at {}; \
                 move it back to roll this migration back",
                dir.display(),
                journal.display()
            ));
        }
    }
    if let Err(e) = fs::remove_dir_all(&staging) {
        eprintln!("warning: could not remove {}: {e}", staging.display());
    }

    let backup = dir.join("data.jsonl.v1.bak");
    let v1_bytes = fs::metadata(&journal).map(|m| m.len()).unwrap_or(0);
    if let Err(e) = fs::rename(&journal, &backup) {
        fatal(&format!(
            "rename {} to {}: {e} — the v3 manifest is already published; \
             move the journal back before reopening as v1",
            journal.display(),
            backup.display()
        ));
    }

    // Reopen what is now the live database and prove parity a second time.
    let live = match ndb::Database::open_ram(dir) {
        Ok(db) => db,
        Err(e) => fatal(&format!(
            "the migrated v3 database does not open: {e}"
        )),
    };
    verify_ram_parity("migrated v3 database", &live, dir, &lineage, &indexes, &docs);
    drop(live);

    let base_size = fs::metadata(dir.join(ndb::ram_format::base_file_name(1)))
        .map(|m| m.len())
        .unwrap_or(0);
    let wal_size = fs::metadata(dir.join(ndb::ram_format::wal_file_name(1)))
        .map(|m| m.len())
        .unwrap_or(0);
    println!("Migrated {} to v3 (RAM-first)", dir.display());
    println!("Source: v1 journal");
    println!("Documents: {}", docs.len());
    println!("source_epoch_uuid: {}", lineage.source_epoch_uuid);
    println!("last_seq: {}", lineage.last_seq);
    println!(
        "min_available_seq: {}",
        lineage.last_seq.saturating_add(1)
    );
    if indexes.is_empty() {
        println!("indexes: none");
    } else {
        println!("indexes: {}", indexes.len());
        for spec in &indexes {
            println!("  - {} ({:?})", spec.field, spec.kind);
        }
    }
    println!("v3 base + WAL: {} bytes", base_size + wal_size);
    println!("v1 journal: {v1_bytes} bytes (kept at data.jsonl.v1.bak)");
    process::exit(EXIT_SUCCESS);
}

/// Strict doc-by-doc parity between a v3 folder and the source it was built
/// from: count, ids, every JSON value, the lineage and every index declaration.
fn verify_ram_parity(
    label: &str,
    db: &ndb::Database,
    dir: &Path,
    lineage: &ndb::SourceLineage,
    indexes: &[ndb::ram_format::IndexSpec],
    docs: &[(String, u64, serde_json::Value)],
) {
    if db.len() != docs.len() {
        fatal(&format!(
            "{label} holds {} documents, expected {}",
            db.len(),
            docs.len()
        ));
    }
    let live = db.lineage();
    if live.source_epoch_uuid != lineage.source_epoch_uuid {
        fatal(&format!(
            "{label} epoch is {}, the source is {}",
            live.source_epoch_uuid, lineage.source_epoch_uuid
        ));
    }
    if live.last_seq != lineage.last_seq {
        fatal(&format!(
            "{label} last_seq is {}, the source is {}",
            live.last_seq, lineage.last_seq
        ));
    }
    if live.min_available_seq != lineage.last_seq.saturating_add(1) {
        fatal(&format!(
            "{label} min_available_seq is {}, expected {} (no event history precedes the base)",
            live.min_available_seq,
            lineage.last_seq.saturating_add(1)
        ));
    }
    let mut actual_ids = db.get_all_ids();
    actual_ids.sort();
    let mut expected_ids: Vec<String> = docs.iter().map(|(id, _, _)| id.clone()).collect();
    expected_ids.sort();
    if actual_ids != expected_ids {
        fatal(&format!("{label} document ids do not match the source"));
    }
    for (id, _, expected) in docs {
        match db.get(id) {
            Ok(actual) => {
                if &actual != expected {
                    fatal(&format!(
                        "document {id:?} does not round-trip through {label}"
                    ));
                }
            }
            Err(e) => fatal(&format!("{label} is missing {id:?}: {e}")),
        }
    }
    let written = match ndb::ram_format::read_ram_manifest(dir) {
        Ok(manifest) => manifest,
        Err(e) => fatal(&format!("read {label} manifest: {e}")),
    };
    if written.indexes != indexes {
        fatal(&format!(
            "{label} index declarations do not match the source ({:?} vs {:?})",
            written.indexes, indexes
        ));
    }
}

fn cleanup_staging(staging: &Path) {
    if let Err(e) = fs::remove_dir_all(staging) {
        eprintln!(
            "warning: could not remove the partial migration at {}: {e}",
            staging.display()
        );
    }
}

/// `ndb fsck <path>` — full integrity audit of a v1 journal, a v2 folder or a
/// v3 (RAM-first) folder.
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

    match manifest_version(&dir) {
        Some(ndb::ram_format::RAM_STORAGE_VERSION) => {
            let manifest = match ndb::ram_format::read_ram_manifest(&dir) {
                Ok(manifest) => manifest,
                Err(e) => corrupt_fatal(&format!("read {}: {e}", dir.display())),
            };
            let report = match ndb::ram_format::fsck_ram_dir(&dir) {
                Ok(report) => report,
                Err(e) => corrupt_fatal(&format!("fsck {}: {e}", dir.display())),
            };
            println!("Database: {}", dir.display());
            println!("Format: v3 (RAM-first)");
            println!(
                "base.gen_{}: {} documents, base_seq {}",
                report.base_gen, report.doc_count, report.base_seq
            );
            println!(
                "wal.gen_{}: {} committed batch(es)",
                report.wal_gen, report.wal_batches
            );
            if let Some(pending) = report.pending_wal_gen {
                println!(
                    "pending wal.gen_{pending}: {} committed batch(es)",
                    report.pending_batches.unwrap_or(0)
                );
            }
            println!("last_seq: {}", report.last_seq);
            println!("min_available_seq: {}", manifest.min_available_seq);
            println!("source_epoch_uuid: {}", manifest.source_epoch_uuid);
            if report.indexes.is_empty() {
                println!("indexes: 0");
            } else {
                println!("indexes: {}", report.indexes.len());
            }
            for warning in &report.warnings {
                println!("warning: {warning}");
            }
            match check_bucket_files(&dir) {
                Ok(count) => println!("bucket files: {count} verified"),
                Err(e) => corrupt_fatal(&format!("fsck {}: {e}", dir.display())),
            }
            println!("OK");
            process::exit(EXIT_SUCCESS);
        }
        Some(version) if version != 2 => corrupt_fatal(&format!(
            "{} holds a manifest with unknown version {version}",
            dir.display()
        )),
        Some(_) => corrupt_fatal(&format!(
            "{} is a v2 (binary/mmap) database, and that engine was retired. Convert it with the \
             preserved v2/transition binary (`ndb migrate {} --ram`) before auditing it here",
            dir.display(),
            dir.display()
        )),
        None => {}
    }

    // v1: verify the journal and the doc trash without opening for write.
    let journal = dir.join("data.jsonl");
    if !journal.exists() {
        corrupt_fatal(&format!(
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

    if manifest_version(path) == Some(ndb::ram_format::RAM_STORAGE_VERSION) {
        let manifest = match ndb::ram_format::read_ram_manifest(path) {
            Ok(manifest) => manifest,
            Err(e) => fatal(&format!("read {}: {e}", path.display())),
        };
        let base_size = fs::metadata(ndb::ram_format::base_file_path(path, manifest.base_gen))
            .map(|m| m.len())
            .unwrap_or(0);
        let wal_size = fs::metadata(ndb::ram_format::wal_file_path(path, manifest.wal_gen))
            .map(|m| m.len())
            .unwrap_or(0);
        let meta: serde_json::Value = fs::read_to_string(&meta_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(serde_json::Value::Null);
        let doc_count = match ndb::ram_format::read_ram_base_file(
            &ndb::ram_format::base_file_path(path, manifest.base_gen),
        ) {
            Ok(base) => base.doc_count,
            Err(e) => fatal(&format!("read base generation: {e}")),
        };
        println!("Status: Valid");
        println!("Format: v3 (RAM-first)");
        println!("Database Path: {}", path.display());
        println!("Schema Version: {}", meta.get("version").and_then(|v| v.as_i64()).unwrap_or(0));
        println!("Active Documents: {doc_count}");
        println!("base.gen_{}: {} bytes", manifest.base_gen, base_size);
        println!("wal.gen_{}: {} bytes", manifest.wal_gen, wal_size);
        if let Some(pending) = manifest.pending_wal_gen {
            println!("pending_wal_gen: {pending}");
        }
        println!("last_seq: {}", manifest.last_seq);
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

    if path.join(ndb::MANIFEST_FILE).exists() {
        fatal(&format!(
            "{} is a v2 (binary/mmap) database, and that engine was retired. Convert it with the \
             preserved v2/transition binary (`ndb migrate {} --ram`) before inspecting it here",
            path.display(),
            path.display()
        ));
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
    // A folder names the database; only a bare file path is a v1 journal. This
    // keeps `export <v2|v3 folder> <dest>` from creating a stray `data.jsonl`
    // beside the binary generations.
    let source = if src_path.is_dir() { src_path } else { &db_path };
    let db = match ndb::Database::open(source) {
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

/// Verify every file reference a document holds: the nested `_file` object form
/// **and** every string form the engine's own mark phase recognizes — compact
/// `bucket:hash.ext` nURIs and the `/api/buckets/…`, `/file/…`, `/files/…`,
/// `/v2/dbs/<db>/files/…` URLs (#12).
///
/// A checker narrower than the collector is worse than none: it reports a clean
/// database while never looking at the references the engine actually honours.
fn check_file_refs(val: &serde_json::Value, base_path: &Path) -> Result<(), String> {
    // 1. Nested `_file` objects carry structured fields rather than a string,
    //    so the normalizer cannot see them.
    if let Some(obj) = val.as_object() {
        if let Some(file_ref) = obj.get("_file").and_then(|f| f.as_object()) {
            let bucket = file_ref.get("bucket").and_then(|x| x.as_str()).unwrap_or("");
            let id = file_ref.get("id").and_then(|x| x.as_str()).unwrap_or("");
            let ext = file_ref.get("ext").and_then(|x| x.as_str()).unwrap_or("");
            if !bucket.is_empty() && !id.is_empty() {
                let path = base_path
                    .join("_files")
                    .join(bucket)
                    .join(format!("{id}.{ext}"));
                if !path.exists() {
                    return Err(format!("{bucket}:{id}.{ext}"));
                }
            }
        }
    }

    // 2. Every string form, normalized exactly like `gc_buckets` does.
    let mut refs = std::collections::HashSet::new();
    ndb::Database::extract_file_refs(val, &mut refs);
    for compact in refs {
        let file_ref = match ndb::FileRef::from_compact(&compact) {
            Some(file_ref) => file_ref,
            None => return Err(format!("{compact} (unparseable normalized ref)")),
        };
        let path = base_path
            .join("_files")
            .join(&file_ref.bucket)
            .join(file_ref.filename());
        if !path.exists() {
            return Err(compact);
        }
    }
    Ok(())
}

fn handle_verify(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ndb verify <path>");
        process::exit(EXIT_GENERAL_ERROR);
    }
    // A binary folder has no JSONL journal: the full audit is `fsck`.
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

    // v3 (RAM-first): quarantine and truncate each WAL generation's bad tail
    // under the maintenance lock.
    if manifest_version(&dir) == Some(ndb::ram_format::RAM_STORAGE_VERSION) {
        let manifest = match ndb::ram_format::read_ram_manifest(&dir) {
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
        let io = ndb::io::RealIo::new();
        let mut generations = vec![("wal", manifest.wal_gen)];
        if let Some(pending) = manifest.pending_wal_gen {
            generations.push(("pending wal", pending));
        }
        let mut primary: Option<(u64, u64, Option<PathBuf>)> = None;
        for (label, gen) in generations {
            let path = ndb::ram_format::wal_file_path(&dir, gen);
            let report = match ndb::ram_format::repair_ram_wal(&path, &io) {
                Ok(report) => report,
                Err(e) => fatal(&format!("repair {}: {e}", path.display())),
            };
            println!("{label}.gen_{gen}: {}", path.display());
            println!(
                "Preserved: {} batch(es), {} of {} byte(s), through seq {}",
                report.batches_kept, report.good_offset, report.scanned_bytes, report.last_valid_seq
            );
            if report.truncated_bytes > 0 {
                println!(
                    "Removed: {} byte(s) after byte {}",
                    report.truncated_bytes, report.good_offset
                );
                match &report.quarantine_path {
                    Some(path) => println!("Original preserved at: {}", path.display()),
                    None => eprintln!("warning: no quarantine copy was written"),
                }
            } else {
                println!("Nothing to repair: the WAL is intact.");
            }
            if let Some(issue) = &report.issue {
                println!("Issue: {issue}");
            }
            if label == "wal" {
                primary = Some((
                    report.last_valid_seq,
                    report.truncated_bytes,
                    report.quarantine_path.clone(),
                ));
            }
        }
        if let Some((last_valid, truncated, quarantine)) = primary {
            if truncated == 0 {
                println!("Lost seq range: none (no committed record was removed)");
            } else {
                // The manifest's `last_seq` is published by compaction and lags
                // the WAL in normal operation, so it is a floor here, never the
                // upper bound of what the damaged tail held.
                println!(
                    "Lost seq range: from {} upward (the damaged tail is preserved at {})",
                    last_valid.saturating_add(1),
                    quarantine
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<no quarantine copy>".to_string())
                );
                if manifest.last_seq > last_valid {
                    println!(
                        "The manifest declared last_seq {}; nothing above {last_valid} is in the \
                         lineage now, so a reopen refuses until the lineage is reconciled",
                        manifest.last_seq
                    );
                }
            }
        }
        process::exit(EXIT_SUCCESS);
    }

    // v2: the retired binary/mmap format cannot be repaired here.
    if manifest_version(&dir) == Some(2) {
        fatal(&format!(
            "{} is a v2 (binary/mmap) database, and that engine was retired. Convert it with the \
             preserved v2/transition binary (`ndb migrate {} --ram`) before repairing it here",
            dir.display(),
            dir.display()
        ));
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
