//! ndb serve — resident HTTP daemon (third interface alongside CLI and the
//! embeddable Rust crate).
//!
//! Design: docs/ndb-serve-design.md (issue #2).
//! Hand-rolled HTTP/1.1 on std::net — zero new crates. Thread-per-connection
//! with a cap, keep-alive, read timeout, body cap, static bearer token.
//! The daemon owns the database in memory; clients receive only results.

use crate::bucket::mime_for_extension;
use crate::report;
use crate::{ChangeEvent, ChangeKind, Database, Error, PatchOp, Persistence, QueryOptions, SortDir};
use parking_lot::{Condvar, Mutex, RwLock};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::storage::Snapshot;

// ─── Config ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Bind address. "127.0.0.1" for local-only, "0.0.0.0" for LAN.
    pub bind: String,
    pub port: u16,
    /// Static bearer token. None = no auth (local-only setups).
    pub token: Option<String>,
    /// Max concurrent connections; excess gets 503.
    pub max_connections: usize,
    /// Max request body size (bytes). Excess gets 413.
    pub max_body: usize,
    /// Page size when a query omits `limit`.
    pub default_limit: usize,
    /// Hard cap for an explicit `limit`. Larger values are rejected (400),
    /// never silently truncated.
    pub max_limit: usize,
    /// Threads for index (re)builds. 0 = all logical cores.
    pub threads: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 8323,
            token: None,
            max_connections: 64,
            max_body: 64 * 1024 * 1024,
            default_limit: 1000,
            max_limit: 50_000,
            threads: 0,
        }
    }
}

// ─── Multi-DB Registry ──────────────────────────────────────────────

/// Default schedule interval when `persistence: "scheduled"` names no millis.
pub const DEFAULT_SCHEDULED_MS: u64 = 5_000;

/// Service-wide persistence selection (the CLI/HTTP-facing form of
/// [`Persistence`], which carries a `Duration` an operator does not type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceMode {
    Immediate,
    Scheduled,
    Lazy,
}

impl PersistenceMode {
    /// Parse the operator spelling. Anything else is refused — a typo in a
    /// persistence mode must not silently become the default.
    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "immediate" => Ok(PersistenceMode::Immediate),
            "scheduled" => Ok(PersistenceMode::Scheduled),
            "lazy" => Ok(PersistenceMode::Lazy),
            other => Err(Error::invalid_arg(format!(
                "persistence must be immediate | scheduled | lazy, got '{other}'"
            ))),
        }
    }

    /// Materialize the mode into a core [`Persistence`] value.
    pub fn to_persistence(self, scheduled_ms: u64) -> Persistence {
        match self {
            PersistenceMode::Immediate => Persistence::Immediate,
            PersistenceMode::Scheduled => Persistence::Scheduled(Duration::from_millis(scheduled_ms)),
            PersistenceMode::Lazy => Persistence::Lazy,
        }
    }
}

/// Why `POST /v2/dbs` (and `DbRegistry::create`) could not create a database.
#[derive(Debug)]
pub enum CreateDbError {
    /// No `root_dir` configured — the service refuses to invent a location.
    Disabled,
    InvalidName(String),
    AlreadyExists(String),
    Core(Error),
}

impl std::fmt::Display for CreateDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreateDbError::Disabled => write!(f, "database creation is disabled (no root dir)"),
            CreateDbError::InvalidName(name) => write!(f, "invalid database name: '{name}'"),
            CreateDbError::AlreadyExists(name) => write!(f, "database '{name}' already exists"),
            CreateDbError::Core(e) => write!(f, "{e}"),
        }
    }
}

/// The set of databases one `ndb serve` process owns.
///
/// Names are the HTTP identity; canonical paths are the filesystem identity.
/// Both are unique — the same database must never answer under two names, and
/// two names must never write the same journal.
pub struct DbRegistry {
    dbs: RwLock<HashMap<String, Arc<Database>>>,
    canonical_paths: RwLock<HashSet<PathBuf>>,
    root_dir: Option<PathBuf>,
    default_persistence: PersistenceMode,
    scheduled_ms: u64,
    /// Storage format used by `POST /v2/dbs` when the request names none.
    create_format: RwLock<crate::StorageFormat>,
    /// Frozen snapshots pinned by `GET /v2/dbs/{name}/snapshot`, so pages of
    /// one read never drift under concurrent writes.
    snapshots: RwLock<HashMap<String, PinnedSnapshot>>,
}

/// How long a pinned snapshot stays addressable. Long enough for a client to
/// walk a large export, short enough that abandoned pins free themselves.
const SNAPSHOT_TTL: Duration = Duration::from_secs(300);

struct PinnedSnapshot {
    db: String,
    snapshot: Arc<Snapshot>,
    created: Instant,
}

impl DbRegistry {
    pub fn new(
        root_dir: Option<PathBuf>,
        default_persistence: PersistenceMode,
        scheduled_ms: u64,
    ) -> Self {
        DbRegistry {
            dbs: RwLock::new(HashMap::new()),
            canonical_paths: RwLock::new(HashSet::new()),
            root_dir,
            default_persistence,
            scheduled_ms,
            create_format: RwLock::new(crate::StorageFormat::Auto),
            snapshots: RwLock::new(HashMap::new()),
        }
    }

    /// Storage format `POST /v2/dbs` creates when the request names none.
    pub fn create_format(&self) -> crate::StorageFormat {
        *self.create_format.read()
    }

    /// Set the default creation format (from `ndb-serve.json` / `--format`).
    pub fn set_create_format(&self, format: crate::StorageFormat) {
        *self.create_format.write() = format;
    }

    /// Freeze `snapshot` under an id the caller can page through.
    pub fn pin_snapshot(&self, db: &str, snapshot: Arc<Snapshot>) -> String {
        let mut pins = self.snapshots.write();
        pins.retain(|_, pin| pin.created.elapsed() < SNAPSHOT_TTL);
        let id = crate::storage::generate_epoch_uuid();
        pins.insert(
            id.clone(),
            PinnedSnapshot {
                db: db.to_string(),
                snapshot,
                created: Instant::now(),
            },
        );
        id
    }

    /// The frozen snapshot behind `id`, or `None` when it expired, was never
    /// issued, or belongs to another database.
    pub fn pinned_snapshot(&self, db: &str, id: &str) -> Option<Arc<Snapshot>> {
        let mut pins = self.snapshots.write();
        pins.retain(|_, pin| pin.created.elapsed() < SNAPSHOT_TTL);
        pins.get(id)
            .filter(|pin| pin.db == db)
            .map(|pin| Arc::clone(&pin.snapshot))
    }

    /// Root under which `POST /v2/dbs` may create databases (and from which
    /// `--dir` discovery mounts children). `None` disables creation.
    pub fn root_dir(&self) -> Option<&Path> {
        self.root_dir.as_deref()
    }

    pub fn default_persistence(&self) -> PersistenceMode {
        self.default_persistence
    }

    pub fn scheduled_ms(&self) -> u64 {
        self.scheduled_ms
    }

    /// A database name is a URL path segment and a folder name: non-empty
    /// ASCII `[a-zA-Z0-9._-]`, never `.`/`..`, never a Windows reserved device
    /// name (`CON`, `NUL`, `COM1`, `LPT9`, … including with an extension), and
    /// never ending in a dot or space — all of which Windows accepts at the
    /// API level and then fails to open or delete.
    pub fn validate_name(name: &str) -> Result<(), Error> {
        if name.is_empty() {
            return Err(Error::invalid_arg("database name must not be empty"));
        }
        if name == "." || name == ".." {
            return Err(Error::invalid_arg(format!(
                "database name '{name}' is a path component, not a name"
            )));
        }
        if let Some(bad) = name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-'))
        {
            return Err(Error::invalid_arg(format!(
                "database name '{name}' contains invalid character '{bad}' (allowed: a-z A-Z 0-9 . _ -)"
            )));
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Err(Error::invalid_arg(format!(
                "database name '{name}' must not end with a dot or space (Windows cannot open it)"
            )));
        }
        let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        if RESERVED.contains(&stem.as_str()) {
            return Err(Error::invalid_arg(format!(
                "database name '{name}' is a Windows reserved device name"
            )));
        }
        Ok(())
    }

    /// Mount a database, opening it with the registry's default persistence.
    pub fn mount(&self, name: &str, path: impl AsRef<Path>) -> Result<(), Error> {
        self.mount_with(name, path, self.default_persistence)
    }

    /// Mount a database with an already-resolved persistence mode.
    pub fn mount_resolved(
        &self,
        name: &str,
        path: impl AsRef<Path>,
        persistence: Persistence,
    ) -> Result<(), Error> {
        self.mount_resolved_format(name, path, persistence, crate::StorageFormat::Auto)
    }

    /// Mount a database with an explicit storage format (used by creation:
    /// a new folder asked to be v2 must initialise as v2).
    pub fn mount_resolved_format(
        &self,
        name: &str,
        path: impl AsRef<Path>,
        persistence: Persistence,
        format: crate::StorageFormat,
    ) -> Result<(), Error> {
        Self::validate_name(name)?;
        let resolved = resolve_db_path(path.as_ref());
        let normalized = normalize_path(&resolved);

        if self.dbs.read().contains_key(name) {
            return Err(Error::invalid_arg(format!("duplicate database name: '{name}'")));
        }
        // Windows and macOS fold case in the filesystem: `Memory` and `memory`
        // would be one folder reached by two names, and therefore two writers
        // on one journal. Refuse the collision instead.
        if let Some(existing) = self
            .dbs
            .read()
            .keys()
            .find(|k| k.eq_ignore_ascii_case(name))
            .cloned()
        {
            return Err(Error::invalid_arg(format!(
                "duplicate database name: '{name}' collides case-insensitively with '{existing}'"
            )));
        }
        if self.canonical_paths.read().contains(&normalized) {
            return Err(Error::invalid_arg(format!(
                "duplicate database path: {} (already mounted under another name)",
                normalized.display()
            )));
        }

        // `Database::open` errors carry the path and context; a failed open is
        // a service fault (500 over HTTP, fatal in the CLI), not a caller
        // argument error, so the kind is preserved rather than flattened.
        let db = Database::open_with_options(
            &normalized,
            crate::OpenOptions {
                format,
                persistence,
            },
        )?;

        // The file exists now, so the OS's own canonical form is checkable —
        // this catches case-folded and symlinked aliases lexical normalization
        // cannot see. Both spellings are recorded.
        let canonical = std::fs::canonicalize(&normalized).unwrap_or_else(|_| normalized.clone());

        let mut paths = self.canonical_paths.write();
        if !paths.insert(canonical.clone()) {
            return Err(Error::invalid_arg(format!(
                "duplicate database path: {}",
                canonical.display()
            )));
        }
        // On Windows `canonicalize` returns the verbatim `\\?\` form, which is
        // the same file as the normalized spelling — record it only when it
        // actually differs.
        if canonical != normalized && !paths.insert(normalized) {
            return Err(Error::invalid_arg(format!(
                "duplicate database path: {}",
                canonical.display()
            )));
        }
        drop(paths);

        let mut dbs = self.dbs.write();
        dbs.insert(name.to_string(), Arc::new(db));
        Ok(())
    }

    /// Mount a discovered or configured database under an explicit persistence.
    /// A path naming an existing directory (or a non-`.jsonl` path that does
    /// not exist yet) becomes `<path>/data.jsonl`.
    pub fn mount_with(
        &self,
        name: &str,
        path: impl AsRef<Path>,
        persistence: PersistenceMode,
    ) -> Result<(), Error> {
        self.mount_resolved(name, path, persistence.to_persistence(self.scheduled_ms))
    }

    /// Adopt an already-open database (the legacy single-DB entry point).
    pub fn add(&self, name: &str, db: Database) -> Result<(), Error> {
        Self::validate_name(name)?;
        if self.dbs.read().contains_key(name) {
            return Err(Error::invalid_arg(format!("duplicate database name: '{name}'")));
        }
        if !db.path().as_os_str().is_empty() {
            let canonical = std::fs::canonicalize(db.path()).unwrap_or_else(|_| normalize_path(db.path()));
            let mut paths = self.canonical_paths.write();
            if !paths.insert(canonical.clone()) {
                return Err(Error::invalid_arg(format!(
                    "duplicate database path: {}",
                    canonical.display()
                )));
            }
        }
        self.dbs.write().insert(name.to_string(), Arc::new(db));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<Database>> {
        self.dbs.read().get(name).cloned()
    }

    /// Every mounted database, sorted by name.
    pub fn entries(&self) -> Vec<(String, Arc<Database>)> {
        let mut entries: Vec<(String, Arc<Database>)> = self
            .dbs
            .read()
            .iter()
            .map(|(name, db)| (name.clone(), Arc::clone(db)))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Mounted names, sorted — the order `/health` and `/v2/dbs` report.
    pub fn names(&self) -> Vec<String> {
        self.entries().into_iter().map(|(name, _)| name).collect()
    }

    /// The `/v2/dbs` payload rows. Never includes a filesystem path.
    pub fn descriptors(&self) -> Vec<Value> {
        self.entries()
            .iter()
            .map(|(name, db)| db_descriptor(name, db))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.dbs.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Create `<root>/<name>` and mount it. Never accepts a path: the root is
    /// operator configuration, the name is the only caller input.
    ///
    /// The created folder is canonicalized and must still live under the
    /// canonical root — a pre-existing junction/symlink at that name would
    /// otherwise let a name-smuggled create write outside the root.
    pub fn create(
        &self,
        name: &str,
        persistence: Option<PersistenceMode>,
    ) -> std::result::Result<Arc<Database>, CreateDbError> {
        self.create_with_format(name, persistence, self.create_format())
    }

    /// Create `<root>/<name>` with an explicit storage format.
    pub fn create_with_format(
        &self,
        name: &str,
        persistence: Option<PersistenceMode>,
        format: crate::StorageFormat,
    ) -> std::result::Result<Arc<Database>, CreateDbError> {
        let root = self.root_dir.clone().ok_or(CreateDbError::Disabled)?;
        Self::validate_name(name).map_err(|e| CreateDbError::InvalidName(e.to_string()))?;
        if let Some(existing) = self
            .dbs
            .read()
            .keys()
            .find(|k| k.eq_ignore_ascii_case(name))
            .cloned()
        {
            return Err(CreateDbError::AlreadyExists(existing));
        }

        fs::create_dir_all(&root)
            .map_err(|e| CreateDbError::Core(Error::io_err(&root, "create database root")(e)))?;
        let root_canon = fs::canonicalize(&root)
            .map_err(|e| CreateDbError::Core(Error::io_err(&root, "canonicalize database root")(e)))?;
        let requested_dir = root.join(name);
        fs::create_dir_all(&requested_dir).map_err(|e| {
            CreateDbError::Core(Error::io_err(&requested_dir, "create database folder")(e))
        })?;
        let dir_canon = fs::canonicalize(&requested_dir).map_err(|e| {
            CreateDbError::Core(Error::io_err(&requested_dir, "canonicalize database folder")(e))
        })?;
        if !dir_canon.starts_with(&root_canon) {
            return Err(CreateDbError::Core(Error::invalid_arg(format!(
                "database path for '{name}' resolves outside the configured root"
            ))));
        }

        let path = dir_canon.join("data.jsonl");
        self.mount_resolved_format(
            name,
            &path,
            persistence.unwrap_or(self.default_persistence).to_persistence(self.scheduled_ms),
            format,
        )
        .map_err(CreateDbError::Core)?;
        self.get(name).ok_or_else(|| {
            CreateDbError::Core(Error::invalid_arg(format!(
                "database '{name}' vanished immediately after mounting"
            )))
        })
    }
}

/// Where a configured path really points. An existing directory, or a path
/// without a `.jsonl` extension that does not exist yet, names a database
/// *folder*; the journal lives at `<folder>/data.jsonl`.
pub fn resolve_db_path(input: &Path) -> PathBuf {
    if input.is_dir() {
        return input.join("data.jsonl");
    }
    if input.exists() {
        return input.to_path_buf();
    }
    let is_journal = input
        .extension()
        .map(|e| e.to_string_lossy().eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false);
    if is_journal {
        input.to_path_buf()
    } else {
        input.join("data.jsonl")
    }
}

/// Absolute, lexically normalized form. `..`/`.` aliases collapse without
/// touching the filesystem, so duplicate detection works before the file
/// exists.
pub fn normalize_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ─── File-backed service configuration ──────────────────────────────

/// `ndb serve --config <path.json>` — the whole service in one file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub token: Option<String>,
    pub persistence: Option<String>,
    pub scheduled_ms: Option<u64>,
    pub dir: Option<PathBuf>,
    pub max_limit: Option<usize>,
    pub default_limit: Option<usize>,
    /// Storage format new databases are created with (`auto` | `v1` | `v2`).
    pub format: Option<String>,
    pub dbs: Option<HashMap<String, DbSpec>>,
}

/// A database entry: the bare path (`"a": "data/a.jsonl"`) or the explicit
/// object form with its own persistence override.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DbSpec {
    Path(String),
    Options(DbOptions),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbOptions {
    pub path: String,
    pub persistence: Option<String>,
    pub scheduled_ms: Option<u64>,
}

impl ServiceConfig {
    /// Parse from JSON text. Unknown fields are rejected by serde, not ignored.
    pub fn from_json(text: &str) -> Result<Self, Error> {
        serde_json::from_str(text)
            .map_err(|e| Error::invalid_arg(format!("invalid serve config: {e}")))
    }

    /// Load and parse a config file. Read errors name the path.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(Error::io_err(path, "read serve config"))?;
        Self::from_json(&text)
    }

    /// Apply the file's non-database settings on top of the built-in defaults.
    /// Returns the databases it declares (already path-resolved values are the
    /// caller's job — this keeps the order deterministic by sorting names).
    pub fn apply_to(&self, config: &mut ServerConfig) -> Result<Vec<(String, String, Option<PersistenceMode>, Option<u64>)>, Error> {
        if let Some(bind) = &self.bind {
            config.bind = bind.clone();
        }
        if let Some(port) = self.port {
            config.port = port;
        }
        if let Some(token) = &self.token {
            config.token = Some(token.clone());
        }
        if let Some(max_limit) = self.max_limit {
            config.max_limit = max_limit;
        }
        if let Some(default_limit) = self.default_limit {
            config.default_limit = default_limit;
        }
        if config.default_limit > config.max_limit {
            return Err(Error::invalid_arg(format!(
                "default_limit {} exceeds max_limit {}",
                config.default_limit, config.max_limit
            )));
        }

        let mut dbs = Vec::new();
        if let Some(entries) = &self.dbs {
            for (name, spec) in entries {
                let (path, persistence, scheduled_ms) = match spec {
                    DbSpec::Path(path) => (path.clone(), None, None),
                    DbSpec::Options(options) => (
                        options.path.clone(),
                        match &options.persistence {
                            Some(p) => Some(PersistenceMode::parse(p)?),
                            None => None,
                        },
                        options.scheduled_ms,
                    ),
                };
                dbs.push((name.clone(), path, persistence, scheduled_ms));
            }
            dbs.sort_by(|a, b| a.0.cmp(&b.0));
        }
        Ok(dbs)
    }

    /// The service-wide persistence declared by the file, if any.
    pub fn persistence(&self) -> Result<Option<PersistenceMode>, Error> {
        match &self.persistence {
            Some(p) => Ok(Some(PersistenceMode::parse(p)?)),
            None => Ok(None),
        }
    }

    /// The storage format new databases are created with, if declared.
    pub fn format(&self) -> Result<Option<crate::StorageFormat>, Error> {
        match &self.format {
            Some(f) => Ok(Some(crate::StorageFormat::parse(f)?)),
            None => Ok(None),
        }
    }
}

// ─── Entry point ────────────────────────────────────────────────────

/// Run the server. Blocks forever; each connection handled on its own thread.
pub fn serve(db: Database, config: ServerConfig) -> Result<(), Error> {
    let registry = DbRegistry::new(None, PersistenceMode::Immediate, DEFAULT_SCHEDULED_MS);
    registry.add("default", db)?;
    serve_registry(Arc::new(registry), config)
}

/// Bind and serve a whole registry (the multi-DB entry point). Blocks.
pub fn serve_registry(registry: Arc<DbRegistry>, config: ServerConfig) -> Result<(), Error> {
    let listener = TcpListener::bind((config.bind.as_str(), config.port))
        .map_err(|e| Error::invalid_arg(format!("bind {}:{} failed: {}", config.bind, config.port, e)))?;
    serve_multi_listener(listener, registry, config)
}

/// Serve one database on an already-bound listener (tests bind port 0 for an
/// ephemeral port). The database is mounted as `default`, so both the legacy
/// single-DB routes and `/v2/dbs/default/...` answer on the same port.
pub fn serve_listener(listener: TcpListener, db: Database, config: ServerConfig) -> Result<(), Error> {
    let registry = DbRegistry::new(None, PersistenceMode::Immediate, DEFAULT_SCHEDULED_MS);
    registry.add("default", db)?;
    serve_multi_listener(listener, Arc::new(registry), config)
}

/// A running multi-DB server: the bound address and the way to stop it.
pub struct ServeHandle {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
}

impl ServeHandle {
    /// Stop accepting connections. The serve loop observes the flag within one
    /// accept poll, closes its queue and lets the workers drain what they hold
    /// (shutdown is idempotent).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }
}

/// Bind and serve a registry on a background thread. `config.port == 0`
/// yields an ephemeral port, reported in the handle.
pub fn serve_multi_in_background(
    registry: Arc<DbRegistry>,
    config: ServerConfig,
) -> Result<ServeHandle, Error> {
    let listener = TcpListener::bind((config.bind.as_str(), config.port))
        .map_err(|e| Error::invalid_arg(format!("bind {}:{} failed: {}", config.bind, config.port, e)))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Error::invalid_arg(format!("local_addr after bind: {e}")))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    std::thread::spawn(move || {
        if let Err(e) = serve_multi_listener_shutdown(listener, registry, config, flag) {
            eprintln!("ndb serve exited: {}", e);
        }
    });
    Ok(ServeHandle { addr, shutdown })
}

/// Blocking multi-DB serve on an already-bound listener.
pub fn serve_multi_listener(
    listener: TcpListener,
    registry: Arc<DbRegistry>,
    config: ServerConfig,
) -> Result<(), Error> {
    serve_multi_listener_shutdown(listener, registry, config, Arc::new(AtomicBool::new(false)))
}

// ─── Shutdown & Worker Pool ─────────────────────────────────────────

/// Process-wide shutdown latch. Set by the OS signal handler that `ndb serve`
/// installs; every serve loop in the process observes it, stops accepting,
/// drains its workers and flushes every mounted database.
static PROCESS_SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Ask every serve loop in this process to drain and exit.
pub fn request_process_shutdown() {
    PROCESS_SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Has a process-wide shutdown been requested (Ctrl-C / SIGINT)?
pub fn process_shutdown_requested() -> bool {
    PROCESS_SHUTDOWN.load(Ordering::SeqCst)
}

/// A live request may spend this long before its first byte (slow-loris).
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Idle keep-alive wait between requests: short enough to reclaim workers from
/// vanished clients and to let a shutdown drain promptly.
const KEEP_ALIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Accept-loop poll interval (the listener is non-blocking so the shutdown
/// flag is observed without a self-connect wake-up).
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Worker count for the connection pool: `config.threads` when set, otherwise
/// one per logical core (at least 4).
fn worker_count(config: &ServerConfig) -> usize {
    if config.threads > 0 {
        return config.threads;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(4)
}

/// Bounded hand-off queue between the accept loop and the worker pool.
///
/// A burst of connections fills the queue instead of spawning an unbounded
/// number of OS threads; a full queue is refused with `503` like the old
/// connection cap. Closing the queue lets workers finish what they already
/// hold — that is the drain.
struct JobQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct QueueState {
    jobs: std::collections::VecDeque<TcpStream>,
    closed: bool,
    cap: usize,
}

impl JobQueue {
    fn new(cap: usize) -> Arc<Self> {
        Arc::new(JobQueue {
            state: Mutex::new(QueueState {
                jobs: std::collections::VecDeque::new(),
                closed: false,
                cap: cap.max(1),
            }),
            ready: Condvar::new(),
        })
    }

    /// Hand a connection to the pool. `Err(stream)` when the queue is full (or
    /// already closed): the caller answers `503` on that very stream.
    fn push(&self, stream: TcpStream) -> std::result::Result<(), TcpStream> {
        let mut state = self.state.lock();
        if state.closed || state.jobs.len() >= state.cap {
            return Err(stream);
        }
        state.jobs.push_back(stream);
        self.ready.notify_one();
        Ok(())
    }

    /// Next connection, or `None` once the queue is closed *and* drained.
    fn pop(&self) -> Option<TcpStream> {
        let mut state = self.state.lock();
        loop {
            if let Some(job) = state.jobs.pop_front() {
                return Some(job);
            }
            if state.closed {
                return None;
            }
            self.ready.wait(&mut state);
        }
    }

    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        self.ready.notify_all();
    }
}

fn serve_multi_listener_shutdown(
    listener: TcpListener,
    registry: Arc<DbRegistry>,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
) -> Result<(), Error> {
    let config = Arc::new(config);
    let workers = worker_count(&config);
    let queue = JobQueue::new(config.max_connections);

    eprintln!(
        "ndb serve listening on {} ({} dbs, {} workers, {} connections max)",
        listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
        registry.len(),
        workers,
        config.max_connections
    );

    let mut worker_handles = Vec::with_capacity(workers);
    for index in 0..workers {
        let queue = Arc::clone(&queue);
        let registry = Arc::clone(&registry);
        let config = Arc::clone(&config);
        let shutdown = Arc::clone(&shutdown);
        let handle = std::thread::Builder::new()
            .name(format!("ndb-worker-{index}"))
            .spawn(move || {
                while let Some(stream) = queue.pop() {
                    handle_connection(stream, &registry, &config, &shutdown);
                }
            })
            .map_err(|e| Error::invalid_arg(format!("spawn worker thread: {e}")))?;
        worker_handles.push(handle);
    }

    // Non-blocking accept: the shutdown flag is a plain atomic read, so a
    // Ctrl-C needs no self-connect to wake the listener.
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::invalid_arg(format!("set listener non-blocking: {e}")))?;

    loop {
        if shutdown.load(Ordering::SeqCst) || process_shutdown_requested() {
            break;
        }
        match listener.accept() {
            Ok((stream, _peer)) => {
                if let Err(e) = stream.set_nodelay(true) {
                    report::suppressed("serve: set nodelay", e);
                }
                if let Err(mut stream) = queue.push(stream) {
                    // Over cap: 503 and close. Fail loud, no queueing. A failed
                    // notice to a client that already vanished changes nothing.
                    let _ = write_response(
                        &mut stream,
                        Response::Json(503, json!({"error": "connection cap exceeded"})),
                        false,
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(e) => eprintln!("accept error: {}", e),
        }
    }

    // Drain: no new work enters, workers finish the connections they hold.
    queue.close();
    for handle in worker_handles {
        if handle.join().is_err() {
            eprintln!("warning: a worker thread panicked during shutdown");
        }
    }

    // Zero data loss at exit: every mounted database is flushed (fsynced)
    // before the process is allowed to go.
    for (name, db) in registry.entries() {
        match db.flush() {
            Ok(()) => eprintln!("flushed database '{}'", name),
            Err(e) => eprintln!("flush of database '{}' on shutdown failed: {}", name, e),
        }
    }
    Ok(())
}

// ─── Connection handling ────────────────────────────────────────────

fn handle_connection(
    stream: TcpStream,
    registry: &Arc<DbRegistry>,
    config: &ServerConfig,
    stop: &AtomicBool,
) {
    // Accepted sockets may inherit the listener's non-blocking mode; requests
    // are answered synchronously on this worker, so force blocking.
    if let Err(e) = stream.set_nonblocking(false) {
        report::suppressed("serve: set connection blocking", e);
        return;
    }
    // Read timeout: kills slow-loris and reclaims threads from dead clients.
    if stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT)).is_err() {
        return;
    }
    // A stream without a write timeout can hold a thread forever on a client
    // that stops reading, so failing to set it is reported, not passed over.
    if let Err(e) = stream.set_write_timeout(Some(Duration::from_secs(30))) {
        report::suppressed("serve: set write timeout", e);
    }
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut stream = stream;

    loop {
        if stop.load(Ordering::SeqCst) || process_shutdown_requested() {
            return;
        }
        let req = match read_request(&mut reader, &mut stream, config) {
            Ok(Some(r)) => r,
            Ok(None) => return, // clean EOF / idle timeout between requests
            Err(e) => {
                // Malformed request or rejected (auth/size): respond, then close.
                // read_request signals auth/size rejections as "401"/"413".
                let status = match e.as_str() {
                    "401" => 401,
                    "413" => 413,
                    _ => 400,
                };
                let body = if status == 401 {
                    json!({"error": "unauthorized"})
                } else if status == 413 {
                    json!({"error": "payload too large"})
                } else {
                    json!({"error": e})
                };
                // The connection is closed immediately after this line on
                // every path, so a failed write to a client that is already
                // gone is not a suppressed failure — there is no later state
                // it could have corrupted.
                let _ = write_response(&mut stream, Response::Json(status, body), false);
                return;
            }
        };

        let resp = route(registry, config, &req);
        // A shutdown in progress closes the connection after this response.
        let keep_alive = req.keep_alive
            && !(stop.load(Ordering::SeqCst) || process_shutdown_requested());
        let ok = write_response(&mut stream, resp, keep_alive).is_ok();
        if !ok || !keep_alive {
            return;
        }
        // Waiting for the next request is an idle wait, not a live request:
        // bound it tighter so vanished clients release their worker.
        if stream
            .set_read_timeout(Some(KEEP_ALIVE_IDLE_TIMEOUT))
            .is_err()
        {
            return;
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    keep_alive: bool,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Read one HTTP/1.1 request. Returns Ok(None) on clean EOF before a request.
fn read_request(
    reader: &mut BufReader<TcpStream>,
    stream: &mut TcpStream,
    config: &ServerConfig,
) -> Result<Option<Request>, String> {
    // Request line
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(_) => return Ok(None), // timeout / reset between requests
    }
    let line = line.trim_end();
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing path")?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.1");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    // Headers
    let mut headers = Vec::new();
    loop {
        let mut hline = String::new();
        reader
            .read_line(&mut hline)
            .map_err(|e| format!("header read error: {}", e))?;
        let hline = hline.trim_end();
        if hline.is_empty() {
            break;
        }
        if let Some((k, v)) = hline.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    let mut req = Request {
        method,
        path,
        query,
        headers,
        body: Vec::new(),
        keep_alive: !version.eq_ignore_ascii_case("HTTP/1.0"),
    };
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("connection") {
            if v.eq_ignore_ascii_case("close") {
                req.keep_alive = false;
            } else if v.eq_ignore_ascii_case("keep-alive") {
                req.keep_alive = true;
            }
        }
    }

    // Auth BEFORE reading the body — bad tokens never cost us the transfer.
    if let Some(expected) = &config.token {
        let authorized = req
            .header("authorization")
            .and_then(|got| got.strip_prefix("Bearer "))
            .map(|bearer| constant_time_eq(bearer.as_bytes(), expected.as_bytes()))
            .unwrap_or(false);
        if !authorized {
            return Err("401".to_string());
        }
    }

    let content_length: usize = req
        .header("content-length")
        .map(|v| v.trim().parse::<usize>())
        .transpose()
        .map_err(|e| format!("invalid content-length: {}", e))?
        .unwrap_or(0);

    if content_length > config.max_body {
        return Err("413".to_string());
    }

    // curl sends this for bodies > ~1KB; not answering stalls the client.
    if let Some(expect) = req.header("expect") {
        if expect.eq_ignore_ascii_case("100-continue") {
            stream
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .map_err(|e| format!("continue write error: {}", e))?;
        }
    }

    if content_length > 0 {
        req.body.resize(content_length, 0);
        reader
            .read_exact(&mut req.body)
            .map_err(|e| format!("body read error: {}", e))?;
    }

    Ok(Some(req))
}

/// Length-independent comparison so token checks don't leak timing info.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ─── Routing ────────────────────────────────────────────────────────

enum Response {
    Json(u16, Value),
    /// A pre-serialized JSON body — the query fast path writes stored document
    /// bytes straight into the response instead of re-encoding each hit.
    JsonRaw(u16, Vec<u8>),
    /// Raw bytes with a real content type — bucket files never ride in JSON.
    Binary(u16, String, Vec<u8>),
}

fn route(registry: &Arc<DbRegistry>, config: &ServerConfig, req: &Request) -> Response {
    let segments: Vec<&str> = req.path.split('/').filter(|s| !s.is_empty()).collect();

    // Service routes: /health and everything under /v2/dbs.
    if segments.as_slice() == ["health"] {
        return Response::Json(200, handle_health(registry));
    }
    if segments.first() == Some(&"v2") {
        return route_v2(registry, config, req, &segments);
    }

    // Legacy single-DB routes address the database mounted as `default`.
    // A multi-DB service without `default` has no such database; say so
    // instead of guessing which one the caller meant.
    let db = match registry.get("default") {
        Some(db) => db,
        None => {
            return Response::Json(
                404,
                json!({"error": format!(
                    "no database mounted as 'default' for legacy route {} {}; use /v2/dbs/<name>/...",
                    req.method, req.path
                )}),
            )
        }
    };

    // Binary route: raw bytes with a real content type, never JSON-wrapped.
    if let ("GET", ["file", bucket, name]) = (req.method.as_str(), segments.as_slice()) {
        return handle_get_file(&db, bucket, name);
    }

    let (status, body) = match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["stats"]) => handle_stats(&db),

        ("POST", ["query"]) => return respond_query(&db, config, req),
        ("POST", ["find"]) => handle_find(&db, req),
        ("POST", ["facets"]) => handle_facets(&db, req),

        ("GET", ["doc", id]) => handle_get_doc(&db, req, id),
        ("PUT", ["doc"]) => handle_insert(&db, req),
        ("PUT", ["doc", id]) => handle_replace(&db, req, id),
        ("PATCH", ["doc", id]) => handle_patch(&db, req, id),
        ("DELETE", ["doc", id]) => handle_delete(&db, id),

        ("POST", ["index"]) => handle_create_index(&db, req),
        ("POST", ["compact"]) => handle_compact(&db),

        ("PUT", ["file", bucket]) => handle_store_file(&db, req, bucket),

        // Unknown route: 404, fail loud.
        _ => (404, json!({"error": format!("no route: {} {}", req.method, req.path)})),
    };
    Response::Json(status, body)
}

fn parse_body(req: &Request) -> Result<Value, (u16, Value)> {
    if req.body.is_empty() {
        return Err((400, json!({"error": "empty body"})));
    }
    serde_json::from_slice(&req.body).map_err(|e| (400, json!({"error": format!("invalid JSON body: {}", e)})))
}

fn db_error(e: Error) -> (u16, Value) {
    (500, json!({"error": e.to_string()}))
}

fn handle_stats(db: &Database) -> (u16, Value) {
    // O(1): the store knows its own size. Counting by materializing every
    // document (the old `db.iter().len()`) deep-cloned the entire database.
    let count = db.len();
    let mut body = json!({
        "ok": true,
        "documents": count,
    });
    if let Some(stats) = ram_stats_json(db) {
        body["ram"] = stats;
    }
    (200, body)
}

fn respond_query(db: &Database, config: &ServerConfig, req: &Request) -> Response {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err((status, body)) => return Response::Json(status, body),
    };

    let filter = body.get("filter").cloned().unwrap_or_else(|| json!({}));
    if let Err(e) = crate::validate_query_ast(&filter) {
        return Response::Json(
            400,
            json!({"error": format!("invalid filter: {}", e)}),
        );
    }

    let opts = match parse_query_options(&body, config, config.default_limit) {
        Ok(o) => o,
        Err(e) => return Response::Json(400, json!({"error": e})),
    };

    let fields: Option<Vec<String>> = body
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect());

    if fields.is_none() {
        return match db.query_raw(filter, opts) {
            Ok((total, rows)) => Response::JsonRaw(200, query_raw_body(total, &rows, None)),
            Err(e) => {
                let (status, body) = db_error(e);
                Response::Json(status, body)
            }
        };
    }

    match db.query_projected(filter, opts, fields.as_deref()) {
        Ok((total, results)) => Response::Json(
            200,
            json!({
                "ok": true,
                "total": total,
                "count": results.len(),
                "results": results,
            }),
        ),
        // Unreachable for invalid ASTs — the pre-validation above already
        // returned 400. Kept as the loud net rather than an unwrap.
        Err(e) => {
            let (status, body) = db_error(e);
            Response::Json(status, body)
        }
    }
}

fn parse_query_options(body: &Value, config: &ServerConfig, limit_cap: usize) -> Result<QueryOptions, String> {
    let mut opts = QueryOptions::default();

    if let Some(sort) = body.get("sort") {
        let field = sort
            .get("field")
            .and_then(|v| v.as_str())
            .ok_or("sort.field required when sort is present")?;
        let dir = match sort.get("dir").and_then(|v| v.as_str()).unwrap_or("asc") {
            "asc" => SortDir::Asc,
            "desc" => SortDir::Desc,
            other => return Err(format!("sort.dir must be 'asc' or 'desc', got '{}'", other)),
        };
        opts.sort_by = Some((field.to_string(), dir));
    }

    if let Some(offset) = body.get("offset") {
        opts.offset = Some(offset.as_u64().ok_or("offset must be a non-negative integer")? as usize);
    }

    if let Some(limit) = body.get("limit") {
        let limit = limit.as_u64().ok_or("limit must be a non-negative integer")? as usize;
        if limit > limit_cap {
            return Err(format!("limit {} exceeds cap {}", limit, limit_cap));
        }
        opts.limit = Some(limit);
    } else {
        // Mandatory default: an unfiltered query must never stream the whole DB.
        opts.limit = Some(config.default_limit);
    }

    Ok(opts)
}

/// POST /facets — term counts per field. Array fields count per element,
/// scalar fields count per value; only string values are counted.
/// Body: {fields: ["categories", "keywords", "age"]}
fn handle_facets(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let fields: Vec<String> = body
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if fields.is_empty() {
        return (400, json!({"error": "fields: non-empty string array required"}));
    }

    let mut facets = serde_json::Map::new();
    for (field, counts) in db.facet_counts(&fields) {
        let mut m = serde_json::Map::new();
        for (term, count) in counts {
            m.insert(term, json!(count));
        }
        facets.insert(field, Value::Object(m));
    }
    (200, json!({"ok": true, "facets": facets}))
}

fn handle_find(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let field = match body.get("field").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return (400, json!({"error": "field required"})),
    };

    if let (Some(min), Some(max)) = (body.get("min"), body.get("max")) {
        let results = db.find_range(field, min, max);
        return (200, json!({"ok": true, "count": results.len(), "results": results}));
    }

    match body.get("value") {
        Some(value) => {
            let results = db.find(field, value);
            (200, json!({"ok": true, "count": results.len(), "results": results}))
        }
        None => (400, json!({"error": "value or min/max required"})),
    }
}

fn projection_from_query(req: &Request) -> Option<Vec<String>> {
    req.query
        .split('&')
        .find(|p| p.starts_with("fields="))
        .and_then(|p| p.strip_prefix("fields="))
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

fn handle_get_doc(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let doc = match db.get(id) {
        Ok(d) => d,
        Err(e) => return db_error(e),
    };
    match projection_from_query(req) {
        Some(fields) if !fields.is_empty() => {
            let mut out = serde_json::Map::new();
            for f in &fields {
                // Dot-notation projection (same path engine as /query).
                if let Some(v) = crate::field_get(&doc, f) {
                    out.insert(f.clone(), v.clone());
                }
            }
            out.insert("_id".to_string(), json!(id));
            (200, json!({"ok": true, "doc": Value::Object(out)}))
        }
        _ => (200, json!({"ok": true, "doc": doc})),
    }
}

fn handle_insert(db: &Database, req: &Request) -> (u16, Value) {
    let doc = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    match db.insert(doc) {
        Ok(id) => (201, json!({"ok": true, "id": id})),
        Err(e) => db_error(e),
    }
}

fn handle_replace(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let doc = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let if_seq = match if_seq_from(req) {
        Ok(v) => v,
        Err(message) => return (400, json!({"error": message})),
    };
    match db.update_if(id, doc, if_seq) {
        Ok(()) => match db.doc_seq(id) {
            Some(seq) => (200, json!({"ok": true, "id": id, "seq": seq})),
            None => (200, json!({"ok": true, "id": id})),
        },
        Err(e) => match &e {
            Error::InvalidArgument { .. } => (400, json!({"error": e.to_string()})),
            Error::Conflict { current_seq, doc } => (
                409,
                json!({
                    "ok": false,
                    "error": "conflict",
                    "message": e.to_string(),
                    "current_seq": current_seq,
                    "doc": doc,
                }),
            ),
            _ => db_error(e),
        },
    }
}

/// Parse the wire form of a patch batch into typed ops.
///
/// Every op object is validated here, BEFORE `Database::apply_ops` is called:
/// a malformed op anywhere in the batch is a `400` with zero ops applied.
fn parse_patch_ops(ops: &[Value]) -> std::result::Result<Vec<PatchOp>, (String, String)> {
    let mut parsed = Vec::with_capacity(ops.len());
    for (idx, op) in ops.iter().enumerate() {
        let kind = op
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ("unknown_op".to_string(), format!("op {idx}: 'op' must be a string")))?;
        let path = |op: &Value| -> std::result::Result<String, (String, String)> {
            match op.get("path").and_then(|v| v.as_str()) {
                Some(p) if !p.is_empty() => Ok(p.to_string()),
                _ => Err(("path_required".to_string(), format!("op {idx}: {kind}: path required"))),
            }
        };
        let value = |op: &Value| -> std::result::Result<Value, (String, String)> {
            match op.get("value") {
                Some(v) => Ok(v.clone()),
                None => Err((
                    "value_required".to_string(),
                    format!("op {idx}: {kind}: value required"),
                )),
            }
        };
        parsed.push(match kind {
            "set" => PatchOp::Set {
                path: path(op)?,
                value: value(op)?,
            },
            "remove" => PatchOp::Remove { path: path(op)? },
            "array_push" => PatchOp::ArrayPush {
                path: path(op)?,
                value: value(op)?,
            },
            other => {
                return Err((
                    "unknown_op".to_string(),
                    format!("op {idx}: unknown op {other:?} (expected set | remove | array_push)"),
                ))
            }
        });
    }
    Ok(parsed)
}

/// `?if_seq=<n>` or `If-Match: <n>` — the optimistic-concurrency token.
/// Disagreeing sources are a caller bug, not a preference to resolve.
fn if_seq_from(req: &Request) -> std::result::Result<Option<u64>, String> {
    let params = query_params(&req.query);
    let from_query = match param(&params, "if_seq") {
        Some(raw) => Some(
            raw.trim()
                .parse::<u64>()
                .map_err(|_| format!("if_seq must be an unsigned integer, got '{raw}'"))?,
        ),
        None => None,
    };
    let from_header = match req.header("if-match") {
        Some(raw) => {
            let raw = raw.trim().trim_matches('"');
            Some(
                raw.parse::<u64>()
                    .map_err(|_| format!("If-Match must be an unsigned document seq, got '{raw}'"))?,
            )
        }
        None => None,
    };
    match (from_query, from_header) {
        (Some(q), Some(h)) if q != h => Err(format!(
            "if_seq ({q}) and If-Match ({h}) disagree; send one"
        )),
        (Some(q), _) => Ok(Some(q)),
        (None, Some(h)) => Ok(Some(h)),
        (None, None) => Ok(None),
    }
}

fn patch_error_v1(e: Error) -> (u16, Value) {
    match &e {
        Error::InvalidArgument { .. } => (400, json!({"error": e.to_string()})),
        Error::Conflict { current_seq, doc } => (
            409,
            json!({
                "ok": false,
                "error": "conflict",
                "message": e.to_string(),
                "current_seq": current_seq,
                "doc": doc,
            }),
        ),
        _ => db_error(e),
    }
}

fn handle_patch(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let ops = match body.get("ops").and_then(|o| o.as_array()) {
        Some(o) if !o.is_empty() => o,
        _ => return (400, json!({"error": "ops: non-empty array required"})),
    };
    let ops = match parse_patch_ops(ops) {
        Ok(ops) => ops,
        Err((_, message)) => return (400, json!({"error": message})),
    };
    let if_seq = match if_seq_from(req) {
        Ok(v) => v,
        Err(message) => return (400, json!({"error": message})),
    };

    match db.apply_ops(id, &ops, if_seq) {
        Ok(result) => (
            200,
            json!({"ok": true, "applied": result.applied, "seq": result.seq}),
        ),
        Err(e) => patch_error_v1(e),
    }
}

fn handle_delete(db: &Database, id: &str) -> (u16, Value) {
    match db.delete(id) {
        Ok(()) => (200, json!({"ok": true, "id": id})),
        Err(e) => db_error(e),
    }
}

fn handle_create_index(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let field = match body.get("field").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return (400, json!({"error": "field required"})),
    };
    let kind = body.get("type").and_then(|v| v.as_str()).unwrap_or("hash");
    let result = match kind {
        "hash" => db.create_index(field),
        "btree" => db.create_btree_index(field),
        other => return (400, json!({"error": format!("type must be hash or btree, got '{}'", other)})),
    };
    match result {
        Ok(()) => (201, json!({"ok": true, "index": field, "type": kind})),
        Err(e) => db_error(e),
    }
}

/// Compaction holds the writer lock and can take minutes at multi-GB scale.
/// 202 + background thread; completion observable via the log.
fn handle_compact(db: &Arc<Database>) -> (u16, Value) {
    let db = Arc::clone(db);
    std::thread::spawn(move || match db.compact() {
        Ok(()) => eprintln!("compact: done"),
        Err(e) => eprintln!("compact: FAILED: {}", e),
    });
    (202, json!({"ok": true, "accepted": true, "note": "compaction running in background; watch the server log"}))
}

fn handle_store_file(db: &Database, req: &Request, bucket: &str) -> (u16, Value) {
    if req.body.is_empty() {
        return (400, json!({"error": "empty body"}));
    }
    let mime = req.header("content-type").unwrap_or("application/octet-stream");
    match db.bucket(bucket).store("upload.bin", &req.body, mime) {
        Ok(meta) => (201, json!({"ok": true, "ref": meta._file.to_string_compact(), "bucket": bucket})),
        Err(e) => db_error(e),
    }
}

fn handle_get_file(db: &Database, bucket: &str, name: &str) -> Response {
    // name = hash.ext
    let (hash, ext) = match name.rsplit_once('.') {
        Some((h, e)) => (h, e),
        None => return Response::Json(400, json!({"error": "file name must be hash.ext"})),
    };
    match db.bucket(bucket).get_by_hash(hash, ext) {
        Ok(data) => Response::Binary(200, content_type_for(ext), data),
        Err(e) => {
            let (status, body) = db_error(e);
            Response::Json(status, body)
        }
    }
}

fn content_type_for(ext: &str) -> String {
    mime_for_extension(ext).to_string()
}

// ─── Multi-DB HTTP API (/health, /v2/dbs) ──────────────────────────

fn v2_error(status: u16, code: &str, message: impl Into<String>) -> (u16, Value) {
    (status, json!({"ok": false, "error": code, "message": message.into()}))
}

fn database_not_found(name: &str) -> Value {
    json!({
        "ok": false,
        "error": "database_not_found",
        "message": format!("no database named '{name}'"),
    })
}

/// Map a core error onto the HTTP contract: not-found is 404, caller error is
/// 400, a bad sync cursor is 409, everything else is a loud 500.
fn map_db_error(e: Error) -> (u16, Value) {
    match &e {
        Error::NotFound { .. } => v2_error(404, "not_found", e.to_string()),
        Error::InvalidArgument { .. } => v2_error(400, "invalid_argument", e.to_string()),
        Error::CursorExpired {
            requested_seq,
            min_available_seq,
            last_seq,
        } => (
            409,
            json!({
                "ok": false,
                "error": "cursor_expired",
                "message": e.to_string(),
                "requested_seq": requested_seq,
                "min_available_seq": min_available_seq,
                "last_seq": last_seq,
            }),
        ),
        Error::CursorAhead {
            requested_seq,
            last_seq,
        } => (
            409,
            json!({
                "ok": false,
                "error": "cursor_ahead",
                "message": e.to_string(),
                "requested_seq": requested_seq,
                "last_seq": last_seq,
            }),
        ),
        Error::Conflict { current_seq, doc } => (
            409,
            json!({
                "ok": false,
                "error": "conflict",
                "message": e.to_string(),
                "current_seq": current_seq,
                "doc": doc,
            }),
        ),
        Error::Poisoned { .. } => v2_error(503, "db_poisoned", e.to_string()),
        Error::MemoryLimitExceeded { .. } => v2_error(507, "memory_limit_exceeded", e.to_string()),
        _ => v2_error(500, "internal_error", e.to_string()),
    }
}

fn parse_body_v2(req: &Request) -> Result<Value, (u16, Value)> {    if req.body.is_empty() {
        return Err(v2_error(400, "empty_body", "empty body"));
    }
    serde_json::from_slice(&req.body)
        .map_err(|e| v2_error(400, "invalid_json", format!("invalid JSON body: {e}")))
}

/// Query-string parameters with percent decoding (`+` = space). Malformed
/// escapes stay literal: `%` is a legal filename character, not an error.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let escape = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match escape.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_params(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn elapsed_ms(start: Instant) -> f64 {
    (start.elapsed().as_micros() as f64) / 1000.0
}

fn db_descriptor(name: &str, db: &Database) -> Value {
    let lineage = db.lineage();
    let format = if db.is_ram() {
        "ram"
    } else if db.is_v2() {
        "v2"
    } else {
        "v1"
    };
    json!({
        "name": name,
        "format": format,
        "documents": db.len(),
        "source_epoch_uuid": lineage.source_epoch_uuid,
        "last_seq": lineage.last_seq,
        "min_available_seq": lineage.min_available_seq,
    })
}

/// The RAM-first counters as JSON, or `None` for a v1/v2 database.
fn ram_stats_json(db: &Database) -> Option<Value> {
    db.ram_stats().map(|stats| {
        json!({
            "live_doc_bytes": stats.live_doc_bytes,
            "index_bytes": stats.index_bytes,
            "ring_bytes": stats.ring_bytes,
            "oldest_retained_seq": stats.oldest_retained_seq,
            "expired_cursors": stats.expired_cursors,
            "active_pins": stats.active_pins,
            "poisoned": stats.poisoned,
            "wal_bytes": stats.wal_bytes,
            "stage_us": {
                "validate": stats.validate_us,
                "queue_wait": stats.queue_wait_us,
                "fsync": stats.fsync_us,
                "publish": stats.publish_us,
                "batches": stats.batch_count,
            },
        })
    })
}

fn lineage_json(db: &Database) -> Value {
    let lineage = db.lineage();
    json!({
        "source_epoch_uuid": lineage.source_epoch_uuid,
        "last_seq": lineage.last_seq,
        "min_available_seq": lineage.min_available_seq,
    })
}

fn handle_health(registry: &DbRegistry) -> Value {
    json!({
        "ok": true,
        "engine": "ndb",
        "version": env!("CARGO_PKG_VERSION"),
        "dbs": registry.names(),
        "allow_create": registry.root_dir().is_some(),
    })
}

fn handle_list_dbs(registry: &DbRegistry) -> Value {
    json!({"ok": true, "dbs": registry.descriptors()})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDbRequest {
    name: String,
    #[serde(default)]
    persistence: Option<String>,
    /// `auto` | `v1` | `v2` | `ram` (`v3`); `v2` initialises a binary/mmap
    /// folder database, `ram` a RAM-first v3 folder.
    #[serde(default)]
    format: Option<String>,
}

fn handle_create_db(registry: &DbRegistry, req: &Request) -> (u16, Value) {
    let body = match parse_body_v2(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let request: CreateDbRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => return v2_error(400, "invalid_request", format!("invalid create-db request: {e}")),
    };
    let persistence = match request.persistence.as_deref().map(PersistenceMode::parse) {
        Some(Ok(mode)) => Some(mode),
        Some(Err(e)) => return map_db_error(e),
        None => None,
    };
    let format = match request.format.as_deref().map(crate::StorageFormat::parse) {
        Some(Ok(format)) => format,
        Some(Err(e)) => return map_db_error(e),
        None => registry.create_format(),
    };
    match registry.create_with_format(&request.name, persistence, format) {
        Ok(db) => (
            201,
            json!({"ok": true, "db": db_descriptor(&request.name, &db)}),
        ),
        Err(CreateDbError::Disabled) => v2_error(
            403,
            "create_disabled",
            "POST /v2/dbs requires --dir <root> (no root directory configured)",
        ),
        Err(CreateDbError::InvalidName(reason)) => v2_error(400, "invalid_name", reason),
        Err(CreateDbError::AlreadyExists(name)) => v2_error(
            409,
            "already_exists",
            format!("database '{name}' already exists"),
        ),
        Err(CreateDbError::Core(e)) => map_db_error(e),
    }
}

fn route_v2(
    registry: &Arc<DbRegistry>,
    config: &ServerConfig,
    req: &Request,
    segments: &[&str],
) -> Response {
    match (req.method.as_str(), segments) {
        ("GET", ["v2", "dbs"]) => return Response::Json(200, handle_list_dbs(registry)),
        ("POST", ["v2", "dbs"]) => {
            let (status, body) = handle_create_db(registry, req);
            return Response::Json(status, body);
        }
        _ => {}
    }

    if segments.len() < 3 {
        return Response::Json(
            404,
            json!({"ok": false, "error": "no_route", "message": format!("no route: {} {}", req.method, req.path)}),
        );
    }

    let name = segments[2];
    let db = match registry.get(name) {
        Some(db) => db,
        None => return Response::Json(404, database_not_found(name)),
    };

    if segments.len() == 3 {
        return match req.method.as_str() {
            "GET" => Response::Json(200, json!({"ok": true, "db": db_descriptor(name, &db)})),
            _ => Response::Json(
                404,
                json!({"ok": false, "error": "no_route", "message": format!("no route: {} {}", req.method, req.path)}),
            ),
        };
    }

    let (status, body) = match (req.method.as_str(), &segments[3..]) {
        ("GET", ["stats"]) => {
            let mut body = json!({
                "ok": true,
                "documents": db.len(),
                "lineage": lineage_json(&db),
            });
            if let Some(stats) = ram_stats_json(&db) {
                body["ram"] = stats;
            }
            (200, body)
        }

        ("POST", ["query"]) => return respond_v2_query(&db, config, req),
        ("POST", ["find"]) => handle_v2_find(&db, req),
        ("POST", ["facets"]) => handle_v2_facets(&db, req),

        ("GET", ["doc", id]) => handle_v2_get_doc(&db, req, id),
        ("PUT", ["doc"]) => handle_v2_insert(&db, req),
        ("PUT", ["doc", id]) => handle_v2_replace(&db, req, id),
        ("PATCH", ["doc", id]) => handle_v2_patch(&db, req, id),
        ("DELETE", ["doc", id]) => handle_v2_delete(&db, id),
        ("POST", ["doc", id, "restore"]) => handle_v2_restore(&db, id),
        ("POST", ["docs", "mget"]) => handle_v2_mget(&db, req),

        ("POST", ["index"]) => handle_v2_create_index(&db, req),
        ("POST", ["compact"]) => handle_v2_compact(&db),
        ("POST", ["flush"]) => match db.flush() {
            Ok(()) => (200, json!({"ok": true})),
            Err(e) => map_db_error(e),
        },

        ("GET", ["lineage"]) => (
            200,
            json!({
                "ok": true,
                "source_epoch_uuid": db.lineage().source_epoch_uuid,
                "last_seq": db.lineage().last_seq,
                "min_available_seq": db.lineage().min_available_seq,
            }),
        ),
        ("GET", ["changes"]) => handle_v2_changes(&db, req),
        ("GET", ["snapshot"]) => handle_v2_snapshot(registry, name, &db, req),

        ("PUT", ["files", bucket]) => handle_v2_store_file(&db, req, bucket),
        ("GET", ["files", bucket]) => handle_v2_list_files(&db, bucket),
        ("GET", ["files", bucket, filename]) => return handle_v2_get_file(&db, bucket, filename),
        ("DELETE", ["files", bucket, filename]) => handle_v2_delete_file(&db, req, bucket, filename),
        ("POST", ["files", "gc"]) => match db.gc_buckets() {
            Ok(moved) => (200, json!({"ok": true, "moved": moved})),
            Err(e) => map_db_error(e),
        },

        _ => (
            404,
            json!({"ok": false, "error": "no_route", "message": format!("no route: {} {}", req.method, req.path)}),
        ),
    };
    Response::Json(status, body)
}

fn respond_v2_query(db: &Database, config: &ServerConfig, req: &Request) -> Response {
    let body = match parse_body_v2(req) {
        Ok(b) => b,
        Err((status, body)) => return Response::Json(status, body),
    };
    let filter = body.get("filter").cloned().unwrap_or_else(|| json!({}));
    if let Err(e) = crate::validate_query_ast(&filter) {
        return Response::Json(
            400,
            json!({"ok": false, "error": "invalid_filter", "message": format!("invalid filter: {e}")}),
        );
    }

    let opts = match parse_query_options(&body, config, config.max_limit) {
        Ok(o) => o,
        Err(e) => {
            let (status, body) = v2_error(400, "invalid_query", e);
            return Response::Json(status, body);
        }
    };

    let fields: Option<Vec<String>> = body.get("fields").and_then(|f| f.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });

    let start = Instant::now();

    if fields.is_none() {
        return match db.query_raw(filter, opts) {
            Ok((total, rows)) => Response::JsonRaw(
                200,
                query_raw_body(total, &rows, Some(elapsed_ms(start))),
            ),
            Err(e) => {
                let (status, body) = map_db_error(e);
                Response::Json(status, body)
            }
        };
    }

    match db.query_projected(filter, opts, fields.as_deref()) {
        Ok((total, results)) => Response::Json(
            200,
            json!({
                "ok": true,
                "total": total,
                "count": results.len(),
                "results": results,
                "took_ms": elapsed_ms(start),
            }),
        ),
        Err(e) => {
            let (status, body) = map_db_error(e);
            Response::Json(status, body)
        }
    }
}

/// Build `{"ok":true,"total":N,"count":M,"results":[<raw>,<raw>]}` from
/// pre-serialized document bytes.
fn query_raw_body(total: usize, rows: &[Arc<[u8]>], took_ms: Option<f64>) -> Vec<u8> {
    let payload: usize = rows.iter().map(|row| row.len() + 1).sum();
    let mut body = Vec::with_capacity(payload + 96);
    body.extend_from_slice(b"{\"ok\":true,\"total\":");
    body.extend_from_slice(total.to_string().as_bytes());
    body.extend_from_slice(b",\"count\":");
    body.extend_from_slice(rows.len().to_string().as_bytes());
    body.extend_from_slice(b",\"results\":[");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.extend_from_slice(row);
    }
    body.push(b']');
    if let Some(took) = took_ms {
        body.extend_from_slice(b",\"took_ms\":");
        body.extend_from_slice(format!("{took:.3}").as_bytes());
    }
    body.push(b'}');
    body
}

fn handle_v2_find(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body_v2(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let field = match body.get("field").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return v2_error(400, "field_required", "field required"),
    };

    let start = Instant::now();
    if let (Some(min), Some(max)) = (body.get("min"), body.get("max")) {
        let results = db.find_range(field, min, max);
        return (
            200,
            json!({
                "ok": true,
                "count": results.len(),
                "results": results,
                "took_ms": elapsed_ms(start),
            }),
        );
    }
    match body.get("value") {
        Some(value) => {
            let results = db.find(field, value);
            (
                200,
                json!({
                    "ok": true,
                    "count": results.len(),
                    "results": results,
                    "took_ms": elapsed_ms(start),
                }),
            )
        }
        None => v2_error(400, "value_required", "value or min/max required"),
    }
}

fn handle_v2_facets(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body_v2(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let fields: Vec<String> = body
        .get("fields")
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if fields.is_empty() {
        return v2_error(400, "fields_required", "fields: non-empty string array required");
    }

    let mut facets = serde_json::Map::new();
    for (field, counts) in db.facet_counts(&fields) {
        let mut m = serde_json::Map::new();
        for (term, count) in counts {
            m.insert(term, json!(count));
        }
        facets.insert(field, Value::Object(m));
    }
    (200, json!({"ok": true, "facets": facets}))
}

fn projection_from_params(params: &[(String, String)]) -> Option<Vec<String>> {
    param(params, "fields").map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn project_doc_for_http(doc: &Value, id: &str, fields: &[String]) -> Value {
    let mut out = serde_json::Map::new();
    for f in fields {
        if let Some(v) = crate::field_get(doc, f) {
            out.insert(f.clone(), v.clone());
        }
    }
    out.insert("_id".to_string(), json!(id));
    Value::Object(out)
}

fn handle_v2_get_doc(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let doc = match db.get(id) {
        Ok(d) => d,
        Err(e) => return map_db_error(e),
    };
    let params = query_params(&req.query);
    match projection_from_params(&params) {
        Some(fields) if !fields.is_empty() => (
            200,
            json!({"ok": true, "doc": project_doc_for_http(&doc, id, &fields)}),
        ),
        _ => (200, json!({"ok": true, "doc": doc})),
    }
}

fn handle_v2_insert(db: &Database, req: &Request) -> (u16, Value) {
    let doc = match parse_body_v2(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let params = query_params(&req.query);
    let prefix = match param(&params, "prefix") {
        Some("") => return v2_error(400, "invalid_prefix", "prefix must not be empty"),
        Some(p) => Some(p.to_string()),
        None => None,
    };

    let result = match prefix {
        Some(p) => db.insert_with_prefix(&p, doc),
        None => db.insert(doc),
    };
    match result {
        Ok(id) => match db.get(&id) {
            Ok(stored) => {
                let seq = db.doc_seq(&id).unwrap_or_else(|| db.lineage().last_seq);
                (201, json!({"ok": true, "id": id, "doc": stored, "seq": seq}))
            }
            Err(e) => map_db_error(e),
        },
        Err(e) => map_db_error(e),
    }
}

fn handle_v2_replace(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let doc = match parse_body_v2(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let if_seq = match if_seq_from(req) {
        Ok(v) => v,
        Err(message) => return v2_error(400, "invalid_if_seq", message),
    };
    match db.update_if(id, doc, if_seq) {
        Ok(()) => match db.get(id) {
            Ok(stored) => {
                let seq = db.doc_seq(id).unwrap_or_else(|| db.lineage().last_seq);
                (200, json!({"ok": true, "id": id, "doc": stored, "seq": seq}))
            }
            Err(e) => map_db_error(e),
        },
        Err(e) => map_db_error(e),
    }
}

fn handle_v2_patch(db: &Database, req: &Request, id: &str) -> (u16, Value) {
    let body = match parse_body_v2(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let ops = match body.get("ops").and_then(|o| o.as_array()) {
        Some(o) if !o.is_empty() => o,
        _ => return v2_error(400, "ops_required", "ops: non-empty array required"),
    };
    let ops = match parse_patch_ops(ops) {
        Ok(ops) => ops,
        Err((code, message)) => return v2_error(400, &code, message),
    };
    let if_seq = match if_seq_from(req) {
        Ok(v) => v,
        Err(message) => return v2_error(400, "invalid_if_seq", message),
    };

    match db.apply_ops(id, &ops, if_seq) {
        Ok(result) => (
            200,
            json!({
                "ok": true,
                "applied": result.applied,
                "seq": result.seq,
                "doc": result.doc,
            }),
        ),
        Err(e) => map_db_error(e),
    }
}

fn handle_v2_delete(db: &Database, id: &str) -> (u16, Value) {
    match db.delete(id) {
        Ok(()) => (
            200,
            json!({"ok": true, "deleted": true, "seq": db.lineage().last_seq}),
        ),
        Err(e) => map_db_error(e),
    }
}

fn handle_v2_restore(db: &Database, id: &str) -> (u16, Value) {
    match db.restore(id) {
        Ok(()) => match db.get(id) {
            Ok(doc) => {
                let seq = db.doc_seq(id).unwrap_or_else(|| db.lineage().last_seq);
                (200, json!({"ok": true, "doc": doc, "seq": seq}))
            }
            Err(e) => map_db_error(e),
        },
        Err(e) => map_db_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MgetRequest {
    ids: Vec<String>,
    #[serde(default)]
    fields: Option<Vec<String>>,
}

/// Hard cap on ids per mget call: an unbounded list turns one request into an
/// arbitrarily long read transaction.
const MGET_MAX_IDS: usize = 1_000;

fn handle_v2_mget(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body_v2(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let request: MgetRequest = match serde_json::from_value(body) {
        Ok(r) => r,
        Err(e) => return v2_error(400, "invalid_request", format!("invalid mget request: {e}")),
    };

    if request.ids.len() > MGET_MAX_IDS {
        return v2_error(
            413,
            "payload_too_large",
            format!(
                "mget accepts at most {MGET_MAX_IDS} ids per call, got {}",
                request.ids.len()
            ),
        );
    }

    let fields = request.fields.filter(|f| !f.is_empty());
    let mut docs = serde_json::Map::new();
    let mut missing = Vec::new();
    let mut seen = HashSet::new();
    for id in request.ids {
        if !seen.insert(id.clone()) {
            // Duplicate ids are one answer, not an error: report each id once.
            continue;
        }
        match db.get(&id) {
            Ok(doc) => {
                let value = match &fields {
                    Some(fs) => project_doc_for_http(&doc, &id, fs),
                    None => doc,
                };
                docs.insert(id, value);
            }
            Err(Error::NotFound { .. }) => missing.push(id),
            Err(e) => return map_db_error(e),
        }
    }
    (
        200,
        json!({"ok": true, "docs": Value::Object(docs), "missing": missing}),
    )
}

fn handle_v2_create_index(db: &Database, req: &Request) -> (u16, Value) {
    let body = match parse_body_v2(req) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let field = match body.get("field").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => return v2_error(400, "field_required", "field required"),
    };
    let kind = body.get("type").and_then(|v| v.as_str()).unwrap_or("hash");
    let created = match kind {
        "hash" => {
            if db.has_hash_index(field) {
                false
            } else {
                match db.create_index(field) {
                    Ok(()) => true,
                    Err(e) => return map_db_error(e),
                }
            }
        }
        "btree" => {
            if db.has_btree_index(field) {
                false
            } else {
                match db.create_btree_index(field) {
                    Ok(()) => true,
                    Err(e) => return map_db_error(e),
                }
            }
        }
        other => {
            return v2_error(
                400,
                "invalid_index_type",
                format!("type must be hash or btree, got '{other}'"),
            )
        }
    };
    (
        200,
        json!({"ok": true, "field": field, "type": kind, "created": created}),
    )
}

fn handle_v2_compact(db: &Database) -> (u16, Value) {
    match db.compact() {
        Ok(()) => (200, json!({"ok": true, "lineage": lineage_json(db)})),
        Err(e) => map_db_error(e),
    }
}

fn change_event_json(event: &ChangeEvent) -> Value {
    match &event.kind {
        ChangeKind::Upsert(doc) => json!({
            "seq": event.seq,
            "id": event.id,
            "kind": "upsert",
            "doc": doc,
        }),
        ChangeKind::Delete => json!({
            "seq": event.seq,
            "id": event.id,
            "kind": "delete",
        }),
    }
}

fn handle_v2_changes(db: &Database, req: &Request) -> (u16, Value) {
    let params = query_params(&req.query);
    let since = match param(&params, "since") {
        Some(s) => match s.parse::<u64>() {
            Ok(v) => v,
            Err(_) => {
                return v2_error(400, "invalid_since", format!("since must be an unsigned integer, got '{s}'"))
            }
        },
        None => return v2_error(400, "since_required", "since query parameter required"),
    };
    let limit = match param(&params, "limit") {
        Some(l) => match l.parse::<usize>() {
            Ok(0) => return v2_error(400, "invalid_limit", "limit must be greater than zero"),
            Ok(v) => Some(v),
            Err(_) => {
                return v2_error(400, "invalid_limit", format!("limit must be an unsigned integer, got '{l}'"))
            }
        },
        None => None,
    };

    let batch = match db.changes_since(since) {
        Ok(b) => b,
        Err(e) => return map_db_error(e),
    };

    let total = batch.events.len();
    let (events, has_more) = match limit {
        Some(n) if n < total => (&batch.events[..n], true),
        _ => (&batch.events[..], false),
    };
    let to_seq = events.last().map(|e| e.seq).unwrap_or(since);
    let changes: Vec<Value> = events.iter().map(change_event_json).collect();

    (
        200,
        json!({
            "ok": true,
            "source_epoch_uuid": batch.lineage.source_epoch_uuid,
            "from_seq": since,
            "to_seq": to_seq,
            "last_seq": batch.lineage.last_seq,
            "has_more": has_more,
            "changes": changes,
        }),
    )
}

fn handle_v2_snapshot(
    registry: &DbRegistry,
    db_name: &str,
    db: &Database,
    req: &Request,
) -> (u16, Value) {
    let params = query_params(&req.query);
    let offset = match param(&params, "offset") {
        Some(o) => match o.parse::<usize>() {
            Ok(v) => v,
            Err(_) => {
                return v2_error(400, "invalid_offset", format!("offset must be an unsigned integer, got '{o}'"))
            }
        },
        None => 0,
    };
    let limit = match param(&params, "limit") {
        Some(l) => match l.parse::<usize>() {
            Ok(v) => Some(v),
            Err(_) => {
                return v2_error(400, "invalid_limit", format!("limit must be an unsigned integer, got '{l}'"))
            }
        },
        None => None,
    };
    let requested_id = param(&params, "snapshot_id").map(str::to_string);
    let paginated = requested_id.is_some() || offset > 0 || limit.is_some();

    // Pages of one read must come from one frozen state: a client paging a
    // large export would otherwise see inserts and deletes land between pages.
    let (snapshot, snapshot_id) = match requested_id {
        Some(id) => match registry.pinned_snapshot(db_name, &id) {
            Some(snapshot) => (snapshot, Some(id)),
            None => {
                return v2_error(
                    410,
                    "snapshot_expired",
                    format!(
                        "snapshot '{id}' is unknown, expired ({}s TTL) or belongs to another database; restart the read without snapshot_id",
                        SNAPSHOT_TTL.as_secs()
                    ),
                )
            }
        },
        None if paginated => {
            let snapshot = Arc::new(db.snapshot_at());
            let id = registry.pin_snapshot(db_name, Arc::clone(&snapshot));
            (snapshot, Some(id))
        }
        None => (Arc::new(db.snapshot_at()), None),
    };

    let total = snapshot.docs.len();
    let docs: Vec<Value> = snapshot
        .docs
        .iter()
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .map(|(_, doc)| doc.clone())
        .collect();

    (
        200,
        json!({
            "ok": true,
            "source_epoch_uuid": snapshot.lineage.source_epoch_uuid,
            "watermark_seq": snapshot.lineage.last_seq,
            "snapshot_id": snapshot_id,
            "total": total,
            "count": docs.len(),
            "docs": docs,
        }),
    )
}

/// A filename or bucket name is a single path component: no separators, no
/// traversal, no control characters.
fn validate_component(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{kind} must not be empty"));
    }
    if name == "." || name == ".." {
        return Err(format!("'{name}' is not a valid {kind}"));
    }
    if name.bytes().any(|b| b < 0x20) {
        return Err(format!("{kind} must not contain control characters"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(format!("{kind} must not contain path separators"));
    }
    if Path::new(name).file_name().and_then(|f| f.to_str()) != Some(name) {
        return Err(format!("'{name}' is not a plain {kind}"));
    }
    Ok(())
}

/// A filename is a single path component and carries a strictly bounded
/// extension: empty, or 1–10 ASCII letters/digits after lowercasing. The
/// extension becomes a stored filename and drives the MIME lookup, so
/// anything else is refused with `400` instead of stored.
fn validate_filename(name: &str) -> Result<(), String> {
    validate_component("filename", name)?;
    let ext = match name.rsplit_once('.') {
        Some((_, ext)) => ext,
        None => "",
    };
    let ext = ext.to_ascii_lowercase();
    if ext.chars().count() > 10 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!(
            "invalid file extension '{ext}' in '{name}' (allowed: at most 10 of a-z 0-9)"
        ));
    }
    Ok(())
}

fn validate_bucket(name: &str) -> Result<(), String> {
    validate_component("bucket", name)
}

fn handle_v2_store_file(db: &Database, req: &Request, bucket: &str) -> (u16, Value) {
    if let Err(reason) = validate_bucket(bucket) {
        return v2_error(400, "invalid_bucket", reason);
    }
    if req.body.is_empty() {
        return v2_error(400, "empty_body", "empty body");
    }
    let params = query_params(&req.query);
    let filename = match req.header("x-filename") {
        Some(name) => name.to_string(),
        None => match param(&params, "filename") {
            Some(name) => name.to_string(),
            None => {
                return v2_error(
                    400,
                    "filename_required",
                    "X-Filename header or ?filename= query parameter required",
                )
            }
        },
    };
    if let Err(reason) = validate_filename(&filename) {
        return v2_error(400, "invalid_filename", reason);
    }

    match db.store_file(bucket, &req.body, &filename) {
        Ok(meta) => (
            201,
            json!({
                "ok": true,
                "ref": meta._file.to_string_compact(),
                "hash": meta._file.id,
                "ext": meta._file.ext,
                "size": meta.size,
            }),
        ),
        Err(e) => map_db_error(e),
    }
}

fn handle_v2_list_files(db: &Database, bucket: &str) -> (u16, Value) {
    if let Err(reason) = validate_bucket(bucket) {
        return v2_error(400, "invalid_bucket", reason);
    }
    match db.list_files(bucket) {
        Ok(mut files) => {
            files.sort();
            (200, json!({"ok": true, "files": files}))
        }
        Err(e) => map_db_error(e),
    }
}

/// A bucket read that found no file is a 404, not a server fault: the bucket
/// path is caller-supplied and "nothing stored there" is a legitimate answer.
fn map_file_error(e: Error) -> (u16, Value) {
    if let Error::Io { source, .. } = &e {
        if source.kind() == std::io::ErrorKind::NotFound {
            return v2_error(404, "not_found", e.to_string());
        }
    }
    map_db_error(e)
}

fn handle_v2_get_file(db: &Database, bucket: &str, name: &str) -> Response {
    if let Err(reason) = validate_bucket(bucket).and_then(|()| validate_filename(name)) {
        return Response::Json(400, json!({"ok": false, "error": "invalid_filename", "message": reason}));
    }
    let (hash, ext) = match name.rsplit_once('.') {
        Some((h, e)) => (h, e),
        None => return Response::Json(400, json!({"ok": false, "error": "invalid_filename", "message": "file name must be hash.ext"})),
    };
    match db.bucket(bucket).get_by_hash(hash, ext) {
        Ok(data) => Response::Binary(200, mime_for_extension(ext).to_string(), data),
        Err(e) => {
            let (status, body) = map_file_error(e);
            Response::Json(status, body)
        }
    }
}

fn handle_v2_delete_file(db: &Database, req: &Request, bucket: &str, filename: &str) -> (u16, Value) {
    if let Err(reason) = validate_bucket(bucket).and_then(|()| validate_filename(filename)) {
        return v2_error(400, "invalid_filename", reason);
    }
    let force = matches!(param(&query_params(&req.query), "force"), Some("true") | Some("1"));
    let reference = format!("{bucket}:{filename}");
    match db.release_file_with(&reference, force) {
        Ok(true) => (200, json!({"ok": true, "deleted": true})),
        Ok(false) => (
            409,
            json!({
                "ok": false,
                "error": "file_referenced",
                "message": format!("{reference} is referenced by a live or restorable document; pass ?force=true to delete it anyway"),
            }),
        ),
        Err(e) => map_file_error(e),
    }
}

// ─── HTTP response ──────────────────────────────────────────────────

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// Write one HTTP/1.1 response frame. `resp` is taken by value so a binary
/// body moves straight into the socket without a clone.
///
/// Headers and body go out in a single `write_all` for ordinary (JSON/small)
/// payloads — one syscall per response — and as head-then-body for a large
/// binary body, which is still one copy less than buffering it.
fn write_response(
    stream: &mut TcpStream,
    resp: Response,
    keep_alive: bool,
) -> std::io::Result<()> {
    let (status, content_type, payload): (u16, String, Vec<u8>) = match resp {
        Response::Json(status, body) => (
            status,
            "application/json".to_string(),
            serde_json::to_vec(&body)
                .unwrap_or_else(|_| b"{\"error\":\"serialization failed\"}".to_vec()),
        ),
        Response::JsonRaw(status, body) => (status, "application/json".to_string(), body),
        Response::Binary(status, ctype, data) => (status, ctype, data),
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        status,
        reason(status),
        content_type,
        payload.len(),
        if keep_alive { "keep-alive" } else { "close" }
    );

    const SINGLE_WRITE_LIMIT: usize = 64 * 1024;
    if payload.len() <= SINGLE_WRITE_LIMIT {
        let mut frame = Vec::with_capacity(head.len() + payload.len());
        frame.extend_from_slice(head.as_bytes());
        frame.extend_from_slice(&payload);
        stream.write_all(&frame)?;
    } else {
        stream.write_all(head.as_bytes())?;
        stream.write_all(&payload)?;
    }
    stream.flush()
}
