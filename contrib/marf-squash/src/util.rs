use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use blockstack_lib::chainstate::stacks::index::marf::{MARFOpenOpts, MarfConnection, MARF};
use blockstack_lib::chainstate::stacks::index::storage::{
    TrieFileStorage, TrieHashCalculationMode,
};
use blockstack_lib::chainstate::stacks::index::trie_sql;
use blockstack_lib::core::{
    BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT, BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT,
    POX_REWARD_CYCLE_LENGTH, POX_TESTNET_CYCLE_LENGTH,
};
use rusqlite::params;
use sha2::{Digest, Sha256};
use stacks_common::types::chainstate::StacksBlockId;

use crate::cli::{ChainstatePaths, TargetPaths, GSS_MANIFEST, SQLITE_SIDECAR_EXTENSIONS};

/// Compute SHA-256 checksums for all files in `out_dir`, enforcing directory
/// cleanliness.  Fails on SQLite sidecars, symlinks, and non-regular files.
///
/// When `expected_files` is `Some`, any regular file on disk that is NOT in the
/// expected set (and not a manifest) is a hard error.  This prevents stale files
/// in a reused output directory from being silently blessed into the manifest.
pub fn compute_checksums(
    out_dir: &Path,
    expected_files: Option<&HashSet<String>>,
) -> Result<BTreeMap<String, String>, String> {
    let mut checksums = BTreeMap::new();
    let mut entries: Vec<PathBuf> = Vec::new();

    collect_files_recursive(out_dir, out_dir, &mut entries)?;
    entries.sort();

    for path in &entries {
        let rel = path
            .strip_prefix(out_dir)
            .map_err(|e| format!("strip_prefix: {e}"))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // Skip the manifest files themselves.
        if rel_str == GSS_MANIFEST {
            continue;
        }

        // When an expected set is provided, reject unexpected files.
        if let Some(expected) = expected_files {
            if !expected.contains(&rel_str) {
                return Err(format!(
                    "unexpected file in output directory: {rel_str} \
                     (reuse a clean --out-dir or remove stale files)"
                ));
            }
        }

        let hash = sha256_file(path)?;
        checksums.insert(rel_str, hash);
    }

    Ok(checksums)
}

/// Recursively collect regular files, rejecting symlinks, non-regular files,
/// and SQLite sidecars.
pub fn collect_files_recursive(
    base: &Path,
    dir: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let read_dir = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;

        // Reject symlinks.
        if metadata.is_symlink() {
            return Err(format!(
                "symlink found in GSS directory: {}",
                path.strip_prefix(base).unwrap_or(&path).display()
            ));
        }

        if metadata.is_dir() {
            collect_files_recursive(base, &path, out)?;
            continue;
        }

        if !metadata.is_file() {
            return Err(format!(
                "non-regular file in GSS directory: {}",
                path.strip_prefix(base).unwrap_or(&path).display()
            ));
        }

        // Reject SQLite sidecars.
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if SQLITE_SIDECAR_EXTENSIONS.contains(&ext) {
                return Err(format!(
                    "SQLite sidecar in GSS directory: {}",
                    path.strip_prefix(base).unwrap_or(&path).display()
                ));
            }
        }

        out.push(path);
    }
    Ok(())
}

/// Compute the SHA-256 hex digest of a file using streaming reads.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn chainstate_paths(root: &Path) -> ChainstatePaths {
    let clarity_db = root.join("chainstate/vm/clarity/marf.sqlite");
    let index_db = root.join("chainstate/vm/index.sqlite");
    let sortition_db = root.join("burnchain/sortition/marf.sqlite");
    ChainstatePaths {
        clarity: TargetPaths {
            blobs: Some(PathBuf::from(format!("{}.blobs", clarity_db.display()))),
            db: clarity_db,
        },
        index: TargetPaths {
            blobs: Some(PathBuf::from(format!("{}.blobs", index_db.display()))),
            db: index_db,
        },
        sortition: TargetPaths {
            blobs: None, // sortition uses internal blobs
            db: sortition_db,
        },
    }
}

pub fn selected_targets(
    clarity: bool,
    index: bool,
    sortition: bool,
    all: bool,
) -> (bool, bool, bool) {
    if all {
        (true, true, true)
    } else {
        (clarity, index, sortition)
    }
}

pub fn ensure_targets_selected(
    clarity: bool,
    index: bool,
    sortition: bool,
    blocks: bool,
    bitcoin: bool,
    all: bool,
) {
    let (c, i, s) = selected_targets(clarity, index, sortition, all);
    if !c && !i && !s && !blocks && !bitcoin {
        eprintln!(
            "Must specify at least one target: --clarity, --index, --sortition, --blocks, --bitcoin, or --all"
        );
        std::process::exit(1);
    }
}

pub fn format_timestamp(unix_ts: i64) -> String {
    // Convert Unix timestamp to ISO 8601 UTC without external crate.
    const SECS_PER_DAY: i64 = 86400;
    const SECS_PER_HOUR: i64 = 3600;
    const SECS_PER_MIN: i64 = 60;

    let days = unix_ts / SECS_PER_DAY;
    let rem = unix_ts % SECS_PER_DAY;
    let hour = rem / SECS_PER_HOUR;
    let min = (rem % SECS_PER_HOUR) / SECS_PER_MIN;
    let sec = rem % SECS_PER_MIN;

    // Civil date from days since 1970-01-01 (algorithm from Howard Hinnant).
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

pub fn ensure_blobs_match(db_path: &str, blobs_path: &str) {
    let expected_blobs = PathBuf::from(format!("{db_path}.blobs"));
    if expected_blobs != Path::new(blobs_path) {
        eprintln!(
            "Expected blobs path '{blobs_path}' to match '{}'",
            expected_blobs.display()
        );
        std::process::exit(1);
    }
}

pub fn target_out_paths(out_dir: &Path, source_db: &Path) -> TargetPaths {
    let file_name = source_db.file_name().expect("source db missing filename");
    let mut rel_path = PathBuf::new();
    if let Some(parent) = source_db.parent() {
        rel_path = parent
            .components()
            .skip_while(|c| c.as_os_str() != "chainstate")
            .collect();
    }
    let out_parent = out_dir.join(rel_path);
    let out_db = out_parent.join(file_name);
    TargetPaths {
        blobs: Some(PathBuf::from(format!("{}.blobs", out_db.display()))),
        db: out_db,
    }
}

pub fn target_out_paths_sortition(out_dir: &Path, source_db: &Path) -> TargetPaths {
    let file_name = source_db.file_name().expect("source db missing filename");
    let mut rel_path = PathBuf::new();
    if let Some(parent) = source_db.parent() {
        rel_path = parent
            .components()
            .skip_while(|c| c.as_os_str() != "burnchain")
            .collect();
    }
    let out_parent = out_dir.join(rel_path);
    let out_db = out_parent.join(file_name);
    TargetPaths {
        blobs: None,
        db: out_db,
    }
}

pub fn default_open_opts() -> MARFOpenOpts {
    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = true;
    open_opts
}

pub fn sortition_open_opts() -> MARFOpenOpts {
    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = false;
    open_opts
}

/// Read `mainnet` from the index DB's `db_config` table and derive PoX constants.
pub fn index_pox_constants(index_db_path: &Path) -> (u64, u64) {
    let conn = rusqlite::Connection::open(index_db_path).unwrap_or_else(|e| {
        eprintln!(
            "Failed to open index DB '{}' for db_config: {e}",
            index_db_path.display()
        );
        std::process::exit(1);
    });
    let mainnet: bool = conn
        .query_row("SELECT mainnet FROM db_config LIMIT 1", [], |row| {
            row.get::<_, i64>(0).map(|v| v != 0)
        })
        .unwrap_or_else(|e| {
            eprintln!("Failed to read db_config.mainnet: {e}");
            std::process::exit(1);
        });
    if mainnet {
        (
            BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT,
            POX_REWARD_CYCLE_LENGTH as u64,
        )
    } else {
        (
            BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT,
            POX_TESTNET_CYCLE_LENGTH as u64,
        )
    }
}

/// Resolve Stacks block height to both the sortition MARF height and the actual
/// Bitcoin block height for the tenure that produced that Stacks block.
///
/// Returns `(marf_height, bitcoin_height)` where:
/// - `marf_height` = bitcoin_height - first_burn_height (for MARF squash/validate)
/// - `bitcoin_height` = the actual Bitcoin block height (for Bitcoin auxiliary DBs and SPV)
///
/// Uses the index MARF to find the canonical block hash at `stacks_height`, then
/// looks up `burn_header_height` from `nakamoto_block_headers` for that exact block.
pub fn resolve_burn_height_for_sortition(
    sortition_db_path: &str,
    index_db_path: &str,
    stacks_height: u32,
) -> (u32, u32) {
    // 1. Open the index MARF and resolve the canonical block hash at this height.
    let canonical_block_hash = {
        let storage =
            TrieFileStorage::<StacksBlockId>::open_readonly(index_db_path, default_open_opts())
                .unwrap_or_else(|e| {
                    eprintln!("Failed to open index MARF: {e:?}");
                    std::process::exit(1);
                });
        let mut marf = MARF::from_storage(storage);
        let tip = trie_sql::get_latest_confirmed_block_hash::<StacksBlockId>(marf.sqlite_conn())
            .unwrap_or_else(|e| {
                eprintln!("Failed to get index MARF tip: {e:?}");
                std::process::exit(1);
            });
        marf.get_bhh_at_height(&tip, stacks_height)
            .unwrap_or_else(|e| {
                eprintln!("Failed to resolve block at Stacks height {stacks_height}: {e:?}");
                std::process::exit(1);
            })
            .unwrap_or_else(|| {
                eprintln!("No canonical block found at Stacks height {stacks_height}");
                std::process::exit(1);
            })
    };

    // 2. Look up burn_header_height for this exact canonical block.
    let bitcoin_height: u64 = {
        let conn = rusqlite::Connection::open_with_flags(
            index_db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap_or_else(|e| {
            eprintln!("Failed to open index DB: {e}");
            std::process::exit(1);
        });
        conn.query_row(
            "SELECT burn_header_height FROM nakamoto_block_headers \
             WHERE index_block_hash = ?1",
            params![canonical_block_hash.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or_else(|e| {
            eprintln!(
                "No nakamoto block found for canonical hash {canonical_block_hash} \
                 at Stacks height {stacks_height}: {e}"
            );
            std::process::exit(1);
        }) as u64
    };

    // 3. Compute sortition MARF height = bitcoin_height - first_burn_height.
    let first_burn_height: u64 = {
        let conn = rusqlite::Connection::open_with_flags(
            sortition_db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap_or_else(|e| {
            eprintln!("Failed to open sortition DB: {e}");
            std::process::exit(1);
        });
        conn.query_row(
            "SELECT MIN(block_height) FROM snapshots WHERE pox_valid = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or_else(|e| {
            eprintln!("Failed to read first burn height from sortition DB: {e}");
            std::process::exit(1);
        }) as u64
    };

    if bitcoin_height < first_burn_height {
        eprintln!("Bitcoin height {bitcoin_height} is below first burn height {first_burn_height}");
        std::process::exit(1);
    }

    let marf_height = (bitcoin_height - first_burn_height) as u32;

    eprintln!(
        "Resolved Stacks height {stacks_height} (canonical block {canonical_block_hash}) \
         to Bitcoin height {bitcoin_height} (sortition MARF height {marf_height})"
    );
    (marf_height, bitcoin_height as u32)
}
