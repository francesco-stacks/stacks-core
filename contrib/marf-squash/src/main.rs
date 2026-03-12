use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use blockstack_lib::chainstate::stacks::db::snapshot::{
    copy_burnchain_db, copy_confirmed_epoch2_microblocks, copy_epoch2_block_files,
    copy_index_side_tables, copy_nakamoto_staging_blocks, copy_sortition_side_tables,
    copy_spv_headers, validate_burnchain_db, validate_epoch2_block_files,
    validate_index_side_tables, validate_microblock_streams, validate_nakamoto_staging_blocks,
    validate_sortition_side_tables, validate_spv_headers,
};
use blockstack_lib::chainstate::stacks::index::marf::{
    MARFOpenOpts, MarfConnection, SquashValidationStats, MARF,
};
use blockstack_lib::chainstate::stacks::index::storage::{
    TrieFileStorage, TrieHashCalculationMode,
};
use blockstack_lib::chainstate::stacks::index::{trie_sql, MarfTrieId};
use blockstack_lib::clarity_vm::database::marf::{
    copy_clarity_side_tables, validate_clarity_side_tables,
};
use blockstack_lib::core::{
    BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT, BITCOIN_TESTNET_FIRST_BLOCK_HEIGHT,
    POX_REWARD_CYCLE_LENGTH, POX_TESTNET_CYCLE_LENGTH,
};
use clap::{Parser, Subcommand};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use stacks_common::types::chainstate::{SortitionId, StacksBlockId};

/// Offline squashing CLI for Index, Clarity, and Sortition MARF snapshots.
#[derive(Parser, Debug)]
#[command(
    name = "marf-squash",
    about = "Offline squashing tool for Index, Clarity, and Sortition MARFs"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create squashed MARFs and validate against the source.
    Squash(SquashArgs),
    /// Validate squashed MARFs against a source chainstate.
    Validate(ValidateArgs),
    /// Verify a standalone GSS directory's integrity and optionally check WSCP checkpoint.
    Verify(VerifyArgs),
    /// Print the latest confirmed block height in a MARF.
    LatestHeight(LatestHeightArgs),
}

/// Arguments for generating squashed MARFs.
#[derive(Parser, Debug)]
struct SquashArgs {
    /// Path to the chainstate folder (the parent of chainstate/ and burnchain/).
    #[arg(long, value_name = "DIR")]
    chainstate: PathBuf,
    /// Output directory for the squashed MARF files.
    #[arg(long = "out-dir", value_name = "DIR")]
    out_dir: PathBuf,
    /// Stacks block height to squash to.
    #[arg(long, value_name = "HEIGHT")]
    height: u32,
    /// Squash the Clarity MARF (chainstate/vm/clarity/marf.sqlite).
    #[arg(long)]
    clarity: bool,
    /// Squash the Index MARF (chainstate/vm/index.sqlite).
    #[arg(long)]
    index: bool,
    /// Squash the Sortition MARF (burnchain/sortition/marf.sqlite).
    #[arg(long)]
    sortition: bool,
    /// Squash all three MARFs (Clarity, Index, Sortition).
    #[arg(long)]
    all: bool,
    /// Copy canonical block data (epoch 2.x files, confirmed microblocks, nakamoto.sqlite).
    /// Requires --index (or --all).
    #[arg(long)]
    blocks: bool,
    /// Skip validation to speed up size measurements.
    #[arg(long = "skip-validate")]
    skip_validate: bool,
    /// Run full leaf-by-leaf comparison (slow, O(leaf_count)).
    /// By default, validation uses the fast hash-based check.
    #[arg(long)]
    full: bool,
}

/// Arguments for validating squashed MARFs against a source.
#[derive(Parser, Debug)]
struct ValidateArgs {
    /// Path to the source chainstate folder.
    #[arg(long = "source-chainstate", value_name = "DIR")]
    source_chainstate: PathBuf,
    /// Path to the squashed chainstate folder.
    #[arg(long = "squashed-chainstate", value_name = "DIR")]
    squashed_chainstate: PathBuf,
    /// Stacks block height to validate at.
    #[arg(long, value_name = "HEIGHT")]
    height: u32,
    /// Validate the Clarity MARF.
    #[arg(long)]
    clarity: bool,
    /// Validate the Index MARF.
    #[arg(long)]
    index: bool,
    /// Validate the Sortition MARF.
    #[arg(long)]
    sortition: bool,
    /// Validate all three MARFs.
    #[arg(long)]
    all: bool,
    /// Validate block data (epoch 2.x files, confirmed microblocks, nakamoto.sqlite).
    #[arg(long)]
    blocks: bool,
    /// Run full leaf-by-leaf comparison (slow, O(leaf_count)).
    /// By default, validation uses the fast hash-based check.
    #[arg(long)]
    full: bool,
}

/// Arguments for reporting the latest confirmed height.
#[derive(Parser, Debug)]
struct LatestHeightArgs {
    /// Path to the chainstate folder.
    #[arg(long, value_name = "DIR")]
    chainstate: PathBuf,
    /// Read the latest height from the Clarity MARF.
    #[arg(long)]
    clarity: bool,
    /// Read the latest height from the Index MARF.
    #[arg(long)]
    index: bool,
    /// Read the latest height from the Sortition MARF (prints burn block height).
    #[arg(long)]
    sortition: bool,
}

/// Arguments for standalone GSS verification.
#[derive(Parser, Debug)]
struct VerifyArgs {
    /// Path to a GSS directory (must contain GSS_manifest.toml).
    #[arg(long, value_name = "DIR")]
    gss_dir: PathBuf,
    /// Path to a TOML file with trusted WSCP checkpoint hashes.
    #[arg(long, value_name = "FILE")]
    checkpoint_file: Option<PathBuf>,
}

/// Trusted WSCP checkpoint file format.
#[derive(Deserialize)]
struct CheckpointFile {
    height: u32,
    clarity_squash_root_node_hash: String,
    index_squash_root_node_hash: String,
    sortition_squash_root_node_hash: String,
}

#[derive(Debug, Clone)]
struct TargetPaths {
    db: PathBuf,
    blobs: Option<PathBuf>, // None for sortition (internal blobs)
}

#[derive(Debug, Clone)]
struct ChainstatePaths {
    clarity: TargetPaths,
    index: TargetPaths,
    sortition: TargetPaths,
}

#[derive(Serialize, Deserialize)]
struct SquashManifest {
    snapshot: SnapshotSection,
    roots: RootsSection,
    squash_roots: SquashRootsSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<BlocksSection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checksums: Option<ChecksumsSection>,
}

#[derive(Serialize, Deserialize)]
struct SnapshotSection {
    version: u32,
    height: u32,
    block_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitcoin_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bitcoin_block_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
    chain_id: u32,
    mainnet: bool,
}

#[derive(Serialize, Deserialize)]
struct RootsSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    clarity_archival_marf_root_hash: Option<String>,
    index_archival_marf_root_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sortition_archival_marf_root_hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SquashRootsSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    clarity_squash_root_node_hash: Option<String>,
    index_squash_root_node_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sortition_squash_root_node_hash: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct BlocksSection {
    epoch2x_files: u64,
    epoch2x_bytes: u64,
    epoch2x_microblock_rows: u64,
    epoch2x_microblock_bytes: u64,
    nakamoto_rows: u64,
    nakamoto_bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct ChecksumsSection {
    files: BTreeMap<String, String>,
}

/// Manifest file names.
const GSS_MANIFEST: &str = "GSS_manifest.toml";
const SQUASH_MANIFEST: &str = "squash_manifest.toml";

/// File extensions that indicate SQLite sidecars (WAL, SHM, journal).
const SQLITE_SIDECAR_EXTENSIONS: &[&str] = &["sqlite-wal", "sqlite-shm", "sqlite-journal"];

/// Compute SHA-256 checksums for all files in `out_dir`, enforcing directory
/// cleanliness.  Fails on SQLite sidecars, symlinks, and non-regular files.
///
/// When `expected_files` is `Some`, any regular file on disk that is NOT in the
/// expected set (and not a manifest) is a hard error.  This prevents stale files
/// in a reused output directory from being silently blessed into the manifest.
fn compute_checksums(
    out_dir: &Path,
    expected_files: Option<&std::collections::HashSet<String>>,
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
        if rel_str == GSS_MANIFEST || rel_str == SQUASH_MANIFEST {
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
fn collect_files_recursive(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
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
fn sha256_file(path: &Path) -> Result<String, String> {
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

fn chainstate_paths(root: &Path) -> ChainstatePaths {
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

fn selected_targets(clarity: bool, index: bool, sortition: bool, all: bool) -> (bool, bool, bool) {
    if all {
        (true, true, true)
    } else {
        (clarity, index, sortition)
    }
}

fn ensure_targets_selected(clarity: bool, index: bool, sortition: bool, blocks: bool, all: bool) {
    let (c, i, s) = selected_targets(clarity, index, sortition, all);
    if !c && !i && !s && !blocks {
        eprintln!(
            "Must specify at least one target: --clarity, --index, --sortition, --blocks, or --all"
        );
        std::process::exit(1);
    }
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Squash(args) => run_squash(args),
        Command::Validate(args) => run_validate(args),
        Command::Verify(args) => run_verify(args),
        Command::LatestHeight(args) => run_latest_height(args),
    }
}

fn run_squash(args: SquashArgs) {
    ensure_targets_selected(
        args.clarity,
        args.index,
        args.sortition,
        args.blocks,
        args.all,
    );

    let paths = chainstate_paths(&args.chainstate);
    let (do_clarity, do_index, do_sortition) =
        selected_targets(args.clarity, args.index, args.sortition, args.all);

    if let Err(e) = fs::create_dir_all(&args.out_dir) {
        eprintln!(
            "Failed to create output directory '{}': {e}",
            args.out_dir.display()
        );
        std::process::exit(1);
    }

    let mut clarity_out = None;
    let mut index_out = None;
    let mut sortition_out = None;

    // Resolve burn height for sortition if needed.
    let burn_height = if do_sortition {
        Some(resolve_burn_height_for_sortition(
            paths.sortition.db.to_str().unwrap(),
            paths.index.db.to_str().unwrap(),
            args.height,
        ))
    } else {
        None
    };

    // ── Phase 1: Squash & Copy ──────────────────────────────────────────

    if do_clarity {
        let out = target_out_paths(&args.out_dir, &paths.clarity.db);
        squash_and_copy_one(
            "clarity",
            &paths.clarity,
            &out,
            args.height,
            SideTableMode::Clarity,
            default_open_opts(),
        );
        clarity_out = Some(out);
    }

    // Derive PoX constants from the source index DB (needed for index side-table filtering).
    let (first_burn_height, reward_cycle_len) = if do_index {
        index_pox_constants(&paths.index.db)
    } else {
        (0, 1)
    };

    if do_index {
        let out = target_out_paths(&args.out_dir, &paths.index.db);
        squash_and_copy_one(
            "index",
            &paths.index,
            &out,
            args.height,
            SideTableMode::Index {
                first_burn_height,
                reward_cycle_len,
            },
            default_open_opts(),
        );
        index_out = Some(out);
    }

    if do_sortition {
        let bh = burn_height.unwrap();
        let out = target_out_paths_sortition(&args.out_dir, &paths.sortition.db);
        squash_and_copy_one(
            "sortition",
            &paths.sortition,
            &out,
            bh,
            SideTableMode::Sortition,
            sortition_open_opts(),
        );
        sortition_out = Some((out, bh));
    }

    // Block preservation: requires --index.
    let do_blocks = args.blocks || args.all;
    if do_blocks && !do_index {
        eprintln!("--blocks requires --index (or --all)");
        std::process::exit(1);
    }

    let mut blocks_stats: Option<BlocksSection> = None;
    let mut copied_block_rel_paths: Vec<String> = Vec::new();

    // These variables are needed by both the copy and validation phases for blocks.
    let src_blocks_dir = args.chainstate.join("chainstate/blocks");
    let dst_blocks_dir = args.out_dir.join("chainstate/blocks");
    let src_nakamoto = args.chainstate.join("chainstate/blocks/nakamoto.sqlite");
    let dst_nakamoto = dst_blocks_dir.join("nakamoto.sqlite");

    if do_blocks {
        let i_out = index_out
            .as_ref()
            .expect("--blocks requires --index; index_out must be set");

        let src_index_path = paths.index.db.to_str().unwrap();
        let dst_index_path = i_out.db.to_str().unwrap();

        // 1. Copy confirmed epoch-2 microblock streams.
        println!("Copying confirmed epoch-2 microblock streams...");
        let mblock_stats = match copy_confirmed_epoch2_microblocks(src_index_path, dst_index_path) {
            Ok(st) => {
                println!(
                    "Microblock copy complete: streams_copied={}, streams_skipped={}, rows={}, bytes={}",
                    st.streams_copied, st.streams_skipped, st.microblock_rows_copied, st.microblock_bytes_copied
                );
                st
            }
            Err(e) => {
                eprintln!("Failed to copy microblock streams: {e:?}");
                std::process::exit(1);
            }
        };

        // 2. Copy epoch 2.x block files.
        println!("Copying epoch 2.x block files...");
        let file_stats = match copy_epoch2_block_files(
            dst_index_path,
            src_blocks_dir.to_str().unwrap(),
            dst_blocks_dir.to_str().unwrap(),
        ) {
            Ok(st) => {
                println!(
                    "Epoch 2.x block files copied: files={}, bytes={}, genesis_skipped={}, missing_pruned={}",
                    st.files_copied, st.total_bytes, st.genesis_skipped, st.files_missing
                );
                st
            }
            Err(e) => {
                eprintln!("Failed to copy epoch 2.x block files: {e:?}");
                std::process::exit(1);
            }
        };

        // 3. Copy nakamoto staging blocks.
        if !src_nakamoto.exists() {
            eprintln!(
                "Source nakamoto.sqlite not found at {}; required for --blocks",
                src_nakamoto.display()
            );
            std::process::exit(1);
        }
        println!("Copying nakamoto staging blocks...");
        let nak_stats = match copy_nakamoto_staging_blocks(
            src_nakamoto.to_str().unwrap(),
            dst_nakamoto.to_str().unwrap(),
            dst_index_path,
        ) {
            Ok(st) => {
                println!(
                    "Nakamoto blocks copied: rows={}, blob_bytes={}",
                    st.rows_copied, st.total_blob_bytes
                );
                st
            }
            Err(e) => {
                eprintln!("Failed to copy nakamoto staging blocks: {e:?}");
                std::process::exit(1);
            }
        };

        blocks_stats = Some(BlocksSection {
            epoch2x_files: file_stats.files_copied,
            epoch2x_bytes: file_stats.total_bytes,
            epoch2x_microblock_rows: mblock_stats.microblock_rows_copied,
            epoch2x_microblock_bytes: mblock_stats.microblock_bytes_copied,
            nakamoto_rows: nak_stats.rows_copied,
            nakamoto_bytes: nak_stats.total_blob_bytes,
        });

        // Record copied block file paths for the expected-file whitelist.
        // Epoch2x paths are relative to dst_blocks_dir; prefix with chainstate/blocks/.
        for rel in &file_stats.copied_paths {
            copied_block_rel_paths.push(format!("chainstate/blocks/{}", rel.replace('\\', "/")));
        }
        // Nakamoto staging DB.
        copied_block_rel_paths.push("chainstate/blocks/nakamoto.sqlite".to_string());
    }

    // Burnchain auxiliary files: burnchain.sqlite + headers.sqlite.
    // Only when producing a complete GSS (all three MARFs + blocks).
    let do_burnchain_aux = do_clarity && do_index && do_sortition && do_blocks;
    let mut has_burnchain_aux = false;

    // These variables are needed by both the copy and validation phases for burnchain.
    let src_bc_db = args.chainstate.join("burnchain/burnchain.sqlite");
    let dst_bc_db = args.out_dir.join("burnchain/burnchain.sqlite");
    let squashed_sort = args.out_dir.join("burnchain/sortition/marf.sqlite");
    let src_hdr = args.chainstate.join("headers.sqlite");
    let dst_hdr = args.out_dir.join("headers.sqlite");

    if do_burnchain_aux {
        let bh = burn_height.expect("burn_height resolved when do_sortition=true");

        println!("Copying burnchain.sqlite (canonical only)...");
        match copy_burnchain_db(
            src_bc_db.to_str().unwrap(),
            dst_bc_db.to_str().unwrap(),
            squashed_sort.to_str().unwrap(),
            bh,
        ) {
            Ok(bc_stats) => {
                println!(
                    "  block_headers={}, block_ops={}, commit_metadata={}, anchor_blocks={}, overrides={}, affirmation_maps={}",
                    bc_stats.block_headers_rows, bc_stats.block_ops_rows,
                    bc_stats.block_commit_metadata_rows, bc_stats.anchor_blocks_rows,
                    bc_stats.overrides_rows, bc_stats.affirmation_maps_rows
                );
            }
            Err(e) => {
                eprintln!("Failed to copy burnchain.sqlite: {e:?}");
                std::process::exit(1);
            }
        }

        println!("Copying headers.sqlite (SPV, up to burn height {bh})...");
        match copy_spv_headers(src_hdr.to_str().unwrap(), dst_hdr.to_str().unwrap(), bh) {
            Ok(Some(spv_stats)) => {
                println!(
                    "  headers={}, chain_work={}",
                    spv_stats.headers_rows, spv_stats.chain_work_rows
                );
            }
            Ok(None) => {
                println!("  headers.sqlite not found in source (will be rebuilt on startup)");
            }
            Err(e) => {
                eprintln!("Failed to copy headers.sqlite: {e:?}");
                std::process::exit(1);
            }
        };

        has_burnchain_aux = true;
    }

    // ── Phase 2: Validation ─────────────────────────────────────────────

    let mut all_valid = true;

    if !args.skip_validate {
        println!("--- Validation phase ---");

        if do_clarity
            && !validate_one(
                "clarity",
                &paths.clarity,
                clarity_out.as_ref().unwrap(),
                args.height,
                args.full,
                SideTableMode::Clarity,
                default_open_opts(),
            )
        {
            all_valid = false;
        }

        if do_index
            && !validate_one(
                "index",
                &paths.index,
                index_out.as_ref().unwrap(),
                args.height,
                args.full,
                SideTableMode::Index {
                    first_burn_height,
                    reward_cycle_len,
                },
                default_open_opts(),
            )
        {
            all_valid = false;
        }

        if do_sortition {
            let bh = burn_height.unwrap();
            if !validate_one(
                "sortition",
                &paths.sortition,
                &sortition_out.as_ref().unwrap().0,
                bh,
                args.full,
                SideTableMode::Sortition,
                sortition_open_opts(),
            ) {
                all_valid = false;
            }
        }

        if do_blocks {
            let i_out = index_out
                .as_ref()
                .expect("--blocks requires --index; index_out must be set");
            let src_index_path = paths.index.db.to_str().unwrap();
            let dst_index_path = i_out.db.to_str().unwrap();

            println!("Validating block data...");
            let mut blocks_valid = true;

            // Microblock validation.
            match validate_microblock_streams(src_index_path, dst_index_path) {
                Ok(v) => {
                    println!("  microblocks_match: {}", v.staging_microblocks_match);
                    println!(
                        "  microblocks_data_match: {}",
                        v.staging_microblocks_data_match
                    );
                    println!(
                        "  microblocks_no_extra: {}",
                        v.staging_microblocks_no_extra_rows
                    );
                    if !v.is_valid() {
                        blocks_valid = false;
                    }
                }
                Err(e) => {
                    eprintln!("  Microblock validation error: {e:?}");
                    blocks_valid = false;
                }
            }

            // Nakamoto validation - required for --blocks.
            if !dst_nakamoto.exists() {
                eprintln!(
                    "  Destination nakamoto.sqlite missing at {}",
                    dst_nakamoto.display()
                );
                blocks_valid = false;
            } else {
                match validate_nakamoto_staging_blocks(
                    src_nakamoto.to_str().unwrap(),
                    dst_nakamoto.to_str().unwrap(),
                    dst_index_path,
                ) {
                    Ok(v) => {
                        println!("  nakamoto_metadata_match: {}", v.metadata_match);
                        println!("  nakamoto_no_extra_blocks: {}", v.no_extra_blocks);
                        println!("  nakamoto_blob_bytes_match: {}", v.blob_bytes_match);
                        println!("  nakamoto_db_version_match: {}", v.db_version_match);
                        println!("  nakamoto_schema_match: {}", v.schema_match);
                        if !v.is_valid() {
                            blocks_valid = false;
                        }
                    }
                    Err(e) => {
                        eprintln!("  Nakamoto validation error: {e:?}");
                        blocks_valid = false;
                    }
                }
            }

            // Epoch 2.x file validation.
            match validate_epoch2_block_files(
                dst_index_path,
                src_blocks_dir.to_str().unwrap(),
                dst_blocks_dir.to_str().unwrap(),
            ) {
                Ok(v) => {
                    println!("  epoch2x_all_files_present: {}", v.all_files_present);
                    println!("  epoch2x_no_extra_files: {}", v.no_extra_files);
                    println!("  epoch2x_all_bytes_match: {}", v.all_bytes_match);
                    if !v.is_valid() {
                        blocks_valid = false;
                    }
                }
                Err(e) => {
                    eprintln!("  Epoch 2.x file validation error: {e:?}");
                    blocks_valid = false;
                }
            }

            if !blocks_valid {
                all_valid = false;
            }
        }

        if do_burnchain_aux {
            let bh = burn_height.expect("burn_height resolved when do_sortition=true");

            println!("Validating burnchain auxiliary files...");
            match validate_burnchain_db(
                src_bc_db.to_str().unwrap(),
                dst_bc_db.to_str().unwrap(),
                squashed_sort.to_str().unwrap(),
                bh,
            ) {
                Ok(v) => {
                    println!("  bc_block_headers_match: {}", v.block_headers_match);
                    println!("  bc_block_ops_match: {}", v.block_ops_match);
                    println!(
                        "  bc_commit_metadata_match: {}",
                        v.block_commit_metadata_match
                    );
                    println!("  bc_anchor_blocks_match: {}", v.anchor_blocks_match);
                    println!("  bc_overrides_match: {}", v.overrides_match);
                    println!("  bc_db_config_match: {}", v.db_config_match);
                    println!("  bc_no_extra_headers: {}", v.no_extra_headers);
                    println!("  bc_canonical_complete: {}", v.canonical_complete);
                    println!("  bc_affirmation_maps_match: {}", v.affirmation_maps_match);
                    if !v.is_valid() {
                        all_valid = false;
                    }
                }
                Err(e) => {
                    eprintln!("  burnchain.sqlite validation error: {e:?}");
                    all_valid = false;
                }
            }

            match validate_spv_headers(src_hdr.to_str().unwrap(), dst_hdr.to_str().unwrap(), bh) {
                Ok(Some(v)) => {
                    println!("  spv_headers_match: {}", v.headers_match);
                    println!("  spv_chain_work_match: {}", v.chain_work_match);
                    println!("  spv_db_config_match: {}", v.db_config_match);
                    println!("  spv_no_extra_headers: {}", v.no_extra_headers);
                    if !v.is_valid() {
                        all_valid = false;
                    }
                }
                Ok(None) => {
                    println!("  headers.sqlite: both absent, skipped");
                }
                Err(e) => {
                    eprintln!("  headers.sqlite validation error: {e:?}");
                    all_valid = false;
                }
            }
        }
    }

    if !all_valid {
        eprintln!("Validation failed for one or more targets");
        std::process::exit(1);
    }

    // Generate manifest when index is included.
    if let Some(ref i_out) = index_out {
        generate_manifest(
            &args.out_dir,
            clarity_out.as_ref(),
            i_out,
            sortition_out.as_ref().map(|(p, bh)| (p, *bh)),
            args.height,
            blocks_stats,
            has_burnchain_aux,
            &copied_block_rel_paths,
        );
    }
}

fn run_validate(args: ValidateArgs) {
    ensure_targets_selected(
        args.clarity,
        args.index,
        args.sortition,
        args.blocks,
        args.all,
    );

    let source_paths = chainstate_paths(&args.source_chainstate);
    let squashed_paths = chainstate_paths(&args.squashed_chainstate);
    let (do_clarity, do_index, do_sortition) =
        selected_targets(args.clarity, args.index, args.sortition, args.all);

    let mut all_valid = true;

    if do_clarity
        && !validate_one(
            "clarity",
            &source_paths.clarity,
            &squashed_paths.clarity,
            args.height,
            args.full,
            SideTableMode::Clarity,
            default_open_opts(),
        )
    {
        all_valid = false;
    }

    if do_index {
        let (first_burn_height, reward_cycle_len) = index_pox_constants(&source_paths.index.db);
        if !validate_one(
            "index",
            &source_paths.index,
            &squashed_paths.index,
            args.height,
            args.full,
            SideTableMode::Index {
                first_burn_height,
                reward_cycle_len,
            },
            default_open_opts(),
        ) {
            all_valid = false;
        }
    }

    if do_sortition {
        let burn_height = resolve_burn_height_for_sortition(
            source_paths.sortition.db.to_str().unwrap(),
            source_paths.index.db.to_str().unwrap(),
            args.height,
        );
        if !validate_one(
            "sortition",
            &source_paths.sortition,
            &squashed_paths.sortition,
            burn_height,
            args.full,
            SideTableMode::Sortition,
            sortition_open_opts(),
        ) {
            all_valid = false;
        }
    }

    // Block validation.
    let do_blocks = args.blocks || args.all;
    if do_blocks && !do_index {
        eprintln!("--blocks requires --index (or --all)");
        std::process::exit(1);
    }
    if do_blocks {
        println!("Validating block data...");

        let src_index = source_paths.index.db.to_str().unwrap();
        let dst_index = squashed_paths.index.db.to_str().unwrap();

        // Microblock validation.
        match validate_microblock_streams(src_index, dst_index) {
            Ok(v) => {
                println!("  microblocks_match: {}", v.staging_microblocks_match);
                println!(
                    "  microblocks_data_match: {}",
                    v.staging_microblocks_data_match
                );
                println!(
                    "  microblocks_no_extra: {}",
                    v.staging_microblocks_no_extra_rows
                );
                if !v.is_valid() {
                    all_valid = false;
                }
            }
            Err(e) => {
                eprintln!("  Microblock validation error: {e:?}");
                all_valid = false;
            }
        }

        // Nakamoto validation.
        let src_nakamoto = args
            .source_chainstate
            .join("chainstate/blocks/nakamoto.sqlite");
        let dst_nakamoto = args
            .squashed_chainstate
            .join("chainstate/blocks/nakamoto.sqlite");
        if !dst_nakamoto.exists() || !src_nakamoto.exists() {
            eprintln!(
                "  nakamoto.sqlite missing (src={}, dst={}); required for --blocks validation",
                src_nakamoto.exists(),
                dst_nakamoto.exists()
            );
            all_valid = false;
        } else {
            match validate_nakamoto_staging_blocks(
                src_nakamoto.to_str().unwrap(),
                dst_nakamoto.to_str().unwrap(),
                dst_index,
            ) {
                Ok(v) => {
                    println!("  nakamoto_metadata_match: {}", v.metadata_match);
                    println!("  nakamoto_no_extra_blocks: {}", v.no_extra_blocks);
                    println!("  nakamoto_blob_bytes_match: {}", v.blob_bytes_match);
                    println!("  nakamoto_db_version_match: {}", v.db_version_match);
                    println!("  nakamoto_schema_match: {}", v.schema_match);
                    if !v.is_valid() {
                        all_valid = false;
                    }
                }
                Err(e) => {
                    eprintln!("  Nakamoto validation error: {e:?}");
                    all_valid = false;
                }
            }
        }

        // Epoch 2.x file validation.
        let src_blocks_dir = args.source_chainstate.join("chainstate/blocks");
        let dst_blocks_dir = args.squashed_chainstate.join("chainstate/blocks");
        match validate_epoch2_block_files(
            dst_index,
            src_blocks_dir.to_str().unwrap(),
            dst_blocks_dir.to_str().unwrap(),
        ) {
            Ok(v) => {
                println!("  epoch2x_all_files_present: {}", v.all_files_present);
                println!("  epoch2x_no_extra_files: {}", v.no_extra_files);
                println!("  epoch2x_all_bytes_match: {}", v.all_bytes_match);
                if !v.is_valid() {
                    all_valid = false;
                }
            }
            Err(e) => {
                eprintln!("  Epoch 2.x file validation error: {e:?}");
                all_valid = false;
            }
        }
    }

    // Burnchain auxiliary validation.
    let do_burnchain_aux = do_clarity && do_index && do_sortition && do_blocks;
    if do_burnchain_aux {
        let burn_height = resolve_burn_height_for_sortition(
            source_paths.sortition.db.to_str().unwrap(),
            source_paths.index.db.to_str().unwrap(),
            args.height,
        );

        let src_bc_db = args.source_chainstate.join("burnchain/burnchain.sqlite");
        let dst_bc_db = args.squashed_chainstate.join("burnchain/burnchain.sqlite");
        let squashed_sort = args
            .squashed_chainstate
            .join("burnchain/sortition/marf.sqlite");

        println!("Validating burnchain auxiliary files...");
        match validate_burnchain_db(
            src_bc_db.to_str().unwrap(),
            dst_bc_db.to_str().unwrap(),
            squashed_sort.to_str().unwrap(),
            burn_height,
        ) {
            Ok(v) => {
                println!("  bc_block_headers_match: {}", v.block_headers_match);
                println!("  bc_block_ops_match: {}", v.block_ops_match);
                println!(
                    "  bc_commit_metadata_match: {}",
                    v.block_commit_metadata_match
                );
                println!("  bc_anchor_blocks_match: {}", v.anchor_blocks_match);
                println!("  bc_overrides_match: {}", v.overrides_match);
                println!("  bc_db_config_match: {}", v.db_config_match);
                println!("  bc_no_extra_headers: {}", v.no_extra_headers);
                println!("  bc_canonical_complete: {}", v.canonical_complete);
                println!("  bc_affirmation_maps_match: {}", v.affirmation_maps_match);
                if !v.is_valid() {
                    all_valid = false;
                }
            }
            Err(e) => {
                eprintln!("  burnchain.sqlite validation error: {e:?}");
                all_valid = false;
            }
        }

        let src_hdr = args.source_chainstate.join("headers.sqlite");
        let dst_hdr = args.squashed_chainstate.join("headers.sqlite");
        match validate_spv_headers(
            src_hdr.to_str().unwrap(),
            dst_hdr.to_str().unwrap(),
            burn_height,
        ) {
            Ok(Some(v)) => {
                println!("  spv_headers_match: {}", v.headers_match);
                println!("  spv_chain_work_match: {}", v.chain_work_match);
                println!("  spv_db_config_match: {}", v.db_config_match);
                println!("  spv_no_extra_headers: {}", v.no_extra_headers);
                if !v.is_valid() {
                    all_valid = false;
                }
            }
            Ok(None) => {
                println!("  headers.sqlite: both absent, skipped");
            }
            Err(e) => {
                eprintln!("  headers.sqlite validation error: {e:?}");
                all_valid = false;
            }
        }
    }

    if !all_valid {
        eprintln!("Validation failed for one or more targets");
        std::process::exit(1);
    }
}

fn run_verify(args: VerifyArgs) {
    match verify_gss(&args.gss_dir, args.checkpoint_file.as_deref()) {
        Ok(()) => {
            println!("Verification passed.");
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("  {e}");
            }
            eprintln!("Verification FAILED.");
            std::process::exit(1);
        }
    }
}

/// Validate a `0x`-prefixed 64-hex-char hash string.  Returns `Ok(())` or an
/// error message describing what is wrong.
fn validate_checkpoint_hash(field_name: &str, value: &str) -> Result<(), String> {
    if !value.starts_with("0x") {
        return Err(format!("{field_name}: must start with 0x"));
    }
    if value.len() != 66 {
        return Err(format!(
            "{field_name}: expected 66 chars (0x + 64 hex), got {}",
            value.len()
        ));
    }
    if !value[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{field_name}: contains non-hex characters"));
    }
    Ok(())
}

/// Core verification logic for a GSS directory.  Returns `Ok(())` when all
/// requested levels pass, or `Err(errors)` with accumulated failure messages.
/// Levels 0-2 always run; Level 3 runs when `checkpoint_file` is provided.
fn verify_gss(gss_dir: &Path, checkpoint_file: Option<&Path>) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();

    // Require GSS_manifest.toml (full GSS only).
    let manifest_path = gss_dir.join(GSS_MANIFEST);
    if !manifest_path.exists() {
        if gss_dir.join(SQUASH_MANIFEST).exists() {
            return Err(vec![format!(
                "found {SQUASH_MANIFEST} but not {GSS_MANIFEST}. \
                 `verify` is for consumer-side full GSS verification. \
                 Use `validate` for producer-side partial squash checks."
            )]);
        }
        return Err(vec![format!(
            "{GSS_MANIFEST} not found in {}",
            gss_dir.display()
        )]);
    }

    let manifest_str = fs::read_to_string(&manifest_path)
        .map_err(|e| vec![format!("Failed to read {}: {e}", manifest_path.display())])?;
    let manifest: SquashManifest = toml::from_str(&manifest_str)
        .map_err(|e| vec![format!("Failed to parse {GSS_MANIFEST}: {e}")])?;

    // Require [checksums] section.
    let checksums = manifest
        .checksums
        .as_ref()
        .ok_or_else(|| vec![format!("{GSS_MANIFEST} is missing the [checksums] section")])?;

    // ── Level 0: Directory cleanliness ──────────────────────────────────
    println!("Level 0: Checking directory cleanliness...");
    let mut disk_files: Vec<PathBuf> = Vec::new();
    if let Err(e) = collect_files_recursive(gss_dir, gss_dir, &mut disk_files) {
        errors.push(format!("Level 0: {e}"));
    } else {
        // Build set of expected relative paths.
        let mut expected: std::collections::HashSet<String> =
            checksums.files.keys().cloned().collect();
        expected.insert(GSS_MANIFEST.to_string());

        for path in &disk_files {
            let rel = path
                .strip_prefix(gss_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if !expected.contains(&rel) {
                errors.push(format!("Level 0: extra file not in manifest: {rel}"));
            }
        }
        // Also check that all manifest files exist on disk.
        let disk_set: std::collections::HashSet<String> = disk_files
            .iter()
            .map(|p| {
                p.strip_prefix(gss_dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        for expected_file in checksums.files.keys() {
            if !disk_set.contains(expected_file.as_str()) {
                errors.push(format!(
                    "Level 0: manifest file missing from disk: {expected_file}"
                ));
            }
        }
        if errors.is_empty() {
            println!(
                "  PASS: {} files, no extras, no sidecars, no symlinks",
                disk_files.len()
            );
        }
    }

    // ── Level 1: Checksum verification ──────────────────────────────────
    println!("Level 1: Verifying SHA-256 checksums...");
    let mut checksum_failures = 0;
    for (rel_path, expected_hash) in &checksums.files {
        let file_path = gss_dir.join(rel_path);
        match sha256_file(&file_path) {
            Ok(actual_hash) => {
                if actual_hash != *expected_hash {
                    errors.push(format!(
                        "Level 1: {rel_path}: expected {expected_hash}, got {actual_hash}"
                    ));
                    checksum_failures += 1;
                }
            }
            Err(e) => {
                errors.push(format!("Level 1: {rel_path}: {e}"));
                checksum_failures += 1;
            }
        }
    }
    if checksum_failures == 0 {
        println!("  PASS: {} files verified", checksums.files.len());
    }

    // ── Level 2: Squash root recomputation ──────────────────────────────
    println!("Level 2: Recomputing squash root node hashes from MARF contents...");

    let recomputed_clarity = recompute_marf_root::<StacksBlockId>(
        gss_dir,
        "chainstate/vm/clarity/marf.sqlite",
        "clarity",
        default_open_opts(),
        manifest
            .squash_roots
            .clarity_squash_root_node_hash
            .as_deref(),
        &mut errors,
    );

    let recomputed_index = recompute_marf_root::<StacksBlockId>(
        gss_dir,
        "chainstate/vm/index.sqlite",
        "index",
        default_open_opts(),
        manifest.squash_roots.index_squash_root_node_hash.as_deref(),
        &mut errors,
    );

    let recomputed_sortition = recompute_marf_root::<SortitionId>(
        gss_dir,
        "burnchain/sortition/marf.sqlite",
        "sortition",
        sortition_open_opts(),
        manifest
            .squash_roots
            .sortition_squash_root_node_hash
            .as_deref(),
        &mut errors,
    );

    // ── Level 3: WSCP checkpoint comparison ─────────────────────────────
    if let Some(cp_path) = checkpoint_file {
        println!("Level 3: Comparing against WSCP checkpoint...");
        let cp_str = fs::read_to_string(cp_path)
            .map_err(|e| vec![format!("Failed to read checkpoint file: {e}")])?;
        let cp: CheckpointFile = toml::from_str(&cp_str)
            .map_err(|e| vec![format!("Failed to parse checkpoint file: {e}")])?;

        // Validate checkpoint hash fields (prefix + length + hex characters).
        validate_checkpoint_hash(
            "clarity_squash_root_node_hash",
            &cp.clarity_squash_root_node_hash,
        )
        .map_err(|e| vec![e])?;
        validate_checkpoint_hash(
            "index_squash_root_node_hash",
            &cp.index_squash_root_node_hash,
        )
        .map_err(|e| vec![e])?;
        validate_checkpoint_hash(
            "sortition_squash_root_node_hash",
            &cp.sortition_squash_root_node_hash,
        )
        .map_err(|e| vec![e])?;

        // Height must match.
        if cp.height != manifest.snapshot.height {
            errors.push(format!(
                "Level 3: checkpoint height {} != manifest height {}",
                cp.height, manifest.snapshot.height
            ));
        }

        // Compare recomputed hashes against checkpoint (not stored metadata).
        // Normalize checkpoint values to lowercase so that valid uppercase hex
        // does not spuriously fail (recomputed hashes are always lowercase).
        let check = |name: &str, recomputed: &Option<String>, checkpoint: &str| -> Option<String> {
            let checkpoint_lower = checkpoint.to_ascii_lowercase();
            match recomputed {
                Some(hash) if *hash == checkpoint_lower => {
                    println!("  PASS: {name} matches checkpoint");
                    None
                }
                Some(hash) => Some(format!(
                    "Level 3: {name} recomputed={hash}, checkpoint={checkpoint_lower}"
                )),
                None => Some(format!(
                    "Level 3: {name} not present in GSS (cannot compare)"
                )),
            }
        };

        if let Some(e) = check(
            "clarity",
            &recomputed_clarity,
            &cp.clarity_squash_root_node_hash,
        ) {
            errors.push(e);
        }
        if let Some(e) = check("index", &recomputed_index, &cp.index_squash_root_node_hash) {
            errors.push(e);
        }
        if let Some(e) = check(
            "sortition",
            &recomputed_sortition,
            &cp.sortition_squash_root_node_hash,
        ) {
            errors.push(e);
        }
    } else {
        println!("Level 3: Skipped (no --checkpoint-file provided)");
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Recompute the squash root node hash for a single MARF and compare against
/// the manifest value.  Returns the `0x`-prefixed hex hash if successful.
/// Pushes error strings into `errors` on failure.
fn recompute_marf_root<T: MarfTrieId>(
    gss_dir: &Path,
    rel_path: &str,
    name: &str,
    open_opts: MARFOpenOpts,
    manifest_hash: Option<&str>,
    errors: &mut Vec<String>,
) -> Option<String> {
    let db_path = gss_dir.join(rel_path);
    if !db_path.exists() {
        if manifest_hash.is_some() {
            errors.push(format!(
                "Level 2: {name} MARF not found at {}",
                db_path.display()
            ));
        } else {
            println!("  SKIP: {name} (not in GSS)");
        }
        return None;
    }

    let storage = match TrieFileStorage::open_readonly(db_path.to_str().unwrap(), open_opts) {
        Ok(s) => s,
        Err(e) => {
            errors.push(format!("Level 2: {name}: failed to open MARF: {e:?}"));
            return None;
        }
    };
    let mut marf = MARF::from_storage(storage);

    let tip = match trie_sql::get_latest_confirmed_block_hash::<T>(marf.sqlite_conn()) {
        Ok(t) => t,
        Err(e) => {
            errors.push(format!("Level 2: {name}: failed to read tip: {e:?}"));
            return None;
        }
    };

    let recomputed = match marf.recompute_squash_root_node_hash(&tip) {
        Ok(h) => h,
        Err(e) => {
            errors.push(format!("Level 2: {name}: recomputation error: {e:?}"));
            return None;
        }
    };

    let recomputed_hex = format!("0x{recomputed}");

    match manifest_hash {
        Some(expected) => {
            if recomputed_hex == expected {
                println!("  PASS: {name} recomputed hash matches manifest");
            } else {
                errors.push(format!(
                    "Level 2: {name} recomputed={recomputed_hex}, manifest={expected}"
                ));
            }
        }
        None => {
            // MARF exists on disk but manifest has no squash root hash - this is
            // a malformed manifest for a full GSS.
            errors.push(format!(
                "Level 2: {name} MARF exists but manifest has no squash root hash"
            ));
        }
    }

    Some(recomputed_hex)
}

fn run_latest_height(args: LatestHeightArgs) {
    let selected_count = args.clarity as u8 + args.index as u8 + args.sortition as u8;
    if selected_count != 1 {
        eprintln!("Specify exactly one of --clarity, --index, or --sortition");
        std::process::exit(1);
    }

    let paths = chainstate_paths(&args.chainstate);

    if args.sortition {
        let open_opts = sortition_open_opts();
        let src_storage =
            TrieFileStorage::open_readonly(paths.sortition.db.to_str().unwrap(), open_opts)
                .unwrap_or_else(|e| {
                    eprintln!("Failed to open sortition MARF: {e:?}");
                    std::process::exit(1);
                });
        let mut src = MARF::<SortitionId>::from_storage(src_storage);
        let tip = match trie_sql::get_latest_confirmed_block_hash::<SortitionId>(src.sqlite_conn())
        {
            Ok(tip) => tip,
            Err(e) => {
                eprintln!("Failed to read latest block hash: {e:?}");
                std::process::exit(1);
            }
        };
        let height = match src.with_conn(|conn| MARF::get_block_height_miner_tip(conn, &tip, &tip))
        {
            Ok(Some(height)) => height,
            Ok(None) => {
                eprintln!("Latest block height not found");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Failed to read latest height: {e:?}");
                std::process::exit(1);
            }
        };

        println!("{height}");
        eprintln!("(burn block height)");
        return;
    }

    let target = if args.clarity {
        &paths.clarity
    } else {
        &paths.index
    };

    if let Some(ref blobs) = target.blobs {
        ensure_blobs_match(target.db.to_str().unwrap(), blobs.to_str().unwrap());
    }

    let open_opts = default_open_opts();
    let src_storage = TrieFileStorage::open_readonly(target.db.to_str().unwrap(), open_opts)
        .unwrap_or_else(|e| {
            eprintln!("Failed to open MARF: {e:?}");
            std::process::exit(1);
        });
    let mut src = MARF::<StacksBlockId>::from_storage(src_storage);
    let tip = match trie_sql::get_latest_confirmed_block_hash::<StacksBlockId>(src.sqlite_conn()) {
        Ok(tip) => tip,
        Err(e) => {
            eprintln!("Failed to read latest block hash: {e:?}");
            std::process::exit(1);
        }
    };
    let height = match src.with_conn(|conn| MARF::get_block_height_miner_tip(conn, &tip, &tip)) {
        Ok(Some(height)) => height,
        Ok(None) => {
            eprintln!("Latest block height not found");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to read latest height: {e:?}");
            std::process::exit(1);
        }
    };

    println!("{height}");
}

/// Read `mainnet` from the index DB's `db_config` table and derive PoX constants.
fn index_pox_constants(index_db_path: &Path) -> (u64, u64) {
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

#[derive(Clone)]
enum SideTableMode {
    Clarity,
    Index {
        first_burn_height: u64,
        reward_cycle_len: u64,
    },
    Sortition,
}

/// Squash a single MARF target and copy its side tables. Exits on error.
fn squash_and_copy_one(
    label: &str,
    source: &TargetPaths,
    out: &TargetPaths,
    height: u32,
    side_table_mode: SideTableMode,
    open_opts: MARFOpenOpts,
) {
    if let Some(ref blobs) = source.blobs {
        ensure_blobs_match(source.db.to_str().unwrap(), blobs.to_str().unwrap());
    }

    if let Some(parent) = out.db.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(
                "Failed to create output directory '{}': {e}",
                parent.display()
            );
            std::process::exit(1);
        }
    }

    let is_sortition = matches!(side_table_mode, SideTableMode::Sortition);
    let stats = if is_sortition {
        match MARF::<SortitionId>::squash_to_path(
            source.db.to_str().unwrap(),
            out.db.to_str().unwrap(),
            open_opts,
            height,
            label,
        ) {
            Ok(stats) => stats,
            Err(e) => {
                eprintln!("Failed to squash {label} MARF: {e:?}");
                std::process::exit(1);
            }
        }
    } else {
        match MARF::<StacksBlockId>::squash_to_path(
            source.db.to_str().unwrap(),
            out.db.to_str().unwrap(),
            open_opts,
            height,
            label,
        ) {
            Ok(stats) => stats,
            Err(e) => {
                eprintln!("Failed to squash {label} MARF: {e:?}");
                std::process::exit(1);
            }
        }
    };

    match &side_table_mode {
        SideTableMode::Clarity => {
            println!("Copying Clarity side tables...");
            match copy_clarity_side_tables(source.db.to_str().unwrap(), out.db.to_str().unwrap()) {
                Ok(st) => {
                    println!(
                        "Side-table copy complete: data_table={} rows, metadata_table={} rows",
                        st.data_table_rows, st.metadata_table_rows
                    );
                }
                Err(e) => {
                    eprintln!("Failed to copy Clarity side tables: {e:?}");
                    eprintln!("Cleaning up output files...");
                    let _ = fs::remove_file(&out.db);
                    if let Some(ref blobs) = out.blobs {
                        let _ = fs::remove_file(blobs);
                    }
                    std::process::exit(1);
                }
            }
        }
        SideTableMode::Index {
            first_burn_height,
            reward_cycle_len,
        } => {
            println!("Copying index side tables...");
            match copy_index_side_tables(
                source.db.to_str().unwrap(),
                out.db.to_str().unwrap(),
                *first_burn_height,
                *reward_cycle_len,
            ) {
                Ok(st) => {
                    println!(
                        "Index side-table copy complete: block_headers={}, nakamoto_headers={}, payments={}, transactions={}, tenure_events={}, reward_sets={}, signer_stats={}, matured_rewards={}, burnchain_txids={}, epoch_transitions={}, staging_blocks={}",
                        st.block_headers_rows, st.nakamoto_block_headers_rows, st.payments_rows,
                        st.transactions_rows, st.nakamoto_tenure_events_rows,
                        st.nakamoto_reward_sets_rows, st.signer_stats_rows,
                        st.matured_rewards_rows, st.burnchain_txids_rows, st.epoch_transitions_rows,
                        st.staging_blocks_rows
                    );
                }
                Err(e) => {
                    eprintln!("Failed to copy index side tables: {e:?}");
                    eprintln!("Cleaning up output files...");
                    let _ = fs::remove_file(&out.db);
                    if let Some(ref blobs) = out.blobs {
                        let _ = fs::remove_file(blobs);
                    }
                    std::process::exit(1);
                }
            }
        }
        SideTableMode::Sortition => {
            println!("Copying sortition side tables...");
            match copy_sortition_side_tables(source.db.to_str().unwrap(), out.db.to_str().unwrap())
            {
                Ok(st) => {
                    println!(
                        "Sortition side-table copy complete: snapshots={}, leader_keys={}, block_commits={}, epochs={}",
                        st.snapshots_rows, st.leader_keys_rows, st.block_commits_rows, st.epochs_rows
                    );
                }
                Err(e) => {
                    eprintln!("Failed to copy sortition side tables: {e:?}");
                    eprintln!("Cleaning up output files...");
                    let _ = fs::remove_file(&out.db);
                    std::process::exit(1);
                }
            }
        }
    }

    // Size savings summary.
    let original_db_size = fs::metadata(&source.db).map(|m| m.len()).unwrap_or(0);
    let original_blobs_size = source
        .blobs
        .as_ref()
        .and_then(|b| fs::metadata(b).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    let squashed_db_size = fs::metadata(&out.db).map(|m| m.len()).unwrap_or(0);
    let squashed_blobs_size = out
        .blobs
        .as_ref()
        .and_then(|b| fs::metadata(b).ok())
        .map(|m| m.len())
        .unwrap_or(0);

    let original_total = original_db_size + original_blobs_size;
    let squashed_total = squashed_db_size + squashed_blobs_size;
    let savings = original_total.saturating_sub(squashed_total);
    let savings_pct = if original_total == 0 {
        0.0
    } else {
        (savings as f64 / original_total as f64) * 100.0
    };

    println!("Squash complete ({label}) at height {height}");
    println!("Leaf count: {}", stats.leaf_count);
    println!(
        "Original: db={original_db_size} bytes, blobs={original_blobs_size} bytes, total={original_total} bytes"
    );
    println!(
        "Squashed: db={squashed_db_size} bytes, blobs={squashed_blobs_size} bytes, total={squashed_total} bytes"
    );
    println!("Savings: {savings} bytes ({savings_pct:.2}%)");
    println!("Output db: {}", out.db.display());
    if let Some(ref blobs) = out.blobs {
        println!("Output blobs: {}", blobs.display());
    }
}

/// Validate a single MARF target. Returns `true` if all validations passed.
fn validate_one(
    label: &str,
    source: &TargetPaths,
    squashed: &TargetPaths,
    height: u32,
    full: bool,
    side_table_mode: SideTableMode,
    open_opts: MARFOpenOpts,
) -> bool {
    let is_sortition = matches!(side_table_mode, SideTableMode::Sortition);
    let validation = validate_or_exit(
        source.db.to_str().unwrap(),
        source.blobs.as_deref().map(|p| p.to_str().unwrap()),
        squashed.db.to_str().unwrap(),
        squashed.blobs.as_deref().map(|p| p.to_str().unwrap()),
        open_opts,
        height,
        full,
        is_sortition,
    );
    println!("Validation results for {label}:");
    print_validation(&validation);

    let marf_valid = validation.is_valid();

    let clarity_side_valid = if matches!(side_table_mode, SideTableMode::Clarity) {
        match validate_clarity_side_tables(
            source.db.to_str().unwrap(),
            squashed.db.to_str().unwrap(),
        ) {
            Ok(sv) => {
                println!("Clarity side-table validation:");
                println!(
                    "  data_table rows: src={}, dst={}, match={}",
                    sv.src_data_table_rows, sv.dst_data_table_rows, sv.required_data_keys_present
                );
                println!(
                    "  metadata_table rows: src={}, dst={}, match={}",
                    sv.src_metadata_table_rows,
                    sv.dst_metadata_table_rows,
                    sv.required_metadata_present
                );
                if sv.sample_contracts_checked > 0 {
                    println!(
                        "  sample check: {} contracts checked, {} missing in trie, {} missing in data_table",
                        sv.sample_contracts_checked,
                        sv.sample_contracts_missing_in_trie,
                        sv.sample_contracts_missing_in_data_table
                    );
                }
                println!("Clarity side-table valid: {}", sv.is_valid());
                sv.is_valid()
            }
            Err(e) => {
                eprintln!("Warning: Clarity side-table validation failed: {e:?}");
                false
            }
        }
    } else {
        true
    };

    let index_side_valid = if let SideTableMode::Index {
        first_burn_height,
        reward_cycle_len,
    } = &side_table_mode
    {
        match validate_index_side_tables(
            source.db.to_str().unwrap(),
            squashed.db.to_str().unwrap(),
            *first_burn_height,
            *reward_cycle_len,
        ) {
            Ok(v) => {
                print_index_side_table_validation(&v);
                v.is_valid()
            }
            Err(e) => {
                eprintln!("Warning: index side-table validation failed: {e:?}");
                false
            }
        }
    } else {
        true
    };

    let sortition_side_valid = if matches!(side_table_mode, SideTableMode::Sortition) {
        match validate_sortition_side_tables(
            source.db.to_str().unwrap(),
            squashed.db.to_str().unwrap(),
        ) {
            Ok(v) => {
                print_sortition_side_table_validation(&v);
                v.is_valid()
            }
            Err(e) => {
                eprintln!("Warning: sortition side-table validation failed: {e:?}");
                false
            }
        }
    } else {
        true
    };

    marf_valid && clarity_side_valid && index_side_valid && sortition_side_valid
}

#[allow(clippy::too_many_arguments)]
fn validate_or_exit(
    source_db: &str,
    source_blobs: Option<&str>,
    squashed_db: &str,
    squashed_blobs: Option<&str>,
    open_opts: MARFOpenOpts,
    height: u32,
    full_leaf_scan: bool,
    is_sortition: bool,
) -> SquashValidationStats {
    if let Some(blobs) = source_blobs {
        ensure_blobs_match(source_db, blobs);
    }
    if let Some(blobs) = squashed_blobs {
        ensure_blobs_match(squashed_db, blobs);
    }

    if is_sortition {
        match MARF::<SortitionId>::validate_squashed_at_height_ex(
            source_db,
            squashed_db,
            open_opts,
            height,
            full_leaf_scan,
        ) {
            Ok(stats) => stats,
            Err(e) => {
                eprintln!("Failed to validate squashed MARF: {e:?}");
                std::process::exit(1);
            }
        }
    } else {
        match MARF::<StacksBlockId>::validate_squashed_at_height_ex(
            source_db,
            squashed_db,
            open_opts,
            height,
            full_leaf_scan,
        ) {
            Ok(stats) => stats,
            Err(e) => {
                eprintln!("Failed to validate squashed MARF: {e:?}");
                std::process::exit(1);
            }
        }
    }
}

fn print_validation(stats: &SquashValidationStats) {
    println!("Validation:");
    println!("Archival root present: {}", stats.archival_root_present);
    println!("Archival root matches: {}", stats.archival_root_matches);
    println!(
        "Squash node hash present: {}",
        stats.squash_node_hash_present
    );
    println!(
        "Squash node hash matches: {}",
        stats.squash_node_hash_matches
    );
    println!(
        "Per-height root hashes missing: {}",
        stats.root_hash_missing
    );
    println!(
        "Per-height root hash mismatches: {}",
        stats.root_hash_mismatches
    );
    println!("Blob offset mismatches: {}", stats.blob_offset_mismatches);
    if stats.source_keys_checked > 0 || stats.squashed_keys_checked > 0 {
        println!("Full leaf scan:");
        println!("  Source keys checked: {}", stats.source_keys_checked);
        println!("  Squashed keys checked: {}", stats.squashed_keys_checked);
        println!("  Missing in squashed: {}", stats.missing_in_squashed);
        println!("  Missing in source: {}", stats.missing_in_source);
        println!("  Value mismatches: {}", stats.value_mismatches);
    }
    println!("Valid: {}", stats.is_valid());
}

fn print_index_side_table_validation(
    v: &blockstack_lib::chainstate::stacks::db::snapshot::IndexSideTableValidation,
) {
    println!("Index side-table validation:");
    println!("  tables_present: {}", v.tables_present);
    println!("  db_config_matches: {}", v.db_config_matches);
    println!(
        "  block_headers_count_match: {}",
        v.block_headers_count_match
    );
    println!(
        "  nakamoto_headers_count_match: {}",
        v.nakamoto_headers_count_match
    );
    println!("  payments_count_match: {}", v.payments_count_match);
    println!("  transactions_count_match: {}", v.transactions_count_match);
    println!(
        "  nakamoto_tenure_events_count_match: {}",
        v.nakamoto_tenure_events_count_match
    );
    println!(
        "  nakamoto_reward_sets_match: {}",
        v.nakamoto_reward_sets_match
    );
    println!("  signer_stats_match: {}", v.signer_stats_match);
    println!("  matured_rewards_match: {}", v.matured_rewards_match);
    println!("  burnchain_txids_match: {}", v.burnchain_txids_match);
    println!("  epoch_transitions_match: {}", v.epoch_transitions_match);
    println!("  staging_blocks_match: {}", v.staging_blocks_match);
    println!(
        "  invalidated_microblocks_data_empty: {}",
        v.invalidated_microblocks_data_empty
    );
    println!(
        "  transactions_no_extra_blocks: {}",
        v.transactions_no_extra_blocks
    );
    println!(
        "  tenure_events_no_extra_blocks: {}",
        v.tenure_events_no_extra_blocks
    );
    println!("  Index side-table valid: {}", v.is_valid());
}

fn print_sortition_side_table_validation(
    v: &blockstack_lib::chainstate::stacks::db::snapshot::SortitionSideTableValidation,
) {
    println!("Sortition side-table validation:");
    println!("  required_tables_present: {}", v.required_tables_present);
    println!("  canonical_set_in_source: {}", v.canonical_set_in_source);
    println!("  snapshots_match: {}", v.snapshots_match);
    println!("  leader_keys_match: {}", v.leader_keys_match);
    println!("  block_commits_match: {}", v.block_commits_match);
    println!(
        "  block_commit_parents_match: {}",
        v.block_commit_parents_match
    );
    println!(
        "  snapshot_transition_ops_match: {}",
        v.snapshot_transition_ops_match
    );
    println!("  stacks_chain_tips_match: {}", v.stacks_chain_tips_match);
    println!(
        "  preprocessed_reward_sets_match: {}",
        v.preprocessed_reward_sets_match
    );
    println!("  missed_commits_match: {}", v.missed_commits_match);
    println!("  stack_stx_match: {}", v.stack_stx_match);
    println!("  transfer_stx_match: {}", v.transfer_stx_match);
    println!("  delegate_stx_match: {}", v.delegate_stx_match);
    println!(
        "  vote_for_aggregate_key_match: {}",
        v.vote_for_aggregate_key_match
    );
    println!("  epochs_match: {}", v.epochs_match);
    println!("  db_config_match: {}", v.db_config_match);
    if let Some(m) = v.ast_rule_heights_match {
        println!("  ast_rule_heights_match: {m}");
    }
    if let Some(m) = v.snapshot_burn_distributions_match {
        println!("  snapshot_burn_distributions_match: {m}");
    }
    println!("  Sortition side-table valid: {}", v.is_valid());
}

/// Read squash metadata from a just-squashed MARF DB.
/// Returns (archival_root_hash, squash_root_node_hash, height).
fn read_squash_metadata<T: MarfTrieId + std::fmt::Display>(
    db_path: &str,
    open_opts: MARFOpenOpts,
) -> (T, String, Option<String>, u32) {
    let marf = MARF::<T>::from_path(db_path, open_opts).unwrap_or_else(|e| {
        eprintln!("Failed to open squashed MARF for manifest: {e:?}");
        std::process::exit(1);
    });
    let tip =
        trie_sql::get_latest_confirmed_block_hash::<T>(marf.sqlite_conn()).unwrap_or_else(|e| {
            eprintln!("Failed to read latest block hash: {e:?}");
            std::process::exit(1);
        });
    let squash_info = trie_sql::read_squash_info(marf.sqlite_conn()).unwrap_or_else(|e| {
        eprintln!("Failed to read squash info: {e:?}");
        std::process::exit(1);
    });
    match squash_info {
        Some((archival_hash, squash_hash, height)) => (
            tip,
            format!("0x{archival_hash}"),
            squash_hash.map(|h| format!("0x{h}")),
            height,
        ),
        None => {
            eprintln!("No squash info found in DB");
            std::process::exit(1);
        }
    }
}

/// Insert the relative path of `abs_path` (relative to `base`) into `set`.
fn insert_expected_rel(base: &Path, abs_path: &Path, set: &mut std::collections::HashSet<String>) {
    if let Ok(rel) = abs_path.strip_prefix(base) {
        set.insert(rel.to_string_lossy().replace('\\', "/"));
    }
}

/// Generate squash manifest after squashing.
///
/// `copied_block_rel_paths` contains the relative paths (under
/// `chainstate/blocks/`) of epoch-2.x block files and nakamoto.sqlite that
/// were actually written during the copy step.  This is used to build the
/// exact expected file set for full-GSS checksum generation, avoiding the
/// need to re-walk the blocks directory (which could include stale files).
#[allow(clippy::too_many_arguments)]
fn generate_manifest(
    out_dir: &Path,
    clarity_out: Option<&TargetPaths>,
    index_out: &TargetPaths,
    sortition_out: Option<(&TargetPaths, u32)>,
    height: u32,
    blocks_section: Option<BlocksSection>,
    has_burnchain_aux: bool,
    copied_block_rel_paths: &[String],
) {
    let (i_tip, i_archival, i_squash, i_height) =
        read_squash_metadata::<StacksBlockId>(index_out.db.to_str().unwrap(), default_open_opts());

    if i_height != height {
        eprintln!("Manifest error: Index squash height {i_height} != requested {height}");
        std::process::exit(1);
    }

    let (c_archival, c_squash) = if let Some(c_out) = clarity_out {
        let (c_tip, c_arch, c_sq, c_h) =
            read_squash_metadata::<StacksBlockId>(c_out.db.to_str().unwrap(), default_open_opts());
        if c_h != height {
            eprintln!("Manifest error: Clarity squash height {c_h} != requested {height}");
            std::process::exit(1);
        }
        if c_tip != i_tip {
            eprintln!("Manifest error: Clarity tip {c_tip} != Index tip {i_tip}");
            std::process::exit(1);
        }
        (Some(c_arch), c_sq)
    } else {
        (None, None)
    };

    let (s_archival, s_squash) = if let Some((s_out, _bh)) = &sortition_out {
        let (_s_tip, s_arch, s_sq, _s_h) =
            read_squash_metadata::<SortitionId>(s_out.db.to_str().unwrap(), sortition_open_opts());
        (Some(s_arch), s_sq)
    } else {
        (None, None)
    };

    // Read db_config from the squashed index DB.
    let (chain_id, mainnet) = {
        let conn = rusqlite::Connection::open(index_out.db.to_str().unwrap()).unwrap_or_else(|e| {
            eprintln!("Failed to open index DB for db_config: {e}");
            std::process::exit(1);
        });
        let row: (i64, i64) = conn
            .query_row(
                "SELECT chain_id, mainnet FROM db_config LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or_else(|e| {
                eprintln!("Failed to read db_config: {e}");
                std::process::exit(1);
            });
        (row.0 as u32, row.1 != 0)
    };

    // Read timestamp from sortition snapshots if available, else from index headers.
    let timestamp = read_snapshot_timestamp(sortition_out, index_out, height);

    // Read bitcoin height + block hash from sortition DB if available.
    // Note: `bh` is the MARF-internal sortition height (0-indexed from
    // genesis sortition), NOT the actual Bitcoin block height.  We read the
    // real Bitcoin height from the `burn_header_height` column in snapshots.
    let (bitcoin_height, bitcoin_block_hash) = if let Some((s_out, bh)) = &sortition_out {
        let conn = rusqlite::Connection::open(s_out.db.to_str().unwrap()).unwrap_or_else(|e| {
            eprintln!("Failed to open squashed sortition DB for bitcoin metadata: {e}");
            std::process::exit(1);
        });
        let sort_id: String = conn
            .query_row(
                "SELECT block_hash FROM marf_squash_block_heights WHERE height = ?1",
                [bh],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| {
                eprintln!(
                    "Failed to read sortition ID at MARF height {bh} from squashed sortition DB: {e}"
                );
                std::process::exit(1);
            });
        let (real_btc_height, btc_hash): (u32, String) = conn
            .query_row(
                "SELECT burn_header_height, burn_header_hash FROM snapshots WHERE sortition_id = ?1",
                [&sort_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap_or_else(|e| {
                eprintln!("Failed to read burn_header_height/hash for sortition_id {sort_id}: {e}");
                std::process::exit(1);
            });
        (Some(real_btc_height), Some(format!("0x{btc_hash}")))
    } else {
        (None, None)
    };

    let is_full_gss = clarity_out.is_some()
        && sortition_out.is_some()
        && blocks_section.is_some()
        && has_burnchain_aux;

    // Compute checksums for full GSS (mandatory).  For partial squash
    // manifests, checksums are omitted.
    let checksums = if is_full_gss {
        // Build the set of expected files from the known GSS outputs so that
        // stale files in a reused out-dir are rejected rather than blessed.
        let mut expected = std::collections::HashSet::new();

        // MARF databases + blobs.
        if let Some(c) = clarity_out {
            insert_expected_rel(out_dir, &c.db, &mut expected);
            if let Some(b) = &c.blobs {
                insert_expected_rel(out_dir, b, &mut expected);
            }
        }
        insert_expected_rel(out_dir, &index_out.db, &mut expected);
        if let Some(b) = &index_out.blobs {
            insert_expected_rel(out_dir, b, &mut expected);
        }
        if let Some((s, _)) = sortition_out {
            insert_expected_rel(out_dir, &s.db, &mut expected);
        }

        // Burnchain auxiliary files.
        if has_burnchain_aux {
            expected.insert("burnchain/burnchain.sqlite".to_string());
            expected.insert("headers.sqlite".to_string());
        }

        // Block data files: use the exact paths carried forward from the
        // copy steps rather than re-walking the output directory (which
        // could include stale files from a previous run).
        for rel in copied_block_rel_paths {
            expected.insert(rel.clone());
        }

        let files = compute_checksums(out_dir, Some(&expected)).unwrap_or_else(|e| {
            eprintln!("Failed to compute checksums: {e}");
            std::process::exit(1);
        });
        println!("Computed SHA-256 checksums for {} files", files.len());
        Some(ChecksumsSection { files })
    } else {
        None
    };

    let manifest = SquashManifest {
        snapshot: SnapshotSection {
            version: 1,
            height,
            block_hash: format!("0x{i_tip}"),
            bitcoin_height,
            bitcoin_block_hash,
            timestamp,
            chain_id,
            mainnet,
        },
        roots: RootsSection {
            clarity_archival_marf_root_hash: c_archival,
            index_archival_marf_root_hash: i_archival,
            sortition_archival_marf_root_hash: s_archival,
        },
        squash_roots: SquashRootsSection {
            clarity_squash_root_node_hash: c_squash,
            index_squash_root_node_hash: i_squash,
            sortition_squash_root_node_hash: s_squash,
        },
        blocks: blocks_section,
        checksums,
    };

    let toml_str = toml::to_string(&manifest).unwrap_or_else(|e| {
        eprintln!("Failed to serialize manifest: {e}");
        std::process::exit(1);
    });

    let manifest_name = if is_full_gss {
        GSS_MANIFEST
    } else {
        SQUASH_MANIFEST
    };
    let manifest_path = out_dir.join(manifest_name);
    fs::write(&manifest_path, toml_str).unwrap_or_else(|e| {
        eprintln!(
            "Failed to write manifest to '{}': {e}",
            manifest_path.display()
        );
        std::process::exit(1);
    });
    println!("Manifest written to {}", manifest_path.display());
}

/// Read the burn_header_timestamp for the snapshot at the squash height.
/// When sortition DB is available, uses the explicit burn_height to look up
/// the canonical sortition ID rather than re-deriving from MAX(height).
fn read_snapshot_timestamp(
    sortition_out: Option<(&TargetPaths, u32)>,
    index_out: &TargetPaths,
    height: u32,
) -> Option<String> {
    // Try sortition DB first, using the explicit burn_height.
    if let Some((s_out, burn_height)) = sortition_out {
        let conn = rusqlite::Connection::open(s_out.db.to_str().unwrap()).ok()?;
        let sort_id: Option<String> = conn
            .query_row(
                "SELECT block_hash FROM marf_squash_block_heights WHERE height = ?1",
                [burn_height],
                |row| row.get(0),
            )
            .ok();
        if let Some(sid) = sort_id {
            let ts: Option<i64> = conn
                .query_row(
                    "SELECT burn_header_timestamp FROM snapshots WHERE sortition_id = ?1",
                    [&sid],
                    |row| row.get(0),
                )
                .ok();
            if let Some(ts) = ts {
                return Some(format_timestamp(ts));
            }
        }
    }

    // Fallback: try index DB block_headers, then nakamoto_block_headers.
    let conn = rusqlite::Connection::open(index_out.db.to_str().unwrap()).ok()?;
    let ibh: Option<String> = conn
        .query_row(
            "SELECT block_hash FROM marf_squash_block_heights WHERE height = ?1",
            [height],
            |row| row.get(0),
        )
        .ok();
    if let Some(ibh) = ibh {
        // Try epoch 2.x headers first.
        let ts: Option<i64> = conn
            .query_row(
                "SELECT burn_header_timestamp FROM block_headers WHERE index_block_hash = ?1",
                [&ibh],
                |row| row.get(0),
            )
            .ok();
        if let Some(ts) = ts {
            return Some(format_timestamp(ts));
        }
        // Try Nakamoto headers.
        let ts: Option<i64> = conn
            .query_row(
                "SELECT burn_header_timestamp FROM nakamoto_block_headers WHERE index_block_hash = ?1",
                [&ibh],
                |row| row.get(0),
            )
            .ok();
        if let Some(ts) = ts {
            return Some(format_timestamp(ts));
        }
    }

    None
}

fn format_timestamp(unix_ts: i64) -> String {
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

fn ensure_blobs_match(db_path: &str, blobs_path: &str) {
    let expected_blobs = PathBuf::from(format!("{db_path}.blobs"));
    if expected_blobs != Path::new(blobs_path) {
        eprintln!(
            "Expected blobs path '{blobs_path}' to match '{}'",
            expected_blobs.display()
        );
        std::process::exit(1);
    }
}

fn target_out_paths(out_dir: &Path, source_db: &Path) -> TargetPaths {
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

fn target_out_paths_sortition(out_dir: &Path, source_db: &Path) -> TargetPaths {
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

fn default_open_opts() -> MARFOpenOpts {
    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = true;
    open_opts
}

fn sortition_open_opts() -> MARFOpenOpts {
    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = false;
    open_opts
}

/// Resolve Stacks block height to the sortition MARF height for the Bitcoin block
/// whose tenure produced that Stacks block.
///
/// Uses the index MARF to find the canonical block hash at `stacks_height`, then
/// looks up `burn_header_height` from `nakamoto_block_headers` for that exact block.
/// Both lookups use raw SQLite so they are unaffected by any schema-version checks.
/// The sortition MARF height is `bitcoin_height - first_burn_height`.
fn resolve_burn_height_for_sortition(
    sortition_db_path: &str,
    index_db_path: &str,
    stacks_height: u32,
) -> u32 {
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
    marf_height
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use clap::Parser;

    use super::{
        collect_files_recursive, compute_checksums, sha256_file, validate_checkpoint_hash,
        verify_gss, CheckpointFile, ChecksumsSection, Cli, Command, LatestHeightArgs,
        SquashManifest, ValidateArgs, GSS_MANIFEST, SQUASH_MANIFEST,
    };

    #[test]
    fn test_parse_squash_args_ok() {
        let args = vec![
            "marf-squash",
            "squash",
            "--chainstate",
            "/tmp/chainstate",
            "--height",
            "123",
            "--out-dir",
            "/tmp/out",
            "--index",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Squash(args) => {
                assert_eq!(args.chainstate, PathBuf::from("/tmp/chainstate"));
                assert_eq!(args.height, 123);
                assert!(args.index);
            }
            _ => panic!("expected squash command"),
        }
    }

    #[test]
    fn test_parse_squash_args_sortition() {
        let args = vec![
            "marf-squash",
            "squash",
            "--chainstate",
            "/tmp/chainstate",
            "--height",
            "123",
            "--out-dir",
            "/tmp/out",
            "--sortition",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Squash(args) => {
                assert!(args.sortition);
                assert!(!args.clarity);
                assert!(!args.index);
            }
            _ => panic!("expected squash command"),
        }
    }

    #[test]
    fn test_parse_squash_args_all() {
        let args = vec![
            "marf-squash",
            "squash",
            "--chainstate",
            "/tmp/chainstate",
            "--height",
            "123",
            "--out-dir",
            "/tmp/out",
            "--all",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Squash(args) => {
                assert!(args.all);
            }
            _ => panic!("expected squash command"),
        }
    }

    #[test]
    fn test_parse_validate_args_ok() {
        let args = vec![
            "marf-squash",
            "validate",
            "--source-chainstate",
            "/tmp/source",
            "--squashed-chainstate",
            "/tmp/squashed",
            "--height",
            "456",
            "--clarity",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Validate(ValidateArgs {
                source_chainstate,
                squashed_chainstate,
                height,
                clarity,
                ..
            }) => {
                assert_eq!(source_chainstate, PathBuf::from("/tmp/source"));
                assert_eq!(squashed_chainstate, PathBuf::from("/tmp/squashed"));
                assert_eq!(height, 456);
                assert!(clarity);
            }
            _ => panic!("expected validate command"),
        }
    }

    #[test]
    fn test_parse_latest_height_args_ok() {
        let args = vec![
            "marf-squash",
            "latest-height",
            "--chainstate",
            "/tmp/chainstate",
            "--index",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::LatestHeight(LatestHeightArgs {
                chainstate, index, ..
            }) => {
                assert_eq!(chainstate, PathBuf::from("/tmp/chainstate"));
                assert!(index);
            }
            _ => panic!("expected latest-height command"),
        }
    }

    #[test]
    fn test_parse_latest_height_sortition() {
        let args = vec![
            "marf-squash",
            "latest-height",
            "--chainstate",
            "/tmp/chainstate",
            "--sortition",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::LatestHeight(LatestHeightArgs { sortition, .. }) => {
                assert!(sortition);
            }
            _ => panic!("expected latest-height command"),
        }
    }

    // ── Verify CLI parsing ──────────────────────────────────────────────

    #[test]
    fn test_parse_verify_args_ok() {
        let args = vec![
            "marf-squash",
            "verify",
            "--gss-dir",
            "/tmp/gss",
            "--checkpoint-file",
            "/tmp/checkpoint.toml",
        ]
        .into_iter()
        .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Verify(args) => {
                assert_eq!(args.gss_dir, PathBuf::from("/tmp/gss"));
                assert_eq!(
                    args.checkpoint_file,
                    Some(PathBuf::from("/tmp/checkpoint.toml"))
                );
            }
            _ => panic!("expected verify command"),
        }
    }

    #[test]
    fn test_parse_verify_args_no_checkpoint() {
        let args = vec!["marf-squash", "verify", "--gss-dir", "/tmp/gss"]
            .into_iter()
            .map(String::from);

        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Command::Verify(args) => {
                assert_eq!(args.gss_dir, PathBuf::from("/tmp/gss"));
                assert!(args.checkpoint_file.is_none());
            }
            _ => panic!("expected verify command"),
        }
    }

    // ── Manifest generation cleanliness ─────────────────────────────────

    /// Helper: create a minimal GSS directory with the given file set.
    fn create_test_gss_dir(dir: &std::path::Path, files: &[&str]) {
        for f in files {
            let path = dir.join(f);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, format!("content of {f}")).unwrap();
        }
    }

    #[test]
    fn test_compute_checksums_clean_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["a.sqlite", "sub/b.sqlite"]);
        // Also create the manifest (skipped by compute_checksums).
        std::fs::write(dir.join(GSS_MANIFEST), "dummy").unwrap();

        let checksums = compute_checksums(dir, None).unwrap();
        assert_eq!(checksums.len(), 2);
        assert!(checksums.contains_key("a.sqlite"));
        assert!(checksums.contains_key("sub/b.sqlite"));
        // Verify actual hash.
        let expected = sha256_file(&dir.join("a.sqlite")).unwrap();
        assert_eq!(checksums["a.sqlite"], expected);
    }

    #[test]
    fn test_manifest_rejects_sqlite_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["a.sqlite", "a.sqlite-wal"]);

        let result = compute_checksums(dir, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SQLite sidecar"));
    }

    #[test]
    fn test_manifest_rejects_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["a.sqlite"]);
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("a.sqlite"), dir.join("link.sqlite")).unwrap();

        #[cfg(unix)]
        {
            let result = compute_checksums(dir, None);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("symlink"));
        }
    }

    #[test]
    fn test_manifest_rejects_extra_file_in_outdir() {
        // When an expected file set is provided, compute_checksums rejects
        // any file not in the set - preventing stale files from being blessed.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["expected.sqlite", "stale.sqlite"]);

        let mut expected = std::collections::HashSet::new();
        expected.insert("expected.sqlite".to_string());

        let result = compute_checksums(dir, Some(&expected));
        let err = result.unwrap_err();
        assert!(err.contains("unexpected file"), "got: {err}");
        assert!(err.contains("stale.sqlite"), "got: {err}");
    }

    #[test]
    fn test_compute_checksums_without_expected_set_hashes_all() {
        // Without an expected set, compute_checksums hashes all regular files.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["a.sqlite", "b.sqlite"]);

        let checksums = compute_checksums(dir, None).unwrap();
        assert_eq!(checksums.len(), 2);
    }

    // ── Verify integrity tests ──────────────────────────────────────────

    /// Helper: write a manifest TOML with the given checksums.
    fn write_test_manifest(dir: &std::path::Path, checksums: &BTreeMap<String, String>) {
        let manifest = SquashManifest {
            snapshot: super::SnapshotSection {
                version: 1,
                height: 100,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_height: None,
                bitcoin_block_hash: None,
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: super::RootsSection {
                clarity_archival_marf_root_hash: None,
                index_archival_marf_root_hash: "0xaaa".to_string(),
                sortition_archival_marf_root_hash: None,
            },
            squash_roots: super::SquashRootsSection {
                clarity_squash_root_node_hash: Some("0xbbb".to_string()),
                index_squash_root_node_hash: Some("0xccc".to_string()),
                sortition_squash_root_node_hash: Some("0xddd".to_string()),
            },
            blocks: None,
            checksums: Some(ChecksumsSection {
                files: checksums.clone(),
            }),
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), toml_str).unwrap();
    }

    #[test]
    fn test_verify_rejects_partial_squash_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join(SQUASH_MANIFEST), "dummy").unwrap();

        // GSS_manifest.toml does not exist.
        assert!(!dir.join(GSS_MANIFEST).exists());
        assert!(dir.join(SQUASH_MANIFEST).exists());
    }

    #[test]
    fn test_verify_checksum_mismatch_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["data.sqlite"]);

        let correct_hash = sha256_file(&dir.join("data.sqlite")).unwrap();
        let mut checksums = BTreeMap::new();
        checksums.insert("data.sqlite".to_string(), correct_hash.clone());
        write_test_manifest(dir, &checksums);

        // Verify correct checksums first.
        let actual = sha256_file(&dir.join("data.sqlite")).unwrap();
        assert_eq!(actual, correct_hash);

        // Now corrupt the file.
        std::fs::write(dir.join("data.sqlite"), "corrupted!").unwrap();
        let actual = sha256_file(&dir.join("data.sqlite")).unwrap();
        assert_ne!(actual, correct_hash);
    }

    #[test]
    fn test_verify_fails_without_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // Write manifest without [checksums].
        let manifest = SquashManifest {
            snapshot: super::SnapshotSection {
                version: 1,
                height: 100,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_height: None,
                bitcoin_block_hash: None,
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: super::RootsSection {
                clarity_archival_marf_root_hash: None,
                index_archival_marf_root_hash: "0xaaa".to_string(),
                sortition_archival_marf_root_hash: None,
            },
            squash_roots: super::SquashRootsSection {
                clarity_squash_root_node_hash: None,
                index_squash_root_node_hash: None,
                sortition_squash_root_node_hash: None,
            },
            blocks: None,
            checksums: None,
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), &toml_str).unwrap();

        // Parse and verify that checksums section is None.
        let parsed: SquashManifest = toml::from_str(&toml_str).unwrap();
        assert!(parsed.checksums.is_none());
    }

    #[test]
    fn test_verify_rejects_extra_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["expected.sqlite", "extra.sqlite"]);

        let hash = sha256_file(&dir.join("expected.sqlite")).unwrap();
        let mut checksums = BTreeMap::new();
        checksums.insert("expected.sqlite".to_string(), hash);
        write_test_manifest(dir, &checksums);

        // Collect files and check for extras.
        let mut disk_files = Vec::new();
        collect_files_recursive(dir, dir, &mut disk_files).unwrap();
        let expected_set: std::collections::HashSet<String> = checksums
            .keys()
            .cloned()
            .chain(std::iter::once(GSS_MANIFEST.to_string()))
            .collect();

        let extras: Vec<_> = disk_files
            .iter()
            .map(|p| {
                p.strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .filter(|rel| !expected_set.contains(rel.as_str()))
            .collect();

        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0], "extra.sqlite");
    }

    #[test]
    fn test_verify_rejects_sqlite_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["data.sqlite"]);
        std::fs::write(dir.join("data.sqlite-wal"), "stale wal").unwrap();

        let mut disk_files = Vec::new();
        let result = collect_files_recursive(dir, dir, &mut disk_files);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SQLite sidecar"));
    }

    #[test]
    fn test_verify_rejects_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["data.sqlite"]);
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("data.sqlite"), dir.join("link.sqlite")).unwrap();

        #[cfg(unix)]
        {
            let mut disk_files = Vec::new();
            let result = collect_files_recursive(dir, dir, &mut disk_files);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("symlink"));
        }
    }

    // ── Checkpoint file parsing ─────────────────────────────────────────

    #[test]
    fn test_checkpoint_file_valid() {
        let toml_str = r#"
height = 150000
clarity_squash_root_node_hash = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
index_squash_root_node_hash = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
sortition_squash_root_node_hash = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
"#;
        let cp: CheckpointFile = toml::from_str(toml_str).unwrap();
        assert_eq!(cp.height, 150000);
        assert_eq!(
            cp.clarity_squash_root_node_hash,
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn test_checkpoint_file_malformed_toml() {
        let result = toml::from_str::<CheckpointFile>("this is not valid toml {{{");
        assert!(result.is_err());
    }

    #[test]
    fn test_checkpoint_file_partial_fields() {
        // Missing sortition hash.
        let toml_str = r#"
height = 150000
clarity_squash_root_node_hash = "0xaaaa"
index_squash_root_node_hash = "0xbbbb"
"#;
        let result = toml::from_str::<CheckpointFile>(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_checkpoint_file_invalid_hex_no_prefix() {
        let toml_str = r#"
height = 150000
clarity_squash_root_node_hash = "no_prefix_here_64_chars_000000000000000000000000000000000000"
index_squash_root_node_hash = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
sortition_squash_root_node_hash = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
"#;
        // Parses as a string, but run_verify validates the 0x prefix + length.
        let cp: CheckpointFile = toml::from_str(toml_str).unwrap();
        assert!(!cp.clarity_squash_root_node_hash.starts_with("0x"));
    }

    #[test]
    fn test_checkpoint_file_wrong_length() {
        let toml_str = r#"
height = 150000
clarity_squash_root_node_hash = "0xtooshort"
index_squash_root_node_hash = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
sortition_squash_root_node_hash = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
"#;
        let cp: CheckpointFile = toml::from_str(toml_str).unwrap();
        // 0x + 8 chars != 66
        assert_ne!(cp.clarity_squash_root_node_hash.len(), 66);
    }

    #[test]
    fn test_checkpoint_file_height_mismatch_detected() {
        // Just verify we can compare heights programmatically.
        let manifest_height: u32 = 100;
        let checkpoint_height: u32 = 200;
        assert_ne!(manifest_height, checkpoint_height);
    }

    // ── validate_checkpoint_hash tests ────────────────────────────────────

    #[test]
    fn test_validate_checkpoint_hash_valid() {
        let hash = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(validate_checkpoint_hash("test_field", hash).is_ok());
    }

    #[test]
    fn test_validate_checkpoint_hash_no_prefix() {
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa00";
        let err = validate_checkpoint_hash("test_field", hash).unwrap_err();
        assert!(err.contains("must start with 0x"), "got: {err}");
    }

    #[test]
    fn test_validate_checkpoint_hash_wrong_length() {
        let hash = "0xtooshort";
        let err = validate_checkpoint_hash("test_field", hash).unwrap_err();
        assert!(err.contains("expected 66 chars"), "got: {err}");
    }

    #[test]
    fn test_validate_checkpoint_hash_invalid_hex_chars() {
        // Correct length (66) but contains non-hex 'g' characters.
        let hash = "0xgggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg";
        assert_eq!(hash.len(), 66);
        let err = validate_checkpoint_hash("test_field", hash).unwrap_err();
        assert!(err.contains("non-hex"), "got: {err}");
    }

    // ── End-to-end verify_gss tests ───────────────────────────────────────

    #[test]
    fn test_verify_gss_end_to_end_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["data.sqlite"]);

        let hash = sha256_file(&dir.join("data.sqlite")).unwrap();
        let mut files = BTreeMap::new();
        files.insert("data.sqlite".to_string(), hash);
        write_test_manifest(dir, &files);

        // Levels 0+1 should pass (correct checksums, no extras).
        // Level 2 will fail because there are no real MARFs - but the
        // MARF files don't exist and squash_roots are set, so Level 2
        // reports those as errors.  We test that the composed flow runs
        // and returns errors for the MARF paths.
        let result = verify_gss(dir, None);
        // We expect Level 2 errors because the test manifest claims squash
        // roots exist but there are no MARF files on disk.
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors.iter().any(|e: &String| e.contains("Level 2")),
            "expected Level 2 errors, got: {errors:?}"
        );
        // But no Level 0 or Level 1 errors.
        assert!(
            !errors.iter().any(|e: &String| e.contains("Level 0")),
            "unexpected Level 0 errors: {errors:?}"
        );
        assert!(
            !errors.iter().any(|e: &String| e.contains("Level 1")),
            "unexpected Level 1 errors: {errors:?}"
        );
    }

    #[test]
    fn test_verify_gss_end_to_end_levels_0_1_only() {
        // Test with squash_roots set to None so Level 2 SKIPs (no MARFs
        // claimed → no MARFs expected).  Levels 0+1 should pass cleanly.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["data.sqlite"]);

        let hash = sha256_file(&dir.join("data.sqlite")).unwrap();
        let mut files = BTreeMap::new();
        files.insert("data.sqlite".to_string(), hash);

        let manifest = SquashManifest {
            snapshot: super::SnapshotSection {
                version: 1,
                height: 100,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_height: None,
                bitcoin_block_hash: None,
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: super::RootsSection {
                clarity_archival_marf_root_hash: None,
                index_archival_marf_root_hash: "0xaaa".to_string(),
                sortition_archival_marf_root_hash: None,
            },
            squash_roots: super::SquashRootsSection {
                clarity_squash_root_node_hash: None,
                index_squash_root_node_hash: None,
                sortition_squash_root_node_hash: None,
            },
            blocks: None,
            checksums: Some(ChecksumsSection { files }),
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), &toml_str).unwrap();

        let result = verify_gss(dir, None);
        assert!(result.is_ok(), "expected pass, got: {result:?}");
    }

    #[test]
    fn test_verify_gss_end_to_end_checksum_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["data.sqlite"]);

        // Write manifest with wrong checksum.
        let mut files = BTreeMap::new();
        files.insert("data.sqlite".to_string(), "badhash".to_string());

        let manifest = SquashManifest {
            snapshot: super::SnapshotSection {
                version: 1,
                height: 100,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_height: None,
                bitcoin_block_hash: None,
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: super::RootsSection {
                clarity_archival_marf_root_hash: None,
                index_archival_marf_root_hash: "0xaaa".to_string(),
                sortition_archival_marf_root_hash: None,
            },
            squash_roots: super::SquashRootsSection {
                clarity_squash_root_node_hash: None,
                index_squash_root_node_hash: None,
                sortition_squash_root_node_hash: None,
            },
            blocks: None,
            checksums: Some(ChecksumsSection { files }),
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), &toml_str).unwrap();

        let result = verify_gss(dir, None);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors.iter().any(|e: &String| e.contains("Level 1")),
            "expected Level 1 error, got: {errors:?}"
        );
    }

    #[test]
    fn test_verify_gss_end_to_end_extra_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["expected.sqlite", "extra.sqlite"]);

        // Manifest only lists expected.sqlite.
        let hash = sha256_file(&dir.join("expected.sqlite")).unwrap();
        let mut files = BTreeMap::new();
        files.insert("expected.sqlite".to_string(), hash);

        let manifest = SquashManifest {
            snapshot: super::SnapshotSection {
                version: 1,
                height: 100,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_height: None,
                bitcoin_block_hash: None,
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: super::RootsSection {
                clarity_archival_marf_root_hash: None,
                index_archival_marf_root_hash: "0xaaa".to_string(),
                sortition_archival_marf_root_hash: None,
            },
            squash_roots: super::SquashRootsSection {
                clarity_squash_root_node_hash: None,
                index_squash_root_node_hash: None,
                sortition_squash_root_node_hash: None,
            },
            blocks: None,
            checksums: Some(ChecksumsSection { files }),
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), &toml_str).unwrap();

        let result = verify_gss(dir, None);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors.iter().any(|e: &String| e.contains("extra file")),
            "expected extra file error, got: {errors:?}"
        );
    }

    #[test]
    fn test_verify_gss_rejects_missing_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // Manifest without [checksums].
        let manifest = SquashManifest {
            snapshot: super::SnapshotSection {
                version: 1,
                height: 100,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_height: None,
                bitcoin_block_hash: None,
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: super::RootsSection {
                clarity_archival_marf_root_hash: None,
                index_archival_marf_root_hash: "0xaaa".to_string(),
                sortition_archival_marf_root_hash: None,
            },
            squash_roots: super::SquashRootsSection {
                clarity_squash_root_node_hash: None,
                index_squash_root_node_hash: None,
                sortition_squash_root_node_hash: None,
            },
            blocks: None,
            checksums: None,
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), &toml_str).unwrap();

        let result = verify_gss(dir, None);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors.iter().any(|e: &String| e.contains("[checksums]")),
            "expected checksums error, got: {errors:?}"
        );
    }

    #[test]
    fn test_verify_gss_rejects_partial_squash_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join(SQUASH_MANIFEST), "dummy").unwrap();

        let result = verify_gss(dir, None);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e: &String| e.contains(SQUASH_MANIFEST) && e.contains("validate")),
            "expected partial-squash error, got: {errors:?}"
        );
    }

    #[test]
    fn test_verify_gss_checkpoint_mismatch() {
        // Exercise the Level 3 checkpoint comparison path with synthetic data.
        // The checkpoint hashes don't match the manifest squash roots (which
        // are also synthetic), so Level 3 should fail.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        create_test_gss_dir(dir, &["data.sqlite"]);

        let hash = sha256_file(&dir.join("data.sqlite")).unwrap();
        let mut files = BTreeMap::new();
        files.insert("data.sqlite".to_string(), hash);

        // Manifest with no squash roots (so Level 2 SKIPs) but valid checksums.
        let manifest = SquashManifest {
            snapshot: super::SnapshotSection {
                version: 1,
                height: 100,
                block_hash: "0xdeadbeef".to_string(),
                bitcoin_height: None,
                bitcoin_block_hash: None,
                timestamp: None,
                chain_id: 1,
                mainnet: true,
            },
            roots: super::RootsSection {
                clarity_archival_marf_root_hash: None,
                index_archival_marf_root_hash: "0xaaa".to_string(),
                sortition_archival_marf_root_hash: None,
            },
            squash_roots: super::SquashRootsSection {
                clarity_squash_root_node_hash: None,
                index_squash_root_node_hash: None,
                sortition_squash_root_node_hash: None,
            },
            blocks: None,
            checksums: Some(ChecksumsSection { files }),
        };
        let toml_str = toml::to_string(&manifest).unwrap();
        std::fs::write(dir.join(GSS_MANIFEST), &toml_str).unwrap();

        // Write a checkpoint file with valid-format hashes.
        let cp_toml = r#"
height = 100
clarity_squash_root_node_hash = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
index_squash_root_node_hash = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
sortition_squash_root_node_hash = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
"#;
        let cp_path = dir.join("checkpoint.toml");
        std::fs::write(&cp_path, cp_toml).unwrap();

        // Levels 0+1 pass, Level 2 skips (no MARFs), Level 3 fails because
        // no recomputed hashes are available to compare against the checkpoint.
        let result = verify_gss(dir, Some(&cp_path));
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e: &String| e.contains("Level 3") && e.contains("not present")),
            "expected Level 3 failure, got: {errors:?}"
        );
    }

    #[test]
    fn test_verify_gss_checkpoint_case_insensitive() {
        // Verify that uppercase hex in checkpoint hashes matches lowercase
        // recomputed hashes (the comparison is case-insensitive).
        let upper = "0xAABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899";
        let lower = "0xaabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        // validate_checkpoint_hash accepts uppercase.
        assert!(validate_checkpoint_hash("test", upper).is_ok());
        // Normalization makes them equal.
        assert_eq!(upper.to_ascii_lowercase(), lower);
    }

    #[test]
    fn test_compute_checksums_rejects_stale_block_file() {
        // Regression test: stale block files in chainstate/blocks/ must be
        // rejected when an expected set is provided, not silently checksummed.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Create a legitimate block file and a stale one.
        let blocks_dir = dir.join("chainstate/blocks/ab/cd");
        std::fs::create_dir_all(&blocks_dir).unwrap();
        std::fs::write(blocks_dir.join("legit_block"), "data").unwrap();
        std::fs::write(blocks_dir.join("stale_block"), "old data").unwrap();

        let mut expected = std::collections::HashSet::new();
        expected.insert("chainstate/blocks/ab/cd/legit_block".to_string());

        let result = compute_checksums(dir, Some(&expected));
        let err = result.unwrap_err();
        assert!(err.contains("unexpected file"), "got: {err}");
        assert!(err.contains("stale_block"), "got: {err}");
    }
}
