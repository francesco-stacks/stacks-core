use std::fs;
use std::path::{Path, PathBuf};

use blockstack_lib::chainstate::stacks::db::snapshot::{
    copy_index_side_tables, validate_index_side_tables,
};
use blockstack_lib::chainstate::stacks::index::marf::{
    MARFOpenOpts, MarfConnection, SquashValidationStats, MARF,
};
use blockstack_lib::chainstate::stacks::index::storage::{
    TrieFileStorage, TrieHashCalculationMode,
};
use blockstack_lib::chainstate::stacks::index::trie_sql;
use blockstack_lib::clarity_vm::database::marf::{
    copy_clarity_side_tables, validate_clarity_side_tables,
};
use clap::{Parser, Subcommand};
use serde::Serialize;
use stacks_common::types::chainstate::StacksBlockId;

/// Offline squashing CLI for Index and Clarity MARF snapshots.
#[derive(Parser, Debug)]
#[command(
    name = "marf-squash",
    about = "Offline squashing tool for Index and Clarity MARFs"
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
    /// Block height to squash to.
    #[arg(long, value_name = "HEIGHT")]
    height: u32,
    /// Squash the Clarity MARF (chainstate/vm/clarity/marf.sqlite).
    #[arg(long)]
    clarity: bool,
    /// Squash the Index MARF (chainstate/vm/index.sqlite).
    #[arg(long)]
    index: bool,
    /// Squash both Clarity and Index MARFs (burnchain/sortition not yet supported).
    #[arg(long)]
    all: bool,
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
    /// Block height to validate at.
    #[arg(long, value_name = "HEIGHT")]
    height: u32,
    /// Validate the Clarity MARF.
    #[arg(long)]
    clarity: bool,
    /// Validate the Index MARF.
    #[arg(long)]
    index: bool,
    /// Validate both Clarity and Index MARFs (burnchain/sortition not yet supported).
    #[arg(long)]
    all: bool,
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
}

#[derive(Debug, Clone)]
struct TargetPaths {
    db: PathBuf,
    blobs: PathBuf,
}

#[derive(Debug, Clone)]
struct ChainstatePaths {
    clarity: TargetPaths,
    index: TargetPaths,
    // burnchain/sortition can be added later.
}

#[derive(Serialize)]
struct SquashManifest {
    snapshot: SnapshotSection,
    roots: RootsSection,
    squash_roots: SquashRootsSection,
}

#[derive(Serialize)]
struct SnapshotSection {
    version: u32,
    height: u32,
    index_block_hash: String,
    chain_id: u32,
    mainnet: bool,
}

#[derive(Serialize)]
struct RootsSection {
    clarity_archival_marf_root_hash: String,
    index_archival_marf_root_hash: String,
}

#[derive(Serialize)]
struct SquashRootsSection {
    clarity_squash_root_node_hash: Option<String>,
    index_squash_root_node_hash: Option<String>,
}

fn chainstate_paths(root: &Path) -> ChainstatePaths {
    let clarity_db = root.join("chainstate/vm/clarity/marf.sqlite");
    let index_db = root.join("chainstate/vm/index.sqlite");
    ChainstatePaths {
        clarity: TargetPaths {
            blobs: PathBuf::from(format!("{}.blobs", clarity_db.display())),
            db: clarity_db,
        },
        index: TargetPaths {
            blobs: PathBuf::from(format!("{}.blobs", index_db.display())),
            db: index_db,
        },
    }
}

fn selected_targets(clarity: bool, index: bool, all: bool) -> (bool, bool) {
    let (mut c, mut i) = (clarity, index);
    if all {
        c = true;
        i = true;
    }
    (c, i)
}

fn ensure_targets_selected(clarity: bool, index: bool, all: bool) {
    let (c, i) = selected_targets(clarity, index, all);
    if !c && !i {
        eprintln!("Must specify at least one target: --clarity, --index, or --all");
        std::process::exit(1);
    }
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Squash(args) => run_squash(args),
        Command::Validate(args) => run_validate(args),
        Command::LatestHeight(args) => run_latest_height(args),
    }
}

fn run_squash(args: SquashArgs) {
    ensure_targets_selected(args.clarity, args.index, args.all);

    let paths = chainstate_paths(&args.chainstate);
    let (do_clarity, do_index) = selected_targets(args.clarity, args.index, args.all);

    if let Err(e) = fs::create_dir_all(&args.out_dir) {
        eprintln!(
            "Failed to create output directory '{}': {e}",
            args.out_dir.display()
        );
        std::process::exit(1);
    }

    let mut all_valid = true;
    let mut clarity_out = None;
    let mut index_out = None;

    if do_clarity {
        let out = target_out_paths(&args.out_dir, &paths.clarity.db);
        if !squash_one(
            "clarity",
            &paths.clarity,
            &out,
            args.height,
            args.skip_validate,
            args.full,
            SideTableMode::Clarity,
        ) {
            all_valid = false;
        }
        clarity_out = Some(out);
    }

    if do_index {
        let out = target_out_paths(&args.out_dir, &paths.index.db);
        if !squash_one(
            "index",
            &paths.index,
            &out,
            args.height,
            args.skip_validate,
            args.full,
            SideTableMode::Index(args.height),
        ) {
            all_valid = false;
        }
        index_out = Some(out);
    }

    if !all_valid {
        eprintln!("Validation failed for one or more targets");
        std::process::exit(1);
    }

    // Generate squash_manifest.toml when both targets squashed and validated successfully.
    if !args.skip_validate {
        if let (Some(ref c_out), Some(ref i_out)) = (&clarity_out, &index_out) {
            generate_manifest(&args.out_dir, c_out, i_out, args.height);
        }
    }
}

fn run_validate(args: ValidateArgs) {
    ensure_targets_selected(args.clarity, args.index, args.all);

    let source_paths = chainstate_paths(&args.source_chainstate);
    let squashed_paths = chainstate_paths(&args.squashed_chainstate);
    let (do_clarity, do_index) = selected_targets(args.clarity, args.index, args.all);

    let mut all_valid = true;

    if do_clarity {
        if !validate_one(
            "clarity",
            &source_paths.clarity,
            &squashed_paths.clarity,
            args.height,
            args.full,
            SideTableMode::Clarity,
        ) {
            all_valid = false;
        }
    }

    if do_index {
        if !validate_one(
            "index",
            &source_paths.index,
            &squashed_paths.index,
            args.height,
            args.full,
            SideTableMode::Index(args.height),
        ) {
            all_valid = false;
        }
    }

    if !all_valid {
        eprintln!("Validation failed for one or more targets");
        std::process::exit(1);
    }
}

fn run_latest_height(args: LatestHeightArgs) {
    let (do_clarity, do_index) = selected_targets(args.clarity, args.index, false);
    if do_clarity == do_index {
        eprintln!("Specify exactly one of --clarity or --index");
        std::process::exit(1);
    }

    let paths = chainstate_paths(&args.chainstate);
    let target = if do_clarity {
        &paths.clarity
    } else {
        &paths.index
    };

    ensure_blobs_match(target.db.to_str().unwrap(), target.blobs.to_str().unwrap());

    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = true;

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

#[derive(Clone)]
enum SideTableMode {
    Clarity,
    Index(u32),
}

/// Squash a single MARF target. Returns `true` if validation passed (or was skipped).
fn squash_one(
    label: &str,
    source: &TargetPaths,
    out: &TargetPaths,
    height: u32,
    skip_validate: bool,
    full: bool,
    side_table_mode: SideTableMode,
) -> bool {
    ensure_blobs_match(source.db.to_str().unwrap(), source.blobs.to_str().unwrap());

    if let Some(parent) = out.db.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(
                "Failed to create output directory '{}': {e}",
                parent.display()
            );
            std::process::exit(1);
        }
    }

    let mut open_opts = MARFOpenOpts::default();
    open_opts.hash_calculation_mode = TrieHashCalculationMode::Deferred;
    open_opts.cache_strategy = "noop".to_string();
    open_opts.external_blobs = true;

    let stats = match MARF::<StacksBlockId>::squash_to_path(
        source.db.to_str().unwrap(),
        out.db.to_str().unwrap(),
        open_opts.clone(),
        height,
    ) {
        Ok(stats) => stats,
        Err(e) => {
            eprintln!("Failed to squash {label} MARF: {e:?}");
            std::process::exit(1);
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
                    let _ = fs::remove_file(&out.blobs);
                    std::process::exit(1);
                }
            }
        }
        SideTableMode::Index(h) => {
            println!("Copying index side tables...");
            match copy_index_side_tables(source.db.to_str().unwrap(), out.db.to_str().unwrap(), *h)
            {
                Ok(st) => {
                    println!(
                        "Index side-table copy complete: block_headers={}, nakamoto_headers={}, payments={}, transactions={}, tenure_events={}, reward_sets={}, signer_stats={}, matured_rewards={}, burnchain_txids={}, epoch_transitions={}",
                        st.block_headers_rows, st.nakamoto_block_headers_rows, st.payments_rows,
                        st.transactions_rows, st.nakamoto_tenure_events_rows,
                        st.nakamoto_reward_sets_rows, st.signer_stats_rows,
                        st.matured_rewards_rows, st.burnchain_txids_rows, st.epoch_transitions_rows
                    );
                }
                Err(e) => {
                    eprintln!("Failed to copy index side tables: {e:?}");
                    eprintln!("Cleaning up output files...");
                    let _ = fs::remove_file(&out.db);
                    let _ = fs::remove_file(&out.blobs);
                    std::process::exit(1);
                }
            }
        }
    }

    let validation = if skip_validate {
        None
    } else {
        Some(validate_or_exit(
            source.db.to_str().unwrap(),
            source.blobs.to_str().unwrap(),
            out.db.to_str().unwrap(),
            out.blobs.to_str().unwrap(),
            open_opts,
            height,
            full,
        ))
    };

    let (side_table_validation, clarity_side_err) = if !skip_validate
        && matches!(side_table_mode, SideTableMode::Clarity)
    {
        match validate_clarity_side_tables(source.db.to_str().unwrap(), out.db.to_str().unwrap()) {
            Ok(v) => (Some(v), false),
            Err(e) => {
                eprintln!("Clarity side-table validation failed: {e:?}");
                (None, true)
            }
        }
    } else {
        (None, false)
    };

    let index_side_valid = if !skip_validate {
        if let SideTableMode::Index(h) = &side_table_mode {
            match validate_index_side_tables(
                source.db.to_str().unwrap(),
                out.db.to_str().unwrap(),
                *h,
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
        }
    } else {
        true
    };

    let original_db_size = fs::metadata(&source.db).map(|m| m.len()).unwrap_or(0);
    let original_blobs_size = fs::metadata(&source.blobs).map(|m| m.len()).unwrap_or(0);
    let squashed_db_size = fs::metadata(&out.db).map(|m| m.len()).unwrap_or(0);
    let squashed_blobs_size = fs::metadata(&out.blobs).map(|m| m.len()).unwrap_or(0);

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
    println!("Output blobs: {}", out.blobs.display());
    let marf_valid = match validation {
        Some(ref v) => {
            print_validation(v);
            v.is_valid()
        }
        None => {
            println!("Validation skipped");
            true
        }
    };
    let side_valid = if let Some(ref sv) = side_table_validation {
        println!("Side-table validation:");
        println!(
            "  data_table rows: src={}, dst={}, match={}",
            sv.src_data_table_rows, sv.dst_data_table_rows, sv.required_data_keys_present
        );
        println!(
            "  metadata_table rows: src={}, dst={}, match={}",
            sv.src_metadata_table_rows, sv.dst_metadata_table_rows, sv.required_metadata_present
        );
        if sv.sample_contracts_checked > 0 {
            println!(
                "  sample check: {} contracts checked, {} missing in trie, {} missing in data_table",
                sv.sample_contracts_checked,
                sv.sample_contracts_missing_in_trie,
                sv.sample_contracts_missing_in_data_table
            );
        }
        println!("Side-table valid: {}", sv.is_valid());
        sv.is_valid()
    } else if clarity_side_err {
        false
    } else {
        true
    };

    marf_valid && side_valid && index_side_valid
}

/// Validate a single MARF target. Returns `true` if all validations passed.
fn validate_one(
    label: &str,
    source: &TargetPaths,
    squashed: &TargetPaths,
    height: u32,
    full: bool,
    side_table_mode: SideTableMode,
) -> bool {
    let validation = validate_or_exit(
        source.db.to_str().unwrap(),
        source.blobs.to_str().unwrap(),
        squashed.db.to_str().unwrap(),
        squashed.blobs.to_str().unwrap(),
        default_open_opts(),
        height,
        full,
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

    let index_side_valid = if let SideTableMode::Index(h) = &side_table_mode {
        match validate_index_side_tables(
            source.db.to_str().unwrap(),
            squashed.db.to_str().unwrap(),
            *h,
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

    marf_valid && clarity_side_valid && index_side_valid
}

fn validate_or_exit(
    source_db: &str,
    source_blobs: &str,
    squashed_db: &str,
    squashed_blobs: &str,
    open_opts: MARFOpenOpts,
    height: u32,
    full_leaf_scan: bool,
) -> SquashValidationStats {
    ensure_blobs_match(source_db, source_blobs);
    ensure_blobs_match(squashed_db, squashed_blobs);

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
        "  nakamoto_reward_sets_count_match: {}",
        v.nakamoto_reward_sets_count_match
    );
    println!("  signer_stats_count_match: {}", v.signer_stats_count_match);
    println!(
        "  matured_rewards_count_match: {}",
        v.matured_rewards_count_match
    );
    println!(
        "  burnchain_txids_count_match: {}",
        v.burnchain_txids_count_match
    );
    println!(
        "  epoch_transitions_count_match: {}",
        v.epoch_transitions_count_match
    );
    println!("  staging_tables_empty: {}", v.staging_tables_empty);
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

/// Read squash metadata from a just-squashed MARF DB.
/// Must be called immediately after squash+validate, before any extension.
/// Returns (block_hash, archival_root_hash, squash_root_node_hash, height).
fn read_squash_metadata(db_path: &str) -> (StacksBlockId, String, Option<String>, u32) {
    let open_opts = default_open_opts();
    let marf = MARF::<StacksBlockId>::from_path(db_path, open_opts).unwrap_or_else(|e| {
        eprintln!("Failed to open squashed MARF for manifest: {e:?}");
        std::process::exit(1);
    });
    let tip = trie_sql::get_latest_confirmed_block_hash::<StacksBlockId>(marf.sqlite_conn())
        .unwrap_or_else(|e| {
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

/// Generate `squash_manifest.toml` after both MARFs have been squashed.
fn generate_manifest(
    out_dir: &Path,
    clarity_out: &TargetPaths,
    index_out: &TargetPaths,
    height: u32,
) {
    let (c_tip, c_archival, c_squash, c_height) =
        read_squash_metadata(clarity_out.db.to_str().unwrap());
    let (i_tip, i_archival, i_squash, i_height) =
        read_squash_metadata(index_out.db.to_str().unwrap());

    if c_height != height {
        eprintln!("Manifest error: Clarity squash height {c_height} != requested {height}");
        std::process::exit(1);
    }
    if i_height != height {
        eprintln!("Manifest error: Index squash height {i_height} != requested {height}");
        std::process::exit(1);
    }
    if c_tip != i_tip {
        eprintln!("Manifest error: Clarity tip {c_tip} != Index tip {i_tip}");
        std::process::exit(1);
    }

    // Read db_config from the squashed index DB (side tables were copied there).
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

    let manifest = SquashManifest {
        snapshot: SnapshotSection {
            version: 1,
            height,
            index_block_hash: format!("0x{c_tip}"),
            chain_id,
            mainnet,
        },
        roots: RootsSection {
            clarity_archival_marf_root_hash: c_archival,
            index_archival_marf_root_hash: i_archival,
        },
        squash_roots: SquashRootsSection {
            clarity_squash_root_node_hash: c_squash,
            index_squash_root_node_hash: i_squash,
        },
    };

    let toml_str = toml::to_string(&manifest).unwrap_or_else(|e| {
        eprintln!("Failed to serialize manifest: {e}");
        std::process::exit(1);
    });

    let manifest_path = out_dir.join("squash_manifest.toml");
    fs::write(&manifest_path, toml_str).unwrap_or_else(|e| {
        eprintln!(
            "Failed to write manifest to '{}': {e}",
            manifest_path.display()
        );
        std::process::exit(1);
    });
    println!("Manifest written to {}", manifest_path.display());
}

fn ensure_blobs_match(db_path: &str, blobs_path: &str) {
    let expected_blobs = PathBuf::from(format!("{db_path}.blobs"));
    if expected_blobs != PathBuf::from(blobs_path) {
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
        blobs: PathBuf::from(format!("{}.blobs", out_db.display())),
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::{Cli, Command, LatestHeightArgs, ValidateArgs};

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
}
