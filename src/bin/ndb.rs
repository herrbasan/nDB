use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

// Core Philosophy Exit Codes
const EXIT_SUCCESS: i32 = 0;
const EXIT_GENERAL_ERROR: i32 = 1;
const EXIT_CORRUPTION: i32 = 2;
const EXIT_LOCKED: i32 = 3;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        process::exit(EXIT_GENERAL_ERROR);
    }

    let command = args[1].as_str();

    match command {
        "init" => handle_init(&args[2..]),
        "destroy" | "drop" => handle_destroy(&args[2..]),
        "info" => handle_info(&args[2..]),
        "compact" => handle_compact(&args[2..]),
        "export" => handle_export(&args[2..]),
        "import" => handle_import(&args[2..]),
        "merge" => handle_merge(&args[2..]),
        "verify" | "check" => handle_verify(&args[2..]),
        "recover" => handle_recover(&args[2..]),
        "dump" => handle_dump(&args[2..]),
        "config" => handle_config(&args[2..]),
        "query" => handle_query(&args[2..]),
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
    eprintln!("  init <path> [--buckets a,b]   Initialize a new database");
    eprintln!("  destroy <path> --force        Safely delete a database");
    eprintln!("  info <path>                   Show database statistics");
    eprintln!("  compact <path>                Compact the database in-place");
    eprintln!("  export <path> <dest>          Create a portable snapshot");
    eprintln!("  import <src> <path>           Restore a snapshot");
    eprintln!("  merge <base> <merge-in>       Combine databases");
    eprintln!("  verify <path>                 Check for corruptions");
    eprintln!("  recover <src> <dest>          Recover corrupted data");
    eprintln!("  dump <path>                   Export JSON Lines to stdout");
    eprintln!("  config <get|set> ...          Manage metadata/config");
    eprintln!("  query <path> <query_ast>      Run a raw JSON AST query");
}

fn handle_init(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: ndb init <path> [--buckets a,b]");
        process::exit(EXIT_GENERAL_ERROR);
    }
    let path = Path::new(&args[0]);
    if path.exists() {
        eprintln!("Error: Path already exists. Must be an empty directory.");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    // Parse buckets
    let mut buckets = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--buckets" && i + 1 < args.len() {
            buckets = args[i + 1].split(',').map(|s| s.trim().to_string()).collect();
            break;
        }
        i += 1;
    }

    // Create directory structure
    if let Err(e) = fs::create_dir_all(path) {
        eprintln!("Failed to create directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Create meta.json
    let meta_json = format!(
        "{{\n  \"version\": 1,\n  \"created\": {},\n  \"buckets\": {:?}\n}}\n",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
        buckets
    );
    if let Err(e) = fs::write(path.join("meta.json"), meta_json) {
        eprintln!("Failed to write meta.json: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Create buckets directory
    let buckets_dir = path.join("buckets");
    if let Err(e) = fs::create_dir_all(&buckets_dir) {
        eprintln!("Failed to create buckets directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    for bucket in &buckets {
        if let Err(e) = fs::create_dir(buckets_dir.join(bucket)) {
            eprintln!("Failed to create bucket dir '{}': {}", bucket, e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }

    // Write empty _meta header for db.jsonl
    let db_jsonl = format!("{{\"_meta\":{{\"version\":1,\"created\":\"{}\"}}}}\n", 
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    
    if let Err(e) = fs::write(path.join("db.jsonl"), db_jsonl.clone()) {
        eprintln!("Failed to write db.jsonl: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Write empty _meta header for trash.jsonl
    if let Err(e) = fs::write(path.join("trash.jsonl"), db_jsonl) {
        eprintln!("Failed to write trash.jsonl: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }
    if !path.join("meta.json").exists() {
        eprintln!("Error: Target is not a valid nDB folder.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    if let Err(e) = fs::remove_dir_all(path) {
        eprintln!("Failed to destroy database: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    println!("Destroyed database at {}", path.display());
    process::exit(EXIT_SUCCESS);
}
use std::io::Write;

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
    
    if !meta_path.exists() {
        eprintln!("Status: Invalid\nReason: Missing meta.json in {}", path.display());
        process::exit(EXIT_GENERAL_ERROR);
    }

    let meta_content = fs::read_to_string(&meta_path).unwrap_or_default();
    let meta: serde_json::Value = serde_json::from_str(&meta_content).unwrap_or_else(|_| serde_json::Value::Null);

    let active_size = fs::metadata(path.join("db.jsonl")).map(|m| m.len()).unwrap_or(0);
    let trash_size = fs::metadata(path.join("trash.jsonl")).map(|m| m.len()).unwrap_or(0);
    let total_size = active_size + trash_size;

    let mut doc_count = 0;
    if let Ok(file) = fs::File::open(path.join("db.jsonl")) {
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(file);
        // Exclude the _meta header line
        doc_count = reader.lines().count().saturating_sub(1);
    }

    let buckets = meta.get("buckets").and_then(|v| v.as_array()).map(|a| {
        a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>()
    }).unwrap_or_default();

    println!("Status: Valid");
    println!("Database Path: {}", path.display());
    println!("Schema Version: {}", meta.get("version").and_then(|v| v.as_i64()).unwrap_or(0));
    println!("Active Documents: {}", doc_count);
    println!("Disk Usage:");
    println!("  - Active (db.jsonl): {} bytes", active_size);
    println!("  - Trash (trash.jsonl): {} bytes", trash_size);

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
        for bucket in buckets {
            let bucket_dir = path.join("buckets").join(bucket);
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
    let db_path = path.join("db.jsonl");

    if path.join(".lock").exists() && !path.join(".readonly").exists() {
        eprintln!("Error: Database is actively locked. Cannot compact without a .readonly lock.");
        process::exit(EXIT_LOCKED);
    }

    if !meta_path.exists() || !db_path.exists() {
        eprintln!("Error: Target is not a valid nDB folder.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[1/2] Connecting to database...");
    
    // Explicit compilation fix using ndb core library!
    let db = match ndb::Database::open(&db_path) {
        Ok(db) => db,
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
        eprintln!("Usage: ndb export <path> <dest> [--consistent]");
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    let src_path = Path::new(&args[0]);
    let dest_path = Path::new(&args[1]);
    let db_path = src_path.join("db.jsonl");

    let consistent = args.iter().any(|a| a == "--consistent");
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

    // Copy db.jsonl
    let db_src = src_path.join("db.jsonl");
    if db_src.exists() {
        if let Err(e) = fs::copy(&db_src, dest_path.join("db.jsonl")) {
            eprintln!("Error copying db.jsonl: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }

    // Copy buckets directory
    let buckets_src = src_path.join("buckets");
    if buckets_src.exists() {
        if let Err(e) = copy_dir_recursive(&buckets_src, &dest_path.join("buckets")) {
            eprintln!("Error copying buckets: {}", e);
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
    let base_db = match ndb::Database::open(base_path.join("db.jsonl")) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open base database: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };
    
    let merge_db = match ndb::Database::open(merge_path.join("db.jsonl")) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Failed to open merge-in database: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    };

    eprintln!("[2/4] Resolving document collisions...");
    let mut resolved_docs: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();

    for id in base_db.get_all_ids() {
        if let Ok(doc) = base_db.get(&id) {
            resolved_docs.insert(id, doc);
        }
    }

    for id in merge_db.get_all_ids() {
        if let Ok(merge_doc) = merge_db.get(&id) {
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
    }

    eprintln!("[3/4] Writing merged state to destination...");
    if let Err(e) = fs::create_dir_all(dest_path) {
        eprintln!("Failed to create destination directory: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    // Write db.jsonl
    let target_db = dest_path.join("db.jsonl");
    let active: Vec<&serde_json::Value> = resolved_docs.values().collect();
    if let Err(e) = ndb::storage::rewrite_atomic(&target_db, &active) {
        eprintln!("Failed to write merged db.jsonl: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }
    
    // Create empty trash matching ndb specs
    let _ = fs::write(dest_path.join("trash.jsonl"), "{\"_meta\":{\"version\":1,\"created\":\"0\"}}\n");

    // Copy and merge meta.json
    // Read meta from both, union their buckets array.
    let base_meta_path = base_path.join("meta.json");
    let merge_meta_path = merge_path.join("meta.json");
    
    let mut meta: serde_json::Value = if base_meta_path.exists() {
        let content = fs::read_to_string(&base_meta_path).unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let mut buckets_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(arr) = meta.get("buckets").and_then(|v| v.as_array()) {
        for b in arr {
            if let Some(s) = b.as_str() {
                buckets_set.insert(s.to_string());
            }
        }
    }
    
    if merge_meta_path.exists() {
        let content = fs::read_to_string(&merge_meta_path).unwrap_or_default();
        if let Ok(merge_meta) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(arr) = merge_meta.get("buckets").and_then(|v| v.as_array()) {
                for b in arr {
                    if let Some(s) = b.as_str() {
                        buckets_set.insert(s.to_string());
                    }
                }
            }
        }
    }
    
    meta["buckets"] = serde_json::Value::Array(buckets_set.into_iter().map(serde_json::Value::String).collect());
    
    if let Err(e) = fs::write(dest_path.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()) {
        eprintln!("Failed to write merged meta.json: {}", e);
        process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[4/4] Deduplicating and copying file buckets...");
    // Copy base buckets
    let base_buckets = base_path.join("buckets");
    let dest_buckets = dest_path.join("buckets");
    if base_buckets.exists() {
        if let Err(e) = copy_dir_recursive(&base_buckets, &dest_buckets) {
            eprintln!("Failed to copy base buckets: {}", e);
            process::exit(EXIT_GENERAL_ERROR);
        }
    }
    // Copy merge-in buckets (overwrites handle native deduplication naturally)
    let merge_buckets = merge_path.join("buckets");
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
                            let path = base_path.join("buckets").join(bucket).join(format!("{}.{}", id, ext));
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
    
    let path = Path::new(&args[0]);
    let db_path = path.join("db.jsonl");
    let trash_path = path.join("trash.jsonl");
    
    if !path.join("meta.json").exists() {
        eprintln!("Error: Target is not a valid nDB folder.");
        process::exit(EXIT_GENERAL_ERROR);
    }

    eprintln!("[1/2] Verifying db.jsonl syntax and references...");
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
    
    eprintln!("[2/2] Verifying trash.jsonl syntax and references...");
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
    
    if corruptions > 0 {
        eprintln!("Verification failed. Found {} corrupt rows/missing references.", corruptions);
        process::exit(EXIT_CORRUPTION); // Code 2
    }
    
    println!("Database integrity verified.");
    process::exit(EXIT_SUCCESS); // Code 0
}

fn handle_recover(args: &[String]) {
    if args.len() < 3 || args[1] != "--output" {
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
    if meta_src.exists() {
        let _ = fs::copy(&meta_src, dest_path.join("meta.json"));
    } else {
        // Mock a meta if missing
        let meta_json = format!(
            "{{\n  \"version\": 1,\n  \"created\": {},\n  \"buckets\": []\n}}\n", 
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
        );
        let _ = fs::write(dest_path.join("meta.json"), meta_json);
    }
    
    // Attempt bucket recovery
    let buckets_src = src_path.join("buckets");
    if buckets_src.exists() {
        let _ = copy_dir_recursive(&buckets_src, &dest_path.join("buckets"));
    }
    
    eprintln!("[2/2] Scanning db.jsonl for surviving rows...");
    let db_src = src_path.join("db.jsonl");
    let target_db = dest_path.join("db.jsonl");
    
    // Write an empty trash file
    let _ = fs::write(dest_path.join("trash.jsonl"), "{\"_meta\":{\"version\":1,\"created\":\"0\"}}\n");

    if db_src.exists() {
        use std::io::{BufRead, BufReader, Write};
        let src_file = fs::File::open(&db_src).unwrap();
        let reader = BufReader::new(src_file);
        
        let mut dest_file = fs::File::create(&target_db).unwrap();
        
        let mut salvaged = 0;
        let mut skipped = 0;
        
        for (i, line_res) in reader.lines().enumerate() {
            if let Ok(line) = line_res {
                if line.trim().is_empty() { continue; }
                if serde_json::from_str::<serde_json::Value>(&line).is_ok() {
                    let _ = writeln!(dest_file, "{}", line);
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
        
        dest_file.sync_all().unwrap();
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
    let db_path = path.join("db.jsonl");

    let db = match ndb::Database::open(&db_path) {
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
    let path = Path::new("."); // By default config usually has the path passed or assumes current directory? 
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
        Ok(c) => serde_json::from_str(&c).unwrap_or(serde_json::json!({})),
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
        if let Some(mut value_str) = val {
            let parsed_val: serde_json::Value = if value_str.contains(',') && key == "buckets" {
                let vec: Vec<serde_json::Value> = value_str.split(',').map(|s| serde_json::Value::String(s.trim().to_string())).collect();
                serde_json::Value::Array(vec)
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

    let db_path = path.join("db.jsonl");
    let db = match ndb::Database::open(&db_path) {
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
